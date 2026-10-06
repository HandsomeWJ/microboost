# Microboost

> **Fork notice:** this is a personal fork of [alexeygrigorev/microboost](https://github.com/alexeygrigorev/microboost)
> by [Alexey Grigorev](https://github.com/alexeygrigorev), who wrote the original app. All credit for the
> audio pipeline, UI, and VB-CABLE integration goes to him. This fork adds:
>
> - Boost cap raised from 5x to 50x (logarithmic slider, auto-calibrate can recommend up to 50x)
> - System tray: minimise/close hides to the tray and keeps boosting; left-click reopens, right-click for Quit
> - Settings section: auto-start boost, start hidden in tray, launch with Windows (per-user Run key)
> - Speaker echo suppression: cancels/ducks whatever the PC is playing out of the mic (see below)
> - Cross-compiling from macOS via `cargo-xwin` (see Development)

A Windows microphone booster that amplifies your mic for other apps (Discord, Teams, etc.) using a real-time audio pipeline through [VB-CABLE](https://vb-audio.com/Cable/) — a free virtual audio cable driver that creates a pair of connected audio devices (one for input, one for output) so audio can be routed between applications.

<img src="screenshot.png" width="400" alt="Microboost screenshot">

[Watch the demo on Loom](https://www.loom.com/share/8ebfbaf4b31f49fba5b1fdbee01ebd5f)

## How it works

```
Microphone → [capture] → gain × boost → noise gate → ring buffer → [playback] → VB-CABLE
                                                                                    ↓
                                                                Discord/Teams/Zoom picks up
                                                                "CABLE Output" as microphone
```

1. The audio driver delivers mic samples via an input callback
2. Each sample is multiplied by the boost factor (e.g. 2.0x) and clamped to [-1, 1]
3. If the noise gate is calibrated, quiet samples below the learned noise floor are faded to silence
4. Processed samples are written to a lock-free ring buffer (a circular array using atomic operations, so the input and output callbacks never block each other)
5. A separate output callback reads from the ring buffer and writes to VB-CABLE's virtual input
6. Apps like Discord see "CABLE Output" as a microphone and receive the boosted audio

On first launch, the app will offer to download and install VB-CABLE (free) automatically.

## Features

- Real-time microphone boost from 0.1x to 50x (10% to 5000%)
- Auto-calibration: detects your voice level and sets the boost to YouTube-recommended loudness (~-16 dBFS)
- Noise gate: learns your background noise and suppresses it
- Live waveform visualizer: see input vs boosted output in real-time
- Per-microphone profiles: saves boost and noise gate settings per device
- Mic hot-plug detection: auto-switches when devices connect/disconnect
- Automatic VB-CABLE setup on first run
- System tray: minimise/close hides to the tray and keeps boosting; optional start-hidden
- Auto-start boost on launch (toggle in Settings)
- Launch with Windows (toggle in Settings; uses the per-user Run registry key)
- Speaker echo suppression: removes the PC's own playback (podcasts, videos, call audio) from the mic
- Test recording and playback to verify your levels
- Lock-free audio pipeline (96.7 dB SNR)
- Native UI built with egui

## Speaker echo suppression

At high boost the mic also picks up whatever the speakers are playing. Microboost
captures the speaker mix via WASAPI loopback and uses it as a reference:

```
speakers ──► loopback copy ──► adaptive filter ──► estimated echo
mic ──────────────────────────► minus estimated echo ──► duck leftover ──► boost ──► VB-CABLE
```

- **Adaptive cancellation** (default): a partitioned frequency-domain adaptive filter
  learns the speaker→mic path (delay up to 1 s, 85 ms window) and subtracts what it can.
  The leftover is then suppressed per frequency band: in each 5 ms frame, bands where
  leftover speaker audio is louder than your voice are attenuated (down to a floor of
  `−strength`), bands where your voice dominates are left alone. So the speaker audio is
  reduced even while you talk, and your voice level is not changed; the cost is a slightly
  thinner voice in bands the echo also occupies while media plays. Strength sets how hard
  the leftover is pushed down (16× over-prediction at 40 dB, doubling every 10 dB).
  The leftover estimate is learned with minimum statistics, so talking cannot poison it,
  and it keeps working when the linear canceller cancels little (mics often pick speakers
  up as desk-borne bass that is only partly coherent with the signal). Suppression only
  engages when echo is actually detected in the mic, so headphone users are left alone.
- **Adaptive off**: the mic is simply attenuated by `strength` whenever the speakers
  are playing. Predictable, but you are muted while media plays.
- The reference is captured from the Windows default output device unless you pick another
  one under **Reference** (Realtek drivers expose separate "Speakers" and "Headphones"
  endpoints, and a video may play on one while the other is the default). When following
  the default, a change of default restarts the capture; if the reference stays silent
  while the mic hears sound, the UI says so.
- The reference is aligned to the mic by capture timestamps (both WASAPI streams stamp
  their buffers from the same performance counter), with the reference read 15 ms
  earlier than the mic so the echo always lags it. Clock drift between mic and sound
  card is nudged out two samples per callback.
- The stage adds two blocks (~11 ms) of latency. Status is shown live in the section:
  whether the speakers are playing, whether echo is detected, how much is being
  cancelled and suppressed, and whether it currently hears you talking.
- Measured on a real room recording (USB mic needing 50x, desktop speakers, bass-heavy
  pickup, canceller limited to ~5 dB): at 50 dB strength the output is 20 dB down with
  you silent; talking 12 dB above the echo, the speaker audio is 8 dB down, your voice
  passes at gain 0.96 with −19 dB distortion. Synthetic airborne echo: 37 dB and 40 dB.
- It cannot remove a *person* in the room: only sound the PC itself is playing.

## Installation

Download the latest release from the [Releases](https://github.com/alexeygrigorev/microboost/releases) page.

Or build from source:

```bash
git clone https://github.com/alexeygrigorev/microboost.git
cd microboost
make build
```

The executable will be at `target/x86_64-pc-windows-msvc/release/microboost.exe`.

> **Note:** The build requires the MSVC target (`x86_64-pc-windows-msvc`). The Makefile handles this automatically.

## Usage

1. Launch Microboost. If VB-CABLE is not installed, click "Install VB-CABLE" and accept the admin prompt.
2. Select your microphone from the dropdown.
3. Click "Auto-Calibrate" to detect your voice level, or manually set the boost.
4. Click "Start Boost" (or "Accept & Start" after calibration).
5. In your other app (Discord, Teams, etc.), select "CABLE Output" as the microphone input.

Use "Record Test" and "Play" to verify the boost sounds right before going live.

Recordings are saved to `%APPDATA%\Microboost\`.

## Requirements

- Windows 10 or later
- VB-CABLE (installed automatically on first launch, or get it from https://vb-audio.com/Cable/)

## Development

### Build

On Windows:

```bash
make build      # Build release (MSVC target)
make run        # Build and run
make open       # Open the built executable
make kill       # Kill running instance
make clean      # Clean build artifacts
make folder     # Open recordings folder
make rebuild    # Kill, rebuild, then run: make open
```

Cross-compiling from macOS (produces the same `x86_64-pc-windows-msvc` binary):

```bash
brew install rustup llvm lld
rustup-init -y && rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
export PATH="/opt/homebrew/opt/llvm/bin:/opt/homebrew/opt/lld/bin:$PATH"
cargo xwin build --release --target x86_64-pc-windows-msvc
```

### Tests

Unit tests verify the audio pipeline produces identical output at 1x boost:

```bash
cargo test --release --target x86_64-pc-windows-msvc
```

Tests include:
- `src/echo.rs` unit tests — synthetic echo paths through the canceller: convergence, 150 ms delay relocation, double-talk, headphones (no echo), ducking. Pure DSP, so they run on any OS with `cargo test --release --lib echo`
- `passthrough_test` — verifies 1x boost is identity, 2x doubles signal, noise gate works, sample rate conversion is correct
- `cable_loopback` — sends a sine wave through VB-CABLE and measures distortion (requires VB-CABLE installed)
- `deep_compare` — sample-level cross-correlation comparison of pipeline output vs original (SNR, alignment)
- `quality_check` — automated quality analysis: clipping, noise floor, smoothness, frequency balance
- `spectral_check` — frequency band comparison across full recordings

End-to-end test tools (in `src/bin/`):
- `e2e_test` — feeds a WAV through the ring buffer + CABLE and compares direct vs ring buffer output
- `audio_test` — records simultaneously from a mic and CABLE Output for comparison
- `pipeline_test` — feeds a WAV through the full pipeline and records from CABLE

Run the e2e test (requires VB-CABLE):

```bash
cargo run --release --target x86_64-pc-windows-msvc --bin e2e_test
```

## Tech Stack

- [egui](https://github.com/emilk/egui) - Native GUI
- [cpal](https://github.com/RustAudio/cpal) - Audio capture and playback
- [hound](https://github.com/ruuda/hound) - WAV encoding/decoding
- [VB-CABLE](https://vb-audio.com/Cable/) - Virtual audio cable driver

## License

MIT
