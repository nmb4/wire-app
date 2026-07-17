# wire-app

Desktop GUI for Wire.

## Run on desktop (Linux / macOS / Windows)

```
cargo run -p wire-app --release
```

The custom files under `fonts/` and `sound-kit/` are optional. When they are
absent, Wire uses egui's bundled default fonts and runs without interface sounds.

Platform-specific audio architecture, the macOS Bluetooth latency findings, and
the Windows regression checklist are documented in
[`docs/audio-platform-notes.md`](../docs/audio-platform-notes.md).

On Linux, you need ALSA and DBUS development headers:
```
apt-get install libasound2-dev libdbus-1-dev libtool automake
```

The crate includes a C dependency for echo cancellation (`webrtc-audio-processing`) that needs C build tools to be installed.
On macOS these can be installed with homebrew:
```
brew install automake libtool
```

Screen sharing on macOS 12.3 or newer uses ScreenCaptureKit. The first time you
share, allow Wire under **System Settings → Privacy & Security → Screen & System
Audio Recording**, then quit and reopen Wire if macOS asks for a relaunch. For a
stable permission identity (especially on macOS 26), run the bundled `.app`
instead of a newly rebuilt bare executable:

```
cargo install cargo-bundle
cargo bundle -p wire-app --release --no-default-features
open target/release/bundle/osx/Wire.app
```

On Windows, or if the build fails, you can disable the audio processing entirely. You should only use Wire with headphones then.
```
cargo run -p wire-app --release --no-default-features
```

## Local three-instance group-call test

Run one command to start three isolated Wire windows:

```
cargo run -p wire-app --release --no-default-features -- --dev-pair
```

The first process spawns two more participants. All three use temporary, non-persisted node identities and automatically form a full mesh, so every window connects directly to the other two. Calls are accepted automatically. Screen sharing is not started automatically, so click **Share screen** in any window when ready to benchmark.

The title bars include the dev-session name. Logs are truncated per run and kept separately as `%LOCALAPPDATA%\wire\wire-app-dev-<session>-host-<pid>.log`, `...-peer-1-<pid>.log`, and `...-peer-2-<pid>.log`. Normal launches continue using the persisted identity and ordinary log names.

For a fully automatic benchmark run, explicitly add `--dev-auto-share`. Only the host starts capture after the call becomes active:

```
cargo run -p wire-app --release --no-default-features -- --dev-pair --dev-auto-share
```
