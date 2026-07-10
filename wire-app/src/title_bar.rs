//! Custom window title bar (borderless chrome) based on the eframe `custom_window_frame` example.

use eframe::egui::{
    self, Align, Align2, Color32, Id, Layout, PointerButton, Sense, Stroke, Ui, UiBuilder, Vec2,
    ViewportCommand, WindowLevel,
};
use lucide_icons::Icon;

use crate::theme::{kh_family, lucide, Palette};

pub const HEIGHT: f32 = 32.0;
const ICON_SIZE: f32 = 14.0;
const BUTTON_SIZE: f32 = HEIGHT;

pub fn ui(
    ui: &mut Ui,
    title_bar_rect: egui::Rect,
    pal: &Palette,
    title: &str,
    always_on_top: &mut bool,
    rounded: bool,
) {
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

    if title_bar_response.double_clicked() {
        let is_maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!is_maximized));
    }

    if title_bar_response.drag_started_by(PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    ui.scope_builder(
        UiBuilder::new()
            .max_rect(title_bar_rect)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            window_controls(ui, pal, always_on_top, rounded);
        },
    );
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
    let maximize_hint = if is_maximized {
        "Restore"
    } else {
        "Maximize"
    };
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
    if title_bar_icon_button(ui, pal, Icon::Layers2, *always_on_top, false, rounded)
        .on_hover_text(pin_hint)
        .clicked()
    {
        *always_on_top = !*always_on_top;
        let level = if *always_on_top {
            WindowLevel::AlwaysOnTop
        } else {
            WindowLevel::Normal
        };
        ui.ctx().send_viewport_cmd(ViewportCommand::WindowLevel(level));
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
        if rounded { egui::CornerRadius::same(5) } else { egui::CornerRadius::ZERO },
        fill,
    );
    if active {
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(2.0, pal.accent),
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
