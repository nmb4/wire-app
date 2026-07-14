# wire-app

Desktop GUI for Wire.

## Run on desktop (Linux / macOS / Windows)

```
cargo run -p wire-app --release
```

On Linux, you need ALSA and DBUS development headers:
```
apt-get install libasound2-dev libdbus-1-dev libtool automake
```

The crate includes a C dependency for echo cancellation (`webrtc-audio-processing`) that needs C build tools to be installed.
On macOS these can be installed with homebrew:
```
brew install automake libtool
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
