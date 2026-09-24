//! Shared UI painting and layout helpers.

use crate::theme::{ghost_icon_button, kh_family, lucide, sans, ui_font_size, Palette};
use egui::{Align, Align2, Color32, CornerRadius, Frame, Layout, Rect, RichText, Stroke, Ui, Vec2};
use egui_phosphor::regular as ph;
use lucide_icons::Icon;
use std::sync::atomic::Ordering;
use tracing::warn;
use wire::audio::VolumeHandle;

pub(super) fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else if bytes >= 1024 * 1024 {
        let value = bytes as f64 / MIB;
        if value.fract() < 0.05 {
            format!("{value:.0} MB")
        } else {
            format!("{value:.1} MB")
        }
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

pub(super) fn section_card<R>(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    Frame::new()
        .fill(chat_surface(pal))
        .corner_radius(CornerRadius::same(CHROME_INNER_RADIUS))
        .inner_margin(12.0)
        .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title.to_uppercase())
                    .family(kh_family())
                    .color(pal.dim)
                    .size(11.0),
            );
            ui.add_space(4.0);
            add_contents(ui)
        })
        .inner
}

/// Shared chrome radii so top-bar pills, participant strip, and chips feel unified.
pub(super) const CHROME_RADIUS: u8 = 14;

pub(super) const CHROME_INNER_RADIUS: u8 = 11;

pub(super) const CHROME_CONTROL_HEIGHT: f32 = 36.0;

/// Side inset for bottom chrome (participants + call dock) so content clears the frame.
pub(super) const CHROME_SIDE_INSET: i8 = 14;

pub(super) fn participant_bar_height(width: f32, participant_count: usize, max_height: f32) -> f32 {
    if participant_count == 0 || max_height <= 0.0 {
        return 0.0;
    }

    let columns = participant_bar_columns(width);
    let rows = participant_count.div_ceil(columns);
    let content_height =
        rows as f32 * PARTICIPANT_CHIP_HEIGHT + rows.saturating_sub(1) as f32 * PARTICIPANT_GAP;
    // Participant frame: 4/6 outer margin plus 8/8 inner margin.
    (content_height + 26.0).min(max_height)
}

pub(super) fn video_display_size(available: Vec2, aspect: f32, _fill_window: bool) -> Vec2 {
    if available.x <= 0.0 || available.y <= 0.0 || aspect <= 0.0 {
        return available;
    }
    if available.x / available.y > aspect {
        Vec2::new(available.y * aspect, available.y)
    } else {
        Vec2::new(available.x, available.x / aspect)
    }
}

pub(super) fn aspect_fit_rect(bounds: egui::Rect, aspect: f32) -> egui::Rect {
    egui::Rect::from_center_size(
        bounds.center(),
        video_display_size(bounds.size(), aspect, false),
    )
}

/// Smallest native window size Wire allows (`with_min_inner_size` in main.rs).
const MIN_WINDOW_SIZE: Vec2 = Vec2::new(460.0, 500.0);

/// Viewport rect that floating panes may be laid out in.
///
/// egui clamps the remembered size of every shown [`egui::Window`] to the
/// viewport each frame and persists that clamp, so minimize/restore frames
/// reporting transient degenerate sizes (0×0/1×1 on Windows, shrinking frames
/// in the macOS animation) would permanently shrink open panes to a minimal
/// size. Panes are therefore pinned to the last seen healthy viewport via
/// [`egui::Window::constrain_to`] instead of trusting the live rect.
pub(super) fn track_pane_viewport(current: Rect, candidate: Rect) -> Rect {
    let healthy = candidate.width() >= MIN_WINDOW_SIZE.x && candidate.height() >= MIN_WINDOW_SIZE.y;
    if healthy {
        candidate
    } else {
        current
    }
}

pub(super) fn peer_volume_slider(
    ui: &mut Ui,
    pal: &Palette,
    volume: &VolumeHandle,
    width: f32,
    height: f32,
    hover: &str,
) {
    let mut value = f32::from_bits(volume.load(Ordering::Relaxed));
    if painted_volume_slider(ui, pal, &mut value, 2.0, width, height, hover) {
        volume.store(value.to_bits(), Ordering::Relaxed);
    }
}

pub(super) fn painted_volume_slider(
    ui: &mut Ui,
    pal: &Palette,
    value: &mut f32,
    max: f32,
    width: f32,
    height: f32,
    hover: &str,
) -> bool {
    const TRACK_H: f32 = 3.0;
    const KNOB: f32 = 6.5;
    let max = max.max(0.001);

    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::click_and_drag());
    let response = response
        .on_hover_text(hover)
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);

    let track = egui::Rect::from_center_size(
        rect.center(),
        Vec2::new((rect.width() - KNOB * 2.0).max(8.0), TRACK_H),
    );
    let mut next = value.clamp(0.0, max);
    let mut changed = false;
    if response.dragged() || response.clicked() {
        if let Some(pointer) = response.interact_pointer_pos() {
            next = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0) * max;
            changed = true;
        }
    }
    *value = next;

    paint_volume_track(
        ui,
        pal,
        track,
        next / max,
        next,
        VolumeKnob {
            radius: KNOB,
            fill: pal.bg,
            border: Some((1.15, pal.text)),
        },
        response.hovered() || response.dragged(),
    );
    changed
}

#[derive(Clone, Copy)]
pub(super) struct VolumeKnob {
    pub(super) radius: f32,
    pub(super) fill: Color32,
    pub(super) border: Option<(f32, Color32)>,
}

pub(super) fn paint_volume_track(
    ui: &Ui,
    pal: &Palette,
    track: egui::Rect,
    fill: f32,
    display_value: f32,
    knob: VolumeKnob,
    show_value: bool,
) {
    let fill = fill.clamp(0.0, 1.0);
    ui.painter()
        .rect_filled(track, CornerRadius::same(2), pal.line_br);
    let filled_w = track.width() * fill;
    if filled_w > 0.0 {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(track.min, Vec2::new(filled_w, track.height())),
            CornerRadius::same(2),
            pal.accent,
        );
    }
    let knob_pos = egui::pos2(track.left() + filled_w, track.center().y);
    ui.painter().circle_filled(knob_pos, knob.radius, knob.fill);
    if let Some((width, color)) = knob.border {
        ui.painter()
            .circle_stroke(knob_pos, knob.radius, Stroke::new(width, color));
    }
    if show_value {
        paint_volume_value(ui, pal, knob_pos, knob.radius, display_value);
    }
}

fn paint_volume_value(ui: &Ui, pal: &Palette, knob_pos: egui::Pos2, knob_radius: f32, value: f32) {
    let text = format!("{:.0}%", (value * 100.0).round());
    let galley = ui.painter().layout_no_wrap(text, sans(10.0), pal.text);
    let pad = Vec2::new(5.0, 2.0);
    let badge_size = galley.size() + pad * 2.0;
    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(
            knob_pos.x,
            knob_pos.y - knob_radius - 3.0 - badge_size.y * 0.5,
        ),
        badge_size,
    );
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        ui.id().with("volume-value"),
    ));
    painter.rect_filled(badge_rect, CornerRadius::same(5), pal.bg);
    painter.rect_stroke(
        badge_rect,
        CornerRadius::same(5),
        Stroke::new(1.0_f32, pal.line_br),
        egui::StrokeKind::Inside,
    );
    painter.galley(badge_rect.min + pad, galley, pal.text);
}

pub(super) fn copy_to_clipboard(text: &str) {
    #[cfg(not(target_os = "android"))]
    {
        if let Err(err) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())) {
            warn!("failed to copy to clipboard: {err}");
        }
    }
    #[cfg(target_os = "android")]
    if let Err(err) = android_clipboard::set_text(text.to_string()) {
        warn!("failed to copy to clipboard: {err}");
    }
}

pub(super) fn read_clipboard() -> Option<String> {
    #[cfg(not(target_os = "android"))]
    {
        arboard::Clipboard::new()
            .ok()
            .and_then(|mut c| c.get_text().ok())
    }
    #[cfg(target_os = "android")]
    {
        android_clipboard::get_text().ok()
    }
}

pub(super) fn fmt_node_id(text: &str) -> RichText {
    let text = format!("{text}…");
    egui::RichText::new(text)
        .underline()
        .family(egui::FontFamily::Monospace)
}

pub(super) fn fmt_error(text: &str) -> RichText {
    egui::RichText::new(text).color(Color32::LIGHT_RED)
}

fn mix_color(base: Color32, tint: Color32, amount: f32) -> Color32 {
    let amount = amount.clamp(0.0, 1.0);
    let channel =
        |base: u8, tint: u8| (base as f32 + (tint as f32 - base as f32) * amount).round() as u8;
    Color32::from_rgb(
        channel(base.r(), tint.r()),
        channel(base.g(), tint.g()),
        channel(base.b(), tint.b()),
    )
}

pub(super) fn chat_surface(pal: &Palette) -> Color32 {
    pal.panel
}

fn chat_hover_surface(pal: &Palette) -> Color32 {
    mix_color(pal.bg, pal.panel2, 0.62)
}

pub(super) fn chat_selected_surface(pal: &Palette) -> Color32 {
    mix_color(pal.bg, pal.panel2, 0.76)
}

pub(super) fn chat_hairline(pal: &Palette) -> Color32 {
    mix_color(pal.bg, pal.line, 0.72)
}

pub(super) fn paint_chat_card(ui: &Ui, rect: egui::Rect, pal: &Palette, radius: u8) {
    let radius = CornerRadius::same(radius);
    ui.painter().rect_filled(rect, radius, chat_surface(pal));
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, chat_hairline(pal)),
        egui::StrokeKind::Inside,
    );
}

/// Shared title band for floating settings/contacts dialogs.
///
/// Uses full available width and top-only rounding so the fill meets the
/// panel edges and does not square-bleed over the outer 12px corners.
/// Returns true when the optional close control was clicked.
pub(super) fn floating_dialog_header(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    subtitle: &str,
    close_tooltip: Option<&str>,
) -> bool {
    const RADIUS: u8 = 12;
    let mut closed = false;
    Frame::new()
        .fill(pal.panel)
        .corner_radius(CornerRadius {
            nw: RADIUS,
            ne: RADIUS,
            sw: 0,
            se: 0,
        })
        .inner_margin(egui::Margin::symmetric(18, 12))
        .show(ui, |ui| {
            // Frame sizes to content — expand to the full content max so both
            // left and right edges meet the panel, not just the text run.
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_width(ui.available_width());
                ui.label(
                    RichText::new(title)
                        .family(kh_family())
                        .color(pal.text)
                        .size(16.0),
                );
                ui.label(
                    RichText::new(subtitle)
                        .color(pal.dim)
                        .size(ui_font_size(11.5)),
                );
                if let Some(tooltip) = close_tooltip {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ghost_icon_button(ui, pal, ph::X)
                            .on_hover_text(tooltip)
                            .clicked()
                        {
                            closed = true;
                        }
                    });
                }
            });
        });
    closed
}

pub(super) fn chat_lucide_icon_button(ui: &mut Ui, pal: &Palette, icon: Icon) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), egui::Sense::click());
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), chat_hover_surface(pal));
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(icon),
        lucide(15.0),
        if response.hovered() {
            pal.text
        } else {
            pal.text2
        },
    );
    response
}

pub(super) fn chat_segment_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    selected: bool,
    unseen: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(100.0, CHROME_CONTROL_HEIGHT),
        egui::Sense::click(),
    );
    let fill = if selected {
        chat_selected_surface(pal)
    } else if response.hovered() {
        chat_hover_surface(pal)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(CHROME_INNER_RADIUS), fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        egui::FontId::new(ui_font_size(12.0), egui::FontFamily::Proportional),
        if selected { pal.text } else { pal.text2 },
    );
    if unseen {
        ui.painter()
            .circle_filled(rect.right_top() + Vec2::new(-8.0, 8.0), 3.5, pal.accent);
    }
    response
}

pub(super) fn chat_navigation_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    subtitle: Option<&str>,
    selected: bool,
    unseen: bool,
) -> egui::Response {
    let height = if subtitle.is_some() { 54.0 } else { 42.0 };
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(1.0), height),
        egui::Sense::click(),
    );
    let fill = if selected {
        chat_selected_surface(pal)
    } else if response.hovered() {
        chat_hover_surface(pal)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(12), fill);
    if selected {
        let marker = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 3.0, rect.top() + 12.0),
            egui::pos2(rect.left() + 6.0, rect.bottom() - 12.0),
        );
        ui.painter()
            .rect_filled(marker, CornerRadius::same(2), pal.accent);
    }
    let text_right = if unseen {
        rect.right() - 28.0
    } else {
        rect.right() - 12.0
    };
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 14.0, rect.top() + 6.0),
        egui::pos2(text_right, rect.bottom() - 6.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.set_max_width(text_rect.width());
            if subtitle.is_some() {
                ui.add_space(2.0);
            } else {
                ui.add_space((text_rect.height() - 18.0).max(0.0) * 0.5);
            }
            ui.add(
                egui::Label::new(
                    RichText::new(label)
                        .color(if selected { pal.text } else { pal.text2 })
                        .size(ui_font_size(13.0)),
                )
                .truncate()
                .selectable(false),
            );
            if let Some(subtitle) = subtitle {
                ui.add(
                    egui::Label::new(
                        RichText::new(subtitle)
                            .color(pal.dim)
                            .size(ui_font_size(10.5)),
                    )
                    .truncate()
                    .selectable(false),
                );
            }
        });
    });
    if unseen {
        ui.painter()
            .circle_filled(rect.right_center() - Vec2::new(14.0, 0.0), 4.0, pal.accent);
    }
    response
}

pub(super) fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let mut shortened: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

pub(super) fn participant_bar_columns(width: f32) -> usize {
    let chrome_width = 2.0 * (f32::from(CHROME_SIDE_INSET) + 10.0);
    let inner_width = (width - chrome_width).max(1.0);
    ((inner_width + PARTICIPANT_GAP) / (PARTICIPANT_CARD_SLOT_WIDTH + PARTICIPANT_GAP))
        .floor()
        .max(1.0) as usize
}

/// Fixed participant-chip metrics keep avatar, label, and actions on one midline.
pub(super) const PARTICIPANT_CHIP_HEIGHT: f32 = 40.0;

pub(super) const PARTICIPANT_GAP: f32 = 8.0;

pub(super) const PARTICIPANT_CARD_SLOT_WIDTH: f32 = 250.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participant_bar_adds_rows_as_the_window_narrows() {
        assert_eq!(participant_bar_height(900.0, 3, 500.0), 66.0);
        assert_eq!(participant_bar_height(560.0, 3, 500.0), 114.0);
        assert_eq!(participant_bar_height(320.0, 3, 500.0), 162.0);
        assert_eq!(participant_bar_height(320.0, 3, 100.0), 100.0);
    }

    #[test]
    fn video_fit_preserves_aspect_ratio() {
        let wide = video_display_size(Vec2::new(1000.0, 400.0), 16.0 / 9.0, false);
        assert!((wide.x - 711.1111).abs() < 0.01);
        assert_eq!(wide.y, 400.0);

        let narrow = video_display_size(Vec2::new(400.0, 1000.0), 16.0 / 9.0, false);
        assert_eq!(narrow.x, 400.0);
        assert!((narrow.y - 225.0).abs() < 0.01);

        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(500.0, 500.0));
        let fitted = aspect_fit_rect(bounds, 16.0 / 9.0);
        assert!((fitted.width() / fitted.height() - 16.0 / 9.0).abs() < 0.001);
        assert_eq!(fitted.center(), bounds.center());
    }

    #[test]
    fn pane_viewport_tracker_ignores_degenerate_viewports() {
        let healthy = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1100.0, 720.0));
        assert_eq!(track_pane_viewport(healthy, healthy), healthy);

        // A larger healthy viewport updates the tracker.
        let grown = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1600.0, 900.0));
        assert_eq!(track_pane_viewport(healthy, grown), grown);

        // Transient minimize/restore sizes below Wire's minimum never shrink
        // or move the tracked rect.
        assert_eq!(
            track_pane_viewport(
                grown,
                Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1.0, 1.0))
            ),
            grown
        );
        assert_eq!(
            track_pane_viewport(
                grown,
                Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(300.0, 480.0))
            ),
            grown
        );

        // The enforced minimum itself is still considered healthy.
        let minimum = Rect::from_min_size(egui::Pos2::ZERO, MIN_WINDOW_SIZE);
        assert_eq!(track_pane_viewport(grown, minimum), minimum);
    }

    #[test]
    fn constrained_panes_lay_out_fully_through_degenerate_frames() {
        let ctx = egui::Context::default();
        let healthy = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1100.0, 720.0));
        let degenerate = Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1.0, 1.0));
        let pane_size = Vec2::new(720.0, 560.0);
        let pass = |pinned: bool, screen: Rect| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            ctx.begin_pass(input);
            let mut shown = None;
            let mut window = egui::Window::new("pane")
                .id(egui::Id::new("test-pane"))
                .fixed_pos(egui::Pos2::ZERO)
                .resizable(false)
                .default_size(pane_size);
            if pinned {
                window = window.constrain_to(healthy);
            }
            window.show(&ctx, |ui| {
                ui.set_min_size(pane_size);
                shown = Some(ui.clip_rect());
            });
            let _ = ctx.end_pass();
            shown.unwrap_or(healthy)
        };

        // A pane pinned to the tracked viewport keeps laying out at full size
        // with a sane clip rect while the live viewport reports transient
        // minimize/restore sizes...
        let established = pass(true, healthy);
        assert!(established.size().x >= pane_size.x && established.size().y >= pane_size.y);
        let clipped = pass(true, degenerate);
        assert!(
            clipped.width() >= pane_size.x && clipped.height() >= pane_size.y,
            "pinning must keep the pane's layout intact, got {clipped:?}"
        );

        // ...while an unpinned pane is clipped into the degenerate viewport,
        // collapsing content-driven layout for those frames.
        let unpinned_established = pass(false, healthy);
        assert!(
            unpinned_established.size().x >= pane_size.x
                && unpinned_established.size().y >= pane_size.y
        );
        let unpinned_clipped = pass(false, degenerate);
        assert!(
            unpinned_clipped.width() < pane_size.x || unpinned_clipped.height() < pane_size.y,
            "unpinned panes get clipped into the degenerate viewport"
        );
    }

    #[test]
    fn long_stream_names_are_clipped_cleanly() {
        assert_eq!(ellipsize("Ada", 8), "Ada");
        assert_eq!(ellipsize("Long display name", 8), "Long di…");
    }
}
