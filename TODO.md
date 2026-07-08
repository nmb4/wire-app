# TODO

Video streaming issue tracker. Log-analysis driven; see commit history for context.

## Open

### 1. Receiving stream is upside down + stalls
- **Status:** fixed by reverting receive decode to OpenH264 (needs verification)
- **Symptom:** Remote video was vertically flipped, then after a few frames the
  receiver stalled with no new frames.
- **Root cause (proper diagnosis from logs):** The MF hardware H.264 decoder has
  an MFT state machine we were not driving correctly. Logs showed, after the
  first keyframe:
  - `MF_E_TRANSFORM_STREAM_CHANGE (0xC00D6D61)` — a stream/SPS change requires
    renegotiating the output media type.
  - `MF_E_NOTACCEPTING (0xC00D36B5)` — the decoder will not accept more input
    until pending output is drained.
  Our `decode()` never renegotiated on stream change nor drained output on
  NOTACCEPTING, so after a few frames the decoder rejected all input → permanent
  stall. The flip was a secondary symptom of the same MF image-buffer stride
  handling.
- **Fix:** Receive decoding now uses the **OpenH264 software decoder** (proven
  reliable and orientation-correct at 1080p; it worked well before the MF decoder
  was forced in). The MF hardware decoder (`win_mf_codec::MfH264Decoder`) is kept
  but only used as a last resort until its state machine (stream-change
  renegotiation + NOTACCEPTING draining) is implemented. The stride/orientation
  helpers (`read_sample_topdown`) remain and still improve the encode direction.

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
