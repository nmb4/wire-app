//! Direct Windows Graphics Capture path.
//!
//! This is intentionally small and conservative. zed-scap's Windows backend always routes frames
//! through `buffer_crop`, even for full-display capture, which adds avoidable per-frame GPU/CPU
//! copy work. This path captures the primary monitor directly and reads the raw frame buffer.

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use tracing::{info, warn};
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

use crate::win_mf_d3d::{GpuVideoProcessor, MfD3d};

const FRAME_QUEUE_DEPTH: usize = 1;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(800);

struct GpuSlot {
    texture: ID3D11Texture2D,
}

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
            && self.source_height == frame.height
            && self.output_width == width
            && self.output_height == height
    }

    pub fn read_bgra(&self, frame: &GpuCapturedFrame) -> Result<Vec<u8>> {
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
    pub fn try_new(target_w: u32, target_h: u32) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE_DEPTH);
        let monitor = Monitor::primary().context("failed to get primary monitor")?;
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::Default,
            ColorFormat::Bgra8,
            CaptureFlags { tx },
        );

        let control = CaptureHandler::start_free_threaded(settings)
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

    pub fn capture_gpu(&mut self) -> Result<GpuCapturedFrame> {
        if let Some(frame) = self.pending.take() {
            return Ok(frame);
        }

        loop {
            let frame = self
                .rx
                .recv()
                .context("direct WGC capture channel closed")?;
            if frame.width == self.src_w && frame.height == self.src_h {
                return Ok(frame);
            }
            warn!(
                "dropping direct WGC frame with unexpected size {}x{} (expected {}x{})",
                frame.width, frame.height, self.src_w, self.src_h
            );
        }
    }
}

impl Drop for WindowsCapturer {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

#[derive(Clone)]
struct CaptureFlags {
    tx: SyncSender<GpuCapturedFrame>,
}

struct CaptureHandler {
    tx: SyncSender<GpuCapturedFrame>,
    dropped: u64,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    slots: Vec<Arc<GpuSlot>>,
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
            dropped: 0,
            device: ctx.device,
            context: ctx.device_context,
            slots: Vec::new(),
            copy_samples: Vec::with_capacity(300),
            copied: 0,
            last_stats_log: Instant::now(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let copy_started = Instant::now();
        let width = frame.width();
        let height = frame.height();
        let source = unsafe { frame.as_raw_texture() };
        if self.slots.is_empty() {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { source.GetDesc(&mut desc) };
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.CPUAccessFlags = 0;
            for _ in 0..3 {
                let mut texture = None;
                unsafe {
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut texture))?
                };
                self.slots.push(Arc::new(GpuSlot {
                    texture: texture.context("WGC GPU ring texture was null")?,
                }));
            }
            info!("direct WGC GPU texture ring ready ({width}x{height}, 3 slots)");
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
                "direct WGC GPU copy: {:.1} fps, {:.2} ms avg / {:.2} ms p95, {} ring drops",
                self.copied as f64 / elapsed,
                avg,
                p95,
                self.dropped
            );
            self.copy_samples.clear();
            self.copied = 0;
            self.last_stats_log = Instant::now();
        }
        match self.tx.try_send(GpuCapturedFrame {
            width,
            height,
            slot,
            device: self.device.clone(),
            context: self.context.clone(),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                if self.dropped <= 5 || self.dropped % 120 == 0 {
                    warn!(
                        "direct WGC capture queue full, dropped {} frame(s)",
                        self.dropped
                    );
                }
            }
            Err(TrySendError::Disconnected(_)) => {}
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
        for _ in 0..8 {
            let mut capture = WindowsCapturer::try_new(1920, 1080).unwrap();
            for _ in 0..30 {
                drop(capture.capture_gpu().unwrap());
            }
            drop(capture);
            std::thread::sleep(Duration::from_millis(750));
            stopped_memory.push(working_set_bytes());
        }
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
