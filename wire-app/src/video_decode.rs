//! Dedicated decode thread.
//!
//! On Windows we prefer the Media Foundation hardware (GPU) H.264 decoder and
//! fall back to the OpenH264 software decoder otherwise.
//!
//! Frames are always decoded in the exact order they arrive. Dropping
//! intermediate H.264 pictures (as the old "newest-wins" loop did) breaks the
//! decoder's reference picture chain: a P-frame whose reference was skipped
//! makes OpenH264 report "no decodable frame found", and while it is erroring
//! the UI just holds the last good frame — that is the freeze. So we never skip
//! encoded pictures here; when the decoder cannot keep up we backpressure the
//! sender via QUIC flow control instead of breaking the reference chain. After
//! a picture has been decoded, the Windows presenter may safely skip displaying
//! it when its bounded surface pool is busy.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

pub struct DecodedFrame {
    pub data: DecodedFrameData,
    pub width: u32,
    pub height: u32,
}

pub enum DecodedFrameData {
    Rgba(Arc<Vec<u8>>),
    #[cfg(windows)]
    D3d11(crate::win_mf_codec::D3d11VideoFrame),
}

/// Common interface for the two decoder backends.
///
/// Not `Send`-bound: the decoder is created on the decode thread (just like the
/// MF encoder), so it never crosses thread boundaries even though the underlying
/// COM `IMFTransform` is `!Send`.
trait FrameDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>>;
}

struct OpenH264Decoder(wire::video::codec::VideoDecoder);

impl OpenH264Decoder {
    fn try_new() -> Result<Self> {
        info!("using OpenH264 software video decoder");
        Ok(Self(wire::video::codec::VideoDecoder::new()?))
    }
}

impl FrameDecoder for OpenH264Decoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>> {
        self.0.decode(data).map(|(rgba, width, height)| {
            vec![DecodedFrame {
                data: DecodedFrameData::Rgba(Arc::new(rgba)),
                width,
                height,
            }]
        })
    }
}

#[cfg(windows)]
struct MfDecoder(crate::win_mf_codec::MfH264Decoder);

#[cfg(windows)]
impl MfDecoder {
    fn try_new() -> Result<Self> {
        info!("using MF hardware (GPU) H.264 decoder");
        Ok(Self(crate::win_mf_codec::MfH264Decoder::try_new()?))
    }
}

#[cfg(windows)]
impl FrameDecoder for MfDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Vec<DecodedFrame>> {
        self.0.decode(data)
    }
}

fn make_decoder() -> Result<(Box<dyn FrameDecoder>, bool)> {
    #[cfg(windows)]
    {
        match MfDecoder::try_new() {
            Ok(decoder) => return Ok((Box::new(decoder), true)),
            Err(e) => warn!("MF hardware decoder unavailable, using OpenH264: {e:?}"),
        }
    }
    Ok((Box::new(OpenH264Decoder::try_new()?), false))
}

/// Bounded hand-off between the QUIC receive task and the decode thread. Small on
/// purpose: under normal load the decoder keeps up, and if it ever falls behind we
/// apply backpressure rather than drop pictures.
const PACKET_QUEUE_DEPTH: usize = 3;

pub struct VideoDecodeWorker {
    packet_tx: Option<SyncSender<EncodedPacket>>,
    submitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
}

struct EncodedPacket {
    data: Vec<u8>,
    keyframe: bool,
}

impl VideoDecodeWorker {
    pub fn spawn<F>(on_frame: F) -> Result<Self>
    where
        F: Fn(DecodedFrame) + Send + 'static,
    {
        let (packet_tx, packet_rx) = mpsc::sync_channel(PACKET_QUEUE_DEPTH);
        let submitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let submitted_for_thread = submitted.clone();
        let dropped_for_thread = dropped.clone();
        let join = thread::spawn(move || {
            if let Err(e) = run_decode_loop(
                packet_rx,
                submitted_for_thread,
                dropped_for_thread,
                on_frame,
            ) {
                info!("video decode thread stopped: {e:?}");
            }
        });
        Ok(Self {
            packet_tx: Some(packet_tx),
            submitted,
            dropped,
            join: Some(join),
        })
    }

    /// Hand a frame to the decode thread.
    ///
    /// Uses a blocking send so the decode thread always receives a contiguous
    /// sequence of H.264 pictures. If the decoder is slower than the network,
    /// this blocks the caller (the QUIC receive task), which backpressures the
    /// sender through QUIC flow control instead of dropping frames.
    pub fn submit(&self, data: Vec<u8>, keyframe: bool) {
        let Some(packet_tx) = &self.packet_tx else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match packet_tx.send(EncodedPacket { data, keyframe }) {
            Ok(()) => {
                self.submitted.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for VideoDecodeWorker {
    fn drop(&mut self) {
        // Close the packet channel before joining. Joining while this sender is
        // alive leaves the decode thread blocked in recv_timeout forever, which
        // used to deadlock video-stream replacement after a QUIC reset.
        self.packet_tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_worker_shutdown_does_not_wait_for_another_packet() {
        let worker = VideoDecodeWorker::spawn(|_| {}).unwrap();
        let started = Instant::now();
        drop(worker);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

fn run_decode_loop<F>(
    packet_rx: mpsc::Receiver<EncodedPacket>,
    submitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    on_frame: F,
) -> Result<()>
where
    F: Fn(DecodedFrame),
{
    #[cfg(windows)]
    let (mut decoder, mut hardware_decoder) = make_decoder()?;
    #[cfg(not(windows))]
    let (mut decoder, _) = make_decoder()?;
    let mut waiting_for_keyframe = false;
    let mut decoded = 0u64;
    let mut decode_errors = 0u64;
    // A freshly joined stream starts mid-GOP, so the decoder cannot produce a
    // frame until the next keyframe arrives (keyframe interval is <= 2s). Pre-
    // keyframe decode errors are expected and not a sign of a broken decoder.
    let mut window_decoded = 0u64;
    let mut window_bytes = 0u64;
    let mut window_decode_ms = 0.0;
    let mut decode_samples_ms = Vec::with_capacity(300);
    let mut last_stats_log = Instant::now();
    let mut last_submitted = 0u64;
    let mut last_dropped = 0u64;

    loop {
        let packet = match packet_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if waiting_for_keyframe && !packet.keyframe {
            continue;
        }
        if packet.keyframe {
            waiting_for_keyframe = false;
        }

        let packet_len = packet.data.len() as u64;
        let decode_start = Instant::now();
        match decoder.decode(&packet.data) {
            Ok(frames) => {
                let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
                window_decode_ms += decode_ms;
                decode_samples_ms.push(decode_ms);
                decode_errors = 0;
                for frame in frames {
                    decoded += 1;
                    window_decoded += 1;
                    window_bytes += packet_len;
                    if decoded == 1 {
                        info!(
                            "decoded first video frame ({}x{})",
                            frame.width, frame.height
                        );
                    }
                    on_frame(frame);
                }
            }
            Err(e) => {
                decode_errors += 1;
                if decode_errors <= 5 || decode_errors % 60 == 0 {
                    warn!("video decode error (#{decode_errors}): {e:?}");
                }
                #[cfg(windows)]
                if hardware_decoder && decode_errors >= 5 {
                    warn!("MF decoder repeatedly failed; falling back to OpenH264 at next IDR");
                    decoder = Box::new(OpenH264Decoder::try_new()?);
                    hardware_decoder = false;
                    waiting_for_keyframe = true;
                    decode_errors = 0;
                }
            }
        }

        if last_stats_log.elapsed() >= Duration::from_secs(5) {
            let elapsed = last_stats_log.elapsed().as_secs_f64();
            let submitted_now = submitted.load(Ordering::Relaxed);
            let dropped_now = dropped.load(Ordering::Relaxed);
            let submitted_window = submitted_now.saturating_sub(last_submitted);
            let dropped_window = dropped_now.saturating_sub(last_dropped);
            let decode_fps = if elapsed > 0.0 {
                window_decoded as f64 / elapsed
            } else {
                0.0
            };
            let avg_decode_ms = if window_decoded > 0 {
                window_decode_ms / window_decoded as f64
            } else {
                0.0
            };
            decode_samples_ms.sort_by(f64::total_cmp);
            let p95_decode_ms = if decode_samples_ms.is_empty() {
                0.0
            } else {
                decode_samples_ms[((decode_samples_ms.len() - 1) as f64 * 0.95).round() as usize]
            };
            let avg_packet_kb = if window_decoded > 0 {
                window_bytes as f64 / window_decoded as f64 / 1024.0
            } else {
                0.0
            };
            info!(
                "video decode pipeline: {:.1} fps, {:.1} ms avg / {:.1} ms p95, {:.1} KiB/frame, {} submitted, {} dropped this window, {} dropped total",
                decode_fps,
                avg_decode_ms,
                p95_decode_ms,
                avg_packet_kb,
                submitted_window,
                dropped_window,
                dropped_now
            );
            last_stats_log = Instant::now();
            last_submitted = submitted_now;
            last_dropped = dropped_now;
            window_decoded = 0;
            window_bytes = 0;
            window_decode_ms = 0.0;
            decode_samples_ms.clear();
        }
    }
    Ok(())
}
