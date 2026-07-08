# TODO

Video streaming issue tracker. Log-analysis driven; see commit history for context.

## Open

### 1. Receiving stream is upside down
- **Status:** fixed (needs verification)
- **Symptom:** Remote video decoded via the MF hardware H.264 decoder renders vertically flipped.
- **Root cause:** MF image buffers (both the decoder's NV12 output and the color
  converter's RGB32 output) use a negative (bottom-up) stride, but they were read
  via `read_sample_bytes` (ConvertToContiguousBuffer) which flattens them
  top-down. Normalizing only the decoder NV12 input was insufficient — the
  converter's own output also needed normalizing. OpenH264 software path is
  unaffected (separate code, top-down).
- **Fix:** Added `read_sample_topdown` (stride-aware, normalizes to top-down for
  both NV12 and RGB32 image samples) and use it for the decoder output and the
  color converter output in `win_mf_codec.rs`. Also normalizes the encode
  direction (converter NV12 output), so sent video is correctly oriented too.

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
