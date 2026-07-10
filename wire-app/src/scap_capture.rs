//! Windows Graphics Capture via zed-scap (same stack Zed uses).

use anyhow::{anyhow, Context, Result};
use wire::video::VideoResolution;
use fast_image_resize as fr;
use fr::images::Image;
use fr::{PixelType, Resizer};
use tracing::info;
use zed_scap::capturer::{Capturer, Options, Resolution as ScapResolution};
use zed_scap::frame::{Frame, FrameType};

pub struct ScapCapturer {
    capturer: Capturer,
    target_w: u32,
    target_h: u32,
    needs_resize: bool,
    resizer: Resizer,
    dst: Image<'static>,
}

impl ScapCapturer {
    pub fn try_new(target_w: u32, target_h: u32, framerate: u32) -> Result<Self> {
        if !zed_scap::is_supported() {
            return Err(anyhow!("zed-scap not supported on this system"));
        }
        if !zed_scap::has_permission() && !zed_scap::request_permission() {
            return Err(anyhow!("screen capture permission denied"));
        }

        let output_resolution = map_resolution(target_w);
        let options = Options {
            fps: framerate,
            show_cursor: true,
            show_highlight: false,
            target: None,
            crop_area: None,
            output_type: FrameType::BGRAFrame,
            output_resolution,
            excluded_targets: None,
        };

        let mut capturer = Capturer::build(options).context("failed to build scap capturer")?;
        capturer.start_capture();

        let [out_w, out_h] = capturer.get_output_frame_size();
        let needs_resize = out_w != target_w || out_h != target_h;
        info!(
            "WGC capture started via zed-scap (native ~{out_w}x{out_h} -> {target_w}x{target_h}, resize={needs_resize})"
        );

        Ok(Self {
            capturer,
            target_w,
            target_h,
            needs_resize,
            resizer: Resizer::new(),
            dst: Image::new(target_w, target_h, PixelType::U8x4),
        })
    }

    pub fn capture_bgra(&mut self) -> Result<Vec<u8>> {
        let frame = self.capturer.get_next_frame()?;
        let (src_w, src_h, data) = match frame {
            Frame::BGRA(f) => (f.width as u32, f.height as u32, f.data),
            Frame::BGRx(f) => (f.width as u32, f.height as u32, f.data),
            Frame::RGBx(f) => (f.width as u32, f.height as u32, f.data),
            other => return Err(anyhow!("unexpected scap frame type: {other:?}")),
        };

        if !self.needs_resize && src_w == self.target_w && src_h == self.target_h {
            return Ok(data);
        }

        let src_img = Image::from_vec_u8(src_w, src_h, data, PixelType::U8x4)
            .map_err(|e| anyhow!("invalid scap frame buffer: {e}"))?;
        self.resizer
            .resize(&src_img, &mut self.dst, None)
            .map_err(|e| anyhow!("resize failed: {e}"))?;
        Ok(self.dst.buffer().to_vec())
    }
}

fn map_resolution(target_w: u32) -> ScapResolution {
    match target_w {
        w if w <= 1280 => ScapResolution::_720p,
        w if w <= 1920 => ScapResolution::_1080p,
        w if w <= 2560 => ScapResolution::_1440p,
        _ => ScapResolution::_2160p,
    }
}

pub fn resolution_from_config(res: VideoResolution) -> ScapResolution {
    match res {
        VideoResolution::P720 => ScapResolution::_720p,
        VideoResolution::P1080 => ScapResolution::_1080p,
        VideoResolution::P1440 => ScapResolution::_1440p,
    }
}
