//! Native D3D11 presentation for decoded Media Foundation video surfaces.
//!
//! eframe currently renders Wire with OpenGL, which cannot portably import a
//! D3D11 NV12 texture. A small child HWND lets the decoder's own D3D11 device
//! scale and color-convert that texture directly into a swap chain while egui
//! continues to own layout and controls around the video rectangle.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};
use windows::core::{w, Interface};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoContext1,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_ERROR_WAS_STILL_DRAWING,
    DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOZORDER,
    SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNA, WINDOW_EX_STYLE, WS_CHILD, WS_CLIPSIBLINGS,
};

use crate::win_mf_codec::D3d11VideoFrame;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhysicalVideoRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

struct VideoProcessorState {
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

struct PresentStats {
    started: Instant,
    samples_ms: Vec<f64>,
    frames: u64,
    busy_drops: u64,
}

impl Default for PresentStats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            samples_ms: Vec::with_capacity(300),
            frames: 0,
            busy_drops: 0,
        }
    }
}

pub(crate) struct NativeVideoPresenter {
    child: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    swap_chain: IDXGISwapChain1,
    processor: Option<VideoProcessorState>,
    rect: PhysicalVideoRect,
    used_this_frame: bool,
    visible: bool,
    last_generation: u64,
    stats: PresentStats,
}

impl NativeVideoPresenter {
    pub(crate) fn new(
        parent: HWND,
        frame: &D3d11VideoFrame,
        rect: PhysicalVideoRect,
    ) -> Result<Self> {
        if rect.width == 0 || rect.height == 0 {
            anyhow::bail!("native video rectangle is empty");
        }
        let child = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS,
                rect.x,
                rect.y,
                rect.width as i32,
                rect.height as i32,
                Some(parent),
                None,
                None,
                None,
            )
            .context("creating native video child window")?
        };

        let result = Self::create_for_child(child, frame, rect);
        if result.is_err() {
            unsafe {
                let _ = DestroyWindow(child);
            }
        }
        result
    }

    fn create_for_child(
        child: HWND,
        frame: &D3d11VideoFrame,
        rect: PhysicalVideoRect,
    ) -> Result<Self> {
        let device = frame.device().clone();
        let context = unsafe { device.GetImmediateContext()? };
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: rect.width,
            Height: rect.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swap_chain = unsafe {
            factory
                .CreateSwapChainForHwnd(&device, child, &desc, None, None)
                .context("creating native video swap chain")?
        };
        info!(
            "native D3D11 video presenter ready ({}x{} child surface)",
            rect.width, rect.height
        );
        Ok(Self {
            child,
            device,
            context,
            video_device,
            video_context,
            swap_chain,
            processor: None,
            rect,
            used_this_frame: false,
            visible: false,
            last_generation: u64::MAX,
            stats: PresentStats::default(),
        })
    }

    pub(crate) fn uses_device(&self, frame: &D3d11VideoFrame) -> bool {
        Interface::as_raw(&self.device) == Interface::as_raw(frame.device())
    }

    pub(crate) fn mark_unused(&mut self) {
        self.used_this_frame = false;
    }

    pub(crate) fn hide_if_unused(&mut self, force: bool) {
        if (force || !self.used_this_frame) && self.visible {
            unsafe {
                let _ = ShowWindow(self.child, SW_HIDE);
            }
            self.visible = false;
        }
    }

    pub(crate) fn hide(&mut self) {
        self.used_this_frame = false;
        self.hide_if_unused(true);
    }

    pub(crate) fn present(
        &mut self,
        frame: &D3d11VideoFrame,
        rect: PhysicalVideoRect,
        generation: u64,
    ) -> Result<()> {
        self.used_this_frame = true;
        if rect.width == 0 || rect.height == 0 {
            self.hide();
            return Ok(());
        }
        let resized = self.rect.width != rect.width || self.rect.height != rect.height;
        if self.rect != rect {
            unsafe {
                SetWindowPos(
                    self.child,
                    None,
                    rect.x,
                    rect.y,
                    rect.width as i32,
                    rect.height as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                )
                .context("positioning native video child window")?;
            }
            self.rect = rect;
        }
        if resized {
            self.processor = None;
            unsafe {
                self.swap_chain.ResizeBuffers(
                    0,
                    rect.width,
                    rect.height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
            }
            self.last_generation = u64::MAX;
        }
        if !self.visible {
            unsafe {
                let _ = ShowWindow(self.child, SW_SHOWNA);
            }
            self.visible = true;
        }
        if self.last_generation == generation && !resized {
            return Ok(());
        }

        let (coded_width, coded_height) = frame.coded_size();
        self.ensure_processor(coded_width, coded_height, rect.width, rect.height)?;
        let started = Instant::now();
        self.blit(frame.texture(), frame.display_rect())?;
        let present = unsafe { self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) };
        if present == DXGI_ERROR_WAS_STILL_DRAWING {
            // A flip-model queue that is momentarily full should drop the display
            // frame instead of blocking decode or accumulating latency.
            self.stats.busy_drops += 1;
        } else if present.is_err() {
            return Err(windows::core::Error::from_hresult(present).into());
        } else {
            self.last_generation = generation;
            self.stats.frames += 1;
            self.stats
                .samples_ms
                .push(started.elapsed().as_secs_f64() * 1000.0);
        }
        self.log_stats();
        Ok(())
    }

    fn ensure_processor(
        &mut self,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<()> {
        if self.processor.as_ref().is_some_and(|state| {
            state.source_width == source_width
                && state.source_height == source_height
                && state.output_width == output_width
                && state.output_height == output_height
        }) {
            return Ok(());
        }
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source_width,
            InputHeight: source_height,
            OutputFrameRate: rate,
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { self.video_device.CreateVideoProcessorEnumerator(&desc)? };
        let processor = unsafe { self.video_device.CreateVideoProcessor(&enumerator, 0)? };
        if let Ok(context1) = self.video_context.cast::<ID3D11VideoContext1>() {
            unsafe {
                context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
                context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                // Make orientation state explicit on every new processor. This
                // prevents a driver from carrying rotation/mirror state across
                // decoder and stream reinitialization.
                context1.VideoProcessorSetStreamRotation(
                    &processor,
                    0,
                    false,
                    D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
                );
                context1.VideoProcessorSetStreamMirror(&processor, 0, false, false, false);
            }
        }
        self.processor = Some(VideoProcessorState {
            source_width,
            source_height,
            output_width,
            output_height,
            enumerator,
            processor,
        });
        Ok(())
    }

    fn blit(&self, texture: &ID3D11Texture2D, source_rect: RECT) -> Result<()> {
        let state = self.processor.as_ref().context("video processor missing")?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let back_buffer: ID3D11Texture2D = unsafe { self.swap_chain.GetBuffer(0)? };
        let mut input_view = None;
        let mut output_view = None;
        let destination_rect = RECT {
            left: 0,
            top: 0,
            right: state.output_width as i32,
            bottom: state.output_height as i32,
        };
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &state.enumerator,
                &input_desc,
                Some(&mut input_view),
            )?;
            self.video_device.CreateVideoProcessorOutputView(
                &back_buffer,
                &state.enumerator,
                &output_desc,
                Some(&mut output_view),
            )?;
            self.video_context.VideoProcessorSetStreamSourceRect(
                &state.processor,
                0,
                true,
                Some(&source_rect),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &state.processor,
                0,
                true,
                Some(&destination_rect),
            );
            self.video_context.VideoProcessorSetOutputTargetRect(
                &state.processor,
                true,
                Some(&destination_rect),
            );
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(input_view),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &state.processor,
                &output_view.context("video processor output view was null")?,
                0,
                std::slice::from_ref(&stream),
            );
            std::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
            result.context("native video processor blit")?;
            self.context.Flush();
        }
        Ok(())
    }

    fn log_stats(&mut self) {
        if self.stats.started.elapsed() < Duration::from_secs(5) {
            return;
        }
        let elapsed = self.stats.started.elapsed().as_secs_f64();
        if !self.stats.samples_ms.is_empty() {
            let avg =
                self.stats.samples_ms.iter().sum::<f64>() / self.stats.samples_ms.len() as f64;
            self.stats.samples_ms.sort_by(f64::total_cmp);
            let p95 = self.stats.samples_ms
                [((self.stats.samples_ms.len() - 1) as f64 * 0.95).round() as usize];
            info!(
                "native video present: {:.1} fps, {:.2} ms avg / {:.2} ms p95, {} nonblocking drops, {}x{} surface",
                self.stats.frames as f64 / elapsed,
                avg,
                p95,
                self.stats.busy_drops,
                self.rect.width,
                self.rect.height
            );
        }
        self.stats = PresentStats::default();
    }
}

impl Drop for NativeVideoPresenter {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.child).map_err(|error| {
                warn!("failed to destroy native video child window: {error}");
                error
            });
        }
    }
}
