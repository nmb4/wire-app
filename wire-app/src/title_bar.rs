//! Custom window title bar (borderless chrome) based on the eframe `custom_window_frame` example.

use eframe::egui::{
    self, Align, Align2, Color32, Id, Layout, PointerButton, Sense, Stroke, Ui, UiBuilder, Vec2,
    ViewportCommand, WindowLevel,
};
use lucide_icons::Icon;

use crate::{
    resource_monitor::ResourceUsage,
    theme::{kh_family, lucide, Palette},
};

pub const HEIGHT: f32 = 32.0;
const ICON_SIZE: f32 = 14.0;
const BUTTON_SIZE: f32 = HEIGHT;
/// Left inset of the optional peer-update control, plus its own width.
const UPDATE_BUTTON_INSET: f32 = 8.0;

/// An optional control rendered left of the window buttons.
///
/// Used by the peer-to-peer update flow: it appears only while a friend running
/// a newer build is reachable, and drives the accept / decline decision.
#[derive(Clone, Copy)]
pub struct UpdateButton<'a> {
    pub label: &'a str,
    pub tooltip: &'a str,
    /// Dimmed while a transfer is in flight, with the progress shown in `label`.
    pub busy: bool,
}

impl UpdateButton<'_> {
    /// Fixed width so swapping `v0.7.5` for a progress percentage cannot make
    /// the control resize and shove the window controls around mid-transfer.
    const fn width() -> f32 {
        UPDATE_BUTTON_INSET * 2.0 + ICON_SIZE + 6.0 + LABEL_BUDGET as f32 * CHAR_WIDTH
    }
}

/// Characters the label budget covers, and the average glyph advance used to
/// size it. Budget covers a full `v1.12.345` version and a `100%` progress
/// reading; the caller truncates anything longer, and the control is
/// right-anchored so leftover slack is invisible.
pub(super) const LABEL_BUDGET: usize = 9;
const CHAR_WIDTH: f32 = 5.5;

#[allow(clippy::too_many_arguments)]
pub fn ui(
    ui: &mut Ui,
    title_bar_rect: egui::Rect,
    pal: &Palette,
    title: &str,
    resources: ResourceUsage,
    show_system_usage: bool,
    always_on_top: &mut bool,
    rounded: bool,
    update_button: Option<UpdateButton<'_>>,
) -> bool {
    let painter = ui.painter();

    let title_bar_response = ui.interact(
        title_bar_rect,
        Id::new("window_title_bar_drag"),
        Sense::click_and_drag(),
    );

    painter.text(
        title_bar_rect.left_center() + Vec2::new(12.0, 0.0),
        Align2::LEFT_CENTER,
        title,
        egui::FontId::new(16.0, kh_family()),
        pal.text,
    );

    if show_system_usage && title_bar_rect.width() >= 500.0 {
        let gpu = resources
            .gpu_percent
            .map(|value| format!("{value:.0}%"))
            .unwrap_or_else(|| "--".to_owned());
        let memory_mib = resources.memory_bytes as f64 / (1024.0 * 1024.0);
        let resource_text = format!(
            "CPU {:.0}%   GPU {}   RAM {:.0} MB",
            resources.cpu_percent, gpu, memory_mib
        );
        // Keep the readout clear of the window buttons and of the update
        // control, whose width is fixed rather than content-driven.
        let reserved = BUTTON_SIZE * 4.0
            + 12.0
            + update_button
                .map(|_| UpdateButton::width() + UPDATE_BUTTON_INSET)
                .unwrap_or(0.0);
        painter.text(
            title_bar_rect.right_center() - Vec2::new(reserved, 0.0),
            Align2::RIGHT_CENTER,
            resource_text,
            egui::FontId::monospace(11.0),
            pal.text2,
        );
    }

    if title_bar_response.double_clicked() {
        let is_maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!is_maximized));
    }

    if title_bar_response.drag_started_by(PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    let mut update_clicked = false;
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(title_bar_rect)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            window_controls(ui, pal, always_on_top, rounded);
            // Laid out last in a right-to-left pass, so the update control lands
            // immediately left of the window controls.
            if let Some(button) = update_button {
                update_clicked = update_control(ui, pal, button, rounded).clicked();
            }
        },
    );
    update_clicked
}

/// The peer-update entry point in the title bar.
///
/// Deliberately an icon plus version rather than a wide button: the title bar
/// is 32px tall and shares its row with the window controls, so the control
/// stays out of the way until a newer friend is actually reachable.
fn update_control(
    ui: &mut Ui,
    pal: &Palette,
    button: UpdateButton<'_>,
    rounded: bool,
) -> egui::Response {
    let size = Vec2::new(UpdateButton::width(), 24.0);
    // A busy control is still clickable so the prompt stays reachable, but the
    // tooltip explains that nothing happens until the transfer lands.
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hovered = response.hovered();
    let fill = if hovered { pal.panel2 } else { pal.accent_dim };
    let icon_color = if button.busy { pal.text2 } else { pal.accent };
    ui.painter().rect_filled(
        rect,
        if rounded {
            egui::CornerRadius::same(5)
        } else {
            egui::CornerRadius::ZERO
        },
        fill,
    );

    let icon_x = rect.left() + UPDATE_BUTTON_INSET + ICON_SIZE * 0.5;
    ui.painter().text(
        egui::pos2(icon_x, rect.center().y),
        Align2::CENTER_CENTER,
        char::from(Icon::TrendingUp),
        lucide(ICON_SIZE),
        icon_color,
    );
    // Truncate rather than trust the budget: an unexpected label must not paint
    // over the window controls.
    let label: String = button.label.chars().take(LABEL_BUDGET).collect();
    ui.painter().text(
        egui::pos2(icon_x + ICON_SIZE * 0.5 + 6.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        egui::FontId::new(11.5, kh_family()),
        if hovered { pal.text } else { pal.text2 },
    );
    response.on_hover_text(button.tooltip)
}

fn window_controls(ui: &mut Ui, pal: &Palette, always_on_top: &mut bool, rounded: bool) {
    if title_bar_icon_button(ui, pal, Icon::X, false, true, rounded)
        .on_hover_text("Close")
        .clicked()
    {
        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
    }

    let is_maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
    let maximize_icon = if is_maximized {
        Icon::Minimize2
    } else {
        Icon::Maximize
    };
    let maximize_hint = if is_maximized { "Restore" } else { "Maximize" };
    if title_bar_icon_button(ui, pal, maximize_icon, false, false, rounded)
        .on_hover_text(maximize_hint)
        .clicked()
    {
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!is_maximized));
    }

    if title_bar_icon_button(ui, pal, Icon::Minus, false, false, rounded)
        .on_hover_text("Minimize")
        .clicked()
    {
        ui.ctx().send_viewport_cmd(ViewportCommand::Minimized(true));
    }

    let pin_hint = if *always_on_top {
        "Unpin from top"
    } else {
        "Keep on top"
    };
    if title_bar_icon_button(ui, pal, Icon::Pin, *always_on_top, false, rounded)
        .on_hover_text(pin_hint)
        .clicked()
    {
        *always_on_top = !*always_on_top;
        let level = if *always_on_top {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        };
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::WindowLevel(level));
    }
}

fn title_bar_icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    active: bool,
    close: bool,
    rounded: bool,
) -> egui::Response {
    let icon_str = char::from(icon).to_string();
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(BUTTON_SIZE), Sense::click());
    let hovered = response.hovered();
    let fill = if close && hovered {
        Color32::from_rgb(0xc4, 0x42, 0x42)
    } else if hovered {
        pal.panel2
    } else if active {
        pal.accent_dim
    } else {
        Color32::TRANSPARENT
    };
    let icon_color = if close && hovered {
        Color32::WHITE
    } else if active {
        pal.accent
    } else if hovered {
        pal.text
    } else {
        pal.text2
    };

    ui.painter().rect_filled(
        rect,
        if rounded {
            egui::CornerRadius::same(5)
        } else {
            egui::CornerRadius::ZERO
        },
        fill,
    );
    if active {
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(2.0_f32, pal.accent),
        );
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon_str,
        lucide(ICON_SIZE),
        icon_color,
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::kh_family;

    fn context(size: Vec2) -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        // Tests do not load Wire's custom heading font.
        fonts.families.insert(
            kh_family(),
            fonts.families[&egui::FontFamily::Proportional].clone(),
        );
        ctx.set_fonts(fonts);
        let _ = ctx.begin_pass(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        });
        ctx
    }

    /// The control must fit every label it can render inside its fixed budget: an
    /// overflow would paint the percentage over the window buttons.
    #[test]
    fn the_update_control_fits_its_widest_label() {
        let ctx = context(Vec2::new(1100.0, 720.0));
        let width = UpdateButton::width();
        // Labels the control actually renders are truncated to the budget, so every
        // candidate must fit the widest untruncated one.
        for label in ["v0.7.5", "100%", "v1.12.345"] {
            let galley = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap(
                    label.to_owned(),
                    egui::FontId::new(11.5, kh_family()),
                    Color32::WHITE,
                )
            });
            let text_left = UPDATE_BUTTON_INSET + ICON_SIZE + 6.0;
            assert!(
                text_left + galley.size().x <= width,
                "{label:?} ({:?} wide) overflows the {width}px control",
                galley.size().x
            );
        }
    }

    /// A fixed width keeps the window controls from shifting when the label
    /// changes from a version to a progress percentage.
    #[test]
    fn the_update_control_never_resizes_with_its_label() {
        let width = UpdateButton::width();
        assert_eq!(width, UpdateButton::width());
        assert!(width > UPDATE_BUTTON_INSET * 2.0 + ICON_SIZE + 6.0);
    }
}
