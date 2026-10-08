# Agent Learnings
> Read this before starting work. Update it whenever you discover something non-obvious. Keep entries short, verified, and actionable.

## Build & Tooling
- **Windows verification**: Run local audio and recording tests plus the release build; GitHub CI is dormant. Commands: `cargo test -p wire --lib`, `cargo test -p wire --lib --no-default-features`, `cargo test -p wire-app --lib recording`, `cargo build --release -p wire-app`. (2026-10-05)
- **Formatting and build output**: Check changed Rust files with `rustfmt --edition 2021 --config skip_children=true --check <files>` and `git diff --check`. The locally built release executable is `G:\wire-app\target\release\wire-app.exe`. (2026-10-05)

## Audio Pipeline
- **Engine versus device format**: Process complete 960-sample, 10 ms frames of 48 kHz stereo before adapting channels/rate to the output device. Why: Sonora rejects device-sized mono or 44.1 kHz chunks. Downmix stereo with `(left + right) * 0.5`. (2026-10-05)
- **Fixed-block resampling**: Keep pulling engine chunks until the callback has enough output or the ring is empty; a single calculated pull can starve Windows' fixed-block resampler. Check `callbacks_leave_backlog_in_the_ring_and_keep_output_continuous`; the playback ring is already bounded to 640 ms. (2026-10-05)
- **Resampler startup and errors**: Validate one/two channels before constructing `FixedResampler<f32, 2>`; larger channel counts assert at construction. Reset and discard staging on processing errors; expect an FFT startup transient rather than bit-exact initial samples after reset. (2026-10-05)
- **Opus input conversion**: Pass `ENGINE_FORMAT` from capture and convert input to the selected encoder quality. Why: unconverted stereo 48 kHz input made Low emit 600 packets for two seconds; Medium also interpreted stereo as mono. Check `every_quality_preserves_engine_audio_duration`. (2026-10-05)
- **Opus RTP clock**: Advance every 20 ms packet by 960 ticks regardless of channels or encoded sample rate; compute received ticks with `sample_rate * duration.as_secs_f64()`, not division. Check `opus_timestamps_use_the_48khz_clock_for_every_quality`. (2026-10-05)
- **Quality diagnostics**: Look for INFO `configured Opus audio track` to identify input format, selected quality, encoding format, and application. The historical encoder initialization message was DEBUG and absent from the failed-call log; a received stereo track establishes sending channels, not the peer's output format. (2026-10-05)

## Recording Lifecycle
- **Mid-call attachment**: Install `self.recording = Some(session)` before calling `attach_recorder_for` for cached peer tracks; that helper reads the installed session and otherwise skips every peer. Remove `peer_voice_tracks` on Abort and reject late stale generations. (2026-10-05)
- **Automatic ownership**: Stop automatic sessions according to `recording_automatic`, not the current preference; the disable command updates the preference before calling the stop helper. Preserve manual sessions. The active UI recording toggle sends stop, not a manual ownership transfer. (2026-10-05)
- **Mute semantics**: Describe local recording as post-mute; capture zeroes samples before dispatching to both encoders and recording sinks. Do not claim muting preserves microphone audio in the recording. (2026-10-05)

## Testing
- **Runtime isolation**: Spawn the exact worker regression in a child process with `WIRE_CONFIG_DIR` pointing to a temporary directory and `WIRE_RECORDING_TEST_CHILD=1`. Exercise `stop_recording()` and its events; calling `RecordingSession::finish()` directly misses runtime state transitions. (2026-10-05)
- **Duplex coverage and limits**: Run `cargo test -p wire --lib duplex_call_delivers_audible_20ms_packets_at_every_quality` for actual local RTP in both directions at all four qualities. Synthetic input does not validate physical devices on two PCs; one participant's track-arrival log cannot establish the other participant's playback failure. (2026-10-05)

## OpenCode Mini & APIs
- **Exact model selection**: Use OpenCode 2.0.22's advertised `--model opencode/muse-spark-1.3-contributor-free#xhigh` for the requested Muse Spark 1.3 Free audit. Isolate config, set `agents.*.model` consistently, disable `agents.title` with `disabled: true`, and set `compaction.auto: false` to prevent unrelated model work. (2026-10-05)
- **API schema**: Inspect `opencode api --server <url> GET /openapi.json`; its response is the schema itself, without a `data` wrapper. Use operation IDs such as `session.get --param sessionID=<id>`; `session.prompt` takes a JSON body with `text`, not a Chat Completions-style messages array. (2026-10-05)
- **Audit output**: Print only selected text, model reference, completion status, and errors from session messages; do not dump complete messages, which can include large encrypted reasoning fields. Verify assistant model references and file hashes before claiming a read-only, single-model audit. (2026-10-05)

## Dead Ends
- **Read-only shell permissions**: Avoid compound audit commands that include extra commands outside the allowlist; the original reviewer shell was denied for a compound invocation. Supply the diff in the prompt and use allowed read/grep/glob tools instead. (2026-10-05)
- **Blocked follow-up audit**: Report the actual rejection and do not claim a second review ran. The revised-diff OpenCode dispatch was rejected by automatic approval review with `blocked by policy`; the original audit succeeded, and local validation completed independently. (2026-10-05)
