//! Custom window chrome: themed border, optional rounded corners, transparency sync.
//!
//! Chrome follows the eframe `custom_window_frame` example: a single [`Frame`]
//! with [`Frame::outer_margin`] keeping the stroke inside the viewport. Fill and
//! border are painted at explicit geometry (not [`egui::Ui::min_rect`]) so the
//! right edge is never truncated by nested panel clipping. The clip rect is
//! expanded by [`egui::style::Visuals::clip_rect_margin`] for anti-aliasing.

use eframe::egui::{
    self, epaint, layers::ShapeIdx, Color32, CornerRadius, Frame, Margin, Rect, Sense, Shape,
    Stroke, Ui, UiBuilder, ViewportCommand,
};
use epaint::{RectShape, StrokeKind};
use crate::theme::{Palette, WindowFrameStyle};

pub const CORNER_RADIUS: u8 = 10;
pub const STROKE_WIDTH: f32 = 1.0;
/// Inset from the native window edge so the stroke is never clipped by the swapchain.
pub const OUTER_MARGIN: f32 = 2.0;

pub fn bottom_edge_radius(rounded: bool) -> CornerRadius {
    if rounded {
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: CORNER_RADIUS,
            se: CORNER_RADIUS,
        }
    } else {
        CornerRadius::ZERO
    }
}

pub fn corner_radius(rounded: bool) -> CornerRadius {
    if rounded {
        CornerRadius::same(CORNER_RADIUS)
    } else {
        CornerRadius::ZERO
    }
}

/// Clip rect for chrome painting, expanded for stroke anti-aliasing.
pub fn clip_rect(ctx: &egui::Context) -> Rect {
    ctx.screen_rect()
        .expand(ctx.style().visuals.clip_rect_margin)
}

/// Layout bounds for chrome children, inset inside the painted fill and corner curve.
pub fn body_rect(content_rect: Rect, rounded: bool) -> Rect {
    content_rect.shrink(if rounded {
        STROKE_WIDTH + 2.0
    } else {
        STROKE_WIDTH
    })
}

pub fn panel_frame(pal: &Palette, rounded: bool) -> Frame {
    Frame::new()
        .fill(pal.bg)
        .corner_radius(corner_radius(rounded))
        .stroke(Stroke::new(STROKE_WIDTH, pal.frame_border))
        .outer_margin(Margin::same(OUTER_MARGIN as i8))
}

fn paint_panel_background(
    ui: &Ui,
    where_to_put_background: ShapeIdx,
    pal: &Palette,
    rounded: bool,
    content_rect: Rect,
) {
    let frame = panel_frame(pal, rounded);
    let fill_rect = frame.fill_rect(content_rect);
    let widget_rect = frame.widget_rect(content_rect);
    if !ui.is_rect_visible(widget_rect) {
        return;
    }

    let radius = corner_radius(rounded);
    let fill_shape = Shape::Rect(RectShape::new(
        fill_rect,
        radius,
        pal.bg,
        Stroke::NONE,
        StrokeKind::Inside,
    ));
    let border_shape = Shape::Rect(RectShape::new(
        widget_rect,
        radius,
        Color32::TRANSPARENT,
        Stroke::new(STROKE_WIDTH, pal.frame_border),
        StrokeKind::Inside,
    ));
    ui.painter().set(
        where_to_put_background,
        Shape::Vec(vec![fill_shape, border_shape]),
    );
}

/// Show the chrome panel, painting fill and border at the full allocated size.
pub fn show_panel<R>(
    ui: &mut Ui,
    pal: &Palette,
    rounded: bool,
    add_contents: impl FnOnce(&mut Ui, Rect) -> R,
) -> R {
    let frame = panel_frame(pal, rounded);
    let outer_bounds = ui.available_rect_before_wrap();
    let mut content_rect = outer_bounds - frame.total_margin();
    content_rect.max.x = content_rect.max.x.max(content_rect.min.x);
    content_rect.max.y = content_rect.max.y.max(content_rect.min.y);

    let where_to_put_background = ui.painter().add(Shape::Noop);
    let mut content_ui = ui.new_child(UiBuilder::new().max_rect(content_rect));

    let ret = add_contents(&mut content_ui, content_rect);

    content_ui.expand_to_include_rect(content_rect);
    content_ui.allocate_rect(content_rect, Sense::hover());
    paint_panel_background(ui, where_to_put_background, pal, rounded, content_rect);
    ui.allocate_rect(frame.outer_rect(content_rect), Sense::hover());
    ret
}

/// Windows 11 is build 22000 and above.
#[cfg(windows)]
pub fn is_windows_11_or_newer() -> bool {
    windows_build_number().is_some_and(|build| build >= 22000)
}

#[cfg(windows)]
fn windows_build_number() -> Option<u32> {
    use std::mem::MaybeUninit;

    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    unsafe {
        let mut info = MaybeUninit::<OSVERSIONINFOW>::uninit();
        (*info.as_mut_ptr()).dwOSVersionInfoSize =
            u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).ok()?;
        if RtlGetVersion(info.as_mut_ptr()).is_err() {
            return None;
        }
        Some((*info.as_ptr()).dwBuildNumber)
    }
}

#[cfg(not(windows))]
pub fn is_windows_11_or_newer() -> bool {
    false
}

pub fn style_wants_rounded(style: WindowFrameStyle) -> bool {
    match style {
        WindowFrameStyle::Auto => is_windows_11_or_newer(),
        WindowFrameStyle::Rounded => true,
        WindowFrameStyle::Square => false,
    }
}

pub fn effective_rounded(ctx: &egui::Context, style: WindowFrameStyle) -> bool {
    if !style_wants_rounded(style) {
        return false;
    }
    !ctx.input(|i| i.viewport().maximized.unwrap_or(false))
}

pub fn sync_viewport_transparent(ctx: &egui::Context, transparent: bool) {
    ctx.send_viewport_cmd(ViewportCommand::Transparent(transparent));
}