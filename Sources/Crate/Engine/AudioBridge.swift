// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import AVFoundation
import Foundation

/// Experimental playback endpoint for the engine's virtio-snd PCM callback.
///
/// The engine delivers 48 kHz, stereo, signed 16-bit little-endian samples on
/// its worker thread. Copy them immediately, then convert and schedule them
/// on a serial dispatch queue, never blocking a vCPU or Core Audio render thread.
final class AudioBridge: @unchecked Sendable {
    private let queue = DispatchQueue(label: "com.crate.audio", qos: .userInitiated)
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(
        commonFormat: .pcmFormatFloat32,
        sampleRate: 48_000,
        channels: 2,
        interleaved: false
    )!
    private var pending = 0
    private var enabled = false

    init() {
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: format)
    }

    func start() {
        queue.async { [self] in
            do {
                let session = AVAudioSession.sharedInstance()
                try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
                try session.setActive(true)
                if !engine.isRunning { try engine.start() }
                if !player.isPlaying { player.play() }
                enabled = true
                AppLog.write("audio: output ready (48 kHz stereo S16LE)")
            } catch {
                enabled = false
                AppLog.write("audio: unable to start iOS audio: \(error)")
            }
        }
    }

    func stop() {
        queue.async { [self] in
            enabled = false
            player.stop()
            engine.stop()
            pending = 0
        }
    }

    /// Copies guest-owned data before returning to the Rust engine.
    func receive(_ pointer: UnsafePointer<UInt8>?, count: Int) {
        guard let pointer, count > 0, count <= 256 * 1024, count % 4 == 0 else { return }
        let data = Data(bytes: pointer, count: count)
        queue.async { [self] in
            guard enabled, engine.isRunning, pending < 12 else { return }
            let frames = data.count / 4
            guard frames > 0,
                  let buffer = AVAudioPCMBuffer(pcmFormat: format,
                                                 frameCapacity: AVAudioFrameCount(frames)),
                  let channels = buffer.floatChannelData else { return }
            buffer.frameLength = AVAudioFrameCount(frames)
            let left = channels[0]
            let right = channels[1]
            data.withUnsafeBytes { raw in
                let bytes = raw.bindMemory(to: UInt8.self)
                for index in 0..<frames {
                    let offset = index * 4
                    let l = UInt16(bytes[offset]) | (UInt16(bytes[offset + 1]) << 8)
                    let r = UInt16(bytes[offset + 2]) | (UInt16(bytes[offset + 3]) << 8)
                    left[index] = Float(Int16(bitPattern: l)) / 32768.0
                    right[index] = Float(Int16(bitPattern: r)) / 32768.0
                }
            }
            pending += 1
            player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
                guard let self else { return }
                self.queue.async {
                    self.pending = max(0, self.pending - 1)
                }
            }
        }
    }
}
