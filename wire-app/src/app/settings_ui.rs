//! Settings and update dialogs.

#[cfg(windows)]
use super::UpdateStatus;
use super::{
    widgets::{floating_dialog_header, format_bytes, painted_volume_slider},
    AppState, ChatStyle, DEFAULT,
};
use crate::{
    chat::RetentionPolicy,
    runtime::Command,
    sounds::Sound,
    theme::{action_button, kh_family, ui_font_size, ButtonTone, Palette, Theme, WindowFrameStyle},
    window_frame,
};
use egui::{Align, Align2, CornerRadius, Frame, Layout, RichText, Stroke, Ui};
use wire::{
    audio::AudioQuality,
    video::{BitratePreset, StreamPreset},
};

impl AppState {
    pub(super) fn ui_settings_window(&mut self, ctx: &egui::Context) {
        let can_close = self.configured;
        let pal = Palette::for_theme(self.theme);
        let pane_rect = self.pane_constrain_rect();
        let dialog_width = (pane_rect.width() - 40.0).clamp(420.0, 500.0);
        // Reserve enough vertical space for the dialog chrome plus a visible
        // inset above and below the centered window.
        let scroll_height = (pane_rect.height() - 230.0).clamp(220.0, 700.0);
        egui::Window::new("settings-dialog")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .constrain_to(pane_rect)
            .default_width(dialog_width)
            .min_width(dialog_width)
            .max_width(dialog_width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                Frame::new()
                    .fill(pal.bg)
                    .stroke(Stroke::new(1.0_f32, pal.line_br))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(0.0),
            )
            .show(ctx, |ui| {
                let width = ui.available_width();
                ui.set_width(width);
                let _ = floating_dialog_header(
                    ui,
                    &pal,
                    "SETTINGS",
                    "appearance, audio, video and updates",
                    None,
                );
                Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 16))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("settings-scroll")
                            .max_height(scroll_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "General",
                                    "Choose how Wire starts.",
                                );
                                Frame::new()
                                    .fill(pal.panel2)
                                    .stroke(Stroke::new(1.0_f32, pal.line))
                                    .corner_radius(CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        ui.checkbox(
                                            &mut self.start_with_system,
                                            RichText::new("Start with system")
                                                .color(pal.text2)
                                                .size(ui_font_size(12.0)),
                                        );
                                        ui.checkbox(
                                            &mut self.show_system_usage,
                                            RichText::new("Show system usage in title bar")
                                                .color(pal.text2)
                                                .size(ui_font_size(12.0)),
                                        );
                                    });
                                settings_divider(ui);
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "Appearance",
                                    "Window frame and corners.",
                                );

                                settings_field_label(ui, &pal, "Theme", None);
                                egui::ComboBox::from_id_salt("settings-theme")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(self.theme.label())
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        for theme in Theme::ALL {
                                            if ui
                                                .selectable_label(
                                                    self.theme == theme,
                                                    theme.label(),
                                                )
                                                .clicked()
                                            {
                                                self.theme = theme;
                                            }
                                        }
                                    });
                                ui.add_space(8.0);

                                settings_field_label(ui, &pal, "Window corners", None);
                                let frame_detail = match self.window_frame_style {
                                    WindowFrameStyle::Auto => {
                                        #[cfg(windows)]
                                        {
                                            if window_frame::is_windows_11_or_newer() {
                                                "Rounded on this PC (Windows 11+)"
                                            } else {
                                                "Square on this PC (before Windows 11)"
                                            }
                                        }
                                        #[cfg(not(windows))]
                                        {
                                            "Square on this platform"
                                        }
                                    }
                                    WindowFrameStyle::Rounded => "Always rounded",
                                    WindowFrameStyle::Square => "Always square",
                                };
                                ui.label(
                                    RichText::new(frame_detail)
                                        .color(pal.dim)
                                        .size(ui_font_size(11.0)),
                                );
                                egui::ComboBox::from_id_salt("settings-window-corners")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(self.window_frame_style.label())
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        for style in WindowFrameStyle::ALL {
                                            if ui
                                                .selectable_label(
                                                    self.window_frame_style == style,
                                                    style.label(),
                                                )
                                                .clicked()
                                            {
                                                self.window_frame_style = style;
                                            }
                                        }
                                    });

                                settings_divider(ui);
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "Text chat",
                                    "Message appearance and local history visibility.",
                                );
                                settings_field_label(ui, &pal, "Message style", None);
                                Frame::new()
                                    .fill(pal.panel2)
                                    .stroke(Stroke::new(1.0_f32, pal.line))
                                    .corner_radius(CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        let mut compact = self.chat_style == ChatStyle::Compact;
                                        if ui
                                            .checkbox(
                                                &mut compact,
                                                RichText::new("Compact (Discord-like)")
                                                    .color(pal.text2)
                                                    .size(ui_font_size(12.0)),
                                            )
                                            .changed()
                                        {
                                            self.chat_style = if compact {
                                                ChatStyle::Compact
                                            } else {
                                                ChatStyle::Bubbles
                                            };
                                        }
                                        ui.label(
                                            RichText::new(
                                                "Removes bubbles and groups consecutive messages from the same sender within one minute.",
                                            )
                                            .color(pal.dim)
                                            .size(ui_font_size(10.5)),
                                        );
                                    });
                                ui.add_space(8.0);

                                settings_field_label(ui, &pal, "Keep history", None);
                                egui::ComboBox::from_id_salt("settings-chat-retention")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(self.chat_retention.label())
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        for policy in [
                                            RetentionPolicy::Unlimited,
                                            RetentionPolicy::Days(7),
                                            RetentionPolicy::Days(30),
                                            RetentionPolicy::Days(90),
                                        ] {
                                            if ui
                                                .selectable_label(
                                                    self.chat_retention == policy,
                                                    policy.label(),
                                                )
                                                .clicked()
                                            {
                                                self.chat_retention = policy;
                                            }
                                        }
                                    });
                                ui.add_space(8.0);
                                settings_field_label(
                                    ui,
                                    &pal,
                                    "Maximum received image size",
                                    Some("Larger images stay remote and appear as placeholders."),
                                );
                                egui::ComboBox::from_id_salt("settings-max-image-bytes")
                                    .width(ui.available_width())
                                    .selected_text(image_limit_label(self.max_image_bytes))
                                    .show_ui(ui, |ui| {
                                        for limit in [
                                            Some(1024 * 1024),
                                            Some(5 * 1024 * 1024),
                                            Some(10 * 1024 * 1024),
                                            Some(25 * 1024 * 1024),
                                            Some(50 * 1024 * 1024),
                                            Some(100 * 1024 * 1024),
                                            Some(250 * 1024 * 1024),
                                            Some(500 * 1024 * 1024),
                                            Some(1024 * 1024 * 1024),
                                            None,
                                        ] {
                                            if ui
                                                .selectable_label(
                                                    self.max_image_bytes == limit,
                                                    image_limit_label(limit),
                                                )
                                                .clicked()
                                            {
                                                self.max_image_bytes = limit;
                                            }
                                        }
                                    });

                                settings_divider(ui);
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "Audio",
                                    "Input, playback and call quality.",
                                );

                                settings_field_label(ui, &pal, "Microphone", None);
                                let input_label = if self.audio_config.selected_input == DEFAULT {
                                    "System default"
                                } else {
                                    &self.audio_config.selected_input
                                };
                                egui::ComboBox::from_id_salt("settings-microphone")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(input_label)
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        if ui
                                            .selectable_label(
                                                self.audio_config.selected_input == DEFAULT,
                                                "System default",
                                            )
                                            .clicked()
                                        {
                                            self.audio_config.selected_input = DEFAULT.to_string();
                                        }
                                        for device in &self.devices.input {
                                            if ui
                                                .selectable_label(
                                                    &self.audio_config.selected_input == device,
                                                    device,
                                                )
                                                .clicked()
                                            {
                                                self.audio_config.selected_input =
                                                    device.to_string();
                                            }
                                        }
                                    });
                                ui.add_space(8.0);

                                settings_field_label(ui, &pal, "Speakers", None);
                                let output_label = if self.audio_config.selected_output == DEFAULT {
                                    "System default"
                                } else {
                                    &self.audio_config.selected_output
                                };
                                egui::ComboBox::from_id_salt("settings-speakers")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(output_label)
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        if ui
                                            .selectable_label(
                                                self.audio_config.selected_output == DEFAULT,
                                                "System default",
                                            )
                                            .clicked()
                                        {
                                            self.audio_config.selected_output = DEFAULT.to_string();
                                        }
                                        for device in &self.devices.output {
                                            if ui
                                                .selectable_label(
                                                    &self.audio_config.selected_output == device,
                                                    device,
                                                )
                                                .clicked()
                                            {
                                                self.audio_config.selected_output =
                                                    device.to_string();
                                            }
                                        }
                                    });

                                ui.add_space(8.0);
                                settings_field_label(ui, &pal, "UI sounds", None);
                                if painted_volume_slider(
                                    ui,
                                    &pal,
                                    &mut self.ui_sound_volume,
                                    1.0,
                                    (ui.available_width() - 14.0).max(40.0),
                                    26.0,
                                    "UI sound volume",
                                ) {
                                    if let Some(sounds) = &mut self.sounds {
                                        sounds.set_volume(self.ui_sound_volume);
                                    }
                                }

                                ui.add_space(8.0);
                                Frame::new()
                                    .fill(pal.panel2)
                                    .stroke(Stroke::new(1.0_f32, pal.line))
                                    .corner_radius(CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 5))
                                    .show(ui, |ui| {
                                        #[cfg(feature = "audio-processing")]
                                        {
                                            ui.checkbox(
                                                &mut self.audio_config.processing_enabled,
                                                RichText::new("Echo cancellation")
                                                    .color(pal.text2)
                                                    .size(ui_font_size(12.0)),
                                            );
                                        }
                                        ui.checkbox(
                                            &mut self.audio_config.noise_suppression_enabled,
                                            RichText::new("Noise suppression (RNNoise)")
                                                .color(pal.text2)
                                                .size(ui_font_size(12.0)),
                                        );
                                    });

                                ui.add_space(8.0);
                                settings_field_label(ui, &pal, "Audio quality", None);
                                let selected_quality = format!(
                                    "{} · {}",
                                    self.audio_config.quality.label(),
                                    self.audio_config.quality.bandwidth_human()
                                );
                                egui::ComboBox::from_id_salt("settings-audio-quality")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(selected_quality)
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        for quality in &[
                                            AudioQuality::Low,
                                            AudioQuality::Medium,
                                            AudioQuality::High,
                                            AudioQuality::Ultra,
                                        ] {
                                            let label = format!(
                                                "{} · {}",
                                                quality.label(),
                                                quality.bandwidth_human()
                                            );
                                            if ui
                                                .selectable_label(
                                                    self.audio_config.quality == *quality,
                                                    &label,
                                                )
                                                .clicked()
                                            {
                                                self.audio_config.quality = *quality;
                                            }
                                        }
                                    });

                                settings_divider(ui);
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "Screen sharing",
                                    "Balance clarity, motion and bandwidth.",
                                );

                                let preset_label = StreamPreset::matches(&self.video_config)
                                    .map(|p| p.label)
                                    .unwrap_or("Custom");
                                let resolution_detail = format!(
                                    "{}×{} @ {} fps",
                                    self.video_config.resolution.width(),
                                    self.video_config.resolution.height(),
                                    self.video_config.framerate
                                );
                                settings_field_label(
                                    ui,
                                    &pal,
                                    "Stream quality",
                                    Some(&resolution_detail),
                                );
                                egui::ComboBox::from_id_salt("settings-stream-quality")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(preset_label)
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        for preset in StreamPreset::all() {
                                            let selected = self.video_config.resolution
                                                == preset.resolution
                                                && self.video_config.framerate == preset.framerate;
                                            if ui.selectable_label(selected, preset.label).clicked()
                                            {
                                                self.video_config.resolution = preset.resolution;
                                                self.video_config.framerate = preset.framerate;
                                            }
                                        }
                                    });

                                ui.add_space(8.0);
                                let bitrate_label =
                                    BitratePreset::from_config(&self.video_config).label();
                                let bitrate_detail = format!(
                                    "{} Mbps effective",
                                    self.video_config.effective_bitrate() / 1_000_000
                                );
                                settings_field_label(ui, &pal, "Bitrate", Some(&bitrate_detail));
                                egui::ComboBox::from_id_salt("settings-bitrate")
                                    .width(ui.available_width())
                                    .selected_text(
                                        RichText::new(bitrate_label)
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .show_ui(ui, |ui| {
                                        for preset in BitratePreset::all() {
                                            let selected =
                                                BitratePreset::from_config(&self.video_config)
                                                    == *preset;
                                            if ui
                                                .selectable_label(selected, preset.label())
                                                .clicked()
                                            {
                                                self.video_config.bitrate_bps = preset.bps();
                                            }
                                        }
                                    });

                                ui.add_space(10.0);
                                if ui
                                    .checkbox(
                                        &mut self.share_system_audio,
                                        RichText::new("Also share system audio")
                                            .color(pal.text2)
                                            .size(ui_font_size(12.0)),
                                    )
                                    .on_hover_text(
                                        "When you share a screen, send this computer's sound to the call",
                                    )
                                    .changed()
                                {
                                    self.set_share_system_audio_from_ui(self.share_system_audio);
                                }

                                #[cfg(windows)]
                                {
                                    settings_divider(ui);
                                    settings_section_heading(
                                        ui,
                                        &pal,
                                        "Updates",
                                        &format!("Installed version v{}", crate::APP_VERSION),
                                    );

                                    let mut check_clicked = false;
                                    let mut download = None;
                                    match &self.update_status {
                                        UpdateStatus::Idle => {
                                            check_clicked = action_button(
                                                ui,
                                                &pal,
                                                "Check for updates",
                                                ButtonTone::Secondary,
                                            )
                                            .clicked();
                                        }
                                        UpdateStatus::Checking => {
                                            ui.horizontal(|ui| {
                                                ui.spinner();
                                                ui.label("Checking for updates...");
                                            });
                                        }
                                        UpdateStatus::UpToDate => {
                                            ui.horizontal(|ui| {
                                                ui.label(
                                                    RichText::new("You are up to date")
                                                        .color(pal.ok),
                                                );
                                                check_clicked = action_button(
                                                    ui,
                                                    &pal,
                                                    "Check again",
                                                    ButtonTone::Secondary,
                                                )
                                                .clicked();
                                            });
                                        }
                                        UpdateStatus::Available(release) => {
                                            ui.label(
                                                RichText::new(format!(
                                                    "Version v{} is available",
                                                    release.version
                                                ))
                                                .color(pal.ok),
                                            );
                                            if action_button(
                                                ui,
                                                &pal,
                                                "Download and relaunch",
                                                ButtonTone::Primary,
                                            )
                                            .clicked()
                                            {
                                                download = Some(release.clone());
                                            }
                                        }
                                        UpdateStatus::Downloading(release) => {
                                            ui.horizontal(|ui| {
                                                ui.spinner();
                                                ui.label(format!(
                                                    "Downloading v{}...",
                                                    release.version
                                                ));
                                            });
                                        }
                                        UpdateStatus::Error(error) => {
                                            ui.label(RichText::new(error).color(pal.err));
                                            check_clicked = action_button(
                                                ui,
                                                &pal,
                                                "Try again",
                                                ButtonTone::Secondary,
                                            )
                                            .clicked();
                                        }
                                    }
                                    if check_clicked {
                                        self.start_update_check(ctx);
                                    }
                                    if let Some(release) = download {
                                        self.start_update_download(ctx, release);
                                    }
                                }

                            });
                        settings_divider(ui);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if action_button(ui, &pal, "Save changes", ButtonTone::Primary)
                                .clicked()
                            {
                                self.play_sound(Sound::Button2);
                                let audio_config = self.audio_config();
                                let video_config = self.video_config;
                                self.cmd(Command::SetAudioConfig { audio_config });
                                self.cmd(Command::SetVideoConfig { video_config });
                                self.cmd(Command::SetMaxImageBytes {
                                    max_image_bytes: self.max_image_bytes,
                                });
                                self.cmd(Command::SetChatRetention {
                                    retention: self.chat_retention,
                                });
                                // Persist all regular settings immediately. The
                                // startup preference is committed after the OS
                                // integration succeeds on a background thread.
                                self.persist_settings();
                                self.update_autostart_in_background(ctx);
                                self.configured = true;
                                self.show_settings = false;
                            }
                            if can_close
                                && action_button(ui, &pal, "Close", ButtonTone::Secondary).clicked()
                            {
                                self.show_settings = false;
                            }
                        });
                    });
            });
    }

    #[cfg(windows)]
    pub(super) fn ui_update_prompt(&mut self, ctx: &egui::Context) {
        let release = match &self.update_status {
            UpdateStatus::Available(release) => release.clone(),
            _ => {
                self.show_update_prompt = false;
                return;
            }
        };
        let mut open = self.show_update_prompt;
        let mut download = false;
        let mut later = false;
        egui::Window::new("Update available")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .constrain_to(self.pane_constrain_rect())
            .show(ctx, |ui| {
                ui.label(format!(
                    "Wire v{} is available. You are running v{}.",
                    release.version,
                    crate::APP_VERSION
                ));
                ui.label("The new executable will be verified and placed on your Desktop.");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let pal = Palette::for_theme(self.theme);
                    if action_button(ui, &pal, "Download and relaunch", ButtonTone::Primary)
                        .clicked()
                    {
                        download = true;
                    }
                    if action_button(ui, &pal, "Later", ButtonTone::Secondary).clicked() {
                        later = true;
                    }
                });
            });
        if later {
            open = false;
        }
        self.show_update_prompt = open;
        if download {
            self.start_update_download(ctx, release);
        }
    }
}

fn settings_section_heading(ui: &mut Ui, pal: &Palette, title: &str, description: &str) {
    ui.label(
        RichText::new(title.to_uppercase())
            .family(kh_family())
            .color(pal.text2)
            .size(13.0),
    );
    ui.label(
        RichText::new(description)
            .color(pal.dim)
            .size(ui_font_size(11.0)),
    );
    ui.add_space(8.0);
}

fn settings_field_label(ui: &mut Ui, pal: &Palette, label: &str, detail: Option<&str>) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(label)
                .color(pal.text2)
                .size(ui_font_size(12.0)),
        );
        if let Some(detail) = detail {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(detail)
                        .color(pal.dim)
                        .size(ui_font_size(10.5)),
                );
            });
        }
    });
    ui.add_space(2.0);
}

fn settings_divider(ui: &mut Ui) {
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
}

fn image_limit_label(limit: Option<u64>) -> String {
    limit
        .map(|bytes| format!("{} per image", format_bytes(bytes)))
        .unwrap_or_else(|| "Unlimited".to_owned())
}
