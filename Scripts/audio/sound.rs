// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

 //! Experimental virtio-snd (VIRTIO 1.2 section 5.14): one stereo PCM output.
 //! Fixed 48 kHz signed 16-bit little-endian; no input, jacks, or channel maps.
 //! Playback/completions run on a worker, never on the vCPU/MMIO thread.

use std::sync::{mpsc, Arc, Mutex};
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread;
use std::time::Duration;

use crate_core::{info, warn};
use super::{Activation, F_VERSION_1, Interrupt, Queue, VirtioDevice, read_config_bytes};

const ID_SOUND: u32 = 25;
const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;
const PCM_INFO: u32 = 0x0100;
const PCM_SET_PARAMS: u32 = 0x0101;
const PCM_PREPARE: u32 = 0x0102;
const PCM_RELEASE: u32 = 0x0103;
const PCM_START: u32 = 0x0104;
const PCM_STOP: u32 = 0x0105;
const RATE_48000: u8 = 7;
const FORMAT_S16: u8 = 5;
const MAX_TRANSFER: usize = 256 * 1024;

// 0 = not configured, 1 = parameters, 2 = prepared, 3 = running, 4 = stopped.
pub type Output = Arc<dyn Fn(&[u8]) + Send + Sync>;

pub struct Sound {
    output: Output,
    state: Arc<AtomicU8>,
    control: Option<(Queue, Arc<Interrupt>)>,
    tx_wake: Option<mpsc::Sender<()>>,
}

impl Sound {
    pub fn new(output: Output) -> Self {
        info!("virtio-snd: presenting stereo output device to the guest");
        Self {
            output,
            state: Arc::new(AtomicU8::new(0)),
            control: None,
            tx_wake: None,
        }
    }

    fn handle_control(state: &AtomicU8, request: &[u8]) -> Vec<u8> {
        let mut status = S_BAD_MSG;
        let mut extra = Vec::new();
        if request.len() >= 4 {
            let code = u32::from_le_bytes(request[0..4].try_into().unwrap());
            match code {
                PCM_INFO if request.len() >= 16 => {
                    let start = u32::from_le_bytes(request[4..8].try_into().unwrap());
                    let count = u32::from_le_bytes(request[8..12].try_into().unwrap());
                    let size = u32::from_le_bytes(request[12..16].try_into().unwrap());
                    if start == 0 && count == 1 && size == 32 {
                        // virtio_snd_pcm_info: info.hda_fn_nid, features, formats,
                        // rates, direction, channels_min, channels_max, padding.
                        extra.resize(32, 0);
                        extra[8..16].copy_from_slice(&(1u64 << FORMAT_S16).to_le_bytes());
                        extra[16..24].copy_from_slice(&(1u64 << RATE_48000).to_le_bytes());
                        extra[24] = 0; // VIRTIO_SND_D_OUTPUT
                        extra[25] = 2;
                        extra[26] = 2;
                        status = S_OK;
                    }
                }
                PCM_SET_PARAMS if request.len() >= 24 => {
                    let stream = u32::from_le_bytes(request[4..8].try_into().unwrap());
                    let buffer = u32::from_le_bytes(request[8..12].try_into().unwrap());
                    let period = u32::from_le_bytes(request[12..16].try_into().unwrap());
                    let features = u32::from_le_bytes(request[16..20].try_into().unwrap());
                    if stream == 0 && buffer >= period && period >= 256 &&
                        period <= 65536 && buffer <= (MAX_TRANSFER as u32) &&
                        features == 0 && request[20] == 2 &&
                        request[21] == FORMAT_S16 && request[22] == RATE_48000 {
                        state.store(1, Ordering::Release);
                        status = S_OK;
                    } else {
                        status = S_NOT_SUPP;
                    }
                }
                PCM_PREPARE | PCM_START | PCM_STOP | PCM_RELEASE if request.len() >= 8 => {
                    let stream = u32::from_le_bytes(request[4..8].try_into().unwrap());
                    if stream == 0 {
                        let previous = state.load(Ordering::Acquire);
                        match code {
                            PCM_PREPARE if matches!(previous, 1 | 2 | 4) => {
                                state.store(2, Ordering::Release);
                                status = S_OK;
                            }
                            PCM_START if matches!(previous, 2 | 4) => {
                                state.store(3, Ordering::Release);
                                status = S_OK;
                            }
                            PCM_STOP if previous == 3 => {
                                state.store(4, Ordering::Release);
                                status = S_OK;
                            }
                            PCM_RELEASE if matches!(previous, 2 | 4) => {
                                state.store(0, Ordering::Release);
                                status = S_OK;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {
                    status = S_NOT_SUPP;
                }
            }
        }
        let mut reply = status.to_le_bytes().to_vec();
        reply.extend_from_slice(&extra);
        reply
    }

    fn process_control(&mut self) {
        let Some((queue, interrupt)) = &mut self.control else { return; };
        let before = self.state.load(Ordering::Acquire);
        let mut used = false;
        loop {
            let chain = match queue.pop() {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => { warn!("virtio-snd control queue: {e}"); break; }
            };
            let response = match chain.read_all(queue.memory(), 64) {
                Ok(request) => Self::handle_control(&self.state, &request),
                Err(e) => { warn!("virtio-snd control request: {e}"); S_BAD_MSG.to_le_bytes().to_vec() }
            };
            let n = chain.write_all(queue.memory(), &response).unwrap_or(0);
            if queue.add_used(chain.head, n as u32).is_err() { break; }
            used = true;
        }
        if used && queue.needs_interrupt() { interrupt.signal_used(); }
        let after = self.state.load(Ordering::Acquire);
        if before != after {
            info!("virtio-snd: playback state {before} -> {after}");
            // Linux can enqueue playback data before PCM_START. The worker
            // must leave it in the virtqueue until the stream is started.
            // Wake it on START/STOP/RELEASE, even without a new TX kick.
            if let Some(wake) = &self.tx_wake {
                let _ = wake.send(());
            }
        }
    }
}

impl VirtioDevice for Sound {
    fn device_id(&self) -> u32 { ID_SOUND }
    fn features(&self) -> u64 { F_VERSION_1 }
    fn queue_max_sizes(&self) -> &[u16] { &[64, 64, 128, 64] }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // virtio_snd_config: jacks = 0, streams = 1, chmaps = 0
        read_config_bytes(&[0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0], offset, data);
    }

    fn activate(&mut self, activation: Activation) {
        self.reset();
        let mut queues = activation.queues.into_iter();
        let (Some(ctrl), Some(_event), Some(tx), Some(_rx)) =
            (queues.next(), queues.next(), queues.next(), queues.next()) else { return; };
        info!("virtio-snd: queues activated by Linux driver");
        self.control = Some((ctrl, activation.interrupt.clone()));
        let (sender, receiver) = mpsc::channel::<()>();
        let queue = Arc::new(Mutex::new(tx));
        let memory = activation.memory;
        let interrupt = activation.interrupt;
        let state = self.state.clone();
        let output = self.output.clone();
        // Dropping the sender on reset makes this worker exit.
        thread::spawn(move || {
            let mut logged_first_pcm = false;
            while receiver.recv().is_ok() {
                loop {
                    // Never pop/complete early playback buffers before START.
                    // A Linux PCM driver may prime the TX ring in PREPARED
                    // state. Completing those buffers silently loses audio.
                    // STOP and RELEASE deliberately drain pending requests.
                    if matches!(state.load(Ordering::Acquire), 1 | 2) {
                        break;
                    }
                    let chain = match queue.lock().unwrap().pop() {
                        Ok(Some(c)) => c,
                        Ok(None) => break,
                        Err(e) => { warn!("virtio-snd tx queue: {e}"); break; }
                    };
                    let mut status = S_BAD_MSG;
                    let bytes = chain.read_all(&memory, MAX_TRANSFER + 4);
                    if let Ok(bytes) = bytes {
                        if bytes.len() >= 4 {
                            let id = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
                            let pcm = &bytes[4..];
                            if id == 0 && pcm.len() % 4 == 0 && pcm.len() <= MAX_TRANSFER {
                                if state.load(Ordering::Acquire) == 3 && !pcm.is_empty() {
                                    if !logged_first_pcm {
                                        info!("virtio-snd: first Linux PCM packet ({} bytes)", pcm.len());
                                        logged_first_pcm = true;
                                    }
                                    output(pcm);
                                    // Complete at roughly the sample clock rate, so the
                                    // guest cannot flood the host player with data.
                                    let seconds = pcm.len() as f64 / (48_000.0 * 4.0);
                                    thread::sleep(Duration::from_secs_f64(seconds.min(0.5)));
                                }
                                status = S_OK;
                            }
                        }
                    }
                    let mut response = [0u8; 8];
                    response[0..4].copy_from_slice(&status.to_le_bytes());
                    let written = chain.write_all(&memory, &response).unwrap_or(0);
                    let mut q = queue.lock().unwrap();
                    if q.add_used(chain.head, written as u32).is_err() { break; }
                    if q.needs_interrupt() { interrupt.signal_used(); }
                }
            }
        });
        self.tx_wake = Some(sender);
    }

    fn notify(&mut self, index: u16) {
        match index {
            0 => self.process_control(),
            2 => { if let Some(wake) = &self.tx_wake { let _ = wake.send(()); } }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.state.store(0, Ordering::Release);
        self.control = None;
        self.tx_wake = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_one_pcm_output() {
        let sound = Sound::new(Arc::new(|_| {}));
        let mut config = [0u8; 12];
        sound.read_config(0, &mut config);
        assert_eq!(u32::from_le_bytes(config[4..8].try_into().unwrap()), 1);

        let mut query = Vec::new();
        for number in [PCM_INFO, 0, 1, 32] {
            query.extend_from_slice(&number.to_le_bytes());
        }
        let response = Sound::handle_control(&sound.state, &query);
        assert_eq!(&response[..4], &S_OK.to_le_bytes());
        assert_eq!(response.len(), 36);
        assert_eq!(response[4 + 25], 2);
        assert_eq!(response[4 + 26], 2);
    }

    #[test]
    fn rejects_unsupported_pcm_format() {
        let state = AtomicU8::new(0);
        let mut query = vec![0u8; 24];
        query[0..4].copy_from_slice(&PCM_SET_PARAMS.to_le_bytes());
        query[8..12].copy_from_slice(&16384u32.to_le_bytes());
        query[12..16].copy_from_slice(&4096u32.to_le_bytes());
        query[20] = 2;
        query[21] = FORMAT_S16;
        query[22] = RATE_48000;
        assert_eq!(&Sound::handle_control(&state, &query)[..4], &S_OK.to_le_bytes());
        query[21] = 0;
        assert_eq!(&Sound::handle_control(&state, &query)[..4], &S_NOT_SUPP.to_le_bytes());
    }
}
