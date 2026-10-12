# Call audio audit validation — 2026-10-05

The mid-call recording failure is confirmed: existing peer tracks were attached
before the recording session was installed, so those attachments were skipped.
The runtime now installs the session first. A regression test starts and restarts
recording over two real Opus tracks and verifies both WAV files contain audio.

The historical one-way audio failure remains unconfirmed. A's log establishes
that a remote voice track arrived and its recording contained only the local
microphone. B's output format, audio quality, and logs were unavailable.
A's logged devices used mono 48 kHz capture and stereo 48 kHz playback, with
the processor configured for engine-format stereo in both directions. The
output-format defect therefore does not explain A's own playback on that call;
B's output configuration is needed to assess whether it explains B's silence.
The remote voice track was logged as stereo; that narrows B's sending format
without establishing B's output device or A's sending quality. New Opus tracks
now log input format, selected quality, encoding rate/channels, and application
at INFO level so future reports can be checked without recalling settings.

## Validation of the Muse audit

The original read-only OpenCode Mini audit ran with
`opencode/muse-spark-1.3-contributor-free#xhigh`, without a fallback model.

| Finding | Validation and result |
| --- | --- |
| R1: callback errors panic | Confirmed error handling gap. The playback callback now emits silence, discards partial buffers, resets the resampler, logs errors at most once a second, and retries next callback. An injected processor-format error verifies recovery. The claimed **new** surround regression was incorrect: `FixedResampler<f32, 2>` already asserted at construction for more than two channels. Unsupported output formats now return a clear startup error. |
| R2: Windows drains the entire ring | Confirmed pull-policy difference. The ring itself was already bounded to 640 ms, so the claim of unbounded seconds was overstated. All platforms now pull only enough input to fill the callback, including additional chunks needed by Windows' fixed-block resampler. A queued-backlog test verifies bounded staging and continuous output for 200 callbacks per device format. |
| R3: missing test coverage | Confirmed. Partial-frame processing now runs with APM enabled. The recording regression uses the actual runtime stop path and checks completion and inactive events, repeated stop, and restart. |
| C1: Abort retains voice tracks | Confirmed and fixed. Tests abort a peer, reject its late old track, and verify a later recording does not attach that peer. |
| C2: pressing record during auto-record should take manual ownership | The claimed UI scenario is inapplicable: pressing the active recording control sends stop. Duplicate start remains idempotent. Investigation exposed a separate confirmed bug: disabling auto-record changed the preference before the stop helper checked it, so automatic recording kept running. The helper now checks session ownership; tests verify automatic sessions stop and manual sessions survive the preference change. |
| C3: pre-mute recording documentation | Confirmed documentation mismatch. The comment now accurately states that muted capture supplies silence to both recording and call encoders. |
| Opus quality/input mismatch | Confirmed experimentally. Before the fix, Low quality produced 600 packets for two seconds of engine audio, and its RTP increment was 320 rather than 960. The track encoder now converts the declared input channels and sample rate to the selected quality. Capture declares its actual 48 kHz stereo input. Every 20 ms Opus packet now advances the 48 kHz RTP clock by 960, regardless of encoding rate or channel count. The receiver's inverse duration calculation was corrected to rate multiplied by duration. |
| Decoder latency heuristic | No evidence establishes it as the cause of the historical silence; unchanged. |

## Local verification

- `cargo test -p wire --lib`: 24 passed.
- `cargo test -p wire --lib --no-default-features`: 20 passed.
- `cargo test -p wire-app --lib recording`: 20 passed.
- `cargo build --release -p wire-app`: passed.
- Formatting checks for changed Rust files and `git diff --check`: passed.

The wire suite includes a real local two-peer RTP call in both directions at
Low, Medium, High, and Ultra quality. Each direction must receive audible audio,
960 decoded frames per packet, and a 960-tick received duration. Playback tests
exercise mono 16 kHz, mono/stereo 44.1 kHz, and stereo 48 kHz with processing
enabled, partial input, a queued backlog, and recovery from processing errors.
These checks use synthesized microphone input and do not exercise physical
microphone/output devices on two Windows PCs.

The next hardware checkpoint is a call using both participants' actual Windows
devices: verify bidirectional speech before recording, start recording mid-call
on each PC, confirm both speaker files have audio, stop/restart recording, and
check that disabling automatic recording stops only automatic sessions.
If one-way silence recurs, preserve both participants' logs and selected device
formats/quality to distinguish capture, transport, and playback failures.

A requested follow-up audit of the revised changes was not dispatched: automatic
approval review rejected its OpenCode command with “blocked by policy.” Local
validation above completed independently; the revised diff has not received a
second external audit.
