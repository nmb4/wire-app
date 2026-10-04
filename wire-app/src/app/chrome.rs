//! Application frame and mode navigation.

use super::{
    calls_ui::RECORDING_BAND_HEIGHT,
    widgets::{
        chat_hairline, chat_segment_button, chat_surface, participant_bar_height,
        participant_strip_width, CHROME_RADIUS, CHROME_SIDE_INSET, PARTICIPANT_STRIP_PAD_BOTTOM,
        PARTICIPANT_STRIP_PAD_TOP, PARTICIPANT_STRIP_PAD_X,
    },
    AppMode, AppState, StreamViewMode,
};
use crate::{theme::Palette, title_bar, window_frame};
use egui::{CornerRadius, Frame, Stroke, Ui};

impl AppState {
    pub(super) fn ui_with_chrome(
        &mut self,
        ctx: &egui::Context,
        pal: &Palette,
        rounded: bool,
        always_on_top: &mut bool,
        viewport_transparent: &mut Option<bool>,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        let transparent = rounded;
        if *viewport_transparent != Some(transparent) {
            window_frame::sync_viewport_transparent(ctx, transparent);
            *viewport_transparent = Some(transparent);
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui| {
                ui.set_clip_rect(window_frame::clip_rect(ctx));

                window_frame::show_panel(ui, pal, rounded, |ui, content_rect| {
                    let app_rect = window_frame::body_rect(content_rect, rounded);
                    let title_bar_rect = {
                        let mut rect = app_rect;
                        rect.max.y = rect.min.y + title_bar::HEIGHT;
                        rect
                    };
                    let title = self
                        .dev_pair
                        .as_ref()
                        .map(|dev_pair| {
                            format!(
                                "Wire · DEV {} · P{}",
                                dev_pair.session(),
                                dev_pair.peer_index()
                            )
                        })
                        .unwrap_or_else(|| "Wire".to_owned());
                    // Built as owned text so the borrow only has to outlive the `ui` call
                    // below, not the enclosing closure.
                    #[cfg(windows)]
                    let update_text = self.title_bar_update_button();
                    #[cfg(windows)]
                    let update_button = update_text.as_ref().map(|text| title_bar::UpdateButton {
                        label: &text.label,
                        tooltip: &text.tooltip,
                        busy: text.busy,
                    });
                    #[cfg(not(windows))]
                    let update_button = None;
                    let update_clicked = title_bar::ui(
                        ui,
                        title_bar_rect,
                        pal,
                        &title,
                        self.resource_monitor.snapshot(),
                        self.show_system_usage,
                        always_on_top,
                        rounded,
                        update_button,
                    );
                    #[cfg(windows)]
                    if update_clicked {
                        if let Some((peer, _)) = self.peer_update.best_candidate() {
                            self.begin_peer_update(ctx, peer);
                        }
                    }

                    let mut body_rect = app_rect;
                    body_rect.min.y = title_bar_rect.max.y;
                    self.ui_chrome_body(
                        ui,
                        ctx,
                        pal,
                        body_rect,
                        #[cfg(windows)]
                        parent_hwnd,
                    );
                    window_frame::resize_edges(ui, ctx.viewport_rect());
                });
            });
    }

    fn ui_chrome_body(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        pal: &Palette,
        body: egui::Rect,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        if self.app_mode == AppMode::Text {
            self.ui_chat_chrome_body(ui, ctx, pal, body);
            return;
        }
        const TOP_BAR_HEIGHT: f32 = 54.0;
        const DOCK_HEIGHT: f32 = 66.0;
        let immersive = self.stream_view_mode != StreamViewMode::Normal;
        let show_participants = self.has_visible_call();
        let top_height = TOP_BAR_HEIGHT.min(body.height());
        // A running recording claims a band of its own at the top of the dock so
        // the clock never has to overlap the controls underneath it.
        let dock_height = DOCK_HEIGHT
            + if self.recording_active {
                RECORDING_BAND_HEIGHT
            } else {
                0.0
            };
        let dock_top = (body.max.y - dock_height).max(body.min.y + top_height);
        let participant_space = (dock_top - (body.min.y + top_height)).max(0.0);
        // The strip and the band reserved for it are sized from the same chip
        // width, so they can never disagree about how many rows there are.
        let participant_bar_height = if show_participants {
            participant_bar_height(
                participant_strip_width(body.width()),
                self.calls.len(),
                participant_space,
            )
        } else {
            0.0
        };

        let top_rect =
            egui::Rect::from_min_max(body.min, egui::pos2(body.max.x, body.min.y + top_height));
        let dock_rect = egui::Rect::from_min_max(egui::pos2(body.min.x, dock_top), body.max);
        let participant_rect = egui::Rect::from_min_max(
            egui::pos2(body.min.x, dock_rect.min.y - participant_bar_height),
            egui::pos2(body.max.x, dock_rect.min.y),
        );
        let stage_rect = egui::Rect::from_min_max(
            egui::pos2(body.min.x, top_rect.max.y),
            egui::pos2(body.max.x, participant_rect.min.y),
        );

        ui.scope_builder(egui::UiBuilder::new().max_rect(stage_rect), |ui| {
            let frame = if immersive {
                Frame::NONE
            } else {
                Frame::new().inner_margin(egui::Margin::symmetric(14, 10))
            };
            frame.show(ui, |ui| {
                self.ui_stage(
                    ui,
                    ctx,
                    pal,
                    #[cfg(windows)]
                    parent_hwnd,
                )
            });
        });

        ui.scope_builder(egui::UiBuilder::new().max_rect(top_rect), |ui| {
            Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::symmetric(14, 6))
                .show(ui, |ui| self.ui_top_bar_content(ui, ctx, pal));
        });

        if show_participants {
            // Rounded strip matching window bg — no hard separators / panel band.
            ui.scope_builder(egui::UiBuilder::new().max_rect(participant_rect), |ui| {
                Frame::new()
                    .fill(pal.bg)
                    .outer_margin(egui::Margin {
                        left: CHROME_SIDE_INSET,
                        right: CHROME_SIDE_INSET,
                        top: 0,
                        bottom: 2,
                    })
                    .inner_margin(egui::Margin {
                        left: PARTICIPANT_STRIP_PAD_X,
                        right: PARTICIPANT_STRIP_PAD_X,
                        top: PARTICIPANT_STRIP_PAD_TOP,
                        bottom: PARTICIPANT_STRIP_PAD_BOTTOM,
                    })
                    .show(ui, |ui| self.ui_call_participant_bar(ui, pal, ctx));
            });
        }

        // Paint the fixed call controls last so scrollable content never covers them.
        ui.scope_builder(egui::UiBuilder::new().max_rect(dock_rect), |ui| {
            Frame::new()
                .fill(pal.bg)
                .outer_margin(egui::Margin {
                    left: CHROME_SIDE_INSET,
                    right: CHROME_SIDE_INSET,
                    top: 0,
                    bottom: 2,
                })
                .inner_margin(egui::Margin::symmetric(12, 4))
                .show(ui, |ui| self.ui_dock_content(ui, pal, ctx));
        });
    }

    pub(super) fn ui_mode_switcher(&mut self, ui: &mut Ui, pal: &Palette) {
        Frame::new()
            .fill(chat_surface(pal))
            .stroke(Stroke::new(1.0_f32, chat_hairline(pal)))
            .corner_radius(CornerRadius::same(CHROME_RADIUS))
            .inner_margin(egui::Margin::same(3))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let text_active = self.app_mode == AppMode::Text;
                    if chat_segment_button(
                        ui,
                        pal,
                        "Text chats",
                        text_active,
                        !self.chat.unseen.is_empty(),
                    )
                    .clicked()
                    {
                        self.app_mode = AppMode::Text;
                    }
                    let calls_active = self.app_mode == AppMode::Calls;
                    let call_available = self.local_group_call.is_some()
                        || self
                            .group_call_reports
                            .values()
                            .flatten()
                            .any(|call| call.ended_at_ms.is_none());
                    if chat_segment_button(ui, pal, "Voice calls", calls_active, call_available)
                        .clicked()
                    {
                        self.app_mode = AppMode::Calls;
                    }
                });
            });
    }
}
