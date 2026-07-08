# TODO

Video streaming issue tracker. Log-analysis driven; see commit history for context.

## Open

### 1. Receiving stream is upside down
- **Status:** in progress
- **Symptom:** Remote video decoded via the MF hardware H.264 decoder renders vertically flipped.
- **Root cause:** MF H.264 decoder output samples use a negative (bottom-up) stride;
  `read_sample_bytes` flattens them as if top-down, so the NV12 fed to the
  NV12->RGBA conversion is upside down. The OpenH264 software path is unaffected.
- **Fix:** Read decoder output with stride awareness and normalize to top-down
  before color conversion (in `MfH264Decoder::decode`, `win_mf_codec.rs`).

### 2. Stopping then restarting screen sharing does not restart
- **Status:** deferred (not working on this yet, per user)
- **Symptom:** Ending a share and starting it again leaves the stream not restarting.
- **Note:** Separate from decode/connection; likely in the capture/stream
  lifecycle in `app.rs` / `screen_capture.rs`.

### 3. External (non-localhost) client connection no longer works
- **Status:** in progress
- **Symptom:** A friend's machine (different network) cannot connect / receive video.
- **Root cause:** The 150 ms `VIDEO_SEND_LATENCY_BUDGET` reset in `run_video_send`
  (`app.rs`) fires on every networked send over a relay (real RTT), so the sender
  resets/reopens the QUIC video stream continuously (logs show `resetting stale
  stream (#1)..#7` in a loop). Local loopback never trips the budget, which is
  why only external clients broke. Cancelling a `send_frame` future mid-write also
  leaves the stream in an undefined state, forcing the reset.
- **Fix:** Remove the timeout+reset; let QUIC apply natural backpressure (the send
  just blocks until the receiver/relay drains). No frames are dropped and the
  stream stays consistent, so external links recover instead of thrashing.

## Resolved
- Receiver freezes (broken H.264 reference chain from frame dropping) — fixed by
  decoding every frame in order and wiring the MF hardware H.264 decoder.
- MF hardware decoder produced no output (async MFT output not drained) — fixed.
- MF hardware decoder discarded during mid-GOP startup warm-up — fixed.
