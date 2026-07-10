//! Custom window title bar (borderless chrome) based on the eframe `custom_window_frame` example.

use eframe::egui::{
    self, Align, Align2, Button, FontId, Id, Layout, PointerButton, RichText, Sense, Stroke, Ui,
    UiBuilder, Vec2, ViewportCommand, WindowLevel,
};
use lucide_icons::Icon;

use crate::theme::{lucide, Palette};

pub const HEIGHT: f32 = 32.0;
const ICON_SIZE: f32 = 14.0;
const BUTTON_SIZE: f32 = 28.0;

pub fn ui(
    ui: &mut Ui,
    title_bar_rect: egui::Rect,
    pal: &Palette,
    title: &str,
    always_on_top: &mut bool,
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
        FontId::proportional(12.0),
        pal.text2,
    );

    painter.line_segment(
        [
            title_bar_rect.left_bottom() + Vec2::new(0.0, 0.0),
            title_bar_rect.right_bottom(),
        ],
        Stroke::new(1.0, pal.line),
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
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.visuals_mut().button_frame = false;
            window_controls(ui, pal, always_on_top);
            ui.add_space(4.0);
        },
    );
}

fn window_controls(ui: &mut Ui, pal: &Palette, always_on_top: &mut bool) {
    if title_bar_icon_button(ui, pal, Icon::X, "Close", false)
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
    if title_bar_icon_button(ui, pal, maximize_icon, maximize_hint, false)
        .on_hover_text(maximize_hint)
        .clicked()
    {
        ui.ctx()
            .send_viewport_cmd(ViewportCommand::Maximized(!is_maximized));
    }

    if title_bar_icon_button(ui, pal, Icon::Minus, "Minimize", false)
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
    if title_bar_icon_button(ui, pal, Icon::Layers2, pin_hint, *always_on_top)
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
    _label: &str,
    active: bool,
) -> egui::Response {
    let icon_str = char::from(icon).to_string();
    let idle_color = if active { pal.accent } else { pal.text2 };

    let button = Button::new(
        RichText::new(&icon_str)
            .font(lucide(ICON_SIZE))
            .color(idle_color),
    )
    .min_size(Vec2::splat(BUTTON_SIZE));

    let response = ui.add(button);
    let rect = response.rect;

    if active || response.hovered() {
        let fill = if response.hovered() {
            pal.panel2
        } else {
            pal.panel
        };
        ui.painter().rect_filled(rect, 4.0, fill);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            icon_str,
            lucide(ICON_SIZE),
            if active { pal.accent } else { pal.text },
        );
    }

    response
}