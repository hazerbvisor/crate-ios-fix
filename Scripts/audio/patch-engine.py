#!/usr/bin/env python3
"""Apply the experimental iOS PCM sound bridge to a pinned crate engine checkout.

The checkout is pinned by the audio-test workflow. Anchors are strict on purpose:
a moved upstream API must cause the build to fail, never silently skip a patch.
The upstream crate engine is MPL-2.0.
"""
from pathlib import Path
import shutil
import sys

root = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else None
if root is None or not (root / "include/crate.h").is_file():
    raise SystemExit("usage: patch-engine.py /path/to/crate-engine")

source = Path(__file__).resolve().parent
shutil.copyfile(
    source / "sound.rs",
    root / "crates/crate-devices/src/virtio/sound.rs",
)

def replace(path: str, old: str, new: str) -> None:
    file = root / path
    content = file.read_text()
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected 1 anchor, found {count}: {old[:80]!r}")
    file.write_text(content.replace(old, new, 1))

replace("crates/crate-devices/src/virtio/mod.rs",
    "pub mod net;\npub mod queue;",
    "pub mod net;\npub mod sound;\npub mod queue;")

virt = "crates/crate-virt/src/lib.rs"
replace(virt,
    "use crate_devices::virtio::mmio::VirtioMmio;",
    "use crate_devices::virtio::mmio::VirtioMmio;\nuse crate_devices::virtio::sound::Sound;")
replace(virt,
    "    pub input: InputConfig,\n}",
    "    pub input: InputConfig,\n    /// Optional stereo PCM output (virtio-snd).\n    pub sound: Option<Output>,\n}")
replace(virt,
    "        let slots = config.disks.len()",
    "        let slots = config.sound.is_some() as usize + config.disks.len()")
replace(virt,
    "        let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();",
    "        let mut devices: Vec<Box<dyn VirtioDevice>> = Vec::new();\n"
    "        if let Some(output) = config.sound.take() {\n"
    "            devices.push(Box::new(Sound::new(output)));\n"
    "        }")

replace("tools/crate-run/src/main.rs",
    "        consoles,\n        displays,\n        input: InputConfig {",
    "        consoles,\n        displays,\n        sound: None,\n        input: InputConfig {")

cfg = "crates/crate-ffi/src/config.rs"
replace(cfg,
    "pub type StateFn = unsafe extern \"C\" fn(*mut c_void, c_int, c_int);",
    "pub type AudioFn = unsafe extern \"C\" fn(*mut c_void, *const u8, usize);\n"
    "\n#[derive(Clone, Copy)]\npub(crate) struct Audio {\n"
    "    pub context: Context,\n    pub output: AudioFn,\n}\n\n"
    "pub type StateFn = unsafe extern \"C\" fn(*mut c_void, c_int, c_int);")
replace(cfg,
    "    pub(crate) state: Option<StateCallback>,",
    "    pub(crate) state: Option<StateCallback>,\n    pub(crate) audio: Option<Audio>,")
replace(cfg,
    "        state: None,",
    "        state: None,\n        audio: None,")
replace(cfg,
    "/// `crate_config_set_state_callback`.",
    '''/// `crate_config_set_audio_output`.
///
/// # Safety
///
/// The app keeps the context live until its machine has been freed. Calls
/// arrive from the audio worker thread, not a real-time audio callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn crate_config_set_audio_output(
    config: *mut Config,
    output: Option<AudioFn>,
    context: *mut c_void,
) -> Status {
    // SAFETY: the caller's contract.
    unsafe {
        with(config, |c| {
            c.audio = output.map(|output| Audio {
                context: Context(context),
                output,
            });
            Status::Ok
        })
    }
}

/// `crate_config_set_state_callback`.''')

machine = "crates/crate-ffi/src/machine.rs"
replace(machine,
    "    let (sinks, displays) = display::build(config);",
    '''    let sound: Option<crate_virt::Output> = config.audio.map(|audio| {
        Arc::new(move |bytes: &[u8]| {
            // SAFETY: the app keeps its callback context alive while the
            // machine exists. Audio runs on a dedicated virtio-snd thread.
            unsafe { (audio.output)(audio.context.get(), bytes.as_ptr(), bytes.len()) };
        }) as crate_virt::Output
    });
    let (sinks, displays) = display::build(config);''')
replace(machine,
    "        displays,\n        input: InputConfig {",
    "        displays,\n        sound,\n        input: InputConfig {")

header = "include/crate.h"
replace(header,
    "/* ---------------------------------------------------------------- display */",
    '''/* ---------------------------------------------------------------- audio */

/* Experimental output-only virtio-snd: one stereo 48 kHz, signed 16-bit
 * little-endian PCM stream. The host callback runs on a worker thread (never
 * the audio output real-time thread); copy bytes before returning. NULL
 * disables the device. The context must remain valid until the machine is
 * freed. This audio extension does not change the baseline API version. */
typedef void (*crate_audio_fn)(void *context, const uint8_t *pcm, size_t length);
crate_status crate_config_set_audio_output(crate_config *config,
                                          crate_audio_fn output, void *context);

/* ---------------------------------------------------------------- display */''')

print("Applied virtio-snd PCM output patch to", root)
