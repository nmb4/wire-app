# IsoCast attribution

Source: https://github.com/R44VC0RP/isocast

Revision: `243aeca23f1d6ee6f6b7caf14b2f43133e9eb418` (2026-10-24 checkout).

Wire adapts the non-spinning high-resolution Windows timer from
`crates/isocast-platform-windows/src/clock.rs` in `win_frame_clock.rs`, and
the timestamp-based admission before GPU capture copies from that crate's
`capture.rs` in `win_capture.rs`. Wire retains the timer's relative due-time
calculation and early-wake rearming, using `Instant` for its deadlines, and
adds phase preservation to capture admission for non-divisible refresh rates.

The unsigned-integer MF keyframe request and activation-object lifetime
(explicit `ShutdownObject` on teardown) also follow `mf.rs`.
The same-device conversion into reusable encoder surfaces in `convert.rs`
and `mf.rs`, and the performance analysis in `docs/windows-performance.md`,
also informed Wire's GPU surface reuse and preview-worker changes.

Used under the MIT license; the complete copyright and permission notice is
in [LICENSE-MIT](LICENSE-MIT).
