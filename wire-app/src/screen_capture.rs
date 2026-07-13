use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(not(windows))]
use anyhow::Context;
use anyhow::Result;
use async_channel::Sender;
use spin_sleep::SpinSleeper;
use tokio::sync::broadcast;
use tracing::{info, warn};
use wire::video::{
    bitstream::contains_idr, codec::VideoEncoder, transport::EncodedVideoFrame, VideoConfig,
};

use fast_image_resize as fr;
use fr::images::Image;
use fr::{PixelType, Resizer};
#[cfg(not(windows))]
use image::DynamicImage;

#[cfg(windows)]
use crate::scap_capture::ScapCapturer;
#[cfg(windows)]
use crate::win_capture::{GpuCapturedFrame, GpuPreviewScaler, WindowsCapturer};
#[cfg(windows)]
use crate::win_mf_codec::MfH264Encoder;

const FRAME_CHANNEL_DEPTH: usize = 1;
const PREVIEW_DIVISOR: u32 = 3;
const PREVIEW_CHANNEL_DEPTH: usize = 1;

pub struct PreviewUpdate {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub actual_fps: f64,
    pub encode_time_ms: f64,
}

struct PreviewInput {
    data: Vec<u8>,
    bgra: bool,
    width: u32,
    height: u32,
    output_width: u32,
    output_height: u32,
    actual_fps: f64,
    encode_time_ms: f64,
}

const MF_EMPTY_FALLBACK: u32 = 8;

enum FrameEncoder {
    #[cfg(windows)]
    MediaFoundation(MfH264Encoder),
    OpenH264(VideoEncoder),
}

impl FrameEncoder {
    fn try_new(config: &VideoConfig) -> Result<Self> {
        info!("using OpenH264 software encoder");
        Ok(Self::OpenH264(VideoEncoder::new(config)?))
    }

    #[cfg(windows)]
    fn try_new_with_mf(config: &VideoConfig) -> Result<Self> {
        match MfH264Encoder::try_new(config) {
            Ok(enc) => {
                let kind = if enc.is_hardware() {
                    "MF hardware (GPU)"
                } else {
                    "MF software"
                };
                info!("using {kind} encoder");
                Ok(Self::MediaFoundation(enc))
            }
            Err(e) => {
                info!("MF encoder unavailable, using OpenH264: {e:?}");
                Self::try_new(config)
            }
        }
    }

    fn is_media_foundation(&self) -> bool {
        #[cfg(windows)]
        {
            matches!(self, Self::MediaFoundation(_))
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn force_keyframe(&mut self) {
        match self {
            #[cfg(windows)]
            Self::MediaFoundation(enc) => enc.force_keyframe(),
            Self::OpenH264(enc) => enc.force_keyframe(),
        }
    }

    fn encode_frame(&mut self, frame: &[u8], bgra: bool) -> Result<Vec<u8>> {
        match self {
            #[cfg(windows)]
            Self::MediaFoundation(enc) => enc.encode_bgra(frame),
            Self::OpenH264(enc) => {
                if bgra {
                    enc.encode_bgra(frame)
                } else {
                    enc.encode(frame)
                }
            }
        }
    }

    #[cfg(windows)]
    fn try_new_with_wgc_device(
        config: &VideoConfig,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    ) -> Result<Self> {
        let encoder = MfH264Encoder::try_new_on_device(config, device)?;
        info!("using GPU-native WGC -> D3D11 processor -> MF encoder path");
        Ok(Self::MediaFoundation(encoder))
    }

    #[cfg(windows)]
    fn encode_gpu_frame(&mut self, frame: &GpuCapturedFrame) -> Result<Vec<u8>> {
        match self {
            Self::MediaFoundation(encoder) => {
                encoder.encode_texture_bgra(frame.texture(), frame.width, frame.height)
            }
            Self::OpenH264(_) => anyhow::bail!("software encoder cannot consume a GPU texture"),
        }
    }
}

enum CaptureFrame {
    Cpu(Vec<u8>),
    #[cfg(windows)]
    Gpu(GpuCapturedFrame),
}

/// Runs capture+resize and encode in parallel threads.
pub fn start(
    config: VideoConfig,
    stop_flag: Arc<AtomicBool>,
    encoded_tx: broadcast::Sender<Arc<EncodedVideoFrame>>,
    preview_tx: Sender<PreviewUpdate>,
    keyframe_tx: broadcast::Sender<()>,
) -> JoinHandle<()> {
    let target_w = config.resolution.width();
    let target_h = config.resolution.height();
    let target_interval = Duration::from_secs_f64(1.0 / config.framerate as f64);

    thread::spawn(move || {
        let (frame_tx, frame_rx) = mpsc::sync_channel::<CaptureFrame>(FRAME_CHANNEL_DEPTH);
        let (preview_input_tx, preview_input_rx) =
            mpsc::sync_channel::<PreviewInput>(PREVIEW_CHANNEL_DEPTH);

        let preview_handle = thread::spawn(move || {
            run_preview_loop(preview_input_rx, preview_tx);
        });

        let capture_stop = stop_flag.clone();
        let capture_fps = config.framerate;
        let capture_handle = thread::spawn(move || {
            if let Err(e) = run_capture_loop(
                &capture_stop,
                target_w,
                target_h,
                target_interval,
                capture_fps,
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
            preview_input_tx,
            keyframe_tx,
        );
        if let Err(e) = encode_result {
            info!("encode thread stopped: {e:?}");
        }

        let _ = capture_handle.join();
        let _ = preview_handle.join();
    })
}

enum CaptureSource {
    #[cfg(windows)]
    Windows(WindowsCapturer),
    #[cfg(windows)]
    Scap(ScapCapturer),
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

fn init_capture_source(target_w: u32, target_h: u32, framerate: u32) -> Result<CaptureSource> {
    #[cfg(windows)]
    {
        // Prefer the direct Windows Graphics Capture path: it avoids zed-scap's unconditional
        // full-frame buffer_crop call for display capture.
        match WindowsCapturer::try_new(target_w, target_h) {
            Ok(capturer) => return Ok(CaptureSource::Windows(capturer)),
            Err(e) => info!("direct WGC capture unavailable, trying zed-scap: {e:?}"),
        }

        // Prefer WGC: GDI StretchBlt blocks the desktop compositor and causes system-wide stutter.
        if let Ok(scap) = ScapCapturer::try_new(target_w, target_h, framerate) {
            return Ok(CaptureSource::Scap(scap));
        }
        let (x, y, src_w, src_h) = crate::win_gdi_capture::primary_monitor_geometry()?;
        info!("capturing primary monitor {src_w}x{src_h} -> {target_w}x{target_h} (gdi fallback)");
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
        Ok(CaptureSource::Xcap {
            monitor,
            resizer: Resizer::new(),
            dst: Image::new(target_w, target_h, PixelType::U8x4),
        })
    }
}

fn capture_frame(source: &mut CaptureSource, target_w: u32, target_h: u32) -> Result<CaptureFrame> {
    match source {
        #[cfg(windows)]
        CaptureSource::Windows(capturer) => capturer.capture_gpu().map(CaptureFrame::Gpu),
        #[cfg(windows)]
        CaptureSource::Scap(scap) => scap.capture_bgra().map(CaptureFrame::Cpu),
        #[cfg(windows)]
        CaptureSource::Gdi { x, y, src_w, src_h } => {
            crate::win_gdi_capture::capture_monitor_scaled(
                *x, *y, *src_w, *src_h, target_w, target_h,
            )
            .map(CaptureFrame::Cpu)
        }
        #[cfg(not(windows))]
        CaptureSource::Xcap {
            monitor,
            resizer,
            dst,
        } => {
            let img = monitor
                .capture_image()
                .map_err(|e| anyhow::anyhow!("capture error: {e}"))?;
            let src = DynamicImage::ImageRgba8(img);
            resizer
                .resize(&src, dst, None)
                .map_err(|e| anyhow::anyhow!("resize error: {e}"))?;
            Ok(CaptureFrame::Cpu(dst.buffer().to_vec()))
        }
    }
}

fn run_capture_loop(
    stop_flag: &AtomicBool,
    target_w: u32,
    target_h: u32,
    target_interval: Duration,
    framerate: u32,
    frame_tx: &mpsc::SyncSender<CaptureFrame>,
) -> Result<()> {
    let mut source = init_capture_source(target_w, target_h, framerate)?;
    let sleeper = SpinSleeper::default();
    let mut window_captured = 0u64;
    let mut window_dropped = 0u64;
    let mut dropped_count = 0u64;
    let mut last_stats_log = Instant::now();
    let mut capture_samples = Vec::with_capacity(framerate as usize * 5);

    while !stop_flag.load(Ordering::Relaxed) {
        let frame_start = Instant::now();

        let frame = capture_frame(&mut source, target_w, target_h)?;
        match frame_tx.try_send(frame) {
            Ok(()) => {
                window_captured += 1;
            }
            Err(TrySendError::Full(_)) => {
                dropped_count += 1;
                window_dropped += 1;
            }
            Err(TrySendError::Disconnected(_)) => break,
        }

        let elapsed = frame_start.elapsed();
        capture_samples.push(elapsed.as_secs_f64() * 1000.0);
        if last_stats_log.elapsed() >= Duration::from_secs(5) {
            let window_elapsed = last_stats_log.elapsed().as_secs_f64();
            let capture_fps = if window_elapsed > 0.0 {
                window_captured as f64 / window_elapsed
            } else {
                0.0
            };
            let (avg_capture_ms, p95_capture_ms) = summarize_ms(&mut capture_samples);
            info!(
                "capture pipeline: {:.1} fps, {:.1} ms avg / {:.1} ms p95, {} dropped this window, {} dropped total (target {} fps)",
                capture_fps,
                avg_capture_ms,
                p95_capture_ms,
                window_dropped,
                dropped_count,
                framerate
            );
            last_stats_log = Instant::now();
            window_captured = 0;
            window_dropped = 0;
        }
        if elapsed < target_interval {
            sleeper.sleep(target_interval - elapsed);
        }
    }
    Ok(())
}

fn run_encode_loop(
    stop_flag: &AtomicBool,
    config: VideoConfig,
    target_w: u32,
    target_h: u32,
    frame_rx: mpsc::Receiver<CaptureFrame>,
    encoded_tx: broadcast::Sender<Arc<EncodedVideoFrame>>,
    preview_input_tx: mpsc::SyncSender<PreviewInput>,
    keyframe_tx: broadcast::Sender<()>,
) -> Result<()> {
    #[cfg(windows)]
    let mut encoder = FrameEncoder::try_new_with_mf(&config)?;
    #[cfg(not(windows))]
    let mut encoder = FrameEncoder::try_new(&config)?;
    let mut keyframe_rx = keyframe_tx.subscribe();
    let mut frame_count = 0u64;
    let mut encoded_count = 0u64;
    let mut next_sequence = 0u64;
    let mut encode_errors = 0u64;
    let mut mf_empty_streak = 0u32;
    let mut no_subscriber_logged = false;
    let mut had_subscribers = false;
    let mut last_stats_log = Instant::now();
    let mut window_input = 0u64;
    let mut window_output = 0u64;
    let mut window_bytes = 0u64;
    let mut window_encode_ms = Vec::with_capacity(config.framerate as usize * 5);
    let mut preview_dropped = 0u64;
    let preview_interval = (config.framerate.max(1) / 5).max(1) as u64;
    #[cfg(windows)]
    let mut gpu_encoder_ready = false;
    #[cfg(windows)]
    let mut gpu_encoder_failed = false;
    #[cfg(windows)]
    let mut cpu_resizer = Resizer::new();
    #[cfg(windows)]
    let mut cpu_resize_dst = Image::new(target_w, target_h, PixelType::U8x4);
    #[cfg(windows)]
    let mut gpu_preview_scaler: Option<GpuPreviewScaler> = None;

    while !stop_flag.load(Ordering::Relaxed) {
        while keyframe_rx.try_recv().is_ok() {
            encoder.force_keyframe();
        }

        let mut frame = match frame_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        while let Ok(newer) = frame_rx.try_recv() {
            frame = newer;
        }

        frame_count += 1;
        window_input += 1;
        let has_subscribers = encoded_tx.receiver_count() != 0;
        if has_subscribers && !had_subscribers {
            // A receiver may have joined in the middle of a GOP while capture was idle.
            encoder.force_keyframe();
            info!("video receiver joined; encoder resumed from a keyframe");
        }
        had_subscribers = has_subscribers;
        if !has_subscribers {
            if !no_subscriber_logged {
                info!("video encoder idle because no send task is subscribed");
                no_subscriber_logged = true;
            }
        } else {
            no_subscriber_logged = false;
        }

        let encode_start = Instant::now();
        #[cfg(windows)]
        let encoded_result = has_subscribers.then(|| match &frame {
            CaptureFrame::Gpu(gpu) => {
                if !gpu_encoder_ready && !gpu_encoder_failed {
                    match FrameEncoder::try_new_with_wgc_device(&config, &gpu.device) {
                        Ok(new_encoder) => {
                            encoder = new_encoder;
                            gpu_encoder_ready = true;
                        }
                        Err(e) => {
                            warn!("GPU-native encode unavailable; using CPU fallback: {e:#}");
                            gpu_encoder_failed = true;
                        }
                    }
                }
                if gpu_encoder_ready {
                    encoder.encode_gpu_frame(gpu)
                } else {
                    let source = gpu.read_bgra()?;
                    let resized = resize_bgra(
                        source,
                        gpu.width,
                        gpu.height,
                        &mut cpu_resizer,
                        &mut cpu_resize_dst,
                    )?;
                    encoder.encode_frame(&resized, true)
                }
            }
            CaptureFrame::Cpu(bgra) => encoder.encode_frame(bgra, true),
        });
        #[cfg(not(windows))]
        let encoded_result = has_subscribers.then(|| match &frame {
            CaptureFrame::Cpu(rgba) => encoder.encode_frame(rgba, false),
        });
        match encoded_result {
            Some(Ok(encoded)) if encoded.is_empty() => {
                if encoder.is_media_foundation() {
                    mf_empty_streak += 1;
                    if mf_empty_streak == MF_EMPTY_FALLBACK {
                        #[cfg(windows)]
                        if gpu_encoder_ready {
                            warn!(
                                "GPU-native MF encoder produced no output for {MF_EMPTY_FALLBACK} frames; retrying the proven CPU-fed MF path"
                            );
                            encoder = FrameEncoder::try_new_with_mf(&config)?;
                            gpu_encoder_ready = false;
                            gpu_encoder_failed = true;
                        } else {
                            warn!(
                                "MF encoder produced no output for {MF_EMPTY_FALLBACK} frames, switching to OpenH264"
                            );
                            encoder = FrameEncoder::try_new(&config)?;
                        }
                        #[cfg(not(windows))]
                        {
                            encoder = FrameEncoder::try_new(&config)?;
                        }
                        mf_empty_streak = 0;
                    }
                }
            }
            Some(Ok(encoded)) => {
                mf_empty_streak = 0;
                encoded_count += 1;
                window_output += 1;
                window_bytes += encoded.len() as u64;
                if encoded_count == 1 {
                    info!("encoded first video frame ({} bytes)", encoded.len());
                }
                let keyframe = contains_idr(&encoded);
                let frame = EncodedVideoFrame::new(next_sequence, keyframe, encoded);
                next_sequence = next_sequence.wrapping_add(1);
                // receiver_count was checked immediately before encoding. A peer can still
                // disconnect during an encode; in that case dropping this completed frame is safe.
                let _ = encoded_tx.send(Arc::new(frame));
            }
            Some(Err(e)) => {
                encode_errors += 1;
                if encode_errors <= 5 || encode_errors % 60 == 0 {
                    warn!("video encode error (#{encode_errors}): {e:?}");
                }
                #[cfg(windows)]
                if gpu_encoder_ready {
                    warn!(
                        "GPU-native MF input failed; retaining hardware encode through the CPU-fed MF fallback (last: {e:#})"
                    );
                    encoder = FrameEncoder::try_new_with_mf(&config)?;
                    gpu_encoder_ready = false;
                    gpu_encoder_failed = true;
                    encode_errors = 0;
                } else if encoder.is_media_foundation() && encode_errors >= 5 {
                    warn!("switching to OpenH264 after repeated CPU-fed MF encode errors (last: {e:?})");
                    encoder = FrameEncoder::try_new(&config)?;
                    encode_errors = 0;
                }
            }
            None => {}
        }
        let encode_time = encode_start.elapsed();
        if has_subscribers {
            window_encode_ms.push(encode_time.as_secs_f64() * 1000.0);
        }

        let window_elapsed = last_stats_log.elapsed().as_secs_f64();
        let actual_fps = if window_elapsed > 0.0 {
            window_input as f64 / window_elapsed
        } else {
            0.0
        };

        if last_stats_log.elapsed() >= Duration::from_secs(5) {
            let (avg_encode_ms, p95_encode_ms) = summarize_ms(&mut window_encode_ms);
            let input_fps = window_input as f64 / window_elapsed;
            let output_fps = window_output as f64 / window_elapsed;
            let bitrate_mbps = window_bytes as f64 * 8.0 / window_elapsed / 1_000_000.0;
            info!(
                "encode pipeline: {:.1} fps in, {:.1} fps out, {:.1} Mbps, {:.1} ms avg / {:.1} ms p95, {} preview drops (target {} fps)",
                input_fps,
                output_fps,
                bitrate_mbps,
                avg_encode_ms,
                p95_encode_ms,
                preview_dropped,
                config.framerate
            );
            last_stats_log = Instant::now();
            window_input = 0;
            window_output = 0;
            window_bytes = 0;
            preview_dropped = 0;
        }

        if frame_count % preview_interval == 0 {
            #[cfg(windows)]
            let preview_source = match frame {
                CaptureFrame::Cpu(data) => Some((data, target_w, target_h)),
                CaptureFrame::Gpu(gpu) => {
                    let output_width = (target_w / PREVIEW_DIVISOR).max(1);
                    let output_height = (target_h / PREVIEW_DIVISOR).max(1);
                    let recreate = gpu_preview_scaler
                        .as_ref()
                        .map(|scaler| !scaler.matches(&gpu, output_width, output_height))
                        .unwrap_or(true);
                    if recreate {
                        gpu_preview_scaler =
                            match GpuPreviewScaler::new(&gpu, output_width, output_height) {
                                Ok(scaler) => Some(scaler),
                                Err(e) => {
                                    warn!("GPU preview scaler unavailable: {e:#}");
                                    None
                                }
                            };
                    }
                    match gpu_preview_scaler
                        .as_ref()
                        .map(|scaler| scaler.read_bgra(&gpu))
                    {
                        Some(Ok(data)) => Some((data, output_width, output_height)),
                        Some(Err(e)) => {
                            warn!("downscaled GPU preview readback failed: {e:#}");
                            gpu_preview_scaler = None;
                            None
                        }
                        None => match gpu.read_bgra() {
                            Ok(data) => Some((data, gpu.width, gpu.height)),
                            Err(e) => {
                                warn!("GPU preview readback failed: {e:#}");
                                None
                            }
                        },
                    }
                }
            };
            #[cfg(not(windows))]
            let preview_source = match frame {
                CaptureFrame::Cpu(data) => Some((data, target_w, target_h)),
            };
            if let Some((data, width, height)) = preview_source {
                let input = PreviewInput {
                    data,
                    bgra: cfg!(windows),
                    width,
                    height,
                    output_width: (target_w / PREVIEW_DIVISOR).max(1),
                    output_height: (target_h / PREVIEW_DIVISOR).max(1),
                    actual_fps,
                    encode_time_ms: encode_time.as_secs_f64() * 1000.0,
                };
                if preview_input_tx.try_send(input).is_err() {
                    preview_dropped += 1;
                }
            }
        }
    }
    Ok(())
}

fn run_preview_loop(preview_rx: mpsc::Receiver<PreviewInput>, preview_tx: Sender<PreviewUpdate>) {
    let mut samples = Vec::with_capacity(32);
    let mut generated = 0u64;
    let mut dropped = 0u64;
    let mut last_log = Instant::now();
    while let Ok(input) = preview_rx.recv() {
        let started = Instant::now();
        let data = make_preview(
            &input.data,
            input.width,
            input.height,
            input.output_width,
            input.output_height,
            input.bgra,
        );
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        generated += 1;
        let update = PreviewUpdate {
            data: Arc::new(data),
            width: input.output_width,
            height: input.output_height,
            actual_fps: input.actual_fps,
            encode_time_ms: input.encode_time_ms,
        };
        if preview_tx.try_send(update).is_err() {
            dropped += 1;
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            let elapsed = last_log.elapsed().as_secs_f64();
            let (avg_ms, p95_ms) = summarize_ms(&mut samples);
            info!(
                "preview pipeline: {:.1} fps, {:.1} ms avg / {:.1} ms p95, {} output drops",
                generated as f64 / elapsed,
                avg_ms,
                p95_ms,
                dropped
            );
            generated = 0;
            dropped = 0;
            last_log = Instant::now();
        }
    }
}

fn summarize_ms(samples: &mut Vec<f64>) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let avg = samples.iter().sum::<f64>() / samples.len() as f64;
    samples.sort_by(f64::total_cmp);
    let index = ((samples.len() - 1) as f64 * 0.95).round() as usize;
    let p95 = samples[index];
    samples.clear();
    (avg, p95)
}

fn make_preview(
    pixels: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    bgra: bool,
) -> Vec<u8> {
    let mut out = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let sy = y * src_h / dst_h;
            let src_i = ((sy * src_w + sx) * 4) as usize;
            let dst_i = ((y * dst_w + x) * 4) as usize;
            if bgra {
                out[dst_i] = pixels[src_i + 2];
                out[dst_i + 1] = pixels[src_i + 1];
                out[dst_i + 2] = pixels[src_i];
                out[dst_i + 3] = pixels[src_i + 3];
            } else {
                out[dst_i..dst_i + 4].copy_from_slice(&pixels[src_i..src_i + 4]);
            }
        }
    }
    out
}

#[cfg(test)]
mod preview_tests {
    use super::make_preview;

    #[test]
    fn converts_bgra_preview_to_rgba() {
        let bgra = [10, 20, 240, 255, 200, 100, 5, 128];
        assert_eq!(
            make_preview(&bgra, 2, 1, 2, 1, true),
            [240, 20, 10, 255, 5, 100, 200, 128]
        );
    }

    #[test]
    fn preserves_rgba_preview() {
        let rgba = [240, 20, 10, 255];
        assert_eq!(make_preview(&rgba, 1, 1, 1, 1, false), rgba);
    }
}

#[cfg(windows)]
fn resize_bgra(
    source: Vec<u8>,
    src_w: u32,
    src_h: u32,
    resizer: &mut Resizer,
    destination: &mut Image<'static>,
) -> Result<Vec<u8>> {
    if src_w == destination.width() && src_h == destination.height() {
        return Ok(source);
    }
    let source = Image::from_vec_u8(src_w, src_h, source, PixelType::U8x4)
        .map_err(|e| anyhow::anyhow!("invalid WGC readback buffer: {e}"))?;
    resizer
        .resize(&source, destination, None)
        .map_err(|e| anyhow::anyhow!("WGC CPU fallback resize failed: {e}"))?;
    Ok(destination.buffer().to_vec())
}
