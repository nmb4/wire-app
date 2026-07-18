// Design system ported from the wire voice-call mockup:
// custom fonts, switchable themes + palette, and shared helper widgets.
//
// NOTE: some mockup widgets (sidenav, fake participants, voice room) are
// intentionally NOT ported -- they don't map onto this real wire app.

use eframe::egui;
#[cfg(any(wire_has_font_fraktion_sans, wire_has_font_fraktion_mono))]
use egui::FontTweak;
use egui::{
    text::{LayoutJob, TextFormat},
    Color32, CornerRadius, FontData, FontFamily, FontId, Margin, RichText, Stroke, TextStyle, Vec2,
};
use lucide_icons::{Icon, LUCIDE_FONT_BYTES};
// uppercase display font family used for headers / big text
pub fn kh_family() -> FontFamily {
    FontFamily::Name("KhInterference".into())
}

// Fraktion fonts render a touch small at a given point size, so nudge them up.
pub const FONT_BOOST: f32 = 2.0;

pub const fn ui_font_size(size: f32) -> f32 {
    size + FONT_BOOST
}

pub fn sans(size: f32) -> FontId {
    FontId::proportional(size + FONT_BOOST)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size + FONT_BOOST)
}

pub fn lucide_family() -> FontFamily {
    FontFamily::Name("lucide".into())
}

pub fn lucide(size: f32) -> FontId {
    FontId::new(size, lucide_family())
}

// -- palette --
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    Amber,
    Terminal,
    DiscordOled,
    Slate,
}

impl Default for Theme {
    fn default() -> Self {
        Self::Amber
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum WindowFrameStyle {
    /// Rounded corners on Windows 11+, square elsewhere.
    #[default]
    Auto,
    Rounded,
    Square,
}

impl WindowFrameStyle {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Rounded, Self::Square];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => {
                #[cfg(windows)]
                {
                    "Auto (Windows 11+)"
                }
                #[cfg(not(windows))]
                {
                    "Auto"
                }
            }
            Self::Rounded => "Rounded",
            Self::Square => "Square",
        }
    }
}

impl Theme {
    pub const ALL: [Self; 4] = [Self::Amber, Self::Terminal, Self::DiscordOled, Self::Slate];

    pub fn next(self) -> Self {
        match self {
            Theme::Amber => Theme::Terminal,
            Theme::Terminal => Theme::DiscordOled,
            Theme::DiscordOled => Theme::Slate,
            Theme::Slate => Theme::Amber,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Theme::Amber => "amber",
            Theme::Terminal => "terminal",
            Theme::DiscordOled => "oled",
            Theme::Slate => "slate",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme::Amber => "Amber",
            Theme::Terminal => "Terminal",
            Theme::DiscordOled => "OLED",
            Theme::Slate => "Slate",
        }
    }
}

pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub panel2: Color32,
    pub line: Color32,
    pub line_br: Color32,
    /// Subtle outer window border, drawn around the custom frame.
    pub frame_border: Color32,
    pub text: Color32,
    pub text2: Color32,
    pub dim: Color32,
    pub dim2: Color32,
    pub accent: Color32,
    pub accent_dim: Color32,
    pub ok: Color32,
    pub err: Color32,
}

#[derive(Clone, Copy)]
pub enum ButtonTone {
    Primary,
    Secondary,
    Danger,
}

impl Palette {
    pub fn for_theme(theme: Theme) -> Self {
        match theme {
            // v3 look: warm amber accent, Inter-ish elegant dark
            Theme::Amber => Self {
                bg: Color32::from_rgb(0x10, 0x10, 0x12),
                panel: Color32::from_rgb(0x15, 0x15, 0x17),
                panel2: Color32::from_rgb(0x1a, 0x1a, 0x1d),
                line: Color32::from_rgb(0x23, 0x23, 0x26),
                line_br: Color32::from_rgb(0x2e, 0x2e, 0x32),
                frame_border: Color32::from_rgba_unmultiplied(0x3a, 0x3a, 0x3e, 90),
                text: Color32::from_rgb(0xe8, 0xe6, 0xe3),
                text2: Color32::from_rgb(0xbc, 0xba, 0xb6),
                dim: Color32::from_rgb(0x91, 0x8e, 0x8a),
                dim2: Color32::from_rgb(0x68, 0x65, 0x65),
                accent: Color32::from_rgb(0xd9, 0x9a, 0x5b),
                accent_dim: Color32::from_rgba_unmultiplied(0xd9, 0x9a, 0x5b, 36),
                ok: Color32::from_rgb(0x7a, 0x9b, 0x7e),
                err: Color32::from_rgb(0xc9, 0x6f, 0x5c),
            },
            // terminal / html-mockup look: green accent, near-black bg
            Theme::Terminal => Self {
                bg: Color32::from_rgb(0x0a, 0x0b, 0x0a),
                panel: Color32::from_rgb(0x0f, 0x11, 0x10),
                panel2: Color32::from_rgb(0x14, 0x17, 0x15),
                line: Color32::from_rgb(0x1c, 0x1f, 0x1d),
                line_br: Color32::from_rgb(0x2a, 0x2e, 0x2b),
                frame_border: Color32::from_rgba_unmultiplied(0x32, 0x3a, 0x34, 80),
                text: Color32::from_rgb(0xd4, 0xd8, 0xd4),
                text2: Color32::from_rgb(0xb3, 0xbd, 0xb3),
                dim: Color32::from_rgb(0x7b, 0x86, 0x7e),
                dim2: Color32::from_rgb(0x56, 0x60, 0x58),
                accent: Color32::from_rgb(0x7e, 0xe7, 0x87),
                accent_dim: Color32::from_rgba_unmultiplied(0x7e, 0xe7, 0x87, 36),
                ok: Color32::from_rgb(0x7e, 0xe7, 0x87),
                err: Color32::from_rgb(0xff, 0x6b, 0x6b),
            },
            // discord oled look: true black bg, discord blurple accent
            Theme::DiscordOled => Self {
                bg: Color32::from_rgb(0x00, 0x00, 0x00),
                panel: Color32::from_rgb(0x0a, 0x0a, 0x0a),
                panel2: Color32::from_rgb(0x13, 0x13, 0x14),
                line: Color32::from_rgb(0x1e, 0x1e, 0x1f),
                line_br: Color32::from_rgb(0x2b, 0x2b, 0x2d),
                frame_border: Color32::from_rgba_unmultiplied(0x38, 0x38, 0x3c, 70),
                text: Color32::from_rgb(0xf2, 0xf3, 0xf5),
                text2: Color32::from_rgb(0xc8, 0xcb, 0xd1),
                dim: Color32::from_rgb(0x98, 0x9d, 0xa7),
                dim2: Color32::from_rgb(0x63, 0x67, 0x70),
                accent: Color32::from_rgb(0x58, 0x65, 0xf2),
                accent_dim: Color32::from_rgba_unmultiplied(0x58, 0x65, 0xf2, 40),
                ok: Color32::from_rgb(0x3b, 0xa5, 0x5c),
                err: Color32::from_rgb(0xed, 0x42, 0x45),
            },
            // slate look: #080807 / #DDDDD5 based, borders = lighter tints of bg
            Theme::Slate => Self {
                bg: Color32::from_rgb(0x08, 0x08, 0x07),
                panel: Color32::from_rgb(0x0e, 0x0e, 0x0d),
                panel2: Color32::from_rgb(0x15, 0x15, 0x13),
                line: Color32::from_rgb(0x1e, 0x1e, 0x1c),
                line_br: Color32::from_rgb(0x2c, 0x2c, 0x28),
                frame_border: Color32::from_rgba_unmultiplied(0x34, 0x34, 0x30, 85),
                text: Color32::from_rgb(0xdd, 0xdd, 0xd5),
                text2: Color32::from_rgb(0xbd, 0xbd, 0xb5),
                dim: Color32::from_rgb(0x90, 0x90, 0x88),
                dim2: Color32::from_rgb(0x60, 0x60, 0x59),
                accent: Color32::from_rgb(0xdd, 0xdd, 0xd5),
                accent_dim: Color32::from_rgba_unmultiplied(0xdd, 0xdd, 0xd5, 28),
                ok: Color32::from_rgb(0x9a, 0xa8, 0x92),
                err: Color32::from_rgb(0xc2, 0x8a, 0x7c),
            },
        }
    }
}

pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    fonts.font_data.insert(
        "lucide".into(),
        FontData::from_static(LUCIDE_FONT_BYTES).into(),
    );
    let mut lucide_fonts = vec!["lucide".into()];
    lucide_fonts.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(lucide_family(), lucide_fonts);

    #[cfg(wire_has_font_fraktion_sans)]
    {
        fonts.font_data.insert(
            "FraktionSans".into(),
            FontData::from_owned(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fonts/PPFraktionSans-Light.otf"
                ))
                .to_vec(),
            )
            .tweak(FontTweak {
                y_offset: 1.0,
                ..Default::default()
            })
            .into(),
        );
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "FraktionSans".into());
    }

    #[cfg(wire_has_font_fraktion_mono)]
    {
        fonts.font_data.insert(
            "FraktionMono".into(),
            FontData::from_owned(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fonts/PPFraktionMono-Regular.otf"
                ))
                .to_vec(),
            )
            .tweak(FontTweak {
                y_offset: 1.0,
                ..Default::default()
            })
            .into(),
        );
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "FraktionMono".into());
    }

    #[allow(unused_mut)]
    let mut display_fonts = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    #[cfg(wire_has_font_kh_interference)]
    {
        fonts.font_data.insert(
            "KhInterference".into(),
            FontData::from_owned(
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/fonts/Kh-Interference.otf"
                ))
                .to_vec(),
            )
            .into(),
        );
        display_fonts.insert(0, "KhInterference".into());
    }
    fonts.families.insert(kh_family(), display_fonts);

    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(TextStyle::Heading, sans(18.0));
    style.text_styles.insert(TextStyle::Body, sans(13.0));
    style.text_styles.insert(TextStyle::Button, sans(13.0));
    style.text_styles.insert(TextStyle::Small, sans(11.0));
    style.text_styles.insert(TextStyle::Monospace, mono(12.0));
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(14.0, 7.0);
    style.spacing.interact_size.y = 34.0;
    style.visuals.window_corner_radius = CornerRadius::same(10);
    ctx.set_style(style);
}

/// Build an `egui::Visuals` from a palette so built-in widgets (comboboxes,
/// sliders, buttons, text edits, windows) pick up the active theme too.
pub fn visuals_for(pal: &Palette) -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.dark_mode = true;
    visuals.window_fill = pal.bg;
    visuals.panel_fill = pal.bg;
    visuals.extreme_bg_color = pal.panel;
    visuals.faint_bg_color = pal.panel2;
    visuals.code_bg_color = pal.panel2;

    visuals.widgets.noninteractive.bg_fill = pal.panel;
    visuals.widgets.noninteractive.weak_bg_fill = pal.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal.line);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal.text2);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(10);

    visuals.widgets.inactive.bg_fill = pal.panel2;
    visuals.widgets.inactive.weak_bg_fill = pal.panel2;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, pal.line);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(10);

    visuals.widgets.hovered.bg_fill = pal.panel2;
    visuals.widgets.hovered.weak_bg_fill = pal.panel2;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, pal.line_br);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(10);

    visuals.widgets.active.bg_fill = pal.panel2;
    visuals.widgets.active.weak_bg_fill = pal.panel2;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, pal.line_br);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, pal.accent);
    visuals.widgets.active.corner_radius = CornerRadius::same(10);

    visuals.widgets.open.bg_fill = pal.panel2;
    visuals.widgets.open.weak_bg_fill = pal.panel2;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0_f32, pal.line_br);
    visuals.widgets.open.corner_radius = CornerRadius::same(10);

    visuals.selection.bg_fill = pal.accent_dim;
    visuals.window_stroke = Stroke::new(1.0_f32, pal.line);
    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals
}

// ----------------------- helper widgets -----------------------

pub fn separator_line(ui: &mut egui::Ui, color: Color32) {
    let rect = ui.available_rect_before_wrap();
    let y = rect.top();
    ui.painter()
        .hline(rect.x_range(), y, Stroke::new(1.0_f32, color));
}

pub fn separator_line_full(ui: &mut egui::Ui, color: Color32) {
    let rect = ui.max_rect();
    ui.painter()
        .hline(rect.x_range(), rect.top(), Stroke::new(1.0_f32, color));
}

pub fn v_sep(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, 26.0), egui::Sense::hover());
    ui.painter()
        .vline(rect.center().x, rect.y_range(), Stroke::new(1.0_f32, color));
}

pub fn dot(ui: &mut egui::Ui, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
}

pub fn circle_avatar(ui: &mut egui::Ui, pal: &Palette, initial: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), size / 2.0, pal.panel2);
    ui.painter()
        .circle_stroke(rect.center(), size / 2.0, Stroke::new(1.0_f32, pal.line_br));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial,
        FontId::new(size * 0.42, kh_family()),
        pal.text2,
    );
}

pub fn badge(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    egui::Frame::default()
        .fill(pal.panel)
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .color(pal.text2)
                    .size(ui_font_size(12.5)),
            );
        });
}

pub fn theme_badge(ui: &mut egui::Ui, pal: &Palette, name: &str) -> egui::Response {
    egui::Frame::default()
        .fill(pal.panel2)
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, pal.accent, 6.0);
                ui.label(
                    RichText::new(name)
                        .color(pal.text2)
                        .size(ui_font_size(12.5)),
                );
            });
        })
        .response
        .interact(egui::Sense::click())
}

pub fn action_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    tone: ButtonTone,
) -> egui::Response {
    let (fill, stroke, text) = match tone {
        ButtonTone::Primary => (pal.accent, Stroke::new(1.0_f32, pal.accent), pal.bg),
        ButtonTone::Secondary => (pal.panel2, Stroke::new(1.0_f32, pal.line_br), pal.text2),
        ButtonTone::Danger => (pal.panel2, Stroke::new(1.0_f32, pal.err), pal.err),
    };
    ui.add(
        egui::Button::new(RichText::new(label).color(text).size(ui_font_size(13.0)))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(10))
            .min_size(Vec2::new(0.0, 34.0)),
    )
}

pub fn action_button_full(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    tone: ButtonTone,
) -> egui::Response {
    let (fill, stroke, text) = match tone {
        ButtonTone::Primary => (pal.accent, Stroke::new(1.0_f32, pal.accent), pal.bg),
        ButtonTone::Secondary => (pal.panel2, Stroke::new(1.0_f32, pal.line_br), pal.text2),
        ButtonTone::Danger => (pal.panel2, Stroke::new(1.0_f32, pal.err), pal.err),
    };
    ui.add_sized(
        Vec2::new(ui.available_width(), 34.0),
        egui::Button::new(RichText::new(label).color(text).size(ui_font_size(13.0)))
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(10)),
    )
}

pub fn toolbar_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
    selected: bool,
) -> egui::Response {
    let fill = if selected { pal.accent_dim } else { pal.panel2 };
    let stroke = if selected {
        Stroke::new(1.0_f32, pal.accent)
    } else {
        Stroke::new(1.0_f32, pal.line_br)
    };
    let text = if selected { pal.accent } else { pal.text2 };

    let mut content = LayoutJob::default();
    content.append(
        &char::from(icon).to_string(),
        0.0,
        TextFormat {
            font_id: lucide(14.0),
            color: text,
            ..Default::default()
        },
    );
    content.append(
        "  ",
        0.0,
        TextFormat {
            font_id: sans(12.0),
            color: text,
            ..Default::default()
        },
    );
    content.append(
        label,
        0.0,
        TextFormat {
            font_id: sans(12.0),
            color: text,
            ..Default::default()
        },
    );

    ui.add(
        egui::Button::new(content)
            .fill(fill)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(10))
            .min_size(Vec2::new(0.0, 32.0)),
    )
}
pub fn ghost_icon_button(ui: &mut egui::Ui, pal: &Palette, glyph: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::click());
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), pal.panel2);
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        sans(18.0),
        if response.hovered() {
            pal.text
        } else {
            pal.text2
        },
    );
    response
}
pub fn dock_icon_btn(
    ui: &mut egui::Ui,
    pal: &Palette,
    glyph: &str,
    active: bool,
) -> egui::Response {
    let size = Vec2::splat(42.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let fill = if active { pal.panel2 } else { pal.panel };
    let border = if active { pal.line_br } else { pal.line };
    let text_color = if active { pal.text } else { pal.text2 };

    ui.painter().circle_filled(rect.center(), 21.0, fill);
    ui.painter()
        .circle_stroke(rect.center(), 21.0, Stroke::new(1.0_f32, border));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        sans(18.0),
        text_color,
    );
    response
}

pub fn dock_control(
    ui: &mut egui::Ui,
    pal: &Palette,
    glyph: &str,
    label: &str,
    active: bool,
) -> egui::Response {
    ui.allocate_ui_with_layout(
        Vec2::new(66.0, 66.0),
        egui::Layout::top_down(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let response = dock_icon_btn(ui, pal, glyph, active);
            ui.label(
                RichText::new(label)
                    .color(pal.text2)
                    .size(ui_font_size(12.0)),
            );
            response
        },
    )
    .inner
}

pub fn leave_button(ui: &mut egui::Ui, pal: &Palette) -> egui::Response {
    let size = Vec2::new(78.0, 36.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let fill = if response.hovered() {
        Color32::from_rgb(0xd1, 0x7c, 0x67)
    } else {
        pal.err
    };
    ui.painter().rect_filled(rect, CornerRadius::same(18), fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Leave",
        sans(12.5),
        Color32::from_rgb(0x1c, 0x0f, 0x0c),
    );
    response
}
