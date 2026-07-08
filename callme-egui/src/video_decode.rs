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
//! frames here; when the decoder cannot keep up we backpressure the sender via
//! QUIC flow control instead of discarding pictures.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

pub struct DecodedFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// Common interface for the two decoder backends.
///
/// Not `Send`-bound: the decoder is created on the decode thread (just like the
/// MF encoder), so it never crosses thread boundaries even though the underlying
/// COM `IMFTransform` is `!Send`.
trait FrameDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)>;
}

struct OpenH264Decoder(callme::video::codec::VideoDecoder);

impl OpenH264Decoder {
    fn try_new() -> Result<Self> {
        info!("using OpenH264 software video decoder");
        Ok(Self(callme::video::codec::VideoDecoder::new()?))
    }
}

impl FrameDecoder for OpenH264Decoder {
    fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        self.0.decode(data)
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
    fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
        self.0.decode(data)
    }
}

fn make_decoder() -> Result<Box<dyn FrameDecoder>> {
    #[cfg(windows)]
    {
        match MfDecoder::try_new() {
            Ok(decoder) => return Ok(Box::new(decoder)),
            Err(e) => {
                warn!("MF hardware H.264 decoder unavailable, falling back to OpenH264: {e:?}")
            }
        }
    }
    Ok(Box::new(OpenH264Decoder::try_new()?))
}

/// Bounded hand-off between the QUIC receive task and the decode thread. Small on
/// purpose: under normal load the decoder keeps up, and if it ever falls behind we
/// apply backpressure rather than drop pictures.
const PACKET_QUEUE_DEPTH: usize = 3;

pub struct VideoDecodeWorker {
    packet_tx: SyncSender<Vec<u8>>,
    submitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
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
            packet_tx,
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
    pub fn submit(&self, data: Vec<u8>) {
        match self.packet_tx.send(data) {
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
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_decode_loop<F>(
    packet_rx: mpsc::Receiver<Vec<u8>>,
    submitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    on_frame: F,
) -> Result<()>
where
    F: Fn(DecodedFrame),
{
    let mut decoder = make_decoder()?;
    let mut decoded = 0u64;
    let mut decode_errors = 0u64;
    let mut window_decoded = 0u64;
    let mut window_bytes = 0u64;
    let mut window_decode_ms = 0.0;
    let mut last_stats_log = Instant::now();
    let mut last_submitted = 0u64;
    let mut last_dropped = 0u64;

    loop {
        let data = match packet_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let packet_len = data.len() as u64;
        let decode_start = Instant::now();
        match decoder.decode(&data) {
            Ok((rgba, w, h)) => {
                let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
                decoded += 1;
                window_decoded += 1;
                window_bytes += packet_len;
                window_decode_ms += decode_ms;
                decode_errors = 0;
                if decoded == 1 {
                    info!("decoded first video frame ({w}x{h})");
                }
                on_frame(DecodedFrame {
                    data: Arc::new(rgba),
                    width: w,
                    height: h,
                });
            }
            Err(e) => {
                decode_errors += 1;
                if decode_errors <= 5 || decode_errors % 60 == 0 {
                    warn!("video decode error (#{decode_errors}): {e:?}");
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
            let avg_packet_kb = if window_decoded > 0 {
                window_bytes as f64 / window_decoded as f64 / 1024.0
            } else {
                0.0
            };
            info!(
                "video decode pipeline: {:.1} fps, {:.1} ms/frame, {:.1} KiB/frame, {} submitted, {} dropped this window, {} dropped total",
                decode_fps,
                avg_decode_ms,
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
        }
    }
    Ok(())
}
