# Experimental Crate VM audio

This branch is an **unverified prototype**, not an upstream-ready audio implementation.

## Goal

Provide Linux guests with one virtio-snd output-only PCM card. The experimental
engine advertises stereo, 48 kHz, signed 16-bit little-endian PCM and forwards
samples to `AudioBridge.swift`, which schedules audio through AVAudioEngine.
The guest output queue is processed by a dedicated worker so guest MMIO/JIT
threads are not delayed by audio playback.

## Build and obtain the IPA

The isolated `.github/workflows/audio-test.yml` runs when
`feat/virtio-audio` is pushed. It:

1. Checks out upstream `oneamendez/crate` at a pinned commit.
2. Applies the strict `Scripts/audio/patch-engine.py` engine changes and
   copies in `Scripts/audio/sound.rs`.
3. Runs Rust checks and sound unit tests.
4. Builds a custom engine XCFramework via `Scripts/build-crate.sh`.
5. Downloads the upstream GPU/network frameworks and StikJIT.
6. Creates `crate-audio-test-unsigned.ipa` and a SHA-256 manifest as an
   Actions artifact.

Find artifacts under the **audio-test-ipa** run in this repository's Actions
tab. The test IPA is **unsigned**: use a compatible developer certificate and
JIT setup to sign and install it, following the original `INSTALL.md`.
Do not overwrite your known-good installation without backing up its VM files.

The original release workflow `.github/workflows/ipa.yml` remains untouched.

## Linux smoke tests

Boot the signed test IPA and open a terminal in Ubuntu / Debian / Arch:

```sh
cat /proc/asound/cards
aplay -l
lsmod | grep -E 'virtio_snd|snd_virtio'
```

If ALSA detects the virtio device, try:

```sh
speaker-test -c 2 -r 48000 -F S16_LE -t sine -l 1
```

If sound is silent, check PulseAudio/PipeWire output selection with
`pactl list short sinks` or `wpctl status`, as well as device volume and
app audio-route settings. Capture `dmesg` messages mentioning virtio/sound.

## Explicit limitations

- Output only. No microphone, jack sensing, channel mapping, or volume controls.
- Only 48 kHz, stereo S16LE; other sample formats/rates are rejected.
- Rudimentary playback pacing; latency, underruns, and dropouts are possible.
- No validated real-device measurements or successful sound test yet.
- Audio output work is on a background queue but is **not** an optimized,
  lock-free real-time renderer. This requires improvement before upstreaming.
- Engine source patch is pinned to commit
  `bd2efb4ae9d8567d581b1e6c2313ac21a112f1f2`. The script intentionally
  fails if upstream moves the API anchor.

## Tests for readiness

- [ ] Rust cargo check and unit tests pass on GitHub Actions
- [ ] Unsigned IPA builds and downloads
- [ ] App signs and launches with JIT enabled
- [ ] Linux lists `virtio_snd` device
- [ ] Stereo audio audible from speakers and Bluetooth headphones
- [ ] 10-minute continuous playback without severe glitches
- [ ] Repeated VM stop/start leaves no audio engine leak
- [ ] Test with 1/2/4 guest CPUs and GPU acceleration intact

Contributions to the native device model should ultimately live in
`oneamendez/crate`; this integration branch keeps an incremental prototype
inside the app fork for rapid testing.
