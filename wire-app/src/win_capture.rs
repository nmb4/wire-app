//! Direct Windows Graphics Capture path.
//!
//! This is intentionally small and conservative. zed-scap's Windows backend always routes frames
//! through `buffer_crop`, even for full-display capture, which adds avoidable per-frame GPU/CPU
//! copy work. This path captures the primary monitor directly and reads the raw frame buffer.

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use fast_image_resize as fr;
use fr::images::Image;
use fr::{PixelType, ResizeAlg, ResizeOptions, Resizer};
use tracing::{info, warn};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings};

const FRAME_QUEUE_DEPTH: usize = 2;
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(800);

struct CapturedFrame {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

pub struct WindowsCapturer {
    control: Option<CaptureControl<CaptureHandler, Box<dyn std::error::Error + Send + Sync>>>,
    rx: Receiver<CapturedFrame>,
    pending: Option<CapturedFrame>,
    src_w: u32,
    src_h: u32,
    needs_resize: bool,
    resizer: Resizer,
    resize_options: ResizeOptions,
    dst: Image<'static>,
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
            needs_resize,
            resizer: Resizer::new(),
            resize_options: ResizeOptions::new()
                .resize_alg(ResizeAlg::Nearest)
                .use_alpha(false),
            dst: Image::new(target_w, target_h, PixelType::U8x4),
        })
    }

    pub fn capture_bgra(&mut self) -> Result<Vec<u8>> {
        if let Some(frame) = self.pending.take() {
            return self.prepare_frame(frame);
        }

        loop {
            let frame = self
                .rx
                .recv()
                .context("direct WGC capture channel closed")?;
            if frame.width == self.src_w && frame.height == self.src_h {
                return self.prepare_frame(frame);
            }
            warn!(
                "dropping direct WGC frame with unexpected size {}x{} (expected {}x{})",
                frame.width, frame.height, self.src_w, self.src_h
            );
        }
    }

    fn prepare_frame(&mut self, frame: CapturedFrame) -> Result<Vec<u8>> {
        if !self.needs_resize {
            return Ok(frame.data);
        }

        let src_img = Image::from_vec_u8(frame.width, frame.height, frame.data, PixelType::U8x4)
            .map_err(|e| anyhow!("invalid direct WGC frame buffer: {e}"))?;
        self.resizer
            .resize(&src_img, &mut self.dst, Some(&self.resize_options))
            .map_err(|e| anyhow!("direct WGC resize failed: {e}"))?;
        Ok(self.dst.buffer().to_vec())
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
    tx: SyncSender<CapturedFrame>,
}

struct CaptureHandler {
    tx: SyncSender<CapturedFrame>,
    dropped: u64,
}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = CaptureFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: ctx.flags.tx,
            dropped: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let mut buffer = frame.buffer()?;
        let data = buffer.as_nopadding_buffer()?.to_vec();
        match self.tx.try_send(CapturedFrame {
            width,
            height,
            data,
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
