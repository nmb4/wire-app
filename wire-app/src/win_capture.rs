//! Direct Windows Graphics Capture path.
//!
//! Frames stay in a bounded GPU pool. Rate admission before copying follows
//! IsoCast's capture.rs; see third-party/isocast/README.md (MIT).

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use tracing::info;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings};
use windows_capture::window::Window;

use crate::latest_frame::{self, Receiver, Sender};
use crate::screen_capture::{CaptureTarget, CaptureTargetKind};
use crate::win_mf_d3d::{GpuVideoProcessor, GpuVideoProcessorOutput, MfD3d};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const FRAME_WAIT: Duration = Duration::from_millis(50);
const CAPTURE_TEXTURES: usize = 4;

struct GpuSlot {
    texture: ID3D11Texture2D,
}

#[derive(Clone)]
pub struct GpuCapturedFrame {
    slot: Arc<GpuSlot>,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub width: u32,
    pub height: u32,
}

impl GpuCapturedFrame {
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.slot.texture
    }

    pub fn read_bgra(&self) -> Result<Vec<u8>> {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { self.slot.texture.GetDesc(&mut source_desc) };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source_desc.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        let staging = staging.context("WGC readback staging texture was null")?;
        unsafe {
            self.context.CopyResource(&staging, &self.slot.texture);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let row_bytes = self.width as usize * 4;
        let mut out = vec![0u8; row_bytes * self.height as usize];
        unsafe {
            let source = mapped.pData as *const u8;
            for row in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    source.add(row * mapped.RowPitch as usize),
                    out.as_mut_ptr().add(row * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(&staging, 0);
        }
        Ok(out)
    }
}

/// Downscales a WGC texture before readback so the local preview never maps a full 4K frame.
pub struct GpuPreviewScaler {
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    d3d: MfD3d,
    processor: GpuVideoProcessor,
    target: ID3D11Texture2D,
    staging: ID3D11Texture2D,
}

impl GpuPreviewScaler {
    pub fn new(frame: &GpuCapturedFrame, output_width: u32, output_height: u32) -> Result<Self> {
        let d3d = MfD3d::from_device(&frame.device)?;
        let processor = GpuVideoProcessor::new(
            &d3d,
            frame.width,
            frame.height,
            output_width,
            output_height,
            5,
            GpuVideoProcessorOutput::BgraFullRange,
        )?;
        let base = D3D11_TEXTURE2D_DESC {
            Width: output_width,
            Height: output_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut target = None;
        unsafe {
            frame
                .device
                .CreateTexture2D(&base, None, Some(&mut target))
                .context("creating GPU preview render target")?;
        }
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..base
        };
        let mut staging = None;
        unsafe {
            frame
                .device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                .context("creating GPU preview staging texture")?;
        }
        Ok(Self {
            source_width: frame.width,
            source_height: frame.height,
            output_width,
            output_height,
            d3d,
            processor,
            target: target.context("GPU preview render target was null")?,
            staging: staging.context("GPU preview staging texture was null")?,
        })
    }

    pub fn matches(&self, frame: &GpuCapturedFrame, width: u32, height: u32) -> bool {
        self.source_width == frame.width
            && self.d3d.device == frame.device
            && self.source_height == frame.height
            && self.output_width == width
            && self.output_height == height
    }

    pub fn read_bgra(&mut self, frame: &GpuCapturedFrame) -> Result<Vec<u8>> {
        self.processor
            .convert(frame.texture(), &self.target)
            .context("downscaling local preview on the GPU")?;
        unsafe {
            self.d3d.context.CopyResource(&self.staging, &self.target);
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.d3d
                .context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("mapping downscaled GPU preview")?;
        }
        let row_bytes = self.output_width as usize * 4;
        let mut out = vec![0u8; row_bytes * self.output_height as usize];
        unsafe {
            let source = mapped.pData as *const u8;
            for row in 0..self.output_height as usize {
                std::ptr::copy_nonoverlapping(
                    source.add(row * mapped.RowPitch as usize),
                    out.as_mut_ptr().add(row * row_bytes),
                    row_bytes,
                );
            }
            self.d3d.context.Unmap(&self.staging, 0);
        }
        Ok(out)
    }
}

pub struct WindowsCapturer {
    control: Option<CaptureControl<CaptureHandler, Box<dyn std::error::Error + Send + Sync>>>,
    rx: Receiver<GpuCapturedFrame>,
    pending: Option<GpuCapturedFrame>,
    src_w: u32,
    src_h: u32,
}

impl WindowsCapturer {
    pub fn try_new(
        target_w: u32,
        target_h: u32,
        framerate: u32,
        target: Option<&CaptureTarget>,
    ) -> Result<Self> {
        anyhow::ensure!(
            (1..=240).contains(&framerate),
            "capture frame rate must be in 1..=240"
        );
        let (tx, rx) = latest_frame::channel();
        let flags = CaptureFlags { tx, framerate };
        let control = match target {
            Some(target) => match target.kind {
                CaptureTargetKind::Display => {
                    let monitor = Monitor::enumerate()
                        .context("failed to enumerate monitors")?
                        .into_iter()
                        .find(|monitor| monitor.as_raw_hmonitor() as usize as u32 == target.id)
                        .context("selected monitor is no longer available")?;
                    CaptureHandler::start_free_threaded(Settings::new(
                        monitor,
                        CursorCaptureSettings::WithCursor,
                        DrawBorderSettings::Default,
                        ColorFormat::Bgra8,
                        flags,
                    ))
                }
                CaptureTargetKind::Window => {
                    let window = Window::enumerate()
                        .context("failed to enumerate windows")?
                        .into_iter()
                        .find(|window| window.as_raw_hwnd() as usize as u32 == target.id)
                        .context("selected window is no longer available")?;
                    CaptureHandler::start_free_threaded(Settings::new(
                        window,
                        CursorCaptureSettings::WithCursor,
                        DrawBorderSettings::Default,
                        ColorFormat::Bgra8,
                        flags,
                    ))
                }
            },
            None => {
                let monitor = Monitor::primary().context("failed to get primary monitor")?;
                CaptureHandler::start_free_threaded(Settings::new(
                    monitor,
                    CursorCaptureSettings::WithCursor,
                    DrawBorderSettings::Default,
                    ColorFormat::Bgra8,
                    flags,
                ))
            }
        }
        .context("failed to start direct Windows Graphics Capture")?;

        let first = match rx.recv_timeout(FIRST_FRAME_TIMEOUT) {
            Ok(frame) => frame,
            Err(e) => {
                let _ = control.stop();
                return Err(anyhow!("timed out waiting for first WGC frame: {e}"));
            }
        };

        let needs_resize = first.width != target_w || first.height != target_h;

        info!(
            "direct WGC capture started (native {}x{} -> {}x{}, resize={needs_resize}, cursor=true)",
            first.width, first.height, target_w, target_h
        );

        Ok(Self {
            control: Some(control),
            rx,
            src_w: first.width,
            src_h: first.height,
            pending: Some(first),
        })
    }

    pub fn capture_gpu(&mut self) -> Result<Option<GpuCapturedFrame>> {
        if let Some(frame) = self.pending.take() {
            return Ok(Some(frame));
        }

        // WGC may deliver nothing for a static/minimized desktop. Bound the
        // wait so stopping sharing never depends on another desktop update.
        let frame = match self.rx.recv_timeout(FRAME_WAIT) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout) => {
                if self
                    .control
                    .as_ref()
                    .is_some_and(|control| control.is_finished())
                {
                    if let Some(control) = self.control.take() {
                        control
                            .wait()
                            .map_err(|error| anyhow!("WGC worker failed: {error}"))?;
                    }
                    anyhow::bail!("the captured display or window closed");
                }
                return Ok(None);
            }
            Err(error) => return Err(error).context("direct WGC capture channel closed"),
        };
        if frame.width != self.src_w || frame.height != self.src_h {
            info!(
                "direct WGC target resized from {}x{} to {}x{}",
                self.src_w, self.src_h, frame.width, frame.height
            );
            self.src_w = frame.width;
            self.src_h = frame.height;
        }
        Ok(Some(frame))
    }
}

impl Drop for WindowsCapturer {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

struct CaptureFlags {
    tx: Sender<GpuCapturedFrame>,
    framerate: u32,
}

/// Keep a phase deadline rather than timing from the last accepted picture:
/// a 144 Hz source must produce 60 fps, not fall to 48 fps. WGC time is 100 ns.
struct CaptureRate {
    interval: i64,
    next: Option<i64>,
    previous: Option<i64>,
}

impl CaptureRate {
    fn new(framerate: u32) -> Self {
        Self {
            interval: 10_000_000 / i64::from(framerate.max(1)),
            next: None,
            previous: None,
        }
    }

    fn admit(&mut self, at: i64) -> bool {
        if self.previous.is_some_and(|previous| at < previous) {
            self.next = None;
        }
        self.previous = Some(at);
        let next = self.next.unwrap_or(at);
        // IsoCast allows 1 ms jitter in the WGC timestamp. Preserve the phase
        // across normal ticks; after a stall start fresh, never burst to catch up.
        if at.saturating_add(10_000) < next {
            return false;
        }
        self.next = Some(if at.saturating_sub(next) >= self.interval {
            at.saturating_add(self.interval)
        } else {
            next.saturating_add(self.interval)
        });
        true
    }
}

struct CaptureHandler {
    tx: Sender<GpuCapturedFrame>,
    rate: CaptureRate,
    rate_skipped: u64,
    dropped: u64,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    slots: Vec<Arc<GpuSlot>>,
    source_dimensions: Option<(u32, u32)>,
    copy_samples: Vec<f64>,
    copied: u64,
    last_stats_log: Instant,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = CaptureFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: ctx.flags.tx,
            rate: CaptureRate::new(ctx.flags.framerate),
            rate_skipped: 0,
            dropped: 0,
            device: ctx.device,
            context: ctx.device_context,
            slots: Vec::new(),
            source_dimensions: None,
            copy_samples: Vec::with_capacity(300),
            copied: 0,
            last_stats_log: Instant::now(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Skip BEFORE acquiring/copying a full-resolution surface.
        if !self.rate.admit(frame.timespan().Duration) {
            self.rate_skipped += 1;
            return Ok(());
        }
        let copy_started = Instant::now();
        let width = frame.width();
        let height = frame.height();
        let source = unsafe { frame.as_raw_texture() };
        if self.source_dimensions != Some((width, height)) {
            if let Some((old_width, old_height)) = self.source_dimensions {
                info!(
                    "recreating direct WGC texture ring after target resized from {old_width}x{old_height} to {width}x{height}"
                );
            }
            self.slots.clear();
            self.source_dimensions = Some((width, height));
        }
        if self.slots.is_empty() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { source.GetDesc(&mut desc) };
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.CPUAccessFlags = 0;
            for _ in 0..CAPTURE_TEXTURES {
                let mut texture = None;
                unsafe {
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut texture))?
                };
                self.slots.push(Arc::new(GpuSlot {
                    texture: texture.context("WGC GPU ring texture was null")?,
                }));
            }
            info!("direct WGC GPU texture ring ready ({width}x{height}, {CAPTURE_TEXTURES} slots)");
        }
        let Some(slot) = self
            .slots
            .iter()
            .find(|slot| Arc::strong_count(slot) == 1)
            .cloned()
        else {
            self.dropped += 1;
            return Ok(());
        };
        unsafe {
            self.context.CopyResource(&slot.texture, source);
        }
        self.copy_samples
            .push(copy_started.elapsed().as_secs_f64() * 1000.0);
        self.copied += 1;
        if self.last_stats_log.elapsed() >= Duration::from_secs(5) {
            let elapsed = self.last_stats_log.elapsed().as_secs_f64();
            let avg = self.copy_samples.iter().sum::<f64>() / self.copy_samples.len() as f64;
            self.copy_samples.sort_by(f64::total_cmp);
            let p95 =
                self.copy_samples[((self.copy_samples.len() - 1) as f64 * 0.95).round() as usize];
            info!(
                "direct WGC GPU copy: {:.1} fps, {:.2} ms avg / {:.2} ms p95, {} replaced/pool drops, {} skipped before copy",
                self.copied as f64 / elapsed,
                avg,
                p95,
                self.dropped,
                self.rate_skipped
            );
            self.copy_samples.clear();
            self.copied = 0;
            self.last_stats_log = Instant::now();
        }
        match self.tx.send(GpuCapturedFrame {
            width,
            height,
            slot,
            device: self.device.clone(),
            context: self.context.clone(),
        }) {
            Ok(replaced) => self.dropped += u64::from(replaced),
            Err(_) => control.stop(),
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::WindowsCapturer;
    use std::time::Duration;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    #[test]
    fn rate_admission_preserves_sixty_fps_on_fast_and_fractional_displays() {
        for refresh in [60, 75, 120, 144, 165, 240] {
            let mut rate = super::CaptureRate::new(60);
            let accepted = (0..refresh * 10)
                .filter(|index| rate.admit(i64::from(*index) * 10_000_000 / i64::from(refresh)))
                .count();
            assert!(
                (599..=601).contains(&accepted),
                "{refresh} Hz: {accepted} copies in 10 s"
            );
        }
        let mut rate = super::CaptureRate::new(30);
        assert!(rate.admit(0));
        assert!(!rate.admit(10_000));
        assert!(rate.admit(100_000_000)); // long static interval
        assert!(!rate.admit(100_010_000)); // no catch-up burst
        assert!(rate.admit(0)); // new timestamp origin
    }

    fn working_set_bytes() -> u64 {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            ProcessRefreshKind::new().with_memory(),
        );
        system
            .process(pid)
            .map(|process| process.memory())
            .unwrap_or(0)
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn repeated_wgc_start_stop_reaches_a_memory_plateau() {
        let mut stopped_memory = Vec::new();
        let mut stopped_handles = Vec::new();
        for _ in 0..8 {
            let mut capture = WindowsCapturer::try_new(1920, 1080, 30, None).unwrap();
            for _ in 0..30 {
                drop(capture.capture_gpu().unwrap());
            }
            drop(capture);
            std::thread::sleep(Duration::from_millis(750));
            stopped_memory.push(working_set_bytes());
            stopped_handles.push(crate::resource_monitor::test_process_handle_count().unwrap());
        }
        println!("WGC post-stop handles: {stopped_handles:?}");
        println!(
            "WGC-only post-stop working sets (MiB): {:?}",
            stopped_memory
                .iter()
                .map(|bytes| *bytes as f64 / (1024.0 * 1024.0))
                .collect::<Vec<_>>()
        );
        let last = *stopped_memory.last().unwrap();
        let late_growth = last.saturating_sub(stopped_memory[stopped_memory.len() / 2]);
        assert!(late_growth < 16 * 1024 * 1024, "WGC memory did not plateau");
    }
}
