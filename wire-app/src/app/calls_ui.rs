//! Call participants, contacts, capture picker, and stream views.

#[cfg(target_os = "macos")]
use super::open_screen_recording_settings;
use super::{
    friend_call_enabled, save_friends,
    widgets::{
        aspect_fit_rect, chat_hairline, chat_selected_surface, chat_surface, copy_to_clipboard,
        ellipsize, floating_dialog_header, fmt_error, fmt_node_id, paint_volume_track,
        participant_bar_columns, peer_volume_slider, read_clipboard, section_card,
        video_display_size, VolumeKnob, CHROME_CONTROL_HEIGHT, CHROME_INNER_RADIUS, CHROME_RADIUS,
        PARTICIPANT_CHIP_HEIGHT, PARTICIPANT_GAP,
    },
    AppMode, AppState, StreamSource, StreamViewMode, TextureUploadStats, STREAM_GRID_GAP,
};
#[cfg(windows)]
use super::{UpdateStatus, VideoFrameState};
use crate::{
    client_status::Availability,
    runtime::{CallState, Command},
    sounds::Sound,
    theme::{
        action_button, action_button_full, button_tone_style, circle_avatar, compact_v_sep,
        dock_control, dot, ghost_icon_button, kh_family, leave_button, lucide, menu_item_button,
        sans, toolbar_ghost_icon_button, ui_font_size, v_sep, ButtonTone, Palette,
    },
    video_decode::DecodedFrameData,
};
#[cfg(windows)]
use anyhow::{Context, Result};
use egui::{Align, Align2, Color32, CornerRadius, Frame, Layout, RichText, Stroke, Ui, Vec2};
use egui_phosphor::regular as ph;
use iroh::NodeId;
use lucide_icons::Icon;
use std::{
    str::FromStr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tracing::{info, warn};
use wire::audio::{AudioLevelHandle, VolumeHandle};

impl AppState {
    pub(super) fn ui_capture_picker(&mut self, ctx: &egui::Context, pal: &Palette) {
        let mut open = self.show_capture_picker;
        let mut selected = self.selected_capture_target;
        let targets = self.capture_targets.clone();
        let mut start = false;
        let mut cancel = false;
        let mut refresh = false;
        let mut share_system_audio = self.share_system_audio;
        let (picker_width, picker_body_height, use_columns) =
            capture_picker_layout(self.pane_constrain_rect().size());

        egui::Window::new("Share a screen or window")
            .id(egui::Id::new("capture-target-picker"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .constrain_to(self.pane_constrain_rect())
            .default_width(picker_width)
            .min_width(picker_width)
            .max_width(picker_width)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_width(picker_width);
                ui.label(
                    RichText::new("Choose exactly what people in this call can see and hear.")
                        .color(pal.text2),
                );
                ui.add_space(14.0);

                if use_columns {
                    ui.columns(2, |columns| {
                        capture_target_column(
                            &mut columns[0],
                            pal,
                            "Screens",
                            crate::screen_capture::CaptureTargetKind::Display,
                            Icon::Monitor,
                            &targets,
                            &mut selected,
                            picker_body_height,
                        );
                        capture_target_column(
                            &mut columns[1],
                            pal,
                            "Windows",
                            crate::screen_capture::CaptureTargetKind::Window,
                            Icon::AppWindow,
                            &targets,
                            &mut selected,
                            picker_body_height,
                        );
                    });
                } else {
                    let compact_height = ((picker_body_height - 8.0) * 0.5).max(86.0);
                    capture_target_column(
                        ui,
                        pal,
                        "Screens",
                        crate::screen_capture::CaptureTargetKind::Display,
                        Icon::Monitor,
                        &targets,
                        &mut selected,
                        compact_height,
                    );
                    ui.add_space(8.0);
                    capture_target_column(
                        ui,
                        pal,
                        "Windows",
                        crate::screen_capture::CaptureTargetKind::Window,
                        Icon::AppWindow,
                        &targets,
                        &mut selected,
                        compact_height,
                    );
                }

                ui.add_space(12.0);
                system_audio_share_row(ui, pal, &mut share_system_audio);
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if action_button(ui, pal, "Refresh", ButtonTone::Secondary).clicked() {
                        refresh = true;
                    }
                    if picker_width >= 640.0 {
                        let target = selected.and_then(|index| targets.get(index));
                        if let Some(target) = target {
                            ui.label(
                                RichText::new(format!(
                                    "Selected: {}",
                                    ellipsize(&target.title, 42)
                                ))
                                .color(pal.dim)
                                .size(ui_font_size(11.5)),
                            );
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_enabled_ui(selected.is_some(), |ui| {
                            if action_button(ui, pal, "Start sharing", ButtonTone::Primary)
                                .clicked()
                            {
                                start = true;
                            }
                        });
                        if action_button(ui, pal, "Cancel", ButtonTone::Secondary).clicked() {
                            cancel = true;
                        }
                        if ui.input(|input| input.key_pressed(egui::Key::Enter))
                            && selected.is_some()
                        {
                            start = true;
                        }
                        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                            cancel = true;
                        }
                    });
                });
            });

        self.selected_capture_target = selected;
        if self.share_system_audio != share_system_audio {
            self.share_system_audio = share_system_audio;
            self.persist_settings();
        }
        if start {
            if let Some(target) = selected.and_then(|index| targets.get(index)).cloned() {
                self.play_control_sound(true);
                self.cmd(Command::ToggleSharing {
                    enabled: true,
                    target: Some(target),
                    share_system_audio: self.share_system_audio,
                });
                self.show_capture_picker = false;
            }
        } else if refresh {
            self.open_capture_picker();
        } else {
            self.show_capture_picker = open && !cancel;
        }
    }

    pub(super) fn ui_call_participant_bar(&mut self, ui: &mut Ui, pal: &Palette) {
        let bar_height = ui.available_height();
        let bar_width = ui.available_width();
        let calls: Vec<_> = self.calls.iter().map(|(id, state)| (*id, *state)).collect();
        egui::ScrollArea::vertical()
            .id_salt("participant-bar-scroll")
            .auto_shrink([false, false])
            .max_height(bar_height)
            .show(ui, |ui| {
                ui.set_min_width(bar_width);
                ui.vertical_centered(|ui| {
                    let columns = participant_bar_columns(bar_width);
                    let item_count = calls.len() + 1;
                    for row_start in (0..item_count).step_by(columns) {
                        let row_end = (row_start + columns).min(item_count);
                        let width_id = ui.id().with(("participant-row-width", row_start));
                        let cached_width = ui
                            .ctx()
                            .data_mut(|data| data.get_temp::<f32>(width_id))
                            .unwrap_or(0.0);
                        let lead = ((bar_width - cached_width) * 0.5).max(0.0);
                        let mut content_rect = egui::Rect::NOTHING;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = PARTICIPANT_GAP;
                            if lead > 0.0 {
                                ui.add_space(lead);
                            }
                            for index in row_start..row_end {
                                let response =
                                    if let Some((node_id, state)) = calls.get(index).copied() {
                                        self.ui_participant_chip(ui, pal, node_id, state)
                                    } else {
                                        self.ui_self_participant_chip(ui, pal)
                                    };
                                content_rect = content_rect.union(response.rect);
                            }
                        });
                        let measured_width = content_rect.width();
                        if measured_width.is_finite() && measured_width > 0.0 {
                            ui.ctx().data_mut(|data| {
                                data.insert_temp(width_id, measured_width);
                            });
                            if (measured_width - cached_width).abs() > 0.5 {
                                ui.ctx().request_repaint();
                            }
                        }
                        if row_end < item_count {
                            ui.add_space(PARTICIPANT_GAP);
                        }
                    }
                });
            });
    }

    fn ui_self_participant_chip(&self, ui: &mut Ui, pal: &Palette) -> egui::Response {
        Frame::new()
            .fill(chat_surface(pal))
            .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
            .corner_radius(CornerRadius::same(CHROME_INNER_RADIUS))
            .inner_margin(CHIP_INNER_MARGIN)
            .show(ui, |ui| {
                ui.set_height(PARTICIPANT_CHIP_HEIGHT);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_height(PARTICIPANT_CHIP_HEIGHT);
                    ui.spacing_mut().item_spacing.x = 0.0;
                    circle_avatar(ui, pal, "Y", PARTICIPANT_AVATAR_SIZE);
                    ui.add_space(CHIP_IDENTITY_GAP);
                    chip_name_label(ui, "You", pal.text2);
                    ui.add_space(CHIP_IDENTITY_GAP);
                    let level = self
                        .local_audio_level
                        .as_ref()
                        .map(load_audio_level)
                        .unwrap_or(0.0);
                    voice_level_meter(ui, pal, level).on_hover_text(if self.muted {
                        "Microphone muted"
                    } else {
                        "Your microphone level"
                    });
                    if self.sharing_active {
                        ui.add_space(CHIP_IDENTITY_GAP);
                        chip_status_icon(
                            ui,
                            pal.ok,
                            Icon::ScreenShare,
                            "You are sharing your screen",
                        );
                    }
                });
            })
            .response
    }

    fn ui_participant_chip(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        node_id: NodeId,
        state: CallState,
    ) -> egui::Response {
        let is_active = matches!(state, CallState::Active);
        let call_waiting = matches!(state, CallState::Incoming)
            && self.local_group_call.is_some()
            && !self.incoming_belongs_to_local_group(node_id);
        let is_streaming = self
            .video_frames
            .get(&node_id)
            .is_some_and(|frame| frame.width > 0 && frame.height > 0);
        let stopped_watching = self
            .stopped_video_stream_generations
            .get(&node_id)
            .is_some_and(|stopped| {
                self.video_stream_generations
                    .get(&node_id)
                    .is_some_and(|current| current == stopped)
            });
        let (status_label, status_color) = match state {
            CallState::Incoming if call_waiting => (Some("call waiting"), pal.accent),
            CallState::Incoming => (Some("incoming"), pal.accent),
            CallState::Calling => (Some("connecting"), pal.accent),
            CallState::Active => (None, pal.ok),
            CallState::Aborted => (Some("ended"), pal.dim),
        };
        let fill = if is_active {
            chat_selected_surface(pal)
        } else {
            chat_surface(pal)
        };
        let voice_level = self
            .remote_audio_levels
            .get(&node_id)
            .map(load_audio_level)
            .unwrap_or(0.0);

        Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
            .corner_radius(CornerRadius::same(CHROME_INNER_RADIUS))
            .inner_margin(CHIP_INNER_MARGIN)
            .show(ui, |ui| {
                ui.set_height(PARTICIPANT_CHIP_HEIGHT);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_height(PARTICIPANT_CHIP_HEIGHT);
                    ui.spacing_mut().item_spacing.x = 0.0;
                    circle_avatar(
                        ui,
                        pal,
                        &self.peer_initial(node_id),
                        PARTICIPANT_AVATAR_SIZE,
                    );
                    ui.add_space(CHIP_IDENTITY_GAP);
                    chip_name_label(
                        ui,
                        &ellipsize(&self.peer_display_name(node_id), 16),
                        if is_active { pal.text } else { pal.text2 },
                    );
                    ui.add_space(CHIP_IDENTITY_GAP);
                    voice_level_meter(ui, pal, voice_level)
                        .on_hover_text("Voice received from this participant");

                    if stopped_watching {
                        ui.add_space(CHIP_IDENTITY_GAP);
                        chip_status_label(ui, "paused", pal.dim);
                    } else if is_streaming {
                        ui.add_space(CHIP_IDENTITY_GAP);
                        chip_status_icon(
                            ui,
                            pal.ok,
                            Icon::ScreenShare,
                            "This participant is sharing their screen",
                        );
                    } else if let Some(status) = status_label {
                        ui.add_space(CHIP_IDENTITY_GAP);
                        chip_status_label(ui, status, status_color);
                    }

                    match state {
                        CallState::Incoming => {
                            ui.add_space(CHIP_ACTION_GAP);
                            let accept_label = if call_waiting {
                                "End & accept"
                            } else {
                                "Accept"
                            };
                            if compact_chip_button(ui, pal, accept_label, ButtonTone::Primary)
                                .on_hover_text(if call_waiting {
                                    "Leave the group call and answer this call"
                                } else {
                                    "Answer this call"
                                })
                                .clicked()
                            {
                                self.accept_incoming_call(node_id);
                            }
                            ui.add_space(6.0);
                            if compact_chip_button(ui, pal, "Decline", ButtonTone::Danger).clicked()
                            {
                                self.cmd(Command::HandleIncoming {
                                    node_id,
                                    accept: false,
                                });
                            }
                        }
                        CallState::Calling | CallState::Active => {
                            ui.add_space(CHIP_ACTION_GAP);
                            if stopped_watching
                                && compact_chip_button(ui, pal, "Watch", ButtonTone::Primary)
                                    .on_hover_text("Resume watching this screen share")
                                    .clicked()
                            {
                                self.resume_watching(node_id);
                            }
                            if let Some(volume) = self.volumes.get(&node_id).cloned() {
                                if stopped_watching {
                                    ui.add_space(6.0);
                                }
                                let open = self.volume_open.contains(&node_id);
                                let open_t = ui.ctx().animate_bool_with_time(
                                    ui.id().with(("volume-open", node_id)),
                                    open,
                                    0.14,
                                );
                                if chip_icon_button(
                                    ui,
                                    pal,
                                    Icon::Volume2,
                                    open,
                                    if open {
                                        "Hide voice volume"
                                    } else {
                                        "Voice volume"
                                    },
                                )
                                .clicked()
                                {
                                    if open {
                                        self.volume_open.remove(&node_id);
                                    } else {
                                        self.volume_open.insert(node_id);
                                    }
                                }
                                let slider_w = 112.0 * open_t;
                                if slider_w > 4.0 {
                                    ui.add_space(6.0 * open_t);
                                    peer_volume_slider(
                                        ui,
                                        pal,
                                        &volume,
                                        slider_w,
                                        PARTICIPANT_ACTION_HEIGHT,
                                        "Voice volume",
                                    );
                                }
                            }
                            ui.add_space(8.0);
                            if compact_chip_button(ui, pal, "End", ButtonTone::Danger)
                                .on_hover_text("End call with this peer")
                                .clicked()
                            {
                                self.hang_up_call(node_id);
                            }
                        }
                        CallState::Aborted => {}
                    }
                });
            })
            .response
    }

    pub(super) fn ui_top_bar_content(&mut self, ui: &mut Ui, _ctx: &egui::Context, pal: &Palette) {
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            self.ui_mode_switcher(ui, pal);

            // Compact active-call indicator — same chrome language as the mode switcher.
            if let Some((label, color, detail)) = self.active_call_indicator(pal) {
                ui.add_space(8.0);
                let chip = Frame::new()
                    .fill(chat_surface(pal))
                    .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
                    .corner_radius(CornerRadius::same(CHROME_RADIUS))
                    .inner_margin(egui::Margin::symmetric(12, 3))
                    .show(ui, |ui| {
                        ui.set_min_height(CHROME_CONTROL_HEIGHT);
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 7.0;
                            dot(ui, color, 6.0);
                            ui.label(
                                RichText::new(label)
                                    .color(pal.text2)
                                    .size(ui_font_size(12.0)),
                            );
                            if self.sharing_active {
                                ui.label(
                                    RichText::new(if self.system_audio_active {
                                        "· sharing audio"
                                    } else {
                                        "· sharing"
                                    })
                                    .color(pal.accent)
                                    .size(ui_font_size(11.5)),
                                );
                            }
                        });
                    })
                    .response
                    .interact(egui::Sense::click())
                    .on_hover_text(detail);
                if chip.clicked() && self.app_mode != AppMode::Calls {
                    self.app_mode = AppMode::Calls;
                }
            } else if self.app_mode == AppMode::Calls {
                // Idle "Ready" stays plain (no pill). Active states use the brighter chip.
                ui.add_space(10.0);
                if self.sharing_active {
                    Frame::new()
                        .fill(chat_surface(pal))
                        .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
                        .corner_radius(CornerRadius::same(CHROME_RADIUS))
                        .inner_margin(egui::Margin::symmetric(12, 3))
                        .show(ui, |ui| {
                            ui.set_min_height(CHROME_CONTROL_HEIGHT);
                            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 7.0;
                                dot(ui, pal.accent, 6.0);
                                ui.label(
                                    RichText::new(if self.system_audio_active {
                                        "Sharing screen + audio"
                                    } else {
                                        "Sharing screen"
                                    })
                                    .color(pal.text2)
                                    .size(ui_font_size(12.0)),
                                );
                            });
                        });
                } else if self.our_node_id.is_some() {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 7.0;
                        dot(ui, pal.ok, 6.0);
                        ui.label(
                            RichText::new("Ready")
                                .color(pal.text2)
                                .size(ui_font_size(12.0)),
                        );
                    });
                } else {
                    ui.label(RichText::new("Connecting…").weak());
                }
            }

            #[cfg(windows)]
            let available_update = match &self.update_status {
                UpdateStatus::Available(release) => Some(release.clone()),
                _ => None,
            };
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_icon_button(ui, pal, ph::GEAR_SIX)
                    .on_hover_text("Settings")
                    .clicked()
                {
                    self.show_settings = true;
                }
                if ghost_icon_button(ui, pal, ph::ADDRESS_BOOK)
                    .on_hover_text("Contacts and calling")
                    .clicked()
                {
                    self.app_mode = AppMode::Calls;
                    self.show_contacts = true;
                }
                #[cfg(windows)]
                {
                    if let Some(release) = available_update {
                        if action_button(
                            ui,
                            pal,
                            &format!("Update v{}", release.version),
                            ButtonTone::Primary,
                        )
                        .on_hover_text("Download the update to Desktop and relaunch")
                        .clicked()
                        {
                            self.start_update_download(_ctx, release);
                        }
                    }
                }
                if self.app_mode == AppMode::Text {
                    let (status, color) = if self.chat.service_error.is_some() {
                        ("Chat unavailable", pal.err)
                    } else if self.our_node_id.is_some() {
                        ("Chat ready", pal.ok)
                    } else {
                        ("Connecting…", pal.dim)
                    };
                    let status =
                        ui.label(RichText::new(status).color(color).size(ui_font_size(12.0)));
                    if let Some(error) = &self.chat.service_error {
                        status.on_hover_text(error);
                    }
                }
            });
        });
    }

    /// Compact summary for the chrome top bar: label, accent color, hover detail.
    fn active_call_indicator(&self, pal: &Palette) -> Option<(String, Color32, String)> {
        if let Some(call) = &self.local_group_call {
            if let Some(waiting) = self.calls.iter().find_map(|(node_id, state)| {
                (matches!(state, CallState::Incoming)
                    && !self.incoming_belongs_to_local_group(*node_id))
                .then_some(*node_id)
            }) {
                let name = self.peer_display_name(waiting);
                return Some((
                    format!("Call waiting · {name}"),
                    Color32::from_rgb(255, 200, 80),
                    format!("{name} is calling while you are in {}", call.title),
                ));
            }
            let participants = call.participants.len().max(
                self.calls
                    .values()
                    .filter(|state| matches!(state, CallState::Active))
                    .count()
                    + 1,
            );
            return Some((
                format!("In group call · {}", call.title),
                pal.ok,
                format!("{} · {participants} in call", call.title),
            ));
        }
        if self.calls.is_empty() {
            return None;
        }

        let mut active: Vec<NodeId> = Vec::new();
        let mut incoming: Vec<NodeId> = Vec::new();
        let mut calling: Vec<NodeId> = Vec::new();
        for (node_id, state) in &self.calls {
            match state {
                CallState::Active => active.push(*node_id),
                CallState::Incoming => incoming.push(*node_id),
                CallState::Calling => calling.push(*node_id),
                CallState::Aborted => {}
            }
        }

        let name = |id: NodeId| self.peer_display_name(id);
        if !incoming.is_empty() {
            let primary = name(incoming[0]);
            let label = if incoming.len() == 1 {
                format!("Incoming · {primary}")
            } else {
                format!("Incoming · {} +{}", primary, incoming.len() - 1)
            };
            let detail = incoming
                .iter()
                .map(|id| name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            return Some((label, Color32::from_rgb(255, 200, 80), detail));
        }
        if !active.is_empty() {
            let primary = name(active[0]);
            let label = if active.len() == 1 {
                format!("In call · {primary}")
            } else {
                format!("In call · {} +{}", primary, active.len() - 1)
            };
            let detail = active
                .iter()
                .map(|id| name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            return Some((label, pal.ok, detail));
        }
        if !calling.is_empty() {
            let primary = name(calling[0]);
            let label = if calling.len() == 1 {
                format!("Calling · {primary}")
            } else {
                format!("Calling · {} +{}", primary, calling.len() - 1)
            };
            let detail = calling
                .iter()
                .map(|id| name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            return Some((label, Color32::from_rgb(120, 170, 255), detail));
        }
        None
    }

    pub(super) fn ui_dock_content(&mut self, ui: &mut Ui, pal: &Palette) {
        let rect = ui.max_rect();
        let active_calls = self
            .calls
            .values()
            .filter(|state| matches!(state, CallState::Active))
            .count();
        let in_call = active_calls > 0 || self.local_group_call.is_some();

        // Keep the control cluster centered at every window width.
        let desired_controls_width: f32 = if in_call { 245.0 } else { 142.0 };
        let controls_width = desired_controls_width.min(rect.width().max(0.0));
        let controls_left = (rect.center().x - controls_width / 2.0).clamp(
            rect.left(),
            (rect.right() - controls_width).max(rect.left()),
        );
        let controls_rect = egui::Rect::from_min_max(
            egui::pos2(controls_left, rect.top()),
            egui::pos2(
                (controls_left + controls_width).min(rect.right()),
                rect.bottom(),
            ),
        );

        ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(controls_rect));
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if dock_control(
                    ui,
                    pal,
                    if self.muted { Icon::MicOff } else { Icon::Mic },
                    self.muted,
                )
                .on_hover_text("Mute or unmute your microphone (Right Shift)")
                .clicked()
                {
                    self.toggle_muted();
                }
                ui.add_space(8.0);
                if dock_control(
                    ui,
                    pal,
                    if self.deafened {
                        Icon::EarOff
                    } else {
                        Icon::Headphones
                    },
                    self.deafened,
                )
                .on_hover_text("Silence or restore all incoming call audio (Right Control)")
                .clicked()
                {
                    self.toggle_deafened();
                }
                ui.add_space(8.0);
                let share_response = ui
                    .add_enabled_ui(in_call, |ui| {
                        dock_control(
                            ui,
                            pal,
                            if self.sharing_active {
                                Icon::ScreenShareOff
                            } else {
                                Icon::ScreenShare
                            },
                            self.sharing_active,
                        )
                    })
                    .inner;
                if share_response
                    .on_hover_text(if self.sharing_active && self.system_audio_active {
                        "Stop sharing your screen and computer sound"
                    } else if self.sharing_active {
                        "Stop sharing"
                    } else if !in_call {
                        "Join a call before sharing your screen"
                    } else {
                        "Share your screen"
                    })
                    .clicked()
                {
                    self.toggle_sharing_from_ui();
                }
                if in_call {
                    ui.add_space(12.0);
                    v_sep(ui, pal.line);
                    ui.add_space(12.0);
                    if leave_button(ui, pal).clicked() {
                        let peers: Vec<_> = self.calls.keys().copied().collect();
                        if !peers.is_empty() {
                            self.play_sound(Sound::Whoosh1);
                        }
                        for node_id in peers {
                            self.voluntary_hangups.fetch_add(1, Ordering::Relaxed);
                            self.cmd(Command::Abort { node_id });
                        }
                        self.leave_group_call();
                    }
                }
            });
        });
    }

    pub(super) fn ui_stage(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        pal: &Palette,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        if self.stream_view_mode != StreamViewMode::Normal {
            self.ui_stream_panel(
                ui,
                ctx,
                #[cfg(windows)]
                parent_hwnd,
            );
            return;
        }

        Frame::new()
            .fill(pal.bg)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                self.ui_stream_panel(
                    ui,
                    ctx,
                    #[cfg(windows)]
                    parent_hwnd,
                )
            });
    }

    /// Collapsed tools: one-off dial, copy own ID, add friend.
    fn ui_call_more_options(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        Frame::new()
            .fill(pal.panel)
            .corner_radius(CornerRadius::same(CHROME_RADIUS))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .stroke(Stroke::new(1.0_f32, chat_hairline(&pal)))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::CollapsingHeader::new(
                    RichText::new("More options")
                        .color(pal.text2)
                        .size(ui_font_size(12.5)),
                )
                .id_salt("call-more-options")
                .default_open(false)
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    self.ui_identity_card(ui);
                    ui.add_space(10.0);
                    self.ui_dial_card(ui);
                    ui.add_space(10.0);
                    self.ui_add_friend_card(ui);
                });
            });
    }

    pub(super) fn ui_contacts_window(&mut self, ctx: &egui::Context) {
        let pal = Palette::for_theme(self.theme);
        let pane_rect = self.pane_constrain_rect();
        let dialog_width = (pane_rect.width() - 32.0).clamp(340.0, 560.0);
        let scroll_height = (pane_rect.height() - 190.0).clamp(220.0, 720.0);
        let can_close = self.has_active_call();

        egui::Window::new("contacts-dialog")
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
                if floating_dialog_header(
                    ui,
                    &pal,
                    "CONTACTS",
                    "friends and calling",
                    can_close.then_some("Close contacts"),
                ) {
                    self.show_contacts = false;
                }
                Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 16))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("contacts-scroll")
                            .max_height(scroll_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                self.ui_friends_card(ui);
                                ui.add_space(12.0);
                                self.ui_call_more_options(ui);
                            });
                    });
            });
    }

    fn ui_identity_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "Your identity", |ui| {
            if let Some(node_id) = &self.our_node_id {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(fmt_node_id(&node_id.fmt_short()));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if action_button(ui, &pal, "Copy ID", ButtonTone::Secondary).clicked() {
                                copy_to_clipboard(&node_id.to_string());
                            }
                        });
                    });
                });
                ui.label(
                    RichText::new(
                        "Your stable Wire ID (saved locally). Share it so friends can add and call you.",
                    )
                    .small()
                    .weak(),
                );
            } else {
                ui.label(RichText::new("Waiting for network…").weak());
            }
        });
    }

    fn ui_dial_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "One-off call", |ui| {
            ui.label(
                RichText::new("Call someone by node ID without saving them as a friend.")
                    .small()
                    .weak(),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.remote_node_input)
                        .hint_text("Paste remote node ID")
                        .desired_width((ui.available_width() - 72.0).max(80.0)),
                );
                if response.changed() {
                    self.remote_node_id = if self.remote_node_input.is_empty() {
                        None
                    } else {
                        Some(NodeId::from_str(self.remote_node_input.trim()))
                    };
                }
                if action_button(ui, &pal, "Paste", ButtonTone::Secondary).clicked() {
                    if let Some(text) = read_clipboard() {
                        self.remote_node_input = text;
                        self.remote_node_id = Some(NodeId::from_str(self.remote_node_input.trim()));
                    }
                }
            });

            ui.horizontal(|ui| {
                let can_call = matches!(self.remote_node_id, Some(Ok(_)));
                ui.add_enabled_ui(can_call, |ui| {
                    if action_button(ui, &pal, "Call", ButtonTone::Primary).clicked() {
                        if let Some(Ok(node_id)) = self.remote_node_id {
                            self.play_sound(Sound::Button2);
                            self.cmd(Command::Call { node_id });
                        }
                    }
                });
                match &self.remote_node_id {
                    Some(Ok(node_id)) => {
                        let status = self
                            .friend_name(*node_id)
                            .map(|name| format!("Ready to call {name}"))
                            .unwrap_or_else(|| "Valid node ID".to_owned());
                        ui.label(RichText::new(status).color(pal.ok));
                    }
                    Some(Err(err)) => {
                        ui.label(fmt_error(&format!("Invalid ID: {err}")));
                    }
                    None => {
                        ui.label(RichText::new("Enter a node ID to call").weak());
                    }
                }
            });
        });
    }

    fn ui_add_friend_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "Add a friend", |ui| {
            let name_width = ui.available_width();
            ui.add(
                egui::TextEdit::singleline(&mut self.new_friend_name)
                    .hint_text("Name (optional)")
                    .desired_width(name_width),
            );
            let id_width = ui.available_width();
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.new_friend_id)
                    .hint_text("Their node ID")
                    .desired_width(id_width),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.add_friend();
            }
            ui.add_space(4.0);
            if action_button_full(ui, &pal, "Add friend", ButtonTone::Primary).clicked() {
                self.add_friend();
            }
        });
    }

    fn ui_friends_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        Frame::new()
            .fill(pal.panel)
            .corner_radius(CornerRadius::same(CHROME_RADIUS))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .stroke(Stroke::new(1.0_f32, chat_hairline(&pal)))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("FRIENDS")
                            .family(kh_family())
                            .color(pal.dim)
                            .size(11.0),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(if self.friends.is_empty() {
                                "no contacts".to_owned()
                            } else if self.friends.len() == 1 {
                                "1 contact".to_owned()
                            } else {
                                format!("{} contacts", self.friends.len())
                            })
                            .color(pal.dim2)
                            .size(ui_font_size(11.5)),
                        );
                    });
                });
                ui.add_space(8.0);

                let mut call: Option<NodeId> = None;
                let mut remove_idx: Option<usize> = None;
                let mut copy_id: Option<String> = None;

                if self.friends.is_empty() {
                    ui.label(
                        RichText::new(
                            "No friends yet. Open More options below to add someone by node ID.",
                        )
                        .color(pal.dim)
                        .size(ui_font_size(12.5)),
                    );
                }

                for (idx, friend) in self.friends.iter().enumerate() {
                    let parsed = NodeId::from_str(friend.node_id.trim());
                    let display_name = if friend.name.trim().is_empty()
                        || friend.name.trim() == friend.node_id.trim()
                    {
                        "Unnamed contact"
                    } else {
                        friend.name.trim()
                    };
                    let initial = display_name
                        .chars()
                        .find(|c| c.is_alphanumeric())
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_else(|| "?".to_owned());
                    let short_id = parsed
                        .as_ref()
                        .ok()
                        .map(|id| id.fmt_short().to_string())
                        .unwrap_or_else(|| {
                            let raw = friend.node_id.trim();
                            if raw.len() > 12 {
                                format!("{}…", &raw[..10])
                            } else {
                                raw.to_owned()
                            }
                        });
                    let availability = parsed.as_ref().ok().and_then(|node_id| {
                        self.friend_status
                            .get(node_id)
                            .map(|status| status.availability)
                    });
                    let call_enabled = parsed
                        .as_ref()
                        .is_ok_and(|node_id| friend_call_enabled(self.calls.get(node_id)));
                    let call_in_progress = parsed.is_ok() && !call_enabled;

                    Frame::new()
                        .fill(chat_surface(&pal))
                        .stroke(Stroke::new(1.0_f32, chat_hairline(&pal)))
                        .corner_radius(CornerRadius::same(CHROME_INNER_RADIUS))
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.set_min_height(40.0);
                                ui.spacing_mut().item_spacing.x = 10.0;
                                circle_avatar(ui, &pal, &initial, 32.0);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    ui.label(
                                        RichText::new(display_name)
                                            .color(pal.text)
                                            .size(ui_font_size(13.0)),
                                    );
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 5.0;
                                        ui.label(
                                            RichText::new(if parsed.is_err() {
                                                "invalid id".to_owned()
                                            } else {
                                                short_id
                                            })
                                            .monospace()
                                            .color(if parsed.is_err() { pal.err } else { pal.dim })
                                            .size(ui_font_size(10.5)),
                                        );
                                        if let Some(availability) = availability {
                                            let (label, color) = match availability {
                                                Availability::Online => ("Online", pal.ok),
                                                Availability::Offline => ("Offline", pal.dim2),
                                            };
                                            ui.label(
                                                RichText::new(format!("• {label}"))
                                                    .color(color)
                                                    .size(ui_font_size(10.5)),
                                            );
                                        }
                                    });
                                });

                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;

                                    let menu_response = ui
                                        .menu_button(
                                            RichText::new(char::from(Icon::EllipsisVertical))
                                                .font(lucide(16.0))
                                                .color(pal.text2),
                                            |ui| {
                                                ui.spacing_mut().item_spacing.y = 2.0;
                                                if menu_item_button(
                                                    ui,
                                                    &pal,
                                                    Icon::Copy,
                                                    "Copy node ID",
                                                    false,
                                                )
                                                .clicked()
                                                {
                                                    copy_id = Some(friend.node_id.clone());
                                                    ui.close();
                                                }
                                                if menu_item_button(
                                                    ui,
                                                    &pal,
                                                    Icon::UserMinus,
                                                    "Remove",
                                                    true,
                                                )
                                                .clicked()
                                                {
                                                    remove_idx = Some(idx);
                                                    ui.close();
                                                }
                                            },
                                        )
                                        .response;
                                    menu_response.on_hover_text("More actions");

                                    let call_response = ui.add_enabled_ui(call_enabled, |ui| {
                                        if action_button(ui, &pal, "Call", ButtonTone::Primary)
                                            .clicked()
                                        {
                                            if let Ok(id) = parsed {
                                                call = Some(id);
                                            }
                                        }
                                    });
                                    if call_in_progress {
                                        call_response.response.on_hover_text(
                                            "A call with this friend is already in progress",
                                        );
                                    }
                                });
                            });
                        });
                    ui.add_space(6.0);
                }

                if let Some(id) = call {
                    self.play_sound(Sound::Button2);
                    self.cmd(Command::Call { node_id: id });
                }
                if let Some(idx) = remove_idx {
                    if let Ok(node_id) = NodeId::from_str(self.friends[idx].node_id.trim()) {
                        self.friend_status.remove(&node_id);
                    }
                    self.friends.remove(idx);
                    save_friends(&self.friends);
                    self.sync_friends_with_worker();
                }
                if let Some(node_id) = copy_id {
                    copy_to_clipboard(&node_id);
                }
            });
    }

    fn ui_stream_panel(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        let pal = Palette::for_theme(self.theme);
        let immersive = self.stream_view_mode != StreamViewMode::Normal;
        let streams = self.active_stream_sources();
        let has_stream = !streams.is_empty();

        if immersive {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if has_stream {
                    ui.label(
                        RichText::new(if streams.len() == 1 {
                            "1 stream".to_owned()
                        } else {
                            format!("{} streams", streams.len())
                        })
                        .color(pal.dim)
                        .size(ui_font_size(13.0)),
                    );
                    compact_v_sep(ui, pal.line);
                }
                self.ui_stream_toolbar(ui, ctx, has_stream, true);
            });
            ui.add_space(4.0);
        } else {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    RichText::new("STAGE")
                        .family(kh_family())
                        .color(pal.text)
                        .size(16.0),
                );
                let stage_detail = if has_stream {
                    Some(if streams.len() == 1 {
                        "1 stream".to_string()
                    } else {
                        format!("{} streams", streams.len())
                    })
                } else if self.sharing_active {
                    Some("starting share…".to_string())
                } else {
                    None
                };
                if let Some(detail) = stage_detail {
                    ui.label(
                        RichText::new(detail)
                            .color(pal.dim)
                            .size(ui_font_size(13.0)),
                    );
                }
                if has_stream {
                    compact_v_sep(ui, pal.line);
                }
                self.ui_stream_toolbar(ui, ctx, has_stream, false);
            });
            ui.add_space(4.0);
        }

        let available = ui.available_size();
        let (area, _) = ui.allocate_exact_size(available, egui::Sense::hover());
        if !immersive {
            ui.painter()
                .rect_filled(area, CornerRadius::same(10), pal.bg);
        }

        if !has_stream {
            self.ui_empty_stream_state(ui, &pal, area, immersive);
            return;
        }

        if let Some(focused) = self
            .focused_stream
            .filter(|source| streams.contains(source))
        {
            let tile_rect = aspect_fit_rect(area, self.stream_aspect_ratio(focused));
            ui.scope_builder(egui::UiBuilder::new().max_rect(tile_rect), |ui| {
                self.ui_stream_tile(
                    ui,
                    &pal,
                    focused,
                    true,
                    immersive,
                    #[cfg(windows)]
                    parent_hwnd,
                );
            });
            return;
        }

        let count = streams.len();
        let (cols, rows) = stream_grid_dims(count, area.size());
        let gap = STREAM_GRID_GAP;
        let total_gap_x = gap * (cols.saturating_sub(1)) as f32;
        let total_gap_y = gap * (rows.saturating_sub(1)) as f32;
        let cell_w = (area.width() - total_gap_x) / cols as f32;
        let cell_h = (area.height() - total_gap_y) / rows as f32;

        for (index, source) in streams.iter().enumerate() {
            let col = index % cols;
            let row = index / cols;
            let cell_rect = egui::Rect::from_min_size(
                egui::pos2(
                    area.min.x + col as f32 * (cell_w + gap),
                    area.min.y + row as f32 * (cell_h + gap),
                ),
                Vec2::new(cell_w, cell_h),
            );
            let tile_rect = aspect_fit_rect(cell_rect, self.stream_aspect_ratio(*source));
            ui.scope_builder(egui::UiBuilder::new().max_rect(tile_rect), |ui| {
                self.ui_stream_tile(
                    ui,
                    &pal,
                    *source,
                    false,
                    immersive,
                    #[cfg(windows)]
                    parent_hwnd,
                );
            });
        }
    }

    fn ui_empty_stream_state(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        area: egui::Rect,
        immersive: bool,
    ) {
        let corner_radius = if immersive {
            CornerRadius::ZERO
        } else {
            CornerRadius::same(10)
        };
        ui.painter().rect_filled(area, corner_radius, pal.bg);
        if !immersive {
            ui.painter().rect_stroke(
                area,
                corner_radius,
                Stroke::new(1.0_f32, chat_hairline(pal)),
                egui::StrokeKind::Inside,
            );
        }

        let roomy = area.height() >= 150.0;
        let has_capture_error = self.capture_error.is_some();
        let stopped_streams = self.stopped_stream_nodes();
        let showing_stopped =
            !has_capture_error && !self.sharing_active && !stopped_streams.is_empty();
        let stopped_title = if stopped_streams.len() == 1 {
            format!(
                "{}'s stream is paused",
                self.peer_display_name(stopped_streams[0])
            )
        } else {
            format!("{} streams are paused", stopped_streams.len())
        };
        let block_height = if roomy {
            if has_capture_error {
                190.0
            } else {
                142.0
            }
        } else {
            70.0
        };
        let block_size = Vec2::new((area.width() - 24.0).min(440.0), block_height);
        let block_rect = egui::Rect::from_center_size(area.center(), block_size);

        ui.scope_builder(egui::UiBuilder::new().max_rect(block_rect), |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                let (icon_rect, _) =
                    ui.allocate_exact_size(Vec2::splat(40.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(icon_rect.center(), 20.0, pal.panel2);
                ui.painter().circle_stroke(
                    icon_rect.center(),
                    20.0,
                    Stroke::new(1.0_f32, pal.line_br),
                );
                ui.painter().text(
                    icon_rect.center(),
                    Align2::CENTER_CENTER,
                    if has_capture_error {
                        ph::WARNING
                    } else if self.sharing_active {
                        ph::SPINNER_GAP
                    } else if showing_stopped {
                        ph::PLAY
                    } else {
                        ph::MONITOR
                    },
                    sans(17.0),
                    if self.sharing_active {
                        pal.accent
                    } else if has_capture_error {
                        pal.err
                    } else {
                        pal.dim
                    },
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(if has_capture_error {
                        "Screen sharing needs access"
                    } else if self.sharing_active {
                        "Starting your screen share"
                    } else if showing_stopped {
                        &stopped_title
                    } else {
                        "Nothing is being shared"
                    })
                    .color(pal.text2)
                    .size(ui_font_size(14.0)),
                );

                if roomy {
                    let detail = self.capture_error.as_deref().unwrap_or({
                        if self.sharing_active {
                            "Preparing the first frame. This usually takes a moment."
                        } else if showing_stopped {
                            "Video data is paused. Resume whenever you want to watch again."
                        } else {
                            "Shared screens and incoming video will appear here."
                        }
                    });
                    ui.label(
                        RichText::new(detail)
                            .color(pal.dim)
                            .size(ui_font_size(11.5)),
                    );
                    ui.add_space(6.0);
                    #[cfg(target_os = "macos")]
                    if has_capture_error
                        && action_button(
                            ui,
                            pal,
                            "Open Screen Recording settings",
                            ButtonTone::Primary,
                        )
                        .clicked()
                    {
                        open_screen_recording_settings();
                    }
                    let (label, tone) = if showing_stopped {
                        ("Resume watching", ButtonTone::Primary)
                    } else if self.sharing_active {
                        ("Stop sharing", ButtonTone::Secondary)
                    } else {
                        ("Share your screen", ButtonTone::Primary)
                    };
                    if action_button(ui, pal, label, tone).clicked() {
                        if showing_stopped {
                            for node_id in stopped_streams.iter().copied() {
                                self.resume_watching(node_id);
                            }
                        } else {
                            // Prefer the capture-picker path from origin/main over a
                            // bare ToggleSharing toggle from the feature branch.
                            self.toggle_sharing_from_ui();
                        }
                    }
                }
            });
        });
    }

    fn ui_stream_tile(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        source: StreamSource,
        expanded: bool,
        immersive: bool,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        let available = ui.available_size();
        let (tile_rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
        let corner_radius = if immersive {
            CornerRadius::ZERO
        } else {
            CornerRadius::same(12)
        };
        ui.painter()
            .rect_filled(tile_rect, corner_radius, Color32::BLACK);
        let show_overlays = tile_rect.width() >= 90.0 && tile_rect.height() >= 44.0;
        let label = self.stream_label(source);
        let content_rect = tile_rect;
        let (width, height) = match source {
            StreamSource::Local => self
                .preview
                .as_ref()
                .map(|preview| (preview.width, preview.height)),
            StreamSource::Remote(node_id) => self
                .video_frames
                .get(&node_id)
                .map(|frame| (frame.width, frame.height)),
        }
        .unwrap_or_default();
        let quality = match source {
            StreamSource::Local => format_stream_quality(
                self.video_config.resolution.height(),
                self.video_config.framerate,
            ),
            StreamSource::Remote(node_id) => self
                .video_frames
                .get(&node_id)
                .and_then(|frame| format_stream_quality(frame.source_height, frame.source_fps)),
        };
        if width == 0 || height == 0 {
            return;
        }
        let image_rect = content_rect;

        let mut texture_id = None;
        #[cfg(windows)]
        let mut native_presented = false;
        #[cfg(not(windows))]
        let native_presented = false;
        match source {
            StreamSource::Local => {
                if let Some(preview) = &mut self.preview {
                    sync_rgba_texture(
                        ui,
                        "preview-stage",
                        width,
                        height,
                        &preview.data,
                        preview.generation,
                        &mut preview.uploaded_generation,
                        &mut preview.texture,
                        &mut preview.upload_stats,
                    );
                    texture_id = preview.texture.as_ref().map(|texture| texture.id());
                }
            }
            StreamSource::Remote(node_id) => {
                let Some(frame) = self.video_frames.get_mut(&node_id) else {
                    return;
                };
                #[cfg(windows)]
                let use_native = !show_overlays
                    && self.configured
                    && !self.show_settings
                    && !self.show_update_prompt;
                #[cfg(windows)]
                if !use_native {
                    let rgba = match &frame.data {
                        DecodedFrameData::D3d11(gpu_frame) => Some(gpu_frame.to_rgba()),
                        DecodedFrameData::Rgba(_) => None,
                    };
                    if let Some(presenter) = &mut frame.presenter {
                        presenter.hide();
                    }
                    if let Some(rgba) = rgba {
                        match rgba {
                            Ok(rgba) => {
                                frame.data = DecodedFrameData::Rgba(Arc::new(rgba));
                                frame.texture = None;
                                frame.uploaded_generation = 0;
                            }
                            Err(error) => {
                                warn!("video overlay composition fallback failed: {error:#}");
                            }
                        }
                    }
                } else if matches!(&frame.data, DecodedFrameData::D3d11(_)) {
                    let rect = physical_video_rect(image_rect, ui.ctx().pixels_per_point());
                    match present_native_video(frame, parent_hwnd, rect) {
                        Ok(presented) => native_presented = presented,
                        Err(error) => warn!("native video fallback failed: {error:#}"),
                    }
                }
                match &frame.data {
                    DecodedFrameData::Rgba(data) => {
                        sync_rgba_texture(
                            ui,
                            &format!("video-{node_id}"),
                            width,
                            height,
                            data,
                            frame.generation,
                            &mut frame.uploaded_generation,
                            &mut frame.texture,
                            &mut frame.upload_stats,
                        );
                        texture_id = frame.texture.as_ref().map(|texture| texture.id());
                    }
                    #[cfg(windows)]
                    DecodedFrameData::D3d11(_) => {}
                }
            }
        }

        if let Some(texture_id) = texture_id {
            ui.put(
                image_rect,
                egui::Image::new((texture_id, image_rect.size())).corner_radius(corner_radius),
            );
        } else if !native_presented {
            ui.painter().text(
                content_rect.center(),
                Align2::CENTER_CENTER,
                "Waiting for video...",
                sans(13.0),
                pal.dim,
            );
        }

        if !immersive {
            let border_color = if expanded {
                pal.accent.gamma_multiply(0.72)
            } else {
                pal.line_br
            };
            ui.painter().rect_stroke(
                tile_rect,
                corner_radius,
                Stroke::new(1.25_f32, border_color),
                egui::StrokeKind::Inside,
            );
        }

        if show_overlays {
            let overlay_fill =
                Color32::from_rgba_unmultiplied(pal.bg.r(), pal.bg.g(), pal.bg.b(), 190);
            let hovered = ui.rect_contains_pointer(tile_rect);
            let left_overlay_width = paint_stream_info_badges(
                ui,
                pal,
                tile_rect,
                &label,
                hovered.then_some(quality.as_deref()).flatten(),
                overlay_fill,
            );

            if hovered {
                // Compact overlay control group (focus + optional stop).
                const BTN: f32 = 22.0;
                const GAP: f32 = 2.0;
                const PAD: f32 = 3.0;
                const MARGIN: f32 = 8.0;

                let stop_node = match source {
                    StreamSource::Remote(node_id) => Some(node_id),
                    StreamSource::Local => None,
                };
                let local_audio = matches!(source, StreamSource::Local);
                let button_count = 1 + usize::from(stop_node.is_some()) + usize::from(local_audio);
                let group_size = Vec2::new(
                    PAD * 2.0
                        + BTN * button_count as f32
                        + GAP * button_count.saturating_sub(1) as f32,
                    PAD * 2.0 + BTN,
                );
                let group_rect = egui::Rect::from_min_size(
                    tile_rect.right_top() + egui::vec2(-group_size.x - MARGIN, MARGIN),
                    group_size,
                );

                ui.painter().rect(
                    group_rect,
                    CornerRadius::same(8),
                    pal.panel,
                    Stroke::new(1.0_f32, pal.line_br),
                    egui::StrokeKind::Inside,
                );

                let mut origin = group_rect.min + egui::vec2(PAD, PAD);
                let focus_icon = if expanded {
                    Icon::LayoutGrid
                } else {
                    Icon::Maximize2
                };
                let focus_tooltip = if expanded {
                    "Show all streams"
                } else {
                    "Focus this stream"
                };
                let focus_rect = egui::Rect::from_min_size(origin, Vec2::splat(BTN));
                if stream_tile_group_icon_button(
                    ui,
                    pal,
                    focus_rect,
                    ui.id().with(("stream_tile_focus", source)),
                    focus_icon,
                    focus_tooltip,
                )
                .clicked()
                {
                    self.focused_stream = if expanded { None } else { Some(source) };
                }

                if local_audio {
                    origin.x += BTN + GAP;
                    let audio_rect = egui::Rect::from_min_size(origin, Vec2::splat(BTN));
                    let audio_on = self.system_audio_active;
                    if stream_tile_group_icon_button(
                        ui,
                        pal,
                        audio_rect,
                        ui.id().with(("stream_tile_audio", source)),
                        if audio_on {
                            Icon::Volume2
                        } else {
                            Icon::VolumeX
                        },
                        if audio_on {
                            "Stop sharing computer sound"
                        } else {
                            "Also share computer sound"
                        },
                    )
                    .clicked()
                    {
                        self.set_share_system_audio_from_ui(!audio_on);
                    }
                }
                if let Some(node_id) = stop_node {
                    origin.x += BTN + GAP;
                    let stop_rect = egui::Rect::from_min_size(origin, Vec2::splat(BTN));
                    if stream_tile_group_icon_button(
                        ui,
                        pal,
                        stop_rect,
                        ui.id().with(("stream_tile_stop", source)),
                        Icon::X,
                        "Stop watching this screen share",
                    )
                    .clicked()
                    {
                        self.stop_watching(node_id);
                    }
                }

                if let StreamSource::Remote(node_id) = source {
                    if let Some(volume) = self.stream_volumes.get(&node_id).cloned() {
                        stream_volume_badge(
                            ui,
                            pal,
                            overlay_fill,
                            &volume,
                            ui.id().with(("stream_volume", node_id)),
                            tile_rect,
                            left_overlay_width,
                        );
                    }
                }
            }
        }
    }

    fn ui_stream_toolbar(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        has_stream: bool,
        compact: bool,
    ) {
        let pal = Palette::for_theme(self.theme);
        if has_stream {
            let fill_selected = self.stream_view_mode == StreamViewMode::FillWindow;
            if toolbar_ghost_icon_button(ui, &pal, Icon::Expand, fill_selected)
                .on_hover_text("Expand stream to fill the client window")
                .clicked()
            {
                self.set_stream_view_mode(
                    ctx,
                    if fill_selected {
                        StreamViewMode::Normal
                    } else {
                        StreamViewMode::FillWindow
                    },
                );
            }

            let fs_selected = self.stream_view_mode == StreamViewMode::Fullscreen;
            if toolbar_ghost_icon_button(ui, &pal, Icon::Fullscreen, fs_selected)
                .on_hover_text("Enter native fullscreen (Esc to exit)")
                .clicked()
            {
                self.set_stream_view_mode(
                    ctx,
                    if fs_selected {
                        StreamViewMode::Normal
                    } else {
                        StreamViewMode::Fullscreen
                    },
                );
            }
        }

        if compact
            && self.stream_view_mode != StreamViewMode::Normal
            && toolbar_ghost_icon_button(ui, &pal, Icon::Minimize2, false)
                .on_hover_text("Return to normal layout (Esc)")
                .clicked()
        {
            self.set_stream_view_mode(ctx, StreamViewMode::Normal);
        }
    }
}

const PARTICIPANT_AVATAR_SIZE: f32 = 26.0;

const PARTICIPANT_ACTION_HEIGHT: f32 = 26.0;

const CHIP_IDENTITY_GAP: f32 = 8.0;

const CHIP_ACTION_GAP: f32 = 12.0;

const CHIP_NAME_OPTICAL_Y: f32 = 1.5;

const CHIP_INNER_MARGIN: egui::Margin = egui::Margin {
    left: 10,
    right: 7,
    top: 0,
    bottom: 0,
};

fn chip_name_label(ui: &mut Ui, text: &str, color: Color32) {
    chip_optical_label(ui, text, color, 12.0);
}

fn chip_status_label(ui: &mut Ui, text: &str, color: Color32) {
    chip_optical_label(ui, text, color, 11.0);
}

fn chip_status_icon(ui: &mut Ui, color: Color32, icon: Icon, tooltip: &str) {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(16.0), egui::Sense::hover());
    response.on_hover_text(tooltip);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(icon),
        lucide(14.0),
        color,
    );
}

fn chip_optical_label(ui: &mut Ui, text: &str, color: Color32, size: f32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), sans(size), color);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(galley.size().x, PARTICIPANT_CHIP_HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter().galley(
        egui::pos2(
            rect.left(),
            rect.center().y - galley.size().y * 0.5 + CHIP_NAME_OPTICAL_Y,
        ),
        galley,
        color,
    );
}

fn chip_icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    selected: bool,
    tooltip: &str,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::splat(PARTICIPANT_ACTION_HEIGHT), egui::Sense::click());
    let response = response.on_hover_text(tooltip);
    let hot = response.hovered() || response.is_pointer_button_down_on() || response.has_focus();
    let (fill, stroke, icon_color) = if selected {
        (
            pal.accent_dim,
            Stroke::new(1.0_f32, pal.accent.gamma_multiply(0.8)),
            pal.accent,
        )
    } else {
        button_tone_style(pal, ButtonTone::Secondary, hot)
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(CHROME_INNER_RADIUS),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(icon),
        lucide(13.0),
        if selected {
            icon_color
        } else if hot {
            pal.text
        } else {
            pal.text2
        },
    );
    response
}

fn load_audio_level(level: &AudioLevelHandle) -> f32 {
    f32::from_bits(level.load(Ordering::Relaxed)).clamp(0.0, 1.0)
}

fn voice_level_meter(ui: &mut Ui, pal: &Palette, level: f32) -> egui::Response {
    const BAR_COUNT: usize = 5;
    const GAP: f32 = 2.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(25.0, 14.0), egui::Sense::hover());
    let bar_width = (rect.width() - GAP * (BAR_COUNT - 1) as f32) / BAR_COUNT as f32;

    for index in 0..BAR_COUNT {
        let height = 4.0 + index as f32 * 2.0;
        let min = egui::pos2(
            rect.left() + index as f32 * (bar_width + GAP),
            rect.bottom() - height,
        );
        let bar = egui::Rect::from_min_size(min, egui::vec2(bar_width, height));
        let threshold = (index + 1) as f32 / BAR_COUNT as f32;
        let color = if level >= threshold {
            pal.ok
        } else {
            chat_hairline(pal)
        };
        ui.painter().rect_filled(bar, 1.0, color);
    }

    response
}

fn compact_chip_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    tone: ButtonTone,
) -> egui::Response {
    let font = sans(11.0);
    let measure = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE);
    let size = Vec2::new(
        (measure.size().x + 16.0).max(PARTICIPANT_ACTION_HEIGHT),
        PARTICIPANT_ACTION_HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let hot = response.hovered() || response.is_pointer_button_down_on() || response.has_focus();
    let (fill, stroke, text_color) = button_tone_style(pal, tone, hot);
    ui.painter().rect(
        rect,
        CornerRadius::same(CHROME_INNER_RADIUS),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, text_color);
    ui.painter().galley(
        Align2::CENTER_CENTER
            .anchor_size(
                rect.center() + egui::vec2(0.0, CHIP_NAME_OPTICAL_Y),
                galley.size(),
            )
            .min,
        galley,
        text_color,
    );
    response
}

fn stream_grid_dims(count: usize, available: Vec2) -> (usize, usize) {
    if count <= 1 || available.x <= 0.0 || available.y <= 0.0 {
        return (1, 1);
    }

    let mut best = (1, count);
    let mut best_score = 0.0;
    for cols in 1..=count {
        let rows = count.div_ceil(cols);
        let width = (available.x - STREAM_GRID_GAP * (cols.saturating_sub(1)) as f32) / cols as f32;
        let height =
            (available.y - STREAM_GRID_GAP * (rows.saturating_sub(1)) as f32) / rows as f32;
        if width <= 0.0 || height <= 0.0 {
            continue;
        }

        let displayed = video_display_size(Vec2::new(width, height), 16.0 / 9.0, false);
        let occupied_cells = count as f32 / (cols * rows) as f32;
        let score = displayed.x * displayed.y * count as f32 * (0.9 + occupied_cells * 0.1);
        if score > best_score {
            best_score = score;
            best = (cols, rows);
        }
    }
    best
}

#[cfg(windows)]
pub(super) fn native_parent_hwnd(
    frame: &eframe::Frame,
) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = frame.window_handle().ok()?.as_raw();
    let RawWindowHandle::Win32(handle) = handle else {
        return None;
    };
    Some(windows::Win32::Foundation::HWND(
        handle.hwnd.get() as *mut std::ffi::c_void
    ))
}

#[cfg(windows)]
fn physical_video_rect(
    rect: egui::Rect,
    pixels_per_point: f32,
) -> crate::win_video_presenter::PhysicalVideoRect {
    let min_x = (rect.min.x * pixels_per_point).round() as i32;
    let min_y = (rect.min.y * pixels_per_point).round() as i32;
    let max_x = (rect.max.x * pixels_per_point).round() as i32;
    let max_y = (rect.max.y * pixels_per_point).round() as i32;
    crate::win_video_presenter::PhysicalVideoRect {
        x: min_x,
        y: min_y,
        width: max_x.saturating_sub(min_x).max(1) as u32,
        height: max_y.saturating_sub(min_y).max(1) as u32,
    }
}

#[cfg(windows)]
fn present_native_video(
    frame: &mut VideoFrameState,
    parent: Option<windows::Win32::Foundation::HWND>,
    rect: crate::win_video_presenter::PhysicalVideoRect,
) -> Result<bool> {
    let was_disabled = frame.native_present_failed;
    let present_result: Result<()> = (|| {
        if was_disabled {
            anyhow::bail!("native presentation disabled after an earlier initialization failure");
        }
        let gpu_frame = match &frame.data {
            DecodedFrameData::D3d11(frame) => frame,
            DecodedFrameData::Rgba(_) => return Ok(()),
        };
        let parent = parent.context("eframe did not expose a Win32 parent handle")?;
        if frame
            .presenter
            .as_ref()
            .is_some_and(|presenter| !presenter.uses_device(gpu_frame))
        {
            frame.presenter = None;
        }
        if frame.presenter.is_none() {
            frame.presenter = Some(crate::win_video_presenter::NativeVideoPresenter::new(
                parent, gpu_frame, rect,
            )?);
        }
        frame
            .presenter
            .as_mut()
            .context("native presenter was not created")?
            .present(gpu_frame, rect, frame.generation)
    })();

    match present_result {
        Ok(()) => Ok(true),
        Err(error) => {
            if !was_disabled {
                warn!("native D3D11 video presentation failed; using egui fallback: {error:#}");
            }
            frame.native_present_failed = true;
            if let Some(presenter) = &mut frame.presenter {
                presenter.hide();
            }
            let rgba = match &frame.data {
                DecodedFrameData::D3d11(gpu_frame) => gpu_frame.to_rgba()?,
                DecodedFrameData::Rgba(_) => return Ok(false),
            };
            frame.data = DecodedFrameData::Rgba(Arc::new(rgba));
            frame.texture = None;
            frame.uploaded_generation = 0;
            Ok(false)
        }
    }
}

fn capture_picker_layout(viewport: Vec2) -> (f32, f32, bool) {
    let width = (viewport.x - 56.0).clamp(280.0, 820.0);
    let body_height = (viewport.y - 250.0).clamp(160.0, 360.0);
    (width, body_height, width >= 560.0)
}

fn stream_volume_badge(
    ui: &mut Ui,
    pal: &Palette,
    fill: Color32,
    volume: &VolumeHandle,
    id: egui::Id,
    tile_rect: egui::Rect,
    occupied_left: f32,
) {
    const HEIGHT: f32 = 26.0;
    const MARGIN: f32 = 8.0;
    const PAD: f32 = 8.0;
    const ICON: f32 = 13.0;
    const GAP: f32 = 7.0;
    const TRACK_W: f32 = 68.0;
    const TRACK_H: f32 = 3.0;
    const KNOB: f32 = 5.0;

    let width = PAD + ICON + GAP + TRACK_W + PAD;
    if tile_rect.width() < width + MARGIN * 2.0 + occupied_left {
        return;
    }

    let rect = egui::Rect::from_min_size(
        tile_rect.right_bottom() + egui::vec2(-width - MARGIN, -HEIGHT - MARGIN),
        Vec2::new(width, HEIGHT),
    );
    let response = ui
        .interact(rect, id, egui::Sense::click_and_drag())
        .on_hover_text("Stream volume")
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);

    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);

    let muted = f32::from_bits(volume.load(Ordering::Relaxed)) <= 0.001;
    ui.painter().text(
        egui::pos2(rect.left() + PAD + ICON * 0.5, rect.center().y),
        Align2::CENTER_CENTER,
        char::from(if muted { Icon::VolumeX } else { Icon::Volume2 }),
        lucide(ICON),
        pal.text2,
    );

    let track = egui::Rect::from_center_size(
        egui::pos2(rect.right() - PAD - TRACK_W * 0.5, rect.center().y),
        Vec2::new(TRACK_W, TRACK_H),
    );
    let mut value = f32::from_bits(volume.load(Ordering::Relaxed)).clamp(0.0, 2.0);
    if response.dragged() || response.clicked() {
        if let Some(pointer) = response.interact_pointer_pos() {
            value = ((pointer.x - track.left()) / track.width()).clamp(0.0, 1.0) * 2.0;
            volume.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    paint_volume_track(
        ui,
        pal,
        track,
        value / 2.0,
        value,
        VolumeKnob {
            radius: KNOB,
            fill: Color32::WHITE,
            border: None,
        },
        response.hovered() || response.dragged(),
    );
}

fn system_audio_share_row(ui: &mut Ui, pal: &Palette, enabled: &mut bool) {
    let response = Frame::new()
        .fill(if *enabled {
            pal.accent_dim
        } else {
            chat_surface(pal)
        })
        .corner_radius(CornerRadius::same(CHROME_INNER_RADIUS))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .stroke(Stroke::new(
            1.0_f32,
            if *enabled {
                pal.accent.gamma_multiply(0.7)
            } else {
                chat_hairline(pal)
            },
        ))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let icon_rect = ui
                    .allocate_exact_size(Vec2::splat(28.0), egui::Sense::hover())
                    .0;
                ui.painter()
                    .rect_filled(icon_rect, CornerRadius::same(8), pal.panel2);
                ui.painter().text(
                    icon_rect.center(),
                    Align2::CENTER_CENTER,
                    char::from(if *enabled {
                        Icon::Volume2
                    } else {
                        Icon::VolumeX
                    }),
                    lucide(14.0),
                    if *enabled { pal.accent } else { pal.dim },
                );
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Also share system audio")
                            .color(pal.text)
                            .size(ui_font_size(12.5)),
                    );
                    ui.label(
                        RichText::new("People in this call will hear this computer.")
                            .color(pal.dim)
                            .size(ui_font_size(11.0)),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (check_rect, _) =
                        ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
                    ui.painter().rect(
                        check_rect,
                        CornerRadius::same(5),
                        if *enabled { pal.accent } else { pal.panel },
                        Stroke::new(1.0_f32, if *enabled { pal.accent } else { pal.line_br }),
                        egui::StrokeKind::Inside,
                    );
                    if *enabled {
                        ui.painter().text(
                            check_rect.center(),
                            Align2::CENTER_CENTER,
                            char::from(Icon::Check),
                            lucide(12.0),
                            pal.bg,
                        );
                    }
                });
            });
        })
        .response
        .interact(egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Share the sound this computer is playing, not just your microphone");
    if response.clicked() {
        *enabled = !*enabled;
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_target_column(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    kind: crate::screen_capture::CaptureTargetKind,
    icon: Icon,
    targets: &[crate::screen_capture::CaptureTarget],
    selected: &mut Option<usize>,
    height: f32,
) {
    let count = targets.iter().filter(|target| target.kind == kind).count();
    Frame::new()
        .fill(chat_surface(pal))
        .corner_radius(CornerRadius::same(CHROME_INNER_RADIUS))
        .inner_margin(10.0)
        .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height(height);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(char::from(icon))
                        .font(lucide(15.0))
                        .color(pal.accent),
                );
                ui.label(
                    RichText::new(title.to_uppercase())
                        .family(kh_family())
                        .color(pal.text2)
                        .size(11.0),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(count.to_string())
                            .color(pal.dim)
                            .size(ui_font_size(11.0)),
                    );
                });
            });
            ui.add_space(8.0);

            egui::ScrollArea::vertical()
                .id_salt(("capture-targets", title))
                .max_height((height - 38.0).max(48.0))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    let mut any = false;
                    for (index, target) in targets.iter().enumerate() {
                        if target.kind != kind {
                            continue;
                        }
                        any = true;
                        if capture_target_row(ui, pal, target, *selected == Some(index)).clicked() {
                            *selected = Some(index);
                        }
                        ui.add_space(6.0);
                    }
                    if !any {
                        Frame::new()
                            .fill(pal.panel)
                            .corner_radius(CornerRadius::same(9))
                            .inner_margin(14.0)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(
                                    RichText::new(format!("No {} available", title.to_lowercase()))
                                        .color(pal.dim)
                                        .size(ui_font_size(12.0)),
                                );
                            });
                    }
                });
        });
}

fn capture_target_row(
    ui: &mut Ui,
    pal: &Palette,
    target: &crate::screen_capture::CaptureTarget,
    selected: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 62.0), egui::Sense::click());
    let hovered = response.hovered();
    let fill = if selected {
        pal.accent_dim
    } else if hovered {
        pal.panel2
    } else {
        pal.panel
    };
    let stroke = if selected {
        Stroke::new(1.25_f32, pal.accent.gamma_multiply(0.9))
    } else {
        Stroke::new(1.0_f32, chat_hairline(pal))
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(9),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 25.0, rect.center().y),
        Vec2::splat(32.0),
    );
    ui.painter().rect_filled(
        icon_rect,
        CornerRadius::same(8),
        if selected { pal.accent_dim } else { pal.panel2 },
    );
    ui.painter().text(
        icon_rect.center(),
        Align2::CENTER_CENTER,
        char::from(match target.kind {
            crate::screen_capture::CaptureTargetKind::Display => Icon::Monitor,
            crate::screen_capture::CaptureTargetKind::Window => Icon::AppWindow,
        }),
        lucide(15.0),
        if selected { pal.accent } else { pal.dim },
    );

    let text_left = rect.left() + 50.0;
    let available_chars = ((rect.right() - text_left - 12.0) / 7.0) as usize;
    ui.painter().text(
        egui::pos2(text_left, rect.top() + 15.0),
        Align2::LEFT_TOP,
        ellipsize(&target.title, available_chars.clamp(12, 48)),
        sans(12.5),
        pal.text,
    );
    let primary = if target.is_primary {
        "  ·  Primary"
    } else {
        ""
    };
    ui.painter().text(
        egui::pos2(text_left, rect.top() + 36.0),
        Align2::LEFT_TOP,
        format!("{} × {}{}", target.width, target.height, primary),
        sans(10.5),
        pal.dim,
    );

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[allow(clippy::too_many_arguments)]
fn sync_rgba_texture(
    ui: &Ui,
    id: &str,
    width: u32,
    height: u32,
    data: &Arc<Vec<u8>>,
    generation: u64,
    uploaded_generation: &mut u64,
    texture: &mut Option<egui::TextureHandle>,
    stats: &mut TextureUploadStats,
) {
    if *uploaded_generation == generation || width == 0 || height == 0 || data.is_empty() {
        return;
    }
    let started = std::time::Instant::now();
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], data);
    let options = egui::TextureOptions::LINEAR;
    if let Some(tex) = texture {
        tex.set(color_image, options);
    } else {
        *texture = Some(ui.ctx().load_texture(id.to_string(), color_image, options));
    }
    *uploaded_generation = generation;
    stats.frames += 1;
    stats
        .samples_ms
        .push(started.elapsed().as_secs_f64() * 1000.0);
    if stats.last_log.elapsed() >= Duration::from_secs(5) {
        let elapsed = stats.last_log.elapsed().as_secs_f64();
        let avg = stats.samples_ms.iter().sum::<f64>() / stats.samples_ms.len() as f64;
        stats.samples_ms.sort_by(f64::total_cmp);
        let p95 = stats.samples_ms[((stats.samples_ms.len() - 1) as f64 * 0.95).round() as usize];
        info!(
            "texture upload {id}: {:.1} fps, {:.1} ms avg / {:.1} ms p95 ({}x{})",
            stats.frames as f64 / elapsed,
            avg,
            p95,
            width,
            height
        );
        stats.samples_ms.clear();
        stats.frames = 0;
        stats.last_log = std::time::Instant::now();
    }
}

/// Compact icon control for stream-tile overlay button groups.
///
/// Uses a fixed rect (overlay, not layout-allocated) and paints hover fill
/// inside the shared group chrome rather than drawing its own border.
fn stream_tile_group_icon_button(
    ui: &mut Ui,
    pal: &Palette,
    rect: egui::Rect,
    id: egui::Id,
    icon: Icon,
    tooltip: &str,
) -> egui::Response {
    let response = ui
        .interact(rect, id, egui::Sense::click())
        .on_hover_text(tooltip);
    if response.hovered() || response.has_focus() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), pal.panel2);
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(icon),
        lucide(13.0),
        if response.hovered() || response.has_focus() {
            pal.text
        } else {
            pal.text2
        },
    );
    response
}

const STREAM_OVERLAY_BADGE_HEIGHT: f32 = 26.0;

const STREAM_OVERLAY_BADGE_PAD_X: f32 = 10.0;

const STREAM_OVERLAY_BADGE_GAP: f32 = 6.0;

const STREAM_OVERLAY_MARGIN: f32 = 8.0;

fn format_stream_quality(height: u32, fps: u32) -> Option<String> {
    (height > 0 && fps > 0).then(|| format!("{height}p@{fps}"))
}

fn stream_info_badge_width(text: &str, measure: impl Fn(&str) -> f32) -> f32 {
    measure(text) + STREAM_OVERLAY_BADGE_PAD_X * 2.0
}

/// Name plus an optional quality badge (`1080p@60`) that fit in `available_width`.
///
/// The name is kept intact when possible. The quality badge is dropped if the
/// tile is too narrow. The name is only ellipsized when it does not fit alone.
fn fit_stream_info_badges(
    name: &str,
    quality: Option<&str>,
    available_width: f32,
    measure: impl Fn(&str) -> f32,
) -> Vec<String> {
    let badge_w = |text: &str| stream_info_badge_width(text, &measure);
    let name_width = badge_w(name);
    if let Some(quality) = quality {
        let extras_width = badge_w(quality) + STREAM_OVERLAY_BADGE_GAP;
        if name_width + extras_width <= available_width {
            return vec![name.to_owned(), quality.to_owned()];
        }
    }
    if name_width <= available_width {
        return vec![name.to_owned()];
    }

    let mut label = name.to_owned();
    let mut chars = label.chars().count();
    while chars > 1 && badge_w(&label) > available_width {
        chars -= 1;
        label = ellipsize(name, chars);
    }
    vec![label]
}

fn paint_stream_info_badges(
    ui: &mut Ui,
    pal: &Palette,
    tile_rect: egui::Rect,
    name: &str,
    quality: Option<&str>,
    fill: Color32,
) -> f32 {
    let available = (tile_rect.width() - STREAM_OVERLAY_MARGIN * 2.0).max(0.0);
    let measure = |text: &str| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), sans(12.0), Color32::WHITE)
            .size()
            .x
    };
    let badges = fit_stream_info_badges(name, quality, available, measure);
    if badges.is_empty() {
        return 0.0;
    }

    let mut x = tile_rect.left() + STREAM_OVERLAY_MARGIN;
    let y = tile_rect.bottom() - STREAM_OVERLAY_MARGIN - STREAM_OVERLAY_BADGE_HEIGHT;
    for (index, text) in badges.iter().enumerate() {
        let color = if index == 0 {
            Color32::WHITE
        } else {
            pal.text2
        };
        let galley = ui.painter().layout_no_wrap(text.clone(), sans(12.0), color);
        let badge_size = Vec2::new(
            galley.size().x + STREAM_OVERLAY_BADGE_PAD_X * 2.0,
            STREAM_OVERLAY_BADGE_HEIGHT,
        );
        let badge_rect = egui::Rect::from_min_size(egui::pos2(x, y), badge_size);
        ui.painter()
            .rect_filled(badge_rect, CornerRadius::same(8), fill);
        ui.painter().galley(
            badge_rect.left_center()
                + egui::vec2(STREAM_OVERLAY_BADGE_PAD_X, -galley.size().y * 0.5),
            galley,
            color,
        );
        x = badge_rect.right() + STREAM_OVERLAY_BADGE_GAP;
    }

    x - STREAM_OVERLAY_BADGE_GAP - tile_rect.left()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_grid_tracks_the_stage_shape() {
        assert_eq!(stream_grid_dims(2, Vec2::new(1200.0, 400.0)), (2, 1));
        assert_eq!(stream_grid_dims(2, Vec2::new(400.0, 1200.0)), (1, 2));
        assert_eq!(stream_grid_dims(4, Vec2::new(800.0, 800.0)), (2, 2));
    }

    #[test]
    fn capture_picker_adapts_to_compact_viewports() {
        assert_eq!(
            capture_picker_layout(Vec2::new(1200.0, 900.0)),
            (820.0, 360.0, true)
        );
        assert_eq!(
            capture_picker_layout(Vec2::new(600.0, 500.0)),
            (544.0, 250.0, false)
        );
        assert_eq!(
            capture_picker_layout(Vec2::new(420.0, 360.0)),
            (364.0, 160.0, false)
        );
    }

    #[test]
    fn stream_overlay_formats_quality_as_height_and_fps() {
        assert_eq!(format_stream_quality(1080, 60).as_deref(), Some("1080p@60"));
        assert_eq!(format_stream_quality(720, 30).as_deref(), Some("720p@30"));
        assert_eq!(format_stream_quality(0, 60), None);
        assert_eq!(format_stream_quality(1080, 0), None);
    }

    #[test]
    fn stream_overlay_keeps_quality_beside_the_name() {
        let measure = |text: &str| text.chars().count() as f32 * 8.0;
        assert_eq!(
            fit_stream_info_badges("Ada", Some("1080p@60"), 400.0, measure),
            ["Ada", "1080p@60"]
        );
    }

    #[test]
    fn stream_overlay_drops_quality_when_the_tile_is_narrow() {
        let measure = |text: &str| text.chars().count() as f32 * 8.0;
        assert_eq!(
            fit_stream_info_badges("Long display name", Some("1080p@60"), 180.0, measure),
            ["Long display name"]
        );
        assert_eq!(
            fit_stream_info_badges("Ada", Some("1080p@60"), 80.0, measure),
            ["Ada"]
        );
        assert_eq!(
            fit_stream_info_badges("Long display name", None, 80.0, measure),
            ["Long d…"]
        );
    }
}
