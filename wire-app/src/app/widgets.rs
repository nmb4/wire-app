//! Shared UI painting and layout helpers.

use crate::theme::{
    ghost_icon_button, kh_family, lucide, sans, ui_font_size, IconButtonLabel, Palette,
};
use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Rect, RichText, Stroke,
    TextureHandle, Ui, Vec2,
};
use egui_phosphor::regular as ph;
use lucide_icons::Icon;
use std::sync::atomic::Ordering;
use tracing::warn;
use wire::audio::VolumeHandle;

pub(crate) fn format_bytes(bytes: u64) -> String {
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

/// A clock-style duration for recording indicators and summaries.
pub(super) fn format_duration_ms(millis: u64) -> String {
    let total_seconds = millis / 1000;
    let (hours, minutes, seconds) = (
        total_seconds / 3_600,
        (total_seconds % 3_600) / 60,
        total_seconds % 60,
    );
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
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

pub(super) const CHROME_CONTROL_HEIGHT: f32 = 32.0;

/// Side inset for bottom chrome (participants + call dock) so content clears the frame.
pub(super) const CHROME_SIDE_INSET: i8 = 14;

/// Horizontal padding the participant strip's frame adds inside its chrome inset.
pub(super) const PARTICIPANT_STRIP_PAD_X: i8 = 10;

/// Vertical padding above the participant strip's first row.
pub(super) const PARTICIPANT_STRIP_PAD_TOP: i8 = 2;

/// Vertical padding below the participant strip's last row.
pub(super) const PARTICIPANT_STRIP_PAD_BOTTOM: i8 = 6;

/// Total vertical space the strip's frame consumes: its own bottom margin plus
/// the top and bottom inner padding above.
///
/// One number, so the band `ui_chrome_body` reserves and the frame the strip
/// actually draws inside can never drift apart.
pub(super) const PARTICIPANT_STRIP_PAD_Y: f32 =
    (PARTICIPANT_STRIP_PAD_TOP + PARTICIPANT_STRIP_PAD_BOTTOM + 2) as f32;

/// Content width available to the participant chips.
///
/// Both the strip's reserved height and the column count are derived from this,
/// so the layout can never mix up "body width" with "chip row width" and
/// reserve a band for a different number of columns than it actually lays out.
pub(super) fn participant_strip_width(body_width: f32) -> f32 {
    (body_width - 2.0 * (f32::from(CHROME_SIDE_INSET) + PARTICIPANT_STRIP_PAD_X as f32)).max(1.0)
}

/// Number of rows the given chips need in a strip of this content width.
pub(super) fn participant_bar_rows(strip_width: f32, participant_count: usize) -> usize {
    if participant_count == 0 {
        return 0;
    }
    participant_count.div_ceil(participant_bar_columns(strip_width))
}

pub(super) fn participant_bar_height(
    strip_width: f32,
    participant_count: usize,
    max_height: f32,
) -> f32 {
    if participant_count == 0 || max_height <= 0.0 {
        return 0.0;
    }
    let rows = participant_bar_rows(strip_width, participant_count) as f32;
    // Each chip's one-point frame stroke sits outside its content rectangle.
    let content_height =
        rows * (PARTICIPANT_CHIP_HEIGHT + 2.0) + (rows - 1.0).max(0.0) * PARTICIPANT_GAP;
    (content_height + PARTICIPANT_STRIP_PAD_Y).min(max_height)
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

/// A single geometry policy for floating panels. Widths are content widths:
/// leave room for the frame, shadow, and the native window's rounded edge.
pub(super) fn floating_panel_width(viewport: Rect, preferred: f32, padding: f32) -> f32 {
    (viewport.width() - 32.0 - padding * 2.0 - 2.0)
        .max(1.0)
        .min(preferred)
}

pub(super) fn floating_panel_frame(pal: &Palette, padding: i8) -> Frame {
    Frame::new()
        .fill(pal.bg)
        .stroke(Stroke::new(1.0_f32, pal.line_br))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(padding)
        .shadow(egui::epaint::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(60),
        })
}

/// Content-driven height, bounded to the viewport; long forms and lists scroll.
/// Pin the current width every frame so remembered geometry cannot fight resize.
pub(super) fn floating_panel<'a>(
    title: &'a str,
    pal: &Palette,
    viewport: Rect,
    preferred_width: f32,
) -> egui::Window<'a> {
    let width = floating_panel_width(viewport, preferred_width, 16.0);
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
        .constrain_to(viewport.shrink(16.0))
        .default_width(width)
        .min_width(width)
        .max_width(width)
        .min_height(0.0)
        .max_height((viewport.height() - 128.0).max(1.0))
        .vscroll(true)
        .frame(floating_panel_frame(pal, 16))
}

/// Shell for a modal-style floating dialog without egui's native title bar.
/// Callers draw `floating_dialog_header` first, then `dialog_body`. Returns
/// the window and the content width to pin with `ui.set_width`.
pub(super) fn dialog_window<'a>(
    id: &'a str,
    pal: &Palette,
    viewport: Rect,
    preferred_width: f32,
) -> (egui::Window<'a>, f32) {
    let width = floating_panel_width(viewport, preferred_width, 0.0);
    let window = floating_panel(id, pal, viewport, preferred_width)
        .title_bar(false)
        .vscroll(false)
        .default_width(width)
        .min_width(width)
        .max_width(width)
        .frame(floating_panel_frame(pal, 0));
    (window, width)
}

/// Padded body below a `floating_dialog_header`.
pub(super) fn dialog_body<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .inner_margin(egui::Margin {
            left: 18,
            right: 18,
            top: 14,
            bottom: 16,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
        .inner
}

/// Right-aligned action row closing a dialog. Add the primary action first:
/// the row lays out right to left, so it ends up rightmost.
pub(super) fn dialog_footer<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.add_space(14.0);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 32.0),
        Layout::right_to_left(Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            add_contents(ui)
        },
    )
    .inner
}

/// Small label above a form field, with an optional muted detail line.
pub(super) fn form_label(ui: &mut Ui, pal: &Palette, label: &str, detail: Option<&str>) {
    ui.label(
        RichText::new(label)
            .color(pal.text2)
            .size(ui_font_size(12.0)),
    );
    if let Some(detail) = detail {
        ui.add(
            egui::Label::new(
                RichText::new(detail)
                    .color(pal.dim)
                    .size(ui_font_size(10.5)),
            )
            .wrap(),
        );
    }
    ui.add_space(4.0);
}

/// Single-line text field with comfortable padding and a visible resting
/// border. Focus switches the border to the accent (see `visuals_for`).
pub(super) fn form_text_input(
    ui: &mut Ui,
    pal: &Palette,
    text: &mut String,
    hint: &str,
) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(RichText::new(hint).color(pal.dim2))
            .margin(egui::Margin::symmetric(10, 7))
            .background_color(pal.panel)
            .desired_width(ui.available_width()),
    )
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

pub(super) fn chat_hover_surface(pal: &Palette) -> Color32 {
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
    let mut closed = false;
    Frame::new()
        .fill(pal.panel)
        .corner_radius(CornerRadius {
            nw: 14,
            ne: 14,
            sw: 0,
            se: 0,
        })
        .inner_margin(egui::Margin::symmetric(18, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // Reserve the close control before laying out text. A long
                // subtitle wraps below the title instead of pushing it offscreen.
                if let Some(tooltip) = close_tooltip {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        closed = ghost_icon_button(ui, pal, ph::X).labeled(tooltip).clicked();
                        ui.allocate_ui_with_layout(
                            Vec2::new(ui.available_width(), 0.0),
                            Layout::top_down(Align::Min),
                            |ui| floating_panel_heading(ui, pal, title, subtitle),
                        );
                    });
                } else {
                    ui.vertical(|ui| floating_panel_heading(ui, pal, title, subtitle));
                }
            });
        });
    closed
}

fn floating_panel_heading(ui: &mut Ui, pal: &Palette, title: &str, subtitle: &str) {
    ui.label(
        RichText::new(title)
            .family(kh_family())
            .color(pal.text)
            .size(16.0),
    );
    if !subtitle.is_empty() {
        ui.add(
            egui::Label::new(
                RichText::new(subtitle)
                    .color(pal.dim)
                    .size(ui_font_size(11.5)),
            )
            .wrap(),
        );
    }
}

pub(super) fn chat_lucide_icon_button(ui: &mut Ui, pal: &Palette, icon: Icon) -> egui::Response {
    chrome_icon_button(ui, pal, icon, 30.0, 15.0)
}

/// Square ghost button with a centered Lucide glyph. All icon buttons in the
/// chrome go through here so hover surfaces and glyph centering match.
pub(super) fn chrome_icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    size: f32,
    glyph: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());
    let hot = response.hovered() || response.has_focus();
    if hot {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), chat_hover_surface(pal));
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(icon),
        lucide(glyph),
        if hot { pal.text } else { pal.text2 },
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
    let font = FontId::proportional(ui_font_size(12.0));
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, pal.text);
    // Content-sized with fixed padding; room on the right for the dot.
    let width = galley.size().x + 28.0;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, CHROME_CONTROL_HEIGHT - 6.0),
        egui::Sense::click(),
    );
    let hot = response.hovered() || response.has_focus();
    let fill = if selected {
        chat_selected_surface(pal)
    } else if hot {
        chat_hover_surface(pal)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(CHROME_INNER_RADIUS - 2), fill);
    let color = if selected || hot { pal.text } else { pal.dim };
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, color);
    if unseen {
        ui.painter()
            .circle_filled(rect.right_center() - Vec2::new(7.0, 0.0), 3.0, pal.accent);
    }
    response
}

/// Leading avatar for a sidebar navigation row: profile picture when known,
/// initial letter otherwise.
pub(super) struct SidebarAvatar {
    pub texture: Option<TextureHandle>,
    pub initial: String,
}

pub(super) fn chat_navigation_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    subtitle: Option<&str>,
    selected: bool,
    unseen: bool,
    avatar: Option<SidebarAvatar>,
) -> egui::Response {
    // Every row has the same height and avatar size, with or without a
    // subtitle, so the list reads as one rhythm.
    const ROW_HEIGHT: f32 = 44.0;
    let height = ROW_HEIGHT;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(1.0), height),
        egui::Sense::click(),
    );
    let hot = response.hovered() || response.has_focus();
    let fill = if selected {
        mix_color(pal.panel2, Color32::WHITE, 0.07)
    } else if hot {
        chat_hover_surface(pal)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
    let text_right = if unseen {
        rect.right() - 26.0
    } else {
        rect.right() - 10.0
    };
    let name_color = if selected || hot { pal.text } else { pal.text2 };
    paint_row_identity(ui, pal, rect, label, subtitle, avatar, text_right, name_color);
    if unseen {
        ui.painter()
            .circle_filled(rect.right_center() - Vec2::new(14.0, 0.0), 4.0, pal.accent);
    }
    response
}

/// Avatar diameter shared by every 44 px list row.
const ROW_AVATAR: f32 = 30.0;

/// Avatar plus a name/subtitle block, centered as a unit on a list row.
/// Shared by sidebar navigation rows and selectable member rows.
#[allow(clippy::too_many_arguments)]
fn paint_row_identity(
    ui: &Ui,
    pal: &Palette,
    rect: Rect,
    label: &str,
    subtitle: Option<&str>,
    avatar: Option<SidebarAvatar>,
    text_right: f32,
    name_color: Color32,
) {
    let label_font = FontId::proportional(ui_font_size(12.5));
    let subtitle_font = FontId::proportional(ui_font_size(10.5));
    let avatar_size = ROW_AVATAR;
    let text_left = if let Some(avatar) = &avatar {
        let center = egui::pos2(rect.left() + 8.0 + avatar_size * 0.5, rect.center().y);
        let avatar_rect = Rect::from_center_size(center, Vec2::splat(avatar_size));
        if let Some(texture) = &avatar.texture {
            super::profile_ui::paint_circular_image(ui, avatar_rect, texture);
        } else {
            ui.painter()
                .circle_filled(center, avatar_size * 0.5, pal.panel2);
        }
        ui.painter()
            .circle_stroke(center, avatar_size * 0.5, Stroke::new(1.0_f32, pal.line_br));
        if avatar.texture.is_none() {
            ui.painter().text(
                center,
                Align2::CENTER_CENTER,
                &avatar.initial,
                FontId::new(avatar_size * 0.4, kh_family()),
                pal.text2,
            );
        }
        rect.left() + 8.0 + avatar_size + 10.0
    } else {
        rect.left() + 12.0
    };
    // Paint text directly from measured galleys: the name/subtitle block is
    // centered on the row as a unit, so one-line and two-line rows share
    // the same optical center.
    let max_text = (text_right - text_left).max(1.0);
    let name = truncated_galley(ui, label, label_font, name_color, max_text);
    let sub = subtitle.map(|text| truncated_galley(ui, text, subtitle_font, pal.dim, max_text));
    let block_h = name.size().y + sub.as_ref().map_or(0.0, |g| g.size().y - 2.0);
    let mut y = rect.center().y - block_h * 0.5;
    ui.painter()
        .galley(egui::pos2(text_left, y), name.clone(), name_color);
    y += name.size().y - 2.0;
    if let Some(sub) = sub {
        ui.painter().galley(egui::pos2(text_left, y), sub, pal.dim);
    }
}

/// Full-width settings row: label on the left, an on/off switch on the right.
/// The whole row toggles; `changed()` reports a flip like `ui.checkbox`.
pub(super) fn settings_toggle(
    ui: &mut Ui,
    pal: &Palette,
    value: &mut bool,
    label: &str,
) -> egui::Response {
    const TRACK: Vec2 = Vec2::new(32.0, 18.0);
    let galley = ui.painter().layout(
        label.to_owned(),
        FontId::proportional(ui_font_size(12.0)),
        pal.text2,
        (ui.available_width() - TRACK.x - 12.0).max(40.0),
    );
    let height = galley.size().y.max(TRACK.y) + 10.0;
    let (rect, mut response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::click(),
    );
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *value, label)
    });
    let hot = response.hovered() || response.has_focus();
    let painter = ui.painter();
    painter.galley(
        egui::pos2(rect.left(), rect.center().y - galley.size().y * 0.5),
        galley,
        if hot { pal.text } else { pal.text2 },
    );
    let on = ui.ctx().animate_bool_responsive(response.id, *value);
    let track = Rect::from_center_size(
        egui::pos2(rect.right() - TRACK.x * 0.5, rect.center().y),
        TRACK,
    );
    let track_fill = crate::theme::mix_rgb(pal.panel, pal.accent, on);
    painter.rect(
        track,
        CornerRadius::same(9),
        track_fill,
        Stroke::new(1.0, if *value { pal.accent } else { pal.line_br }),
        egui::StrokeKind::Inside,
    );
    let knob_x = egui::lerp(track.left() + 9.0..=track.right() - 9.0, on);
    painter.circle_filled(
        egui::pos2(knob_x, track.center().y),
        6.0,
        if *value { pal.bg } else { pal.dim },
    );
    if response.has_focus() {
        painter.rect_stroke(
            track.expand(2.0),
            CornerRadius::same(11),
            Stroke::new(1.0, pal.accent),
            egui::StrokeKind::Outside,
        );
    }
    response
}

/// A 44 px member row with a trailing check box, for multi-select lists such
/// as the group editor. The whole row toggles.
pub(super) fn member_toggle_row(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    subtitle: Option<&str>,
    checked: bool,
    avatar: Option<SidebarAvatar>,
) -> egui::Response {
    const ROW_HEIGHT: f32 = 44.0;
    const CHECK: f32 = 18.0;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(1.0), ROW_HEIGHT),
        egui::Sense::click(),
    );
    let hot = response.hovered() || response.has_focus();
    let fill = if checked {
        chat_selected_surface(pal)
    } else if hot {
        chat_hover_surface(pal)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(10), fill);
    let name_color = if checked || hot { pal.text } else { pal.text2 };
    let text_right = rect.right() - 12.0 - CHECK - 10.0;
    paint_row_identity(ui, pal, rect, label, subtitle, avatar, text_right, name_color);
    let check = Rect::from_center_size(
        egui::pos2(rect.right() - 12.0 - CHECK * 0.5, rect.center().y),
        Vec2::splat(CHECK),
    );
    if checked {
        ui.painter()
            .rect_filled(check, CornerRadius::same(5), pal.accent);
        ui.painter().text(
            check.center(),
            Align2::CENTER_CENTER,
            char::from(Icon::Check),
            lucide(12.0),
            pal.bg,
        );
    } else {
        ui.painter().rect_stroke(
            check,
            CornerRadius::same(5),
            Stroke::new(1.25_f32, if hot { pal.text2 } else { pal.line_br }),
            egui::StrokeKind::Inside,
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, checked, label)
    });
    response
}

/// Drop-down select in the settings style: a 32 px field showing the current
/// value with a chevron, opening the shared popup menu at the field's width.
///
/// `options` yields `(selected, label)` pairs and is only walked while the
/// menu is open. Returns the index of the option the user picked.
pub(super) fn select_field<S: AsRef<str>>(
    ui: &mut Ui,
    pal: &Palette,
    name: &str,
    selected_text: &str,
    options: impl IntoIterator<Item = (bool, S)>,
) -> Option<usize> {
    const RADIUS: u8 = 7;
    // A persistent id keeps the menu open when rows above it appear or
    // disappear (update status, device lists).
    let (_, rect) = ui.allocate_space(Vec2::new(
        ui.available_width().max(1.0),
        CHROME_CONTROL_HEIGHT,
    ));
    let response = ui.interact(
        rect,
        ui.make_persistent_id(("select-field", name)),
        egui::Sense::click(),
    );
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&response));
    let hot = response.hovered() || response.has_focus();
    let stroke = if open || response.has_focus() {
        pal.accent
    } else if hot {
        mix_color(pal.line_br, pal.text2, 0.35)
    } else {
        pal.line_br
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(RADIUS),
        if hot {
            chat_hover_surface(pal)
        } else {
            pal.panel
        },
        Stroke::new(1.0_f32, stroke),
        egui::StrokeKind::Inside,
    );
    let chevron = if open {
        Icon::ChevronUp
    } else {
        Icon::ChevronDown
    };
    painter.text(
        egui::pos2(rect.right() - 16.0, rect.center().y),
        Align2::CENTER_CENTER,
        char::from(chevron),
        lucide(14.0),
        if hot || open { pal.text } else { pal.dim },
    );
    let text_color = if hot || open { pal.text } else { pal.text2 };
    let galley = truncated_galley(
        ui,
        selected_text,
        FontId::proportional(ui_font_size(12.0)),
        text_color,
        (rect.width() - 10.0 - 32.0).max(1.0),
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y * 0.5),
        galley,
        text_color,
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, ui.is_enabled(), name)
    });
    let hover_text = (selected_text.chars().count() > 40).then(|| selected_text.to_owned());

    let mut picked = None;
    let popup_width = rect.width();
    egui::Popup::menu(&response)
        .width(popup_width)
        .align(egui::RectAlign::BOTTOM_START)
        .gap(4.0)
        .show(|ui| {
            let inner = (popup_width - ui.spacing().menu_margin.sum().x).max(1.0);
            ui.set_width(inner);
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);
            egui::ScrollArea::vertical()
                .id_salt("select-field-options")
                .max_height(280.0)
                .show(ui, |ui| {
                    for (index, (selected, label)) in options.into_iter().enumerate() {
                        if select_option_row(ui, pal, label.as_ref(), selected).clicked() {
                            picked = Some(index);
                            ui.close();
                        }
                    }
                });
        });
    if let Some(text) = hover_text {
        response.on_hover_text(text);
    }
    picked
}

/// One full-width row in a `select_field` menu, with a check on the current
/// value.
fn select_option_row(ui: &mut Ui, pal: &Palette, label: &str, selected: bool) -> egui::Response {
    const HEIGHT: f32 = 28.0;
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(1.0), HEIGHT),
        egui::Sense::click(),
    );
    let hot = response.hovered() || response.has_focus();
    if hot {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), chat_hover_surface(pal));
    }
    let color = if hot || selected { pal.text } else { pal.text2 };
    let galley = truncated_galley(
        ui,
        label,
        FontId::proportional(ui_font_size(12.0)),
        color,
        (rect.width() - 8.0 - 28.0).max(1.0),
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y * 0.5),
        galley,
        color,
    );
    if selected {
        ui.painter().text(
            egui::pos2(rect.right() - 16.0, rect.center().y),
            Align2::CENTER_CENTER,
            char::from(Icon::Check),
            lucide(13.0),
            pal.accent,
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, label)
    });
    response
}

/// Single-line galley shortened with an ellipsis to fit `max_width`.
pub(super) fn truncated_galley(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

pub(super) fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let mut shortened: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

/// Chips that fit side by side in a participant strip of this content width.
pub(super) fn participant_bar_columns(strip_width: f32) -> usize {
    ((strip_width + PARTICIPANT_GAP) / (PARTICIPANT_CARD_SLOT_WIDTH + PARTICIPANT_GAP))
        .floor()
        .max(1.0) as usize
}

/// Fixed participant-chip metrics keep avatar, label, and actions on one midline.
pub(super) const PARTICIPANT_CHIP_HEIGHT: f32 = 40.0;

pub(super) const PARTICIPANT_GAP: f32 = 8.0;

/// Smallest slot a participant chip is assumed to need when choosing how many
/// fit side by side.
///
/// Chips are content-sized, so this is only a wrap-point heuristic. It has to
/// be at least as wide as a real chip (long name plus the End control) or the
/// last column runs past the strip and gets clipped by the window edge.
pub(super) const PARTICIPANT_CARD_SLOT_WIDTH: f32 = 284.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_panels_stay_bounded_when_resized_and_restore_their_width() {
        let ctx = egui::Context::default();
        let pal = Palette::for_theme(crate::theme::Theme::default());
        for size in [
            Vec2::new(1200.0, 900.0),
            Vec2::new(460.0, 500.0),
            Vec2::new(1200.0, 900.0),
        ] {
            let viewport = Rect::from_min_size(egui::Pos2::ZERO, size);
            // Two passes allow egui to settle a content-driven window's position.
            for _ in 0..2 {
                ctx.begin_pass(egui::RawInput {
                    screen_rect: Some(viewport),
                    ..Default::default()
                });
                let shown = floating_panel("test floating panel", &pal, viewport, 560.0)
                    .show(&ctx, |ui| {
                        ui.set_width(ui.available_width());
                        for _ in 0..50 {
                            ui.add(
                                egui::Label::new("A very long member or device name ".repeat(8))
                                    .wrap(),
                            );
                        }
                    })
                    .unwrap();
                assert!(
                    shown.response.rect.width() <= size.x - 30.0,
                    "{:?}",
                    shown.response.rect
                );
                assert!(
                    shown.response.rect.height() <= size.y - 30.0,
                    "{:?}",
                    shown.response.rect
                );
                let expected = floating_panel_width(viewport, 560.0, 16.0) + 34.0;
                assert!(
                    (shown.response.rect.width() - expected).abs() < 2.0,
                    "expected {expected}, got {:?}",
                    shown.response.rect
                );
                let _ = ctx.end_pass();
            }
        }
    }

    #[test]
    fn floating_header_wraps_without_expanding_panel() {
        let ctx = egui::Context::default();
        let pal = Palette::for_theme(crate::theme::Theme::default());
        // Tests do not load Wire's custom heading font.
        let mut fonts = egui::FontDefinitions::default();
        fonts.families.insert(
            kh_family(),
            fonts.families[&egui::FontFamily::Proportional].clone(),
        );
        ctx.set_fonts(fonts);
        let viewport = Rect::from_min_size(egui::Pos2::ZERO, MIN_WINDOW_SIZE);
        for close in [None, Some("Close settings")] {
            ctx.begin_pass(egui::RawInput {
                screen_rect: Some(viewport),
                ..Default::default()
            });
            let width = floating_panel_width(viewport, 500.0, 0.0);
            let shown = floating_panel("header test", &pal, viewport, 500.0)
                .title_bar(false)
                .vscroll(false)
                .min_width(width)
                .max_width(width)
                .frame(floating_panel_frame(&pal, 0))
                .show(&ctx, |ui| {
                    ui.set_width(width);
                    floating_dialog_header(
                        ui,
                        &pal,
                        "SETTINGS",
                        "appearance, audio, video and updates ".repeat(4).as_str(),
                        close,
                    );
                })
                .unwrap();
            assert!(
                shown.response.rect.width() <= viewport.width() - 30.0,
                "{:?}",
                shown.response.rect
            );
            let _ = ctx.end_pass();
        }
    }

    #[test]
    fn participant_bar_adds_rows_as_the_window_narrows() {
        // Strip content widths, i.e. body width minus the chrome inset.
        // 412pt is the narrowest strip Wire's minimum window size allows.
        assert_eq!(participant_bar_height(1000.0, 3, 500.0), 52.0);
        assert_eq!(participant_bar_height(700.0, 3, 500.0), 102.0);
        assert_eq!(participant_bar_height(412.0, 3, 500.0), 152.0);
        assert_eq!(participant_bar_height(412.0, 3, 100.0), 100.0);
        assert_eq!(participant_bar_height(1000.0, 0, 500.0), 0.0);
    }

    #[test]
    fn strip_width_and_reserved_height_agree_on_the_column_count() {
        // The reserved band and the laid-out rows must come from one width, or
        // the strip reserves room for rows it never draws.
        for body_width in [460.0, 560.0, 604.0, 900.0, 1600.0] {
            let strip = participant_strip_width(body_width);
            let rows = participant_bar_rows(strip, 3);
            let reserved = participant_bar_height(strip, 3, 5000.0);
            let needed = rows as f32 * (PARTICIPANT_CHIP_HEIGHT + 2.0)
                + (rows as f32 - 1.0).max(0.0) * PARTICIPANT_GAP
                + PARTICIPANT_STRIP_PAD_Y;
            assert!(
                (reserved - needed).abs() < 0.001,
                "body {body_width}: reserved {reserved}, needed {needed}"
            );
        }
    }

    #[test]
    fn participant_band_covers_egui_frame_strokes_and_explicit_row_gaps() {
        for count in 1..=4 {
            let ctx = egui::Context::default();
            let mut painted = Rect::NOTHING;
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for row in 0..count {
                        let chip = egui::Frame::new()
                            .stroke(egui::Stroke::new(1.0_f32, egui::Color32::WHITE))
                            .show(ui, |ui| {
                                ui.allocate_exact_size(
                                    Vec2::new(200.0, PARTICIPANT_CHIP_HEIGHT),
                                    egui::Sense::hover(),
                                );
                            });
                        painted = painted.union(chip.response.rect);
                        if row + 1 < count {
                            ui.add_space(PARTICIPANT_GAP);
                        }
                    }
                });
            });
            let needed = painted.height() + PARTICIPANT_STRIP_PAD_Y;
            let reserved = participant_bar_height(412.0, count, 5000.0);
            assert!(
                (reserved - needed).abs() < 0.001,
                "{count} rows: reserved {reserved}, egui painted {needed}"
            );
        }
    }

    #[test]
    fn a_wide_chip_never_forces_a_row_past_the_strip_edge() {
        // A rendered chip (avatar + name + meter + volume + End) is about 272pt
        // wide. Whenever the column count says two fit, two of them plus the gap
        // must genuinely fit, otherwise the trailing End control is clipped by
        // the window edge.
        let widest_chip = 272.0_f32;
        for strip_width in (284..=1400).map(|value| value as f32) {
            let columns = participant_bar_columns(strip_width);
            let needed = columns as f32 * widest_chip + (columns - 1) as f32 * PARTICIPANT_GAP;
            assert!(
                needed <= strip_width,
                "strip {strip_width}: {columns} columns need {needed}"
            );
        }
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
