//! Settings and update dialogs.

use super::{
    profile_ui::paint_profile_avatar,
    widgets::{
        floating_dialog_header, floating_panel, floating_panel_frame, floating_panel_width,
        format_bytes, painted_volume_slider,
    },
    AppState, ChatStyle, DEFAULT,
};
#[cfg(windows)]
use super::{PeerUpdateTransfer, UpdateStatus};
use crate::{
    chat::RetentionPolicy,
    runtime::Command,
    sounds::Sound,
    theme::{action_button, kh_family, ui_font_size, ButtonTone, Palette, Theme, WindowFrameStyle},
    window_frame,
};
use egui::{Align, CornerRadius, Frame, Layout, RichText, Stroke, Ui};
use wire::{
    audio::AudioQuality,
    video::{BitratePreset, StreamPreset},
};

impl AppState {
    /// Where recordings live, plus a way to get there.
    ///
    /// Without this the files would exist somewhere only the logs mention.
    fn ui_recordings_folder_row(&mut self, ui: &mut Ui, pal: &Palette) {
        settings_field_label(ui, pal, "Recordings folder", None);
        let label = match crate::recording::recordings_dir() {
            Some(dir) => dir.display().to_string(),
            None => "No data directory available".to_owned(),
        };
        // A long path must not widen the dialog, so it is truncated and shows
        // the full value on hover.
        ui.add(
            egui::Label::new(RichText::new(label.clone()).color(pal.dim).size(ui_font_size(11.0)))
                .truncate(),
        )
        .on_hover_text(label);

        if let Some(last) = &self.last_recording {
            let mut detail = format!(
                "Last recording: {} · {} · {}",
                last.started_at,
                super::widgets::format_duration_ms(last.duration_ms),
                if last.captured == 1 {
                    "1 speaker".to_owned()
                } else {
                    format!("{} speakers", last.captured)
                }
            );
            if last.silent > 0 {
                // Silence is normal (someone muted all call), but worth stating so
                // a missing file does not look like a lost one.
                detail.push_str(&format!(
                    " · {} said nothing",
                    if last.silent == 1 {
                        "1 person".to_owned()
                    } else {
                        format!("{} people", last.silent)
                    }
                ));
            }
            ui.label(
                RichText::new(detail)
                    .color(pal.dim)
                    .size(ui_font_size(10.5)),
            );
            if !last.failed.is_empty() {
                ui.label(
                    RichText::new(format!("Missing files for: {}", last.failed.join(", ")))
                        .color(pal.err)
                        .size(ui_font_size(10.5)),
                );
            }
        }

        ui.horizontal(|ui| {
            if action_button(ui, pal, "Open recordings", ButtonTone::Secondary).clicked() {
                self.reveal_recordings();
            }
            let has_last = self.last_recording.is_some();
            if ui
                .add_enabled_ui(has_last, |ui| {
                    action_button(ui, pal, "Open last recording", ButtonTone::Secondary)
                })
                .inner
                .clicked()
            {
                if let Some(target) = self.last_recording.as_ref().map(|last| last.dir.clone()) {
                    if let Err(error) = crate::recording::reveal_in_file_manager(&target) {
                        let message = format!("{error:#}");
                        tracing::warn!("could not open the recording folder: {message}");
                        self.notifications.error(
                            "recordings-folder",
                            "Could not open the folder",
                            message,
                        );
                    }
                }
            }
        });
    }

    pub(super) fn ui_settings_window(&mut self, ctx: &egui::Context) {
        let can_close = self.configured;
        let pal = Palette::for_theme(self.theme);
        let pane_rect = self.pane_constrain_rect();
        let dialog_width = floating_panel_width(pane_rect, 500.0, 0.0);
        // Reserve enough vertical space for the dialog chrome plus a visible
        // inset above and below the centered window.
        let scroll_height = (pane_rect.height() - 220.0).clamp(1.0, 700.0);
        floating_panel("settings-dialog", &pal, pane_rect, 500.0)
            .title_bar(false)
            .vscroll(false)
            .default_width(dialog_width)
            .min_width(dialog_width)
            .max_width(dialog_width)
            .frame(floating_panel_frame(&pal, 0))
            .show(ctx, |ui| {
                // Do not reuse an oversized width remembered from a previous
                // frame: one unwrapped child used to expand the dialog permanently.
                ui.set_width(dialog_width);
                if floating_dialog_header(
                    ui,
                    &pal,
                    "SETTINGS",
                    "appearance, audio, video and updates",
                    can_close.then_some("Close settings"),
                ) {
                    self.show_settings = false;
                }
                Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 16))
                    .show(ui, |ui| {
                        let content_width = ui.available_width();
                        ui.set_width(content_width);
                        egui::ScrollArea::vertical()
                            .id_salt("settings-scroll")
                            .max_height(scroll_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                // A vertical scroll area still permits its contents to report a
                                // wider minimum size. Pin rows to the dialog so long labels wrap
                                // instead of changing the window geometry.
                                let scroll_content_width = ui.available_width();
                                ui.set_min_width(scroll_content_width);
                                ui.set_max_width(scroll_content_width);
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "Profile",
                                    "Your display name and picture, shown to everyone you message or call.",
                                );
                                Frame::new()
                                    .fill(pal.panel2)
                                    .stroke(Stroke::new(1.0_f32, pal.line))
                                    .corner_radius(CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 12.0;
                                            let avatar = self.own_avatar_texture(ctx);
                                            let preview_name =
                                                crate::profile::sanitize_display_name(
                                                    &self.profile_edit_name,
                                                );
                                            let preview_name = if preview_name.is_empty() {
                                                self.own_label()
                                            } else {
                                                preview_name
                                            };
                                            let initials =
                                                crate::profile::display_name_initial(&preview_name)
                                                    .unwrap_or_else(|| "?".to_owned());
                                            paint_profile_avatar(
                                                ui, &pal, avatar, &initials, 56.0,
                                            );
                                            ui.vertical(|ui| {
                                                if action_button(
                                                    ui,
                                                    &pal,
                                                    "Choose picture…",
                                                    ButtonTone::Secondary,
                                                )
                                                .on_hover_text(
                                                    "PNG, JPEG, GIF or WebP up to 8 MiB",
                                                )
                                                .clicked()
                                                {
                                                    if let Some(path) = rfd::FileDialog::new()
                                                        .set_title("Choose profile picture")
                                                        .add_filter(
                                                            "Images",
                                                            &[
                                                                "png", "jpg", "jpeg", "gif",
                                                                "webp", "bmp",
                                                            ],
                                                        )
                                                        .pick_file()
                                                    {
                                                        self.set_own_avatar_from_file(ctx, &path);
                                                    }
                                                }
                                                if self.own_avatar_hash.is_some() {
                                                    if action_button(
                                                        ui,
                                                        &pal,
                                                        "Remove",
                                                        ButtonTone::Secondary,
                                                    )
                                                    .clicked()
                                                    {
                                                        self.clear_own_avatar();
                                                    }
                                                }
                                            });
                                        });
                                        ui.add_space(6.0);
                                        settings_field_label(ui, &pal, "Display name", None);
                                        if ui
                                            .add(
                                                egui::TextEdit::singleline(
                                                    &mut self.profile_edit_name,
                                                )
                                                .hint_text("e.g. Ada Lovelace")
                                                .desired_width(f32::INFINITY),
                                            )
                                            .changed()
                                        {
                                            self.profile_edit_error = None;
                                        }
                                        ui.label(
                                            RichText::new(format!(
                                                "{}/32",
                                                crate::profile::sanitize_display_name(
                                                    &self.profile_edit_name
                                                )
                                                .chars()
                                                .count()
                                            ))
                                            .color(pal.dim)
                                            .size(ui_font_size(10.5)),
                                        );
                                        ui.add_space(6.0);
                                        self.ui_accent_picker(ui, &pal);
                                        if let Some(error) = &self.profile_edit_error {
                                            ui.label(
                                                RichText::new(error)
                                                    .color(pal.err)
                                                    .size(ui_font_size(11.5)),
                                            );
                                        }
                                        ui.add_space(4.0);
                                        if action_button(
                                            ui,
                                            &pal,
                                            "Save profile",
                                            ButtonTone::Primary,
                                        )
                                        .clicked()
                                        {
                                            self.save_own_profile_edit();
                                        }
                                    });
                                settings_divider(ui);
                                settings_section_heading(
                                    ui,
                                    &pal,
                                    "About",
                                    &format!(
                                        "Wire v{} · build {}",
                                        crate::APP_VERSION,
                                        crate::GIT_HASH
                                    ),
                                );
                                settings_divider(ui);
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
                                    .truncate()
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
                                    .truncate()
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
                                ui.add_space(8.0);
                                settings_field_label(
                                    ui,
                                    &pal,
                                    "KLIPY API key",
                                    Some("Used on this device to search, load and share GIFs."),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.klipy_api_key)
                                        .password(true)
                                        .hint_text("Enter your KLIPY app key")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(
                                        RichText::new("Powered by KLIPY")
                                            .color(pal.dim)
                                            .size(ui_font_size(10.0)),
                                    );
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
                                    "Call recording",
                                    "Save every speaker to a separate file.",
                                );
                                Frame::new()
                                    .fill(pal.panel2)
                                    .stroke(Stroke::new(1.0_f32, pal.line))
                                    .corner_radius(CornerRadius::same(7))
                                    .inner_margin(egui::Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        let mut automatic = self.record_calls_automatically;
                                        if ui
                                            .checkbox(
                                                &mut automatic,
                                                RichText::new("Record every call automatically")
                                                    .color(pal.text2)
                                                    .size(ui_font_size(12.0)),
                                            )
                                            .on_hover_text(
                                                "Start recording as soon as a call connects, and \
                                                 stop when the last person leaves",
                                            )
                                            .changed()
                                        {
                                            self.set_record_calls_automatically(automatic);
                                        }
                                        ui.label(
                                            RichText::new(
                                                "Every participant is written to their own file, so \
                                                 each one can be transcribed on its own and put \
                                                 back together by timestamp.",
                                            )
                                            .color(pal.dim)
                                            .size(ui_font_size(10.5)),
                                        );
                                        ui.label(
                                            RichText::new(
                                                "People you call are not told that this device is \
                                                 recording. Only you see the REC indicator.",
                                            )
                                            .color(pal.dim)
                                            .size(ui_font_size(10.5)),
                                        );
                                        ui.add_space(4.0);
                                        self.ui_recordings_folder_row(ui, &pal);
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
                                self.cmd(Command::SetRecordCallsAutomatically {
                                    enabled: self.record_calls_automatically,
                                });
                                self.chat.inline_file_data.clear();
                                self.chat.attachment_textures = Default::default();
                                self.chat.attachment_requests.clear();
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

    /// Accept or decline an executable offered by a connected friend.
    ///
    /// Nothing is fetched until the user confirms, so a peer can never push a
    /// binary onto this machine by itself.
    #[cfg(windows)]
    pub(super) fn ui_peer_update_prompt(&mut self, ctx: &egui::Context) {
        let Some(peer) = self.peer_update.active else {
            self.show_peer_update_prompt = false;
            return;
        };
        // A finished transfer must not be discarded just because presence has
        // not caught up yet; only a peer that vanished before anything started
        // closes the panel.
        let transfer_in_flight = self
            .peer_update
            .transfer
            .as_ref()
            .is_some_and(|transfer| transfer.peer == peer);
        if !transfer_in_flight && !self.peer_update.candidates.contains_key(&peer) {
            self.peer_update.reset();
            self.show_peer_update_prompt = false;
            return;
        }

        let pal = Palette::for_theme(self.theme);
        let peer_name = self.peer_display_name(peer);
        let mut open = self.show_peer_update_prompt;
        let mut start = false;
        let mut later = false;
        floating_panel(
            "Update from a peer",
            &pal,
            self.pane_constrain_rect(),
            420.0,
        )
        .open(&mut open)
        .show(ctx, |ui| {
            match self.peer_update.transfer.clone() {
                Some(transfer) => {
                    let percent = transfer.percent();
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Receiving from {peer_name} · {percent}%"));
                    });
                    ui.add(egui::ProgressBar::new(percent as f32 / 100.0).desired_height(6.0));
                }
                None => {
                    ui.label(format!("{peer_name} is running a newer Wire."));
                    match &self.peer_update.offer {
                        Some(offer) => {
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(offer.describe(crate::APP_VERSION))
                                    .color(pal.text2)
                                    .size(ui_font_size(12.0)),
                            );
                            ui.label(
                                RichText::new("Wire restarts once the transfer is verified.")
                                    .color(pal.dim)
                                    .size(ui_font_size(11.5)),
                            );
                        }
                        None => {
                            ui.label("Asking for their executable…");
                        }
                    }
                    if let Some(error) = &self.peer_update.error {
                        ui.add_space(6.0);
                        ui.label(RichText::new(error).color(pal.err));
                    }
                    ui.add_space(10.0);
                    // Nothing to accept until the peer has answered, and an
                    // offer that is not runnable here must not be startable.
                    let can_start = self.peer_update.offer.as_ref().is_some_and(|offer| {
                        offer.is_usable_for(crate::APP_VERSION) && offer.is_runnable_here()
                    });
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(can_start, |ui| {
                            if action_button(ui, &pal, "Update and restart", ButtonTone::Primary)
                                .clicked()
                            {
                                start = true;
                            }
                        });
                        // Deliberately outside the gate above: declining has to stay
                        // reachable in every state, including the error states where
                        // there is no valid offer to accept.
                        if action_button(ui, &pal, "Not now", ButtonTone::Secondary).clicked() {
                            later = true;
                        }
                    });
                }
            }
            if transfer_in_flight {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Keep Wire open until the transfer finishes.")
                        .color(pal.dim)
                        .size(ui_font_size(11.0)),
                );
            }
        });
        if later {
            open = false;
            self.peer_update.reset();
        }
        self.show_peer_update_prompt = open;
        if start {
            self.peer_update.transfer = Some(PeerUpdateTransfer {
                peer,
                received: 0,
                total: self
                    .peer_update
                    .offer
                    .as_ref()
                    .map(|offer| offer.total_bytes)
                    .unwrap_or(0),
            });
            self.show_peer_update_prompt = false;
            self.cmd(Command::DownloadPeerUpdate { peer });
        }
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
        let pal = Palette::for_theme(self.theme);
        floating_panel("Update available", &pal, self.pane_constrain_rect(), 440.0)
            .open(&mut open)
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

fn settings_field_label(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    detail: Option<&str>,
) -> egui::Response {
    let response = ui
        .vertical(|ui| {
            ui.set_max_width(ui.available_width());
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
        })
        .response;
    ui.add_space(2.0);
    response
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_settings_details_stay_inside_the_dialog_width() {
        let context = egui::Context::default();
        let mut measured = egui::Rect::NOTHING;
        let _ = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.allocate_ui(egui::vec2(320.0, 160.0), |ui| {
                    ui.set_width(320.0);
                    measured = settings_field_label(
                        ui,
                        &Palette::for_theme(Theme::Amber),
                        "Keep history",
                        Some(
                            "Long descriptions wrap below the setting name instead of widening the dialog or shifting neighboring controls.",
                        ),
                    )
                    .rect;
                });
            });
        });

        assert!(measured.width() <= 320.5, "field widened to {measured:?}");
        assert!(
            measured.height() > 30.0,
            "detail did not wrap: {measured:?}"
        );
    }
}
