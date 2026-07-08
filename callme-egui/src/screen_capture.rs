use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
#[cfg(not(windows))]
use anyhow::Context;
use async_channel::Sender;
use callme::video::{codec::VideoEncoder, VideoConfig};
use tokio::sync::broadcast;
use tracing::info;

#[cfg(not(windows))]
use fast_image_resize as fr;
#[cfg(not(windows))]
use fr::images::Image;
#[cfg(not(windows))]
use fr::{PixelType, Resizer};
#[cfg(not(windows))]
use image::DynamicImage;

const FRAME_CHANNEL_DEPTH: usize = 2;
const PREVIEW_EVERY_N_FRAMES: u64 = 2;
const PREVIEW_DIVISOR: u32 = 2;

pub struct PreviewUpdate {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub actual_fps: f64,
    pub encode_time_ms: f64,
}

/// Runs capture+resize and encode in parallel threads.
/// Returns a join handle for the coordinator thread.
pub fn start(
    config: VideoConfig,
    stop_flag: Arc<AtomicBool>,
    encoded_tx: broadcast::Sender<Arc<Vec<u8>>>,
    preview_tx: Sender<PreviewUpdate>,
    keyframe_tx: broadcast::Sender<()>,
) -> JoinHandle<()> {
    let target_w = config.resolution.width();
    let target_h = config.resolution.height();
    let target_interval = Duration::from_secs_f64(1.0 / config.framerate as f64);

    thread::spawn(move || {
        let (frame_tx, frame_rx) = mpsc::sync_channel::<Vec<u8>>(FRAME_CHANNEL_DEPTH);

        let capture_stop = stop_flag.clone();
        let capture_handle = thread::spawn(move || {
            if let Err(e) = run_capture_loop(
                &capture_stop,
                target_w,
                target_h,
                target_interval,
                &frame_tx,
            ) {
                info!("capture thread stopped: {e:?}");
            }
        });

        let encode_stop = stop_flag;
        let encode_result = run_encode_loop(
            &encode_stop,
            config,
            target_w,
            target_h,
            frame_rx,
            encoded_tx,
            preview_tx,
            keyframe_tx,
        );
        if let Err(e) = encode_result {
            info!("encode thread stopped: {e:?}");
        }

        let _ = capture_handle.join();
    })
}

enum CaptureSource {
    #[cfg(windows)]
    Gdi {
        x: i32,
        y: i32,
        src_w: i32,
        src_h: i32,
    },
    #[cfg(not(windows))]
    Xcap {
        monitor: xcap::Monitor,
        resizer: Resizer,
        dst: Image,
    },
}

fn init_capture_source(target_w: u32, target_h: u32) -> Result<CaptureSource> {
    #[cfg(windows)]
    {
        let (x, y, src_w, src_h) = crate::win_gdi_capture::primary_monitor_geometry()?;
        info!(
            "capturing primary monitor {src_w}x{src_h} -> {target_w}x{target_h} (gdi stretch)"
        );
        Ok(CaptureSource::Gdi { x, y, src_w, src_h })
    }
    #[cfg(not(windows))]
    {
        let monitors = xcap::Monitor::all().context("failed to enumerate monitors")?;
        let monitor = monitors
            .iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .or(monitors.first())
            .context("no monitors found")?
            .clone();
        if let (Ok(w), Ok(h)) = (monitor.width(), monitor.height()) {
            info!("capturing primary monitor {w}x{h} -> {target_w}x{target_h} (xcap+resize)");
        }
        Ok(CaptureSource::Xcap {
            monitor,
            resizer: Resizer::new(),
            dst: Image::new(target_w, target_h, PixelType::U8x4),
        })
    }
}

fn capture_frame(source: &mut CaptureSource, target_w: u32, target_h: u32) -> Result<Vec<u8>> {
    match source {
        #[cfg(windows)]
        CaptureSource::Gdi { x, y, src_w, src_h } => {
            crate::win_gdi_capture::capture_monitor_scaled(*x, *y, *src_w, *src_h, target_w, target_h)
        }
        #[cfg(not(windows))]
        CaptureSource::Xcap { monitor, resizer, dst } => {
            let img = monitor
                .capture_image()
                .map_err(|e| anyhow::anyhow!("capture error: {e}"))?;
            let src = DynamicImage::ImageRgba8(img);
            resizer
                .resize(&src, dst, None)
                .map_err(|e| anyhow::anyhow!("resize error: {e}"))?;
            Ok(dst.buffer().to_vec())
        }
    }
}

fn run_capture_loop(
    stop_flag: &AtomicBool,
    target_w: u32,
    target_h: u32,
    target_interval: Duration,
    frame_tx: &mpsc::SyncSender<Vec<u8>>,
) -> Result<()> {
    let mut source = init_capture_source(target_w, target_h)?;

    while !stop_flag.load(Ordering::Relaxed) {
        let frame_start = Instant::now();

        let rgba = capture_frame(&mut source, target_w, target_h)?;
        if frame_tx.send(rgba).is_err() {
            break;
        }

        let elapsed = frame_start.elapsed();
        if elapsed < target_interval {
            thread::sleep(target_interval - elapsed);
        }
    }
    Ok(())
}

fn run_encode_loop(
    stop_flag: &AtomicBool,
    config: VideoConfig,
    target_w: u32,
    target_h: u32,
    frame_rx: mpsc::Receiver<Vec<u8>>,
    encoded_tx: broadcast::Sender<Arc<Vec<u8>>>,
    preview_tx: Sender<PreviewUpdate>,
    keyframe_tx: broadcast::Sender<()>,
) -> Result<()> {
    let mut encoder = VideoEncoder::new(&config)?;
    let mut keyframe_rx = keyframe_tx.subscribe();
    let mut frame_count = 0u64;
    let loop_start = Instant::now();

    while !stop_flag.load(Ordering::Relaxed) {
        while keyframe_rx.try_recv().is_ok() {
            encoder.force_keyframe();
        }

        let rgba = match frame_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let encode_start = Instant::now();
        match encoder.encode(&rgba) {
            Ok(encoded) => {
                let _ = encoded_tx.send(Arc::new(encoded));
            }
            Err(e) => {
                info!("video encode error: {e:?}");
            }
        }
        let encode_time = encode_start.elapsed();
        frame_count += 1;

        let elapsed = loop_start.elapsed();
        let actual_fps = if elapsed.as_secs_f64() > 0.0 {
            frame_count as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        if frame_count % PREVIEW_EVERY_N_FRAMES == 0 {
            let (preview_data, preview_w, preview_h) =
                make_preview(&rgba, target_w, target_h);
            let update = PreviewUpdate {
                data: Arc::new(preview_data),
                width: preview_w,
                height: preview_h,
                actual_fps,
                encode_time_ms: encode_time.as_secs_f64() * 1000.0,
            };
            let _ = preview_tx.try_send(update);
        }
    }
    Ok(())
}

fn make_preview(rgba: &[u8], src_w: u32, src_h: u32) -> (Vec<u8>, u32, u32) {
    let dst_w = (src_w / PREVIEW_DIVISOR).max(1);
    let dst_h = (src_h / PREVIEW_DIVISOR).max(1);
    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let sy = y * src_h / dst_h;
            let src_i = ((sy * src_w + sx) * 4) as usize;
            let dst_i = ((y * dst_w + x) * 4) as usize;
            out[dst_i..dst_i + 4].copy_from_slice(&rgba[src_i..src_i + 4]);
        }
    }
    (out, dst_w, dst_h)
}