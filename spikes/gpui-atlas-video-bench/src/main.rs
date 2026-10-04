//! GPUI-style CPU BGRA upload benchmark.
//!
//! One window, one 1920x1080 image painted full-window every frame.
//!
//! Modes:
//! - `fixed`: one `ImageId` for the life of the window + monotonic
//!   `content_version` (the Step-2 in-place atlas tile primitive).
//! - `control`: a fresh `RenderImage::new(..)` (new `ImageId`) per frame, i.e.
//!   what unmodified GPUI does; the previous frame's tile is dropped right
//!   after the new one paints (same shape as Zed's
//!   current/previous_rendered_frame + drop_image bookkeeping).
//!
//! Usage:
//!   gpui-atlas-video-bench --mode fixed|control --fps 30|60 --seconds N
//!                          [--width 1920 --height 1080]
//!                          [--win-width 1280 --win-height 720]
//!
//! Window size is in logical pixels; image size is in device pixels. On a
//! 1.5x display, --win-width 1280 --win-height 720 shows a 1920x1080 image 1:1.

mod metrics;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, Context, Corners, ImageId, RenderImage, Window, WindowBounds, WindowOptions,
    canvas, div, prelude::*, px, size,
};
use gpui_platform::application;
use smallvec::SmallVec;

use metrics::{Metrics, bytes_per_frame, print_report};

const GPUI_REV: &str = "254b5dbd47cbb5acbcc5bbdcbb322a339276c88a";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Fixed,
    Control,
}

struct Args {
    mode: Mode,
    fps: u32,
    seconds: u32,
    width: u32,
    height: u32,
    win_width: f32,
    win_height: f32,
}

fn parse_args() -> Args {
    let mut mode = Mode::Fixed;
    let mut fps = 30u32;
    let mut seconds = 20u32;
    let mut width = 1920u32;
    let mut height = 1080u32;
    let mut win_width: Option<f32> = None;
    let mut win_height: Option<f32> = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let value = it.next().unwrap_or_else(|| {
            eprintln!("missing value after {arg}");
            std::process::exit(2);
        });
        match arg.as_str() {
            "--mode" => {
                mode = match value.as_str() {
                    "fixed" => Mode::Fixed,
                    "control" => Mode::Control,
                    _ => {
                        eprintln!("unknown --mode {value}");
                        std::process::exit(2);
                    }
                }
            }
            "--fps" => fps = value.parse().expect("--fps must be a number"),
            "--seconds" => seconds = value.parse().expect("--seconds must be a number"),
            "--width" => width = value.parse().expect("--width must be a number"),
            "--height" => height = value.parse().expect("--height must be a number"),
            "--win-width" => win_width = Some(value.parse().expect("--win-width must be a number")),
            "--win-height" => {
                win_height = Some(value.parse().expect("--win-height must be a number"))
            }
            _ => {
                eprintln!("unknown arg {arg}");
                std::process::exit(2);
            }
        }
    }
    Args {
        mode,
        fps,
        seconds,
        width,
        height,
        win_width: win_width.unwrap_or(width as f32),
        win_height: win_height.unwrap_or(height as f32),
    }
}

/// Fill `buf` (width*height*4 BGRA bytes) with synthetic content that actually
/// changes every frame: B/G gradients plus a moving R pattern. Every byte is
/// written, so the alloc+write cost is honestly in the measurement.
fn fill_bgra(buf: &mut [u8], width: u32, height: u32, frame_no: u64) {
    debug_assert_eq!(buf.len(), width as usize * height as usize * 4);
    let tick = frame_no.wrapping_mul(3) as u8;
    let stride = width as usize * 4;
    for (y, row) in buf.chunks_exact_mut(stride).enumerate() {
        let gy = y as u8;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            px[0] = x as u8; // B gradient
            px[1] = gy; // G gradient
            px[2] = (x as u8).wrapping_add(gy).wrapping_add(tick); // R moving pattern
            px[3] = 255;
        }
    }
    let _ = height;
}

struct VideoBench {
    mode: Mode,
    width: u32,
    height: u32,
    stable_id: ImageId,
    content_version: u64,
    generation: u64,
    current: Arc<RenderImage>,
    /// Control mode: tiles superseded between paints are dropped on the next
    /// paint (a paint context is required for `drop_image`).
    pending_drops: Vec<Arc<RenderImage>>,
    metrics: Arc<Mutex<Metrics>>,
    scale_logged: bool,
}

impl VideoBench {
    fn initial_image() -> Arc<RenderImage> {
        let buf = image::RgbaImage::from_raw(64, 64, vec![0u8; 64 * 64 * 4]).expect("dims match");
        let mut frames = SmallVec::new();
        frames.push(image::Frame::new(buf));
        Arc::new(RenderImage::new(frames))
    }

    fn publish_frame(&mut self, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        let len = self.width as usize * self.height as usize * 4;
        // Fresh allocation per frame, like Zed's `alloc(w*h*4)`: the alloc is
        // part of what is being measured.
        let mut bytes = vec![0u8; len];
        // Generation is assigned here so the fill pattern changes every frame.
        let fill_generation = self.next_generation_hint();
        fill_bgra(&mut bytes, self.width, self.height, fill_generation);
        let build_us = t0.elapsed().as_micros() as u64;

        let buf = image::RgbaImage::from_raw(self.width, self.height, bytes).expect("dims match");
        let mut frames = SmallVec::new();
        frames.push(image::Frame::new(buf));
        let img = match self.mode {
            Mode::Fixed => {
                self.content_version += 1;
                Arc::new(RenderImage::new_with_id(
                    self.stable_id,
                    self.content_version,
                    frames,
                ))
            }
            Mode::Control => Arc::new(RenderImage::new(frames)),
        };

        let old = std::mem::replace(&mut self.current, img);
        if self.mode == Mode::Control {
            // The paint closure drops these after the replacement has painted.
            self.pending_drops.push(old);
        }
        if let Ok(mut metrics) = self.metrics.lock() {
            self.generation = metrics.on_published(build_us);
        }
        cx.notify();
    }

    fn next_generation_hint(&self) -> u64 {
        // Cheap peek so the fill pattern advances even before Metrics assigns
        // the authoritative generation; exact equality does not matter, only
        // that content changes frame to frame.
        self.generation.wrapping_add(1)
    }
}

impl Render for VideoBench {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if !self.scale_logged {
            self.scale_logged = true;
            eprintln!(
                "window scale_factor={:?} (image is {}x{} device px; 1.0 means 1:1)",
                window.scale_factor(),
                self.width,
                self.height
            );
        }
        let img = Arc::clone(&self.current);
        let generation = self.generation;
        let metrics = Arc::clone(&self.metrics);
        let drops = std::mem::take(&mut self.pending_drops);
        div().size_full().child(
            canvas(
                move |_bounds, _, _| (img, generation, metrics, drops),
                move |bounds, (img, generation, metrics, drops), window, _| {
                    let t0 = Instant::now();
                    let paint_result =
                        window.paint_image(bounds, bounds, Corners::default(), img, 0, false);
                    let upload_us = t0.elapsed().as_micros() as u64;
                    if let Err(err) = paint_result {
                        eprintln!("paint_image failed: {err:#}");
                    }
                    for old in drops {
                        if let Err(err) = window.drop_image(old) {
                            eprintln!("drop_image failed: {err:#}");
                        }
                    }
                    if let Ok(mut metrics) = metrics.lock() {
                        metrics.on_presented(generation, upload_us);
                    }
                },
            )
            .size_full(),
        )
    }
}

fn main() {
    // Bench-driver pacing fix (NOT part of the measured path): stock Windows
    // ticks the system timer at ~15.6ms, so any Sleep-based pacing overshoots
    // short periods (Sleep(6.7ms) wakes at ~15.6ms) and a 60fps driver drops
    // frames it should not. timeBeginPeriod(1) is what media apps use;
    // winmm is linked raw so the bench gains no new deps. Restored by the OS
    // at process exit.
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(u_period: u32) -> u32;
        }
        let rc = unsafe { timeBeginPeriod(1) };
        eprintln!("timeBeginPeriod(1) -> {rc}");
    }

    let args = parse_args();
    let mode = args.mode;
    let fps = args.fps;
    let seconds = args.seconds;
    let width = args.width;
    let height = args.height;
    let win_width = args.win_width;
    let win_height = args.win_height;
    let bpf = bytes_per_frame(width, height);
    eprintln!(
        "gpui-atlas-video-bench mode={mode:?} target={fps}fps seconds={seconds} {width}x{height} bytes_per_frame={bpf} rev={GPUI_REV}"
    );

    application().run(move |cx: &mut App| {
        let view = cx.new(|_| VideoBench {
            mode,
            width,
            height,
            stable_id: RenderImage::new_image_id(),
            content_version: 0,
            generation: 0,
            current: VideoBench::initial_image(),
            pending_drops: Vec::new(),
            metrics: Arc::new(Mutex::new(Metrics::with_warmup(
                // Skip the first second (size-change realloc + window warmup)
                // so steady-state numbers are not polluted.
                fps as u64,
            ))),
            scale_logged: false,
        });
        let root = view.clone();
        let bounds = Bounds::centered(None, size(px(win_width), px(win_height)), cx);
        cx.open_window(
            WindowOptions::new().window_bounds(Some(WindowBounds::Windowed(bounds))),
            move |_, _| root,
        )
        .unwrap();

        // Try to take the foreground: a backgrounded window's presents can be
        // throttled by DWM, which would stall the driver (it shares the UI
        // thread) and read as drops. Windows may still refuse from a
        // non-interactive launch; the run logs whether intervals stay clean.
        cx.activate(true);

        cx.spawn(async move |cx| {
            // Hybrid pacing: coarse executor-timer sleep (which yields to the
            // platform event loop so frames actually render) plus a short
            // spin for precision. A pure Sleep-paced driver cannot hold
            // 30/60fps on stock Windows (~15.6ms timer granularity turns
            // Sleep(33) into ~46.9ms), and a pure spin never yields, which
            // starves rendering entirely (measured: 600 publishes, 1 present).
            // The spin window stays small (3ms) because the driver task shares
            // the UI thread: a long spin would block the renders being
            // measured. Combined with timeBeginPeriod(1) (see main), the
            // coarse sleep lands within ~1ms.
            let period = Duration::from_micros(1_000_000 / fps as u64);
            let total = fps as u64 * seconds as u64;
            let start = Instant::now();
            for i in 0..total {
                let deadline = start + period * (i as u32 + 1);
                while Instant::now() + Duration::from_millis(3) < deadline {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let sleep = (deadline - now)
                        .checked_sub(Duration::from_millis(3))
                        .unwrap_or_default();
                    if sleep.is_zero() {
                        break;
                    }
                    cx.background_executor().timer(sleep).await;
                }
                while Instant::now() < deadline {
                    std::hint::spin_loop();
                }
                cx.update(|cx| {
                    view.update(cx, |bench, cx| {
                        bench.publish_frame(cx);
                    });
                });
            }
            // Grace period so the last published frames get painted.
            cx.background_executor().timer(Duration::from_secs(2)).await;
            cx.update(|cx| {
                if let Ok(metrics) = view.read(cx).metrics.lock() {
                    let mode_str = match mode {
                        Mode::Fixed => "fixed-tile",
                        Mode::Control => "control",
                    };
                    print_report(mode_str, fps, seconds, width, height, GPUI_REV, &metrics);
                }
                cx.quit();
            });
        })
        .detach();
    });
}
