//! Chat views, composer actions, attachments, and group dialogs.

use super::{
    format_group_member_summary,
    profile_ui::{accent_color_for, paint_profile_avatar},
    unknown_direct_conversations,
    widgets::{
        chat_hairline, chat_lucide_icon_button, chat_navigation_button, chat_selected_surface,
        chat_surface, copy_to_clipboard, format_bytes, paint_chat_card,
    },
    AppMode, AppState, AttachmentTextureCache, ChatStyle, FileTransferUiState, GroupMemberKind,
    ImagePreview, ImagePreviewAction, ImagePreviewMode,
};
use crate::{
    chat::{
        self, AttachmentKind, ChatAttachment, ChatMessage, ConversationKind, DeleteScope,
        DeliveryState, FileTransferPhase, MessageDeletion,
    },
    client_status::Availability,
    runtime::Command,
    theme::{
        action_button, ghost_icon_button, kh_family, lucide, menu_item_button, toolbar_button,
        ui_font_size, ButtonTone, Palette,
    },
};
use egui::{Align, Align2, CornerRadius, Frame, Layout, RichText, Stroke, Ui, Vec2};
use egui_phosphor::regular as ph;
use iroh::NodeId;
use lucide_icons::Icon;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use tracing::warn;

const FILE_OFFER_CARD_MAX_WIDTH: f32 = 420.0;

impl AppState {
    pub(super) fn ui_chat_chrome_body(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        pal: &Palette,
        body: egui::Rect,
    ) {
        const TOP_HEIGHT: f32 = 54.0;
        let top =
            egui::Rect::from_min_max(body.min, egui::pos2(body.max.x, body.min.y + TOP_HEIGHT));
        let content = egui::Rect::from_min_max(egui::pos2(body.min.x, top.max.y), body.max);
        let sidebar_width = (content.width() * 0.27)
            .clamp(220.0, 310.0)
            .min(content.width() * 0.46);
        let sidebar = egui::Rect::from_min_max(
            content.min + Vec2::new(12.0, 10.0),
            egui::pos2(content.min.x + sidebar_width - 6.0, content.max.y - 12.0),
        );
        let main = egui::Rect::from_min_max(
            egui::pos2(content.min.x + sidebar_width + 6.0, content.min.y),
            content.max,
        );

        ui.scope_builder(egui::UiBuilder::new().max_rect(top), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(top));
            Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::symmetric(14, 6))
                .show(ui, |ui| self.ui_top_bar_content(ui, ctx, pal));
        });
        paint_chat_card(ui, sidebar, pal, 18);
        let sidebar_inner = sidebar.shrink2(Vec2::new(12.0, 12.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(sidebar_inner), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(sidebar_inner));
            self.ui_chat_sidebar(ui, pal);
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(main), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(main));
            Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::same(0))
                .show(ui, |ui| self.ui_chat_main(ui, pal));
        });
        if self.chat.show_group_editor {
            self.ui_group_editor(ctx, pal);
        }
        if self.chat.show_group_members {
            self.ui_group_members(ctx, pal);
        }
        if self.chat.friend_candidate.is_some() {
            self.ui_add_chat_friend(ctx, pal);
        }
    }

    fn ui_chat_sidebar(&mut self, ui: &mut Ui, pal: &Palette) {
        const FOOTER_HEIGHT: f32 = 62.0;
        const FOOTER_GAP: f32 = 10.0;
        let bounds = ui.max_rect();
        let has_identity = self.our_node_id.is_some();
        let footer_space = if has_identity {
            FOOTER_HEIGHT + FOOTER_GAP
        } else {
            0.0
        };
        let list = egui::Rect::from_min_max(
            bounds.min,
            egui::pos2(
                bounds.max.x,
                (bounds.max.y - footer_space).max(bounds.min.y),
            ),
        );

        ui.scope_builder(egui::UiBuilder::new().max_rect(list), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(list));
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("CONVERSATIONS")
                        .family(kh_family())
                        .color(pal.text2)
                        .size(12.0),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ghost_icon_button(ui, pal, ph::PLUS)
                        .on_hover_text("Create a group")
                        .clicked()
                        && self.our_node_id.is_some()
                    {
                        self.chat.show_group_editor = true;
                    }
                });
            });
            ui.add_space(2.0);

            egui::ScrollArea::vertical()
                .id_salt("chat-conversations")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let friends = self.friends.clone();
                    for friend in friends {
                        let Ok(peer) = NodeId::from_str(friend.node_id.trim()) else {
                            continue;
                        };
                        let id = self
                            .our_node_id
                            .map(|ours| chat::direct_conversation_id(ours, peer));
                        let selected = id
                            .as_ref()
                            .is_some_and(|id| self.chat.selected.as_deref() == Some(id.as_str()));
                        let unseen = id.as_ref().is_some_and(|id| self.chat.unseen.contains(id));
                        let label = format!("{}   {}", self.peer_initial(peer), friend.name);
                        if chat_navigation_button(ui, pal, &label, None, selected, unseen).clicked()
                        {
                            if let Some(id) = id {
                                self.chat.selected = Some(id.clone());
                                if !self.chat.conversations.contains_key(&id) {
                                    self.cmd(Command::EnsureDirectChat {
                                        peer,
                                        title: friend.name,
                                    });
                                }
                            }
                        }
                        ui.add_space(3.0);
                    }

                    let known_peers = self
                        .friends
                        .iter()
                        .filter_map(|friend| NodeId::from_str(friend.node_id.trim()).ok())
                        .collect::<BTreeSet<_>>();
                    let unknown_directs =
                        unknown_direct_conversations(&self.chat.conversations, &known_peers);
                    if !unknown_directs.is_empty() {
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("UNKNOWN")
                                .family(kh_family())
                                .color(pal.dim)
                                .size(11.0),
                        );
                        ui.add_space(5.0);
                    }
                    for (id, peer) in unknown_directs {
                        let selected = self.chat.selected.as_deref() == Some(id.as_str());
                        let label =
                            format!("{}   Peer {}", self.peer_initial(peer), peer.fmt_short());
                        if chat_navigation_button(
                            ui,
                            pal,
                            &label,
                            None,
                            selected,
                            self.chat.unseen.contains(&id),
                        )
                        .on_hover_text("Unknown sender · add them from the chat header")
                        .clicked()
                        {
                            self.chat.selected = Some(id);
                        }
                        ui.add_space(3.0);
                    }

                    let groups: Vec<_> = self
                        .chat
                        .conversations
                        .values()
                        .filter(|conversation| matches!(conversation.kind, ConversationKind::Group))
                        .cloned()
                        .collect();
                    if !groups.is_empty() {
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("GROUPS")
                                .family(kh_family())
                                .color(pal.dim)
                                .size(11.0),
                        );
                        ui.add_space(5.0);
                    }
                    for group in groups {
                        let selected = self.chat.selected.as_deref() == Some(group.id.as_str());
                        let members = self.group_members_for(&group);
                        let member_summary = format_group_member_summary(&members);
                        let active_call = self.group_call_for(&group.id);
                        let missed_call = self
                            .group_call_reports
                            .values()
                            .flatten()
                            .filter(|call| call.conversation_id == group.id)
                            .filter(|call| !self.seen_group_calls.contains_key(&call.call_id))
                            .filter(|call| {
                                self.our_node_id.is_none_or(|ours| {
                                    !call.participants.contains(&ours.to_string())
                                })
                            })
                            .filter_map(|call| call.ended_at_ms.map(|ended| (ended, call)))
                            .filter(|(ended, _)| chat::now_millis() - *ended < 24 * 60 * 60 * 1000)
                            .max_by_key(|(ended, _)| *ended);
                        let sidebar_summary = active_call
                            .as_ref()
                            .map(|call| {
                                format!(
                                    "● Active call · {} {}",
                                    call.participants.len().max(1),
                                    if call.participants.len() == 1 {
                                        "participant"
                                    } else {
                                        "participants"
                                    }
                                )
                            })
                            .or_else(|| missed_call.map(|_| "Missed group call".to_owned()))
                            .unwrap_or_else(|| member_summary.clone());
                        if chat_navigation_button(
                            ui,
                            pal,
                            &format!("#   {}", group.title),
                            Some(&sidebar_summary),
                            selected,
                            self.chat.unseen.contains(&group.id),
                        )
                        .on_hover_text(&member_summary)
                        .clicked()
                        {
                            self.acknowledge_missed_group_calls(&group.id);
                            self.chat.selected = Some(group.id);
                        }
                        ui.add_space(3.0);
                    }
                });
        });

        if let Some(node_id) = self.our_node_id {
            let footer_bottom = bounds.max.y - 2.0;
            let footer = egui::Rect::from_min_max(
                egui::pos2(bounds.min.x, footer_bottom - FOOTER_HEIGHT),
                egui::pos2(bounds.max.x, footer_bottom),
            );
            paint_chat_card(ui, footer, pal, 14);
            let footer_inner = footer.shrink2(Vec2::new(11.0, 8.0));
            let ctx_clone = ui.ctx().clone();
            let own_name = self.own_label();
            let own_initial = crate::profile::display_name_initial(&own_name)
                .unwrap_or_else(|| "Y".to_owned());
            let own_avatar = self.own_avatar_texture(&ctx_clone);
            let mut open_editor = false;
            ui.scope_builder(egui::UiBuilder::new().max_rect(footer_inner), |ui| {
                ui.set_clip_rect(footer);
                ui.horizontal(|ui| {
                    paint_profile_avatar(ui, pal, own_avatar, &own_initial, 32.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&own_name)
                                .color(pal.text)
                                .size(ui_font_size(12.5)),
                        );
                        ui.label(
                            RichText::new(node_id.fmt_short().to_string())
                                .monospace()
                                .color(pal.dim)
                                .size(ui_font_size(10.5)),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if chat_lucide_icon_button(ui, pal, Icon::Copy)
                            .on_hover_text("Copy my ID")
                            .clicked()
                        {
                            copy_to_clipboard(&node_id.to_string());
                        }
                    });
                });
                // Clicking the self card opens the profile editor.
                let footer_response = ui.interact(
                    footer_inner,
                    ui.id().with("sidebar-self-card"),
                    egui::Sense::click(),
                );
                if footer_response
                    .on_hover_text("Edit your profile (name + picture)")
                    .clicked()
                {
                    open_editor = true;
                }
            });
            if open_editor {
                self.profile_edit_name = self.own_profile_name.clone();
                self.profile_edit_error = None;
                self.show_profile_editor = true;
            }
        }
    }

    fn ui_chat_main(&mut self, ui: &mut Ui, pal: &Palette) {
        let selected = self
            .chat
            .selected
            .as_ref()
            .and_then(|id| self.chat.conversations.get(id))
            .cloned();
        let Some(conversation) = selected else {
            ui.centered_and_justified(|ui| {
                ui.vertical_centered(|ui| {
                    let unavailable = self.chat.service_error.as_deref();
                    ui.label(
                        RichText::new(if unavailable.is_some() {
                            "Chat is unavailable"
                        } else {
                            "Choose a conversation"
                        })
                        .color(if unavailable.is_some() {
                            pal.err
                        } else {
                            pal.text
                        })
                        .size(ui_font_size(18.0)),
                    );
                    ui.label(
                        RichText::new(
                            unavailable.unwrap_or("Messages are available independently of calls."),
                        )
                        .color(pal.dim)
                        .size(ui_font_size(12.5)),
                    );
                });
            });
            return;
        };
        let display_title = conversation
            .direct_peer()
            .map(|peer| self.peer_display_name(peer))
            .unwrap_or_else(|| conversation.title.clone());
        // Resolve unknown conversation members / senders in the background.
        {
            let mut visible: Vec<NodeId> = conversation
                .members
                .iter()
                .filter_map(|member| NodeId::from_str(member).ok())
                .collect();
            if let Some(peer) = conversation.direct_peer() {
                visible.push(peer);
            }
            if let Some(timeline) = self.chat.timelines.get(&conversation.id) {
                for message in timeline.iter().take(200) {
                    if let Ok(peer) = NodeId::from_str(&message.author_id) {
                        visible.push(peer);
                    }
                }
            }
            self.ensure_peer_profiles(visible);
        }
        let header_peer = conversation.direct_peer();
        let header_initial = crate::profile::display_name_initial(&display_title)
            .unwrap_or_else(|| "#".to_owned());
        let ctx_clone = ui.ctx().clone();
        let header_avatar = header_peer.and_then(|peer| self.peer_avatar_texture(&ctx_clone, peer));

        const HEADER: f32 = 72.0;
        const GAP: f32 = 10.0;
        self.collect_chat_image_input(ui.ctx());
        let composer_rows = composer_visual_rows(
            &self.chat.composer,
            (ui.available_width() - 80.0).max(120.0),
        );
        let editor_height = 10.0 + composer_rows as f32 * 20.0;
        let composer_height = editor_height
            + 64.0
            + if self.chat.preparing_file_offers > 0 {
                20.0
            } else {
                0.0
            };
        let preview_height =
            if self.chat.draft_attachments.is_empty() && self.chat.draft_files.is_empty() {
                0.0
            } else {
                62.0
            };
        let rect = ui.max_rect();
        let surface_rect = egui::Rect::from_min_max(
            rect.min + Vec2::new(0.0, 10.0),
            rect.max - Vec2::new(12.0, 12.0),
        );
        let header =
            egui::Rect::from_min_size(surface_rect.min, Vec2::new(surface_rect.width(), HEADER));
        let composer = egui::Rect::from_min_max(
            egui::pos2(surface_rect.min.x, surface_rect.max.y - composer_height),
            surface_rect.max,
        );
        let previews = egui::Rect::from_min_max(
            egui::pos2(
                surface_rect.min.x,
                composer.min.y - preview_height - if preview_height > 0.0 { GAP } else { 0.0 },
            ),
            egui::pos2(surface_rect.max.x, composer.min.y - GAP),
        );
        let message_bottom = if preview_height > 0.0 {
            previews.min.y - GAP
        } else {
            composer.min.y - GAP
        };
        let messages = egui::Rect::from_min_max(
            egui::pos2(surface_rect.min.x, header.max.y + GAP),
            egui::pos2(surface_rect.max.x, message_bottom),
        );

        paint_chat_card(ui, header, pal, 18);
        let header_inner = header.shrink2(Vec2::new(18.0, 12.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(header_inner), |ui| {
            ui.set_clip_rect(header);
            let show_call = ui.available_width() >= 430.0;
            ui.horizontal(|ui| {
                paint_profile_avatar(ui, pal, header_avatar, &header_initial, 34.0);
                let group_members = matches!(conversation.kind, ConversationKind::Group)
                    .then(|| self.group_members_for(&conversation));
                let group_member_summary = group_members
                    .as_ref()
                    .map(|members| format_group_member_summary(members));
                ui.vertical(|ui| {
                    ui.set_max_width((ui.available_width() - 160.0).max(80.0));
                    ui.add(
                        egui::Label::new(
                            RichText::new(&display_title)
                                .color(pal.text)
                                .size(ui_font_size(15.0)),
                        )
                        .truncate(),
                    );
                    let subtitle = match &conversation.kind {
                        ConversationKind::Direct { peer_id } => NodeId::from_str(peer_id)
                            .ok()
                            .and_then(|peer| self.friend_status.get(&peer))
                            .map(|status| match status.availability {
                                Availability::Online => "Online".to_owned(),
                                Availability::Offline => "Offline".to_owned(),
                            })
                            .unwrap_or_else(|| "Connection status unknown".to_owned()),
                        ConversationKind::Group => group_member_summary
                            .clone()
                            .unwrap_or_else(|| "No members".to_owned()),
                    };
                    let subtitle_response = ui.add(
                        egui::Label::new(
                            RichText::new(&subtitle)
                                .color(pal.dim)
                                .size(ui_font_size(11.5)),
                        )
                        .truncate()
                        .sense(egui::Sense::click()),
                    );
                    if matches!(conversation.kind, ConversationKind::Group) {
                        let hover = group_member_summary
                            .as_deref()
                            .unwrap_or("Show group members");
                        if subtitle_response.on_hover_text(hover).clicked() {
                            self.chat.show_group_members = true;
                        }
                    }
                });
                let mut clear_history = false;
                let mut open_members = false;
                let direct_peer = conversation.direct_peer();
                let active_group_call = matches!(conversation.kind, ConversationKind::Group)
                    .then(|| self.group_call_for(&conversation.id))
                    .flatten();
                let peer_is_friend = direct_peer.is_some_and(|peer| self.is_friend(peer));
                let mut friend_to_add = None;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let menu_response = ui
                        .menu_button(
                            RichText::new(char::from(Icon::EllipsisVertical))
                                .font(lucide(16.0))
                                .color(pal.text2),
                            |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                if matches!(conversation.kind, ConversationKind::Group)
                                    && menu_item_button(ui, pal, Icon::Users, "Members", false)
                                        .clicked()
                                {
                                    open_members = true;
                                    ui.close();
                                }
                                if !show_call && !peer_is_friend {
                                    if let Some(peer) = direct_peer {
                                        if menu_item_button(
                                            ui,
                                            pal,
                                            Icon::UserPlus,
                                            "Add friend",
                                            false,
                                        )
                                        .clicked()
                                        {
                                            friend_to_add = Some(peer);
                                            ui.close();
                                        }
                                    }
                                }
                                if menu_item_button(ui, pal, Icon::Trash2, "Clear history", true)
                                    .on_hover_text(
                                        "Permanently delete this chat history for everyone",
                                    )
                                    .clicked()
                                {
                                    clear_history = true;
                                    ui.close();
                                }
                            },
                        )
                        .response;
                    menu_response.on_hover_text("Chat actions");
                    if show_call {
                        if let Some(peer) = direct_peer {
                            if peer_is_friend {
                                if action_button(ui, pal, "Start call", ButtonTone::Secondary)
                                    .on_hover_text("Start a separate voice call")
                                    .clicked()
                                {
                                    self.cmd(Command::Call { node_id: peer });
                                    self.app_mode = AppMode::Calls;
                                }
                            } else if action_button(ui, pal, "Add friend", ButtonTone::Primary)
                                .on_hover_text("Save this sender using their full node ID")
                                .clicked()
                            {
                                friend_to_add = Some(peer);
                            }
                        } else if matches!(conversation.kind, ConversationKind::Group) {
                            let already_joined = self
                                .local_group_call
                                .as_ref()
                                .is_some_and(|call| call.conversation_id == conversation.id);
                            let label = if already_joined {
                                "Open call"
                            } else if active_group_call.is_some() {
                                "Join call"
                            } else {
                                "Start call"
                            };
                            if action_button(ui, pal, label, ButtonTone::Primary)
                                .on_hover_text(if active_group_call.is_some() {
                                    "Join the active group call"
                                } else {
                                    "Ring group members who are currently online"
                                })
                                .clicked()
                            {
                                if already_joined {
                                    self.app_mode = AppMode::Calls;
                                } else {
                                    self.enter_group_call(&conversation, active_group_call.clone());
                                }
                            }
                        }
                    }
                });
                if let Some(peer) = friend_to_add {
                    self.chat.friend_candidate = Some(peer);
                    self.chat.friend_candidate_name.clear();
                }
                if open_members {
                    self.chat.show_group_members = true;
                }
                if clear_history {
                    self.clear_chat_history(&conversation.id);
                }
            });
        });

        ui.scope_builder(egui::UiBuilder::new().max_rect(messages), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(messages));
            Frame::new()
                .fill(pal.bg)
                .inner_margin(egui::Margin::symmetric(18, 8))
                .show(ui, |ui| {
                    if self
                        .chat
                        .conversations_with_older_messages
                        .contains(&conversation.id)
                    {
                        ui.vertical_centered(|ui| {
                            if ui.small_button("Load older messages").clicked() {
                                self.chat
                                    .conversations_with_older_messages
                                    .remove(&conversation.id);
                                self.cmd(Command::LoadOlderChatMessages {
                                    conversation_id: conversation.id.clone(),
                                });
                            }
                        });
                        ui.add_space(6.0);
                    }
                    let timeline = self
                        .chat
                        .timelines
                        .get(&conversation.id)
                        .cloned()
                        .unwrap_or_default();
                    let now = chat::now_millis();
                    let retention = self.chat_retention;
                    egui::ScrollArea::vertical()
                        .id_salt(("chat-timeline", &conversation.id))
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            // The theme defaults to 6px vertical item spacing,
                            // which egui adds between every message row on top
                            // of the explicit gaps below (continuations were
                            // really 8px, groups really 20px). Zero it so the
                            // Discord-style rhythm is exact: 2px inside a
                            // burst, 14px between bursts, 9px for bubbles.
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add_space(8.0);
                            let visible_messages = timeline
                                .into_iter()
                                .filter(|message| message.visible_under(retention, now))
                                .collect::<Vec<_>>();
                            for (index, message) in visible_messages.iter().enumerate() {
                                let starts_group = index == 0
                                    || !messages_share_compact_group(
                                        &visible_messages[index - 1],
                                        message,
                                    );
                                self.ui_chat_message(
                                    ui,
                                    pal,
                                    &conversation.id,
                                    message,
                                    starts_group,
                                );
                                let next_is_grouped =
                                    visible_messages.get(index + 1).is_some_and(|next| {
                                        messages_share_compact_group(message, next)
                                    });
                                ui.add_space(match self.chat_style {
                                    ChatStyle::Bubbles => 9.0,
                                    ChatStyle::Compact if next_is_grouped => 2.0,
                                    ChatStyle::Compact => 14.0,
                                });
                            }
                        });
                });
        });

        if !self.chat.draft_attachments.is_empty() || !self.chat.draft_files.is_empty() {
            ui.scope_builder(egui::UiBuilder::new().max_rect(previews), |ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(previews));
                egui::ScrollArea::horizontal()
                    .id_salt("chat-draft-images")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for path in self.chat.draft_files.clone() {
                                let name = path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("File");
                                ui.label(RichText::new(name).color(pal.text2))
                                    .on_hover_text(path.display().to_string());
                                if ui.small_button("×").clicked() {
                                    self.chat.draft_files.retain(|item| item != &path);
                                }
                            }
                            for attachment in self.chat.draft_attachments.clone() {
                                if attachment.kind == AttachmentKind::InlineFile {
                                    ui.label(RichText::new(&attachment.name).color(pal.text2));
                                    if ui.small_button("×").clicked() {
                                        self.chat
                                            .draft_attachments
                                            .retain(|item| item.id != attachment.id);
                                    }
                                    continue;
                                }
                                if let Some(texture) = attachment_texture(
                                    ui.ctx(),
                                    &mut self.chat.attachment_textures,
                                    &attachment,
                                ) {
                                    let response = square_attachment_preview(
                                        ui,
                                        pal,
                                        texture,
                                        attachment.width,
                                        attachment.height,
                                        56.0,
                                    );
                                    if response.clicked() {
                                        self.chat.image_preview = Some(ImagePreview {
                                            attachment,
                                            draft: true,
                                            mode: ImagePreviewMode::Floating,
                                        });
                                    }
                                }
                            }
                        });
                    });
            });
        }

        paint_chat_card(ui, composer, pal, 22);
        let composer_inner = composer.shrink2(Vec2::new(14.0, 10.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(composer_inner), |ui| {
            ui.set_clip_rect(ui.clip_rect().intersect(composer_inner));
            let edit = ui.add_sized(
                [ui.available_width(), editor_height],
                egui::TextEdit::multiline(&mut self.chat.composer)
                    .hint_text(format!("Message {display_title}"))
                    .desired_rows(composer_rows)
                    .desired_width(f32::INFINITY)
                    .frame(false),
            );
            let keyboard_send = edit.has_focus()
                && ui.input(|input| !input.modifiers.shift && input.key_pressed(egui::Key::Enter));
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                ui.horizontal(|ui| {
                    if composer_ghost_icon_button(ui, pal, Icon::Plus, "Attach files").clicked() {
                        self.pick_chat_files();
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let button_send = chat_send_button(ui, pal).clicked();
                        if keyboard_send || button_send {
                            self.send_chat_composer(&conversation.id);
                        }
                    });
                });
            });
            if let Some(error) = self.chat.error.take() {
                ui.add(
                    egui::Label::new(RichText::new(error).color(pal.err).size(ui_font_size(10.5)))
                        .truncate(),
                );
            }
            if self.chat.preparing_file_offers > 0 {
                ui.label(
                    RichText::new("Preparing file…")
                        .color(pal.dim)
                        .size(ui_font_size(10.5)),
                );
            }
        });
        self.ui_image_preview(ui.ctx(), pal);
    }

    fn ui_chat_message(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        conversation_id: &str,
        message: &ChatMessage,
        starts_group: bool,
    ) {
        match self.chat_style {
            ChatStyle::Bubbles => self.ui_bubble_chat_message(ui, pal, conversation_id, message),
            ChatStyle::Compact => {
                self.ui_compact_chat_message(ui, pal, conversation_id, message, starts_group)
            }
        }
    }

    fn ui_bubble_chat_message(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        conversation_id: &str,
        message: &ChatMessage,
    ) {
        let own = self
            .our_node_id
            .is_some_and(|node| message.author_id == node.to_string());
        let (state, detail) = self
            .chat
            .delivery
            .get(&message.message_id)
            .cloned()
            .unwrap_or((DeliveryState::Delivered, None));
        let author = if own {
            self.own_label()
        } else {
            NodeId::from_str(&message.author_id)
                .ok()
                .map(|node| self.peer_display_name(node))
                .unwrap_or_else(|| "Unknown peer".to_owned())
        };
        let author_peer = NodeId::from_str(&message.author_id).ok();
        let author_color = if own {
            accent_color_for(self.own_accent_color.as_deref(), pal.text)
        } else {
            let snapshot_accent = message.author_accent_color.as_deref();
            let peer_accent =
                author_peer.and_then(|peer| snapshot_accent.or_else(|| self.peer_accent_hex(peer)));
            accent_color_for(peer_accent, pal.text)
        };
        let opacity = chat_delivery_opacity(state);
        let time = format_chat_timestamp(message.sent_at, chat::now_millis());
        let mut requested_deletion = None;
        let mut requested_restore = false;
        // Profile avatar for the author (own picture for self).
        let bubble_initial = if own {
            crate::profile::display_name_initial(&self.own_label())
                .unwrap_or_else(|| "Y".to_owned())
        } else {
            crate::profile::display_name_initial(&author).unwrap_or_else(|| "?".to_owned())
        };
        let ctx_clone = ui.ctx().clone();
        let bubble_avatar = if own {
            self.own_avatar_texture(&ctx_clone)
        } else if let Some(peer) = author_peer {
            self.peer_avatar_texture(&ctx_clone, peer)
        } else {
            None
        };
        ui.with_layout(
            if own {
                Layout::right_to_left(Align::Min)
            } else {
                Layout::left_to_right(Align::Min)
            },
            |ui| {
                paint_profile_avatar(ui, pal, bubble_avatar, &bubble_initial, 28.0);
                ui.add_space(8.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width().min(680.0), 0.0),
                    Layout::top_down(if own { Align::Max } else { Align::Min }),
                    |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(
                                RichText::new(&author)
                                    .strong()
                                    .color(author_color.gamma_multiply(opacity))
                                    .size(ui_font_size(11.5)),
                            );
                            ui.label(
                                RichText::new(&time)
                                    .color(pal.dim.gamma_multiply(opacity))
                                    .size(ui_font_size(10.5)),
                            );
                            if own && message.deletion.is_none() {
                                chat_delivery_status_icon(
                                    ui,
                                    pal,
                                    state,
                                    detail.as_deref(),
                                    opacity,
                                );
                            }
                        });
                        let bubble = Frame::new()
                            .fill(if own {
                                chat_selected_surface(pal).gamma_multiply(opacity)
                            } else {
                                chat_surface(pal).gamma_multiply(opacity)
                            })
                            .stroke(Stroke::new(
                                1.0_f32,
                                chat_hairline(pal).gamma_multiply(opacity),
                            ))
                            .corner_radius(CornerRadius::same(14))
                            .inner_margin(egui::Margin::symmetric(12, 9))
                            .show(ui, |ui| {
                                ui.set_max_width(640.0);
                                if message.deletion.is_some() || !message.body.trim().is_empty() {
                                    let body_response = ui.add(
                                        egui::Label::new(chat_message_body_text(
                                            message, pal, opacity,
                                        ))
                                        .wrap()
                                        .selectable(true),
                                    );
                                    body_response.context_menu(|ui| {
                                        chat_message_context_menu(
                                            ui,
                                            pal,
                                            message,
                                            own,
                                            &mut requested_restore,
                                            &mut requested_deletion,
                                        );
                                    });
                                }
                                if message.deletion.is_none() {
                                    self.ui_message_attachments(
                                        ui,
                                        pal,
                                        conversation_id,
                                        message,
                                        own,
                                        opacity,
                                        &mut requested_restore,
                                        &mut requested_deletion,
                                    );
                                }
                            });
                        bubble.response.context_menu(|ui| {
                            chat_message_context_menu(
                                ui,
                                pal,
                                message,
                                own,
                                &mut requested_restore,
                                &mut requested_deletion,
                            );
                        });
                    },
                );
            },
        );
        if requested_restore {
            self.restore_chat_message(conversation_id, &message.message_id);
        } else if let Some(scope) = requested_deletion {
            self.delete_chat_message(conversation_id, &message.message_id, scope);
        }
    }

    fn ui_compact_chat_message(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        conversation_id: &str,
        message: &ChatMessage,
        starts_group: bool,
    ) {
        let own = self
            .our_node_id
            .is_some_and(|node| message.author_id == node.to_string());
        let (state, detail) = self
            .chat
            .delivery
            .get(&message.message_id)
            .cloned()
            .unwrap_or((DeliveryState::Delivered, None));
        let author = if own {
            self.own_label()
        } else {
            NodeId::from_str(&message.author_id)
                .ok()
                .map(|node| self.peer_display_name(node))
                .unwrap_or_else(|| "Unknown peer".to_owned())
        };
        let author_peer = NodeId::from_str(&message.author_id).ok();
        let author_color = if own {
            accent_color_for(self.own_accent_color.as_deref(), pal.text)
        } else {
            let snapshot_accent = message.author_accent_color.as_deref();
            let peer_accent =
                author_peer.and_then(|peer| snapshot_accent.or_else(|| self.peer_accent_hex(peer)));
            accent_color_for(peer_accent, pal.text)
        };
        let initial = if own {
            crate::profile::display_name_initial(&self.own_label())
                .unwrap_or_else(|| "Y".to_owned())
        } else {
            crate::profile::display_name_initial(&author).unwrap_or_else(|| "?".to_owned())
        };
        let ctx_clone = ui.ctx().clone();
        let compact_avatar = if own {
            self.own_avatar_texture(&ctx_clone)
        } else if let Ok(peer) = NodeId::from_str(&message.author_id) {
            self.peer_avatar_texture(&ctx_clone, peer)
        } else {
            None
        };
        let opacity = chat_delivery_opacity(state);
        let mut requested_deletion = None;
        let mut requested_restore = false;
        let now = chat::now_millis();
        let mut gutter_hover_rect: Option<egui::Rect> = None;

        // Discord-cozy layout: a fixed gutter holds the 40px avatar for group
        // starts (top-aligned) and reveals a small timestamp on hover for
        // continuations. The content column always starts at the same x, so
        // names, timestamps, and bodies line up across the timeline.
        const COMPACT_AVATAR: f32 = 40.0;
        const COMPACT_GAP: f32 = 12.0;
        const COMPACT_GUTTER: f32 = COMPACT_AVATAR + COMPACT_GAP;
        // Optical nudge: the gutter stays top-aligned with the name row,
        // but name glyphs start ~3px lower (font ascent gap above the
        // cap-top). A crisp circle edge makes those 3px read as "avatar too
        // far up", so the painted avatar drops to meet the cap-top.
        const COMPACT_AVATAR_NUDGE: f32 = 3.0;

        let row_response = ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if starts_group {
                // Nested layout: the outer row stacks gutter + content
                // horizontally (widths accumulate to the fixed 52px gutter),
                // while the inner column drops the painted avatar 3px to the
                // glyph cap-top. A single top-down gutter would collapse to
                // the 40px avatar and eat the 12px gap (widths don't
                // accumulate vertically).
                ui.allocate_ui_with_layout(
                    Vec2::new(COMPACT_GUTTER, COMPACT_AVATAR),
                    Layout::left_to_right(Align::Min),
                    |ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(COMPACT_AVATAR, COMPACT_AVATAR),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.add_space(COMPACT_AVATAR_NUDGE);
                                paint_profile_avatar(
                                    ui,
                                    pal,
                                    compact_avatar,
                                    &initial,
                                    COMPACT_AVATAR,
                                );
                            },
                        );
                        ui.add_space(COMPACT_GAP);
                    },
                );
            } else {
                // Gutter slot for the hover-revealed timestamp, painted
                // after the row so we know whether the row is hovered.
                let (gutter_rect, _) = ui.allocate_exact_size(
                    Vec2::new(COMPACT_GUTTER, 18.0),
                    egui::Sense::hover(),
                );
                gutter_hover_rect = Some(gutter_rect);
            }
            let content_width = (ui.available_width() - 4.0).max(80.0);
            ui.allocate_ui_with_layout(
                Vec2::new(content_width, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_max_width(content_width);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if starts_group {
                        // Bottom-aligned like Discord: the timestamp sits on
                        // the name baseline instead of floating centered.
                        ui.with_layout(Layout::left_to_right(Align::Max), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(
                                RichText::new(&author)
                                    .strong()
                                    .color(author_color.gamma_multiply(opacity))
                                    .size(ui_font_size(14.0)),
                            );
                            ui.label(
                                RichText::new(format_chat_timestamp(message.sent_at, now))
                                    .color(pal.dim.gamma_multiply(opacity))
                                    .size(ui_font_size(11.0)),
                            );
                            if own && message.deletion.is_none() {
                                chat_delivery_status_icon(
                                    ui,
                                    pal,
                                    state,
                                    detail.as_deref(),
                                    opacity,
                                );
                            }
                        });
                        ui.add_space(1.0);
                    }

                    // Constrain width so the label wraps (horizontal layouts
                    // otherwise give infinite width and never break lines).
                    let status_slot = if own
                        && message.deletion.is_none()
                        && !starts_group
                        && !matches!(state, DeliveryState::Delivered)
                    {
                        18.0
                    } else {
                        0.0
                    };
                    let body_width = (ui.available_width() - status_slot).max(40.0);
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(body_width, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_max_width(body_width);
                                if message.deletion.is_some() || !message.body.trim().is_empty() {
                                    let body_response = ui.add(
                                        egui::Label::new(chat_message_body_text(
                                            message, pal, opacity,
                                        ))
                                        .wrap()
                                        .selectable(true),
                                    );
                                    body_response.context_menu(|ui| {
                                        chat_message_context_menu(
                                            ui,
                                            pal,
                                            message,
                                            own,
                                            &mut requested_restore,
                                            &mut requested_deletion,
                                        );
                                    });
                                }
                                if message.deletion.is_none() {
                                    self.ui_message_attachments(
                                        ui,
                                        pal,
                                        conversation_id,
                                        message,
                                        own,
                                        opacity,
                                        &mut requested_restore,
                                        &mut requested_deletion,
                                    );
                                }
                            },
                        );
                        if status_slot > 0.0 {
                            chat_delivery_status_icon(ui, pal, state, detail.as_deref(), opacity);
                        }
                    });
                },
            );
        });

        // Discord-style: hovering a continuation reveals its time in the
        // gutter, aligned with the message's first line.
        if let Some(gutter) = gutter_hover_rect {
            if row_response.response.hovered() {
                ui.painter().text(
                    egui::pos2(gutter.right() - 4.0, gutter.top() + 1.0),
                    egui::Align2::RIGHT_TOP,
                    format_chat_time(message.sent_at),
                    egui::FontId::proportional(10.0),
                    pal.dim2.gamma_multiply(opacity),
                );
            }
        }

        if requested_restore {
            self.restore_chat_message(conversation_id, &message.message_id);
        } else if let Some(scope) = requested_deletion {
            self.delete_chat_message(conversation_id, &message.message_id, scope);
        }
    }

    fn delete_chat_message(&mut self, conversation_id: &str, message_id: &str, scope: DeleteScope) {
        if let Some(message) = self
            .chat
            .timelines
            .get_mut(conversation_id)
            .and_then(|timeline| {
                timeline
                    .iter_mut()
                    .find(|message| message.message_id == message_id)
            })
        {
            message.deletion = Some(match scope {
                DeleteScope::Local => MessageDeletion::Local,
                DeleteScope::Everyone => MessageDeletion::Everyone,
            });
        }
        self.chat.delivery.remove(message_id);
        self.cmd(Command::DeleteChatMessage {
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
            scope,
        });
    }

    fn clear_chat_history(&mut self, conversation_id: &str) {
        if let Some(timeline) = self.chat.timelines.get_mut(conversation_id) {
            for message in timeline.drain(..) {
                self.chat.delivery.remove(&message.message_id);
            }
        }
        self.cmd(Command::ClearChatHistory {
            conversation_id: conversation_id.to_owned(),
        });
    }

    fn restore_chat_message(&mut self, conversation_id: &str, message_id: &str) {
        if let Some(message) = self
            .chat
            .timelines
            .get_mut(conversation_id)
            .and_then(|timeline| {
                timeline
                    .iter_mut()
                    .find(|message| message.message_id == message_id)
            })
        {
            message.deletion = None;
        }
        self.cmd(Command::RestoreChatMessage {
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
        });
    }

    fn send_chat_composer(&mut self, conversation_id: &str) {
        let body = self.chat.composer.trim().to_owned();
        if body.is_empty()
            && self.chat.draft_attachments.is_empty()
            && self.chat.draft_files.is_empty()
        {
            self.chat.composer.clear();
            return;
        }
        let Some(author) = self.our_node_id else {
            self.chat.error = Some("Chat is still connecting to Iroh.".to_owned());
            return;
        };
        self.chat.composer.clear();
        let attachments = std::mem::take(&mut self.chat.draft_attachments);
        if !self.chat.draft_files.is_empty() {
            let paths = std::mem::take(&mut self.chat.draft_files);
            self.chat.preparing_file_offers += 1;
            self.cmd(Command::OfferChatFiles {
                conversation_id: conversation_id.to_owned(),
                body,
                attachments,
                paths,
            });
            return;
        }
        let mut message = ChatMessage::new_with_attachments(author, body, attachments);
        // Stamp our current profile so strangers see a name + avatar + color.
        if !self.own_profile_name.trim().is_empty() {
            message = message.with_author_profile(
                Some(self.own_profile_name.clone()),
                self.own_avatar_hash.clone(),
                self.own_accent_color.clone(),
            );
        } else if self.own_avatar_hash.is_some() || self.own_accent_color.is_some() {
            message = message.with_author_profile(
                None,
                self.own_avatar_hash.clone(),
                self.own_accent_color.clone(),
            );
        }
        self.chat
            .delivery
            .insert(message.message_id.clone(), (DeliveryState::Pending, None));
        self.chat
            .timelines
            .entry(conversation_id.to_owned())
            .or_default()
            .push(message.clone());
        self.cmd(Command::SendChatMessage {
            conversation_id: conversation_id.to_owned(),
            message,
        });
    }

    fn collect_chat_image_input(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        for file in dropped {
            if let Some(path) = file.path {
                self.add_chat_file_path(&path);
            } else if let Some(bytes) = file.bytes {
                self.add_chat_image_bytes(
                    if file.name.is_empty() {
                        "dropped-image".to_owned()
                    } else {
                        file.name
                    },
                    bytes.to_vec(),
                );
            }
        }

        let paste_image =
            ctx.input(|input| input.modifiers.command && input.key_pressed(egui::Key::V));
        if paste_image {
            let before = self.chat.draft_attachments.len();
            #[cfg(windows)]
            {
                use clipboard_win::Getter;
                let mut paths = Vec::<PathBuf>::new();
                if let Ok(_clipboard) = clipboard_win::Clipboard::new_attempts(5) {
                    let _ = clipboard_win::formats::FileList.read_clipboard(&mut paths);
                }
                for path in paths {
                    self.add_chat_file_path(&path);
                }
            }

            #[cfg(not(target_os = "android"))]
            if self.chat.draft_attachments.len() == before {
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Ok(image) = clipboard.get_image() {
                        let width = image.width as u32;
                        let height = image.height as u32;
                        if let Some(rgba) =
                            image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
                        {
                            let mut bytes = std::io::Cursor::new(Vec::new());
                            if image::DynamicImage::ImageRgba8(rgba)
                                .write_to(&mut bytes, image::ImageFormat::Png)
                                .is_ok()
                            {
                                self.add_chat_image_bytes(
                                    format!("pasted-{}.png", chat::now_millis()),
                                    bytes.into_inner(),
                                );
                            }
                        }
                    }
                }
            }
            if self.chat.draft_attachments.len() > before {
                ctx.input_mut(|input| {
                    input
                        .events
                        .retain(|event| !matches!(event, egui::Event::Paste(_)));
                });
            }
        }
    }

    fn pick_chat_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new().pick_files() {
            for path in paths {
                self.add_chat_file_path(&path);
            }
        }
    }

    fn add_chat_file_path(&mut self, path: &Path) {
        let Ok(metadata) = std::fs::metadata(path) else {
            self.chat.error = Some(format!("Could not open {}", path.display()));
            return;
        };
        if !metadata.is_file() || metadata.len() == 0 {
            self.chat.error = Some("Choose a nonempty file.".to_owned());
            return;
        }
        if metadata.len() <= 8 * 1024 * 1024 {
            if let Ok(bytes) = std::fs::read(path) {
                if image::guess_format(&bytes).is_ok() {
                    self.add_chat_image_bytes(
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("image")
                            .to_owned(),
                        bytes,
                    );
                    return;
                }
                if metadata.len() <= 64 * 1024 && std::str::from_utf8(&bytes).is_ok() {
                    let hash = iroh_blobs::Hash::new(&bytes).to_string();
                    if !self
                        .chat
                        .draft_attachments
                        .iter()
                        .any(|item| item.hash == hash)
                    {
                        self.chat.draft_attachments.push(ChatAttachment {
                            kind: AttachmentKind::InlineFile,
                            id: hash.clone(),
                            name: path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("text.txt")
                                .to_owned(),
                            media_type: "text/plain".to_owned(),
                            byte_len: bytes.len() as u64,
                            width: 0,
                            height: 0,
                            hash,
                            data: Some(Arc::new(bytes)),
                        });
                    }
                    return;
                }
            }
        }
        if !self.chat.draft_files.iter().any(|item| item == path) {
            self.chat.draft_files.push(path.to_owned());
        }
    }

    fn add_chat_image_bytes(&mut self, name: String, bytes: Vec<u8>) {
        let decoded = match image::load_from_memory(&bytes) {
            Ok(decoded) => decoded,
            Err(error) => {
                self.chat.error = Some(format!("{name} is not a supported image: {error}"));
                return;
            }
        };
        let hash = iroh_blobs::Hash::new(&bytes);
        let hash_string = hash.to_string();
        if self
            .chat
            .draft_attachments
            .iter()
            .any(|attachment| attachment.hash == hash_string)
        {
            return;
        }
        let media_type = image::guess_format(&bytes)
            .ok()
            .map(image_media_type)
            .unwrap_or("application/octet-stream")
            .to_owned();
        self.chat.draft_attachments.push(ChatAttachment {
            kind: AttachmentKind::Image,
            id: hash_string.clone(),
            name,
            media_type,
            byte_len: bytes.len() as u64,
            width: decoded.width(),
            height: decoded.height(),
            hash: hash_string,
            data: Some(Arc::new(bytes)),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn ui_file_download_controls(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        conversation_id: &str,
        message: &ChatMessage,
        attachment: &ChatAttachment,
        own: bool,
        receivers: &[String],
    ) {
        let key = (message.message_id.clone(), attachment.hash.clone());
        let transfer = self.chat.file_transfers.get(&key).cloned();
        let owner_offline = NodeId::from_str(&message.author_id)
            .ok()
            .and_then(|owner| self.friend_status.get(&owner))
            .is_some_and(|status| status.availability == Availability::Offline);
        if own {
            if attachment.kind == AttachmentKind::FileOffer {
                let stopped = message.stopped_file_offers.contains(&attachment.hash);
                let active: Vec<_> = self
                    .chat
                    .file_serving
                    .iter()
                    .filter(|((hash, _, _), (_, _, phase))| {
                        hash == &attachment.hash && *phase == chat::FileServingPhase::Sending
                    })
                    .collect();
                let active_progress = active
                    .first()
                    .map(|(_, (position, total, _))| (*position, *total));
                let latest = self
                    .chat
                    .file_serving
                    .iter()
                    .rev()
                    .find(|((hash, _, _), _)| hash == &attachment.hash)
                    .map(|(_, (position, total, phase))| (*position, *total, *phase));
                let status = owner_file_offer_status(
                    stopped,
                    receivers,
                    active.len(),
                    latest.map(|(_, _, phase)| phase),
                );
                let status_detail = if !stopped {
                    active_progress
                        .filter(|(_, total)| *total > 0)
                        .map(|(position, total)| {
                            format!("{} of {} sent", format_bytes(position), format_bytes(total))
                        })
                        .or_else(|| {
                            latest.and_then(|(position, total, phase)| {
                                (phase == chat::FileServingPhase::Interrupted).then(|| {
                                    format!(
                                        "Transfer interrupted after {} of {}",
                                        format_bytes(position),
                                        format_bytes(total)
                                    )
                                })
                            })
                        })
                } else {
                    None
                };
                let mut toggle_sharing = false;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    toggle_sharing = action_button(
                        ui,
                        pal,
                        if stopped { "Resume" } else { "Stop" },
                        ButtonTone::Secondary,
                    )
                    .clicked();
                    if let Some(status) = status {
                        let response = ui.add(
                            egui::Label::new(
                                RichText::new(status)
                                    .color(pal.dim)
                                    .size(ui_font_size(11.0)),
                            )
                            .truncate(),
                        );
                        if let Some(detail) = status_detail.as_deref() {
                            response.on_hover_text(detail);
                        }
                    }
                });
                if toggle_sharing {
                    self.cmd(Command::SetChatFileServing {
                        hash: attachment.hash.clone(),
                        serving: stopped,
                    });
                }
                if !stopped {
                    if let Some((position, total)) = active_progress.filter(|(_, total)| *total > 0)
                    {
                        file_transfer_progress(ui, pal, position, total, "sent");
                    }
                }
            }
            return;
        }
        let stopped = message.stopped_file_offers.contains(&attachment.hash);
        let active = transfer.as_ref().is_some_and(FileTransferUiState::active);
        let saving = transfer
            .as_ref()
            .is_some_and(|state| matches!(state.phase, Some(FileTransferPhase::Saving)));
        let resume_path = transfer.as_ref().and_then(|state| {
            (matches!(state.phase, Some(FileTransferPhase::Paused(_)))
                || matches!(state.result, Some(Err(_))))
            .then(|| state.path.clone())
            .flatten()
        });
        let (status, status_detail) =
            receiver_file_offer_status(transfer.as_ref(), stopped, owner_offline, receivers);

        let mut start_download = false;
        let mut cancel_download = false;
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if !stopped {
                if active {
                    if !saving {
                        cancel_download =
                            action_button(ui, pal, "Cancel", ButtonTone::Secondary).clicked();
                    }
                } else {
                    start_download = action_button(
                        ui,
                        pal,
                        if resume_path.is_some() {
                            "Resume"
                        } else {
                            "Download"
                        },
                        ButtonTone::Secondary,
                    )
                    .clicked();
                    if resume_path.is_some()
                        && action_button(ui, pal, "Discard", ButtonTone::Secondary).clicked()
                    {
                        cancel_download = true;
                    }
                }
            }
            if let Some(status) = status {
                let response = ui.add(
                    egui::Label::new(
                        RichText::new(status)
                            .color(pal.dim)
                            .size(ui_font_size(11.0)),
                    )
                    .truncate(),
                );
                if let Some(detail) = status_detail.as_deref() {
                    response.on_hover_text(detail);
                }
            }
        });
        if let Some(state) = transfer
            .as_ref()
            .filter(|state| state.total > 0 && state.result.is_none())
        {
            file_transfer_progress(ui, pal, state.received, state.total, "received");
        }
        if cancel_download {
            self.cmd(Command::CancelChatFile {
                message_id: message.message_id.clone(),
                hash: attachment.hash.clone(),
            });
        }
        if start_download {
            let path = resume_path.clone().or_else(|| {
                rfd::FileDialog::new()
                    .set_file_name(&attachment.name)
                    .save_file()
            });
            if let Some(path) = path {
                self.chat.file_transfers.insert(
                    key,
                    FileTransferUiState {
                        path: Some(path.clone()),
                        received: transfer.as_ref().map_or(0, |state| state.received),
                        total: attachment.byte_len,
                        phase: Some(FileTransferPhase::Connecting),
                        result: None,
                    },
                );
                self.cmd(Command::ReceiveChatFile {
                    conversation_id: conversation_id.to_owned(),
                    message_id: message.message_id.clone(),
                    hash: attachment.hash.clone(),
                    path,
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ui_message_attachments(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        conversation_id: &str,
        message: &ChatMessage,
        own: bool,
        opacity: f32,
        requested_restore: &mut bool,
        requested_deletion: &mut Option<DeleteScope>,
    ) {
        for (index, attachment) in message.attachments.iter().enumerate() {
            let receivers = message
                .file_receivers
                .get(&attachment.hash)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| NodeId::from_str(id).ok())
                        .map(|id| self.peer_display_name(id))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut downloadable_attachment = attachment.clone();
            if downloadable_attachment.data.is_none() {
                downloadable_attachment.data = self.chat.attachment_textures.data(&attachment.id);
            }
            if index > 0 || !message.body.trim().is_empty() {
                ui.add_space(6.0);
            }
            if attachment.kind == AttachmentKind::FileOffer {
                let response = Frame::new()
                    .fill(pal.panel2.gamma_multiply(opacity))
                    .stroke(Stroke::new(1.0_f32, pal.line.gamma_multiply(opacity)))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width().min(FILE_OFFER_CARD_MAX_WIDTH));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&attachment.name).color(pal.text));
                            ui.label(
                                RichText::new(format_bytes(attachment.byte_len))
                                    .color(pal.dim)
                                    .size(ui_font_size(11.0)),
                            );
                        });
                        self.ui_file_download_controls(
                            ui,
                            pal,
                            conversation_id,
                            message,
                            attachment,
                            own,
                            &receivers,
                        );
                    })
                    .response;
                response.context_menu(|ui| {
                    chat_message_context_menu(
                        ui,
                        pal,
                        message,
                        own,
                        requested_restore,
                        requested_deletion,
                    );
                });
                continue;
            }
            if !self
                .chat_retention
                .includes(message.sent_at, chat::now_millis())
            {
                ui.label(RichText::new(format!("{} · expired", attachment.name)).color(pal.dim));
                if !receivers.is_empty() {
                    ui.label(
                        RichText::new(format!("Received by {}", receivers.join(", ")))
                            .color(pal.dim)
                            .size(ui_font_size(11.0)),
                    );
                }
                continue;
            }
            if attachment.kind == AttachmentKind::InlineFile {
                let data = attachment
                    .data
                    .clone()
                    .or_else(|| self.chat.inline_file_data.get(&attachment.hash).cloned());
                let response = Frame::new()
                    .fill(pal.panel2.gamma_multiply(opacity))
                    .stroke(Stroke::new(1.0_f32, pal.line.gamma_multiply(opacity)))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                attachment.name,
                                format_bytes(attachment.byte_len)
                            ))
                            .color(pal.text),
                        );
                        if !receivers.is_empty() {
                            ui.label(
                                RichText::new(format!("Received by {}", receivers.join(", ")))
                                    .color(pal.dim)
                                    .size(ui_font_size(11.0)),
                            );
                        }
                        if let Some(data) = data {
                            if let Ok(contents) = std::str::from_utf8(&data) {
                                let preview: String = contents.chars().take(2_000).collect();
                                egui::ScrollArea::vertical()
                                    .max_height(160.0)
                                    .show(ui, |ui| {
                                        ui.label(
                                            RichText::new(preview)
                                                .monospace()
                                                .color(pal.text2)
                                                .size(ui_font_size(11.0)),
                                        );
                                    });
                            }
                            if action_button(ui, pal, "Save", ButtonTone::Secondary).clicked() {
                                let mut copy = attachment.clone();
                                copy.data = Some(data);
                                save_chat_attachment(&copy);
                            }
                        } else {
                            if self
                                .chat
                                .attachment_requests
                                .insert(attachment.hash.clone())
                            {
                                self.cmd(Command::LoadChatAttachment {
                                    conversation_id: conversation_id.to_owned(),
                                    hash: attachment.hash.clone(),
                                    byte_len: attachment.byte_len,
                                });
                            }
                            ui.label(RichText::new("Loading text file…").color(pal.dim));
                        }
                    })
                    .response;
                response.context_menu(|ui| {
                    chat_message_context_menu(
                        ui,
                        pal,
                        message,
                        own,
                        requested_restore,
                        requested_deletion,
                    );
                });
                continue;
            }
            if self
                .max_image_bytes
                .is_some_and(|limit| attachment.byte_len > limit)
            {
                let response = Frame::new()
                    .fill(pal.panel2.gamma_multiply(opacity))
                    .stroke(Stroke::new(1.0_f32, pal.line.gamma_multiply(opacity)))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!(
                                "Image is too big ({})",
                                format_bytes(attachment.byte_len)
                            ))
                            .color(pal.dim.gamma_multiply(opacity))
                            .size(ui_font_size(11.5)),
                        );
                        self.ui_file_download_controls(
                            ui,
                            pal,
                            conversation_id,
                            message,
                            attachment,
                            own,
                            &receivers,
                        );
                    })
                    .response;
                response.context_menu(|ui| {
                    chat_message_context_menu(
                        ui,
                        pal,
                        message,
                        own,
                        requested_restore,
                        requested_deletion,
                    );
                });
                continue;
            }
            let Some(texture) =
                attachment_texture(ui.ctx(), &mut self.chat.attachment_textures, attachment)
            else {
                if attachment.data.is_none()
                    && self
                        .chat
                        .attachment_requests
                        .insert(attachment.hash.clone())
                {
                    self.cmd(Command::LoadChatAttachment {
                        conversation_id: conversation_id.to_owned(),
                        hash: attachment.hash.clone(),
                        byte_len: attachment.byte_len,
                    });
                }
                let response = ui.label(
                    RichText::new(format!(
                        "Loading image… ({})",
                        format_bytes(attachment.byte_len)
                    ))
                    .color(pal.dim.gamma_multiply(opacity)),
                );
                response.context_menu(|ui| {
                    chat_message_context_menu(
                        ui,
                        pal,
                        message,
                        own,
                        requested_restore,
                        requested_deletion,
                    );
                });
                continue;
            };
            let scale = (ui.available_width().min(620.0) / attachment.width as f32)
                .min(180.0 / attachment.height as f32);
            let size = Vec2::new(
                attachment.width as f32 * scale,
                attachment.height as f32 * scale,
            );
            let response = ui.add(
                egui::Image::new((texture.id(), size))
                    .fit_to_exact_size(size)
                    .sense(egui::Sense::click())
                    .corner_radius(8.0),
            );
            if response.clicked() {
                self.chat.image_preview = Some(ImagePreview {
                    attachment: attachment.clone(),
                    draft: false,
                    mode: ImagePreviewMode::Floating,
                });
            }
            response.context_menu(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if menu_item_button(ui, pal, Icon::Download, "Download image", false).clicked() {
                    save_chat_attachment(&downloadable_attachment);
                    ui.close();
                }
                ui.separator();
                chat_message_context_menu(
                    ui,
                    pal,
                    message,
                    own,
                    requested_restore,
                    requested_deletion,
                );
            });
            if !receivers.is_empty() {
                ui.label(
                    RichText::new(format!("Received by {}", receivers.join(", ")))
                        .color(pal.dim)
                        .size(ui_font_size(11.0)),
                );
            }
        }
    }

    fn ui_image_preview(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(preview) = self.chat.image_preview.clone() else {
            return;
        };
        let pane_rect = self.pane_constrain_rect();
        let mut action = None;
        match preview.mode {
            ImagePreviewMode::Floating => {
                let mut open = true;
                egui::Window::new("Image preview")
                    .id(egui::Id::new("chat-image-preview"))
                    .open(&mut open)
                    .collapsible(false)
                    .resizable(true)
                    .constrain_to(pane_rect)
                    .min_size(IMAGE_PREVIEW_MIN_SIZE)
                    .default_size(image_preview_default_size(pane_rect.size()))
                    .default_pos(pane_rect.center())
                    .show(ctx, |ui| {
                        action = image_preview_panel(
                            ui,
                            pal,
                            &preview,
                            &mut self.chat.attachment_textures,
                        );
                    });
                if !open {
                    action = Some(ImagePreviewAction::Close);
                }
            }
            ImagePreviewMode::FillWindow | ImagePreviewMode::Fullscreen => {
                let rect = if preview.mode == ImagePreviewMode::Fullscreen {
                    ctx.viewport_rect()
                } else {
                    ctx.content_rect()
                };
                egui::Area::new(egui::Id::new("chat-image-preview-immersive"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(rect.min)
                    .show(ctx, |ui| {
                        ui.set_min_size(rect.size());
                        Frame::new()
                            .fill(pal.bg)
                            .inner_margin(egui::Margin::symmetric(18, 14))
                            .show(ui, |ui| {
                                ui.set_min_size(rect.size() - Vec2::new(36.0, 28.0));
                                action = image_preview_panel(
                                    ui,
                                    pal,
                                    &preview,
                                    &mut self.chat.attachment_textures,
                                );
                            });
                    });
            }
        }

        match action {
            Some(ImagePreviewAction::Close) => {
                if preview.mode == ImagePreviewMode::Fullscreen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
                self.chat.image_preview = None;
            }
            Some(ImagePreviewAction::Delete) => {
                if preview.mode == ImagePreviewMode::Fullscreen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
                self.chat
                    .draft_attachments
                    .retain(|attachment| attachment.id != preview.attachment.id);
                self.chat.image_preview = None;
            }
            Some(ImagePreviewAction::SetMode(mode)) => {
                if preview.mode == ImagePreviewMode::Fullscreen
                    && mode != ImagePreviewMode::Fullscreen
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                } else if mode == ImagePreviewMode::Fullscreen
                    && preview.mode != ImagePreviewMode::Fullscreen
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                }
                if let Some(current) = &mut self.chat.image_preview {
                    current.mode = mode;
                }
            }
            None => {}
        }
    }

    fn ui_group_editor(&mut self, ctx: &egui::Context, pal: &Palette) {
        let mut open = self.chat.show_group_editor;
        egui::Window::new("Create group")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .constrain_to(self.pane_constrain_rect())
            .min_width(340.0)
            .show(ctx, |ui| {
                ui.label("Group name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.chat.group_name)
                        .hint_text("Weekend plans")
                        .desired_width(300.0),
                );
                ui.add_space(8.0);
                ui.label("Members");
                for friend in self.friends.clone() {
                    let Ok(node) = NodeId::from_str(friend.node_id.trim()) else {
                        continue;
                    };
                    let mut selected = self.chat.group_members.contains(&node);
                    if ui.checkbox(&mut selected, friend.name).changed() {
                        if selected {
                            self.chat.group_members.insert(node);
                        } else {
                            self.chat.group_members.remove(&node);
                        }
                    }
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if action_button(ui, pal, "Cancel", ButtonTone::Secondary).clicked() {
                        self.chat.show_group_editor = false;
                    }
                    if action_button(ui, pal, "Create", ButtonTone::Primary).clicked() {
                        let title = self.chat.group_name.trim().to_owned();
                        let members = self.chat.group_members.iter().copied().collect();
                        if title.is_empty() || self.chat.group_members.is_empty() {
                            self.chat.error = Some(
                                "Give the group a name and choose at least one friend.".to_owned(),
                            );
                        } else {
                            self.cmd(Command::CreateGroupChat { title, members });
                            self.chat.group_name.clear();
                            self.chat.group_members.clear();
                            self.chat.show_group_editor = false;
                        }
                    }
                });
            });
        self.chat.show_group_editor &= open;
    }

    fn ui_group_members(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(conversation) = self
            .chat
            .selected
            .as_ref()
            .and_then(|id| self.chat.conversations.get(id))
            .filter(|conversation| matches!(conversation.kind, ConversationKind::Group))
            .cloned()
        else {
            self.chat.show_group_members = false;
            return;
        };
        let members = self.group_members_for(&conversation);
        let mut open = self.chat.show_group_members;
        let mut friend_to_add = None;
        egui::Window::new("Group members")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .constrain_to(self.pane_constrain_rect())
            .min_width(340.0)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label(
                    RichText::new(&conversation.title)
                        .color(pal.text)
                        .size(ui_font_size(14.0)),
                );
                ui.label(
                    RichText::new(format!(
                        "{} {}",
                        members.len(),
                        if members.len() == 1 {
                            "member"
                        } else {
                            "members"
                        }
                    ))
                    .color(pal.dim)
                    .size(ui_font_size(11.5)),
                );
                ui.add_space(10.0);
                for member in &members {
                    // Resolve the avatar before painting (fetch is cool-down
                    // guarded so this stays cheap per frame).
                    let member_avatar = if member.kind == GroupMemberKind::You {
                        self.own_avatar_texture(ctx)
                    } else if let Some(peer) = member.node_id {
                        self.peer_avatar_texture(ctx, peer)
                    } else {
                        None
                    };
                    ui.horizontal(|ui| {
                        let initial = crate::profile::display_name_initial(&member.text)
                            .unwrap_or_else(|| "?".to_owned());
                        paint_profile_avatar(ui, pal, member_avatar, &initial, 28.0);
                        let member_color = if member.kind == GroupMemberKind::You {
                            accent_color_for(self.own_accent_color.as_deref(), pal.text)
                        } else if let Some(peer) = member.node_id {
                            accent_color_for(self.peer_accent_hex(peer), pal.text)
                        } else {
                            pal.text
                        };
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(&member.text)
                                    .color(member_color)
                                    .size(ui_font_size(13.0)),
                            );
                            ui.label(
                                RichText::new(match member.kind {
                                    GroupMemberKind::You => "You",
                                    GroupMemberKind::Friend => "Friend",
                                    GroupMemberKind::Unknown => "Unknown ID",
                                })
                                .color(pal.dim)
                                .size(ui_font_size(10.5)),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some(node_id) = member.node_id {
                                if chat_lucide_icon_button(ui, pal, Icon::Copy)
                                    .on_hover_text("Copy node ID")
                                    .clicked()
                                {
                                    copy_to_clipboard(&node_id.to_string());
                                }
                                if member.kind == GroupMemberKind::Unknown
                                    && action_button(ui, pal, "Add friend", ButtonTone::Primary)
                                        .on_hover_text("Save this member using their full node ID")
                                        .clicked()
                                {
                                    friend_to_add = Some(node_id);
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
            });
        self.chat.show_group_members = open;
        if let Some(peer) = friend_to_add {
            self.chat.friend_candidate = Some(peer);
            self.chat.friend_candidate_name.clear();
        }
    }

    fn ui_add_chat_friend(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(peer) = self.chat.friend_candidate else {
            return;
        };
        let mut open = true;
        let mut add = false;
        let mut cancel = false;
        egui::Window::new("Add friend")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .constrain_to(self.pane_constrain_rect())
            .min_width(340.0)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label(
                    RichText::new(format!("Peer {}", peer.fmt_short()))
                        .monospace()
                        .color(pal.text2),
                );
                ui.label(
                    RichText::new("The complete node ID will be saved automatically.")
                        .color(pal.dim)
                        .size(ui_font_size(11.5)),
                );
                ui.add_space(10.0);
                ui.label(RichText::new("Name").color(pal.text2));
                let name = ui.add(
                    egui::TextEdit::singleline(&mut self.chat.friend_candidate_name)
                        .hint_text(format!("Peer {}", peer.fmt_short()))
                        .desired_width(ui.available_width()),
                );
                if name.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    add = true;
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if action_button(ui, pal, "Add friend", ButtonTone::Primary).clicked() {
                        add = true;
                    }
                    if action_button(ui, pal, "Cancel", ButtonTone::Secondary).clicked() {
                        cancel = true;
                    }
                    if action_button(ui, pal, "Copy ID", ButtonTone::Secondary).clicked() {
                        copy_to_clipboard(&peer.to_string());
                    }
                });
            });

        if add {
            let name = if self.chat.friend_candidate_name.trim().is_empty() {
                format!("Peer {}", peer.fmt_short())
            } else {
                self.chat.friend_candidate_name.trim().to_owned()
            };
            self.add_friend_record(peer, name);
            self.chat.friend_candidate = None;
            self.chat.friend_candidate_name.clear();
        } else if cancel || !open {
            self.chat.friend_candidate = None;
            self.chat.friend_candidate_name.clear();
        }
    }
}

fn composer_visual_rows(text: &str, width: f32) -> usize {
    let columns = (width / 7.2).floor().max(12.0) as usize;
    text.split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(columns))
        .sum::<usize>()
        .clamp(1, 8)
}

fn image_media_type(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Bmp => "image/bmp",
        image::ImageFormat::Tiff => "image/tiff",
        image::ImageFormat::Ico => "image/x-icon",
        image::ImageFormat::Pnm => "image/x-portable-anymap",
        image::ImageFormat::Qoi => "image/qoi",
        image::ImageFormat::Tga => "image/x-tga",
        _ => "application/octet-stream",
    }
}

fn attachment_texture<'a>(
    ctx: &egui::Context,
    textures: &'a mut AttachmentTextureCache,
    attachment: &ChatAttachment,
) -> Option<&'a egui::TextureHandle> {
    textures.get_or_insert(ctx, attachment)
}

fn square_attachment_preview(
    ui: &mut Ui,
    pal: &Palette,
    texture: &egui::TextureHandle,
    width: u32,
    height: u32,
    side: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), egui::Sense::click());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(7), pal.panel2);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(7),
        Stroke::new(1.0_f32, pal.line),
        egui::StrokeKind::Inside,
    );
    let scale = (side / width.max(1) as f32).min(side / height.max(1) as f32);
    let image_size = Vec2::new(width as f32 * scale, height as f32 * scale);
    let image_rect = egui::Rect::from_center_size(rect.center(), image_size);
    ui.put(
        image_rect,
        egui::Image::new((texture.id(), image_size))
            .fit_to_exact_size(image_size)
            .corner_radius(5.0),
    );
    response
}

fn image_preview_panel(
    ui: &mut Ui,
    pal: &Palette,
    preview: &ImagePreview,
    textures: &mut AttachmentTextureCache,
) -> Option<ImagePreviewAction> {
    let mut action = None;
    let mut downloadable_attachment = preview.attachment.clone();
    if downloadable_attachment.data.is_none() {
        downloadable_attachment.data = textures.data(&preview.attachment.id);
    }
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{} × {} · {}",
                preview.attachment.width,
                preview.attachment.height,
                format_bytes(preview.attachment.byte_len)
            ))
            .color(pal.dim),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if toolbar_button(ui, pal, Icon::X, "Close", false).clicked() {
                action = Some(ImagePreviewAction::Close);
            }
            if preview.draft && toolbar_button(ui, pal, Icon::Trash2, "Delete", false).clicked() {
                action = Some(ImagePreviewAction::Delete);
            }
            if toolbar_button(ui, pal, Icon::Download, "Download", false).clicked() {
                save_chat_attachment(&downloadable_attachment);
            }
            if toolbar_button(
                ui,
                pal,
                Icon::Fullscreen,
                "Fullscreen",
                preview.mode == ImagePreviewMode::Fullscreen,
            )
            .clicked()
            {
                action = Some(ImagePreviewAction::SetMode(
                    if preview.mode == ImagePreviewMode::Fullscreen {
                        ImagePreviewMode::Floating
                    } else {
                        ImagePreviewMode::Fullscreen
                    },
                ));
            }
            if toolbar_button(
                ui,
                pal,
                Icon::Expand,
                "Fill window",
                preview.mode == ImagePreviewMode::FillWindow,
            )
            .clicked()
            {
                action = Some(ImagePreviewAction::SetMode(
                    if preview.mode == ImagePreviewMode::FillWindow {
                        ImagePreviewMode::Floating
                    } else {
                        ImagePreviewMode::FillWindow
                    },
                ));
            }
            if preview.mode != ImagePreviewMode::Floating
                && toolbar_button(ui, pal, Icon::Minimize2, "Floating", false).clicked()
            {
                action = Some(ImagePreviewAction::SetMode(ImagePreviewMode::Floating));
            }
        });
    });
    ui.separator();
    if let Some(texture) = attachment_texture(ui.ctx(), textures, &preview.attachment) {
        let available = ui.available_size().max(Vec2::splat(1.0));
        let scale = (available.x / preview.attachment.width.max(1) as f32)
            .min(available.y / preview.attachment.height.max(1) as f32);
        let size = Vec2::new(
            preview.attachment.width as f32 * scale,
            preview.attachment.height as f32 * scale,
        );
        ui.centered_and_justified(|ui| {
            ui.add(egui::Image::new((texture.id(), size)).fit_to_exact_size(size));
        });
    }
    action
}

fn save_chat_attachment(attachment: &ChatAttachment) {
    let Some(data) = &attachment.data else {
        return;
    };
    let mut dialog = rfd::FileDialog::new().set_file_name(&attachment.name);
    if let Some(extension) = Path::new(&attachment.name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        dialog = dialog.add_filter("Image", &[extension]);
    }
    if let Some(path) = dialog.save_file() {
        if let Err(error) = std::fs::write(&path, data.as_slice()) {
            warn!("failed to save image to {}: {error}", path.display());
        }
    }
}

/// Smallest image preview window size once the user can resize it.
const IMAGE_PREVIEW_MIN_SIZE: Vec2 = Vec2::new(320.0, 220.0);

/// Opening size for the floating image preview: never larger than 90% of the
/// viewport so it fits without re-clamping, never below its resize floor.
fn image_preview_default_size(viewport: Vec2) -> Vec2 {
    Vec2::new(720.0, 560.0)
        .min(viewport * 0.9)
        .max(IMAGE_PREVIEW_MIN_SIZE)
}

fn chat_send_button(ui: &mut Ui, pal: &Palette) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(38.0), egui::Sense::click());
    let fill = if response.hovered() {
        pal.text2
    } else {
        pal.text
    };
    ui.painter().circle_filled(rect.center(), 19.0, fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(Icon::ArrowUp),
        lucide(19.0),
        pal.bg,
    );
    response.on_hover_text("Send message")
}

fn composer_ghost_icon_button(
    ui: &mut Ui,
    pal: &Palette,
    icon: Icon,
    label: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(36.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(9), pal.panel2);
    }
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        char::from(icon),
        lucide(20.0),
        pal.text2,
    );
    response.on_hover_text(label)
}

fn chat_delivery_opacity(state: DeliveryState) -> f32 {
    if matches!(
        state,
        DeliveryState::Pending | DeliveryState::Queued | DeliveryState::Retrying
    ) {
        0.58
    } else {
        1.0
    }
}

fn chat_message_body_text(message: &ChatMessage, pal: &Palette, opacity: f32) -> RichText {
    let body = RichText::new(match message.deletion {
        Some(MessageDeletion::Local) => "You deleted this message",
        Some(MessageDeletion::Everyone) => "This message was deleted",
        None => message.body.as_str(),
    })
    .color(pal.text.gamma_multiply(opacity))
    .size(ui_font_size(13.0));
    if message.deletion.is_some() {
        body.italics()
    } else {
        body
    }
}

fn chat_message_context_menu(
    ui: &mut Ui,
    pal: &Palette,
    message: &ChatMessage,
    own: bool,
    requested_restore: &mut bool,
    requested_deletion: &mut Option<DeleteScope>,
) {
    ui.spacing_mut().item_spacing.y = 2.0;
    if message.deletion == Some(MessageDeletion::Local) {
        if menu_item_button(ui, pal, Icon::RotateCcw, "Restore message", false).clicked() {
            *requested_restore = true;
            ui.close();
        }
    } else if message.deletion.is_none() {
        let (label, scope) = if own {
            ("Delete for everyone", DeleteScope::Everyone)
        } else {
            ("Delete for me", DeleteScope::Local)
        };
        if menu_item_button(ui, pal, Icon::Trash2, label, true).clicked() {
            *requested_deletion = Some(scope);
            ui.close();
        }
    }
}

/// Compact delivery affordance next to a message. Hover shows the detail string.
fn chat_delivery_status_icon(
    ui: &mut Ui,
    pal: &Palette,
    state: DeliveryState,
    detail: Option<&str>,
    opacity: f32,
) {
    let (icon, color, tip) = match state {
        DeliveryState::Delivered => return,
        DeliveryState::Pending => (Icon::Loader, pal.dim2, "Sending…"),
        DeliveryState::Retrying => (
            Icon::RefreshCw,
            pal.dim2,
            detail.unwrap_or("Retrying delivery…"),
        ),
        DeliveryState::Queued => (
            Icon::CloudOff,
            pal.dim2,
            detail.unwrap_or("Queued — waiting for peer"),
        ),
        DeliveryState::Failed => (Icon::CircleX, pal.err, detail.unwrap_or("Failed to send")),
    };
    let response = ui.label(
        RichText::new(char::from(icon))
            .font(lucide(12.0))
            .color(color.gamma_multiply(opacity)),
    );
    response.on_hover_text(tip);
}

fn format_chat_time(sent_at: i64) -> String {
    let total_minutes = sent_at.div_euclid(60_000);
    let hour = total_minutes.div_euclid(60).rem_euclid(24);
    let minute = total_minutes.rem_euclid(60);
    format!("{hour:02}:{minute:02}")
}

/// Discord-style group header timestamp: time for today's messages, day +
/// time for yesterday, full date for anything older. Day boundaries use UTC
/// day numbers, matching `format_chat_time`'s naive UTC clock.
fn format_chat_timestamp(sent_at: i64, now_ms: i64) -> String {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let time = format_chat_time(sent_at);
    let day = sent_at.div_euclid(DAY_MS);
    let today = now_ms.div_euclid(DAY_MS);
    match today - day {
        0 => format!("Today at {time}"),
        1 => format!("Yesterday at {time}"),
        _ => {
            //naive UTC calendar date (no timezone database on this stack).
            let days = day;
            // Convert days-since-epoch to Y/M/D via Howard Hinnant's algorithm.
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z.rem_euclid(146_097);
            let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
            let mut year = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let month = if mp < 10 { mp + 3 } else { mp - 9 };
            year += i64::from(month <= 2);
            let date_day = doy - (153 * mp + 2) / 5 + 1;
            format!("{month}/{date_day}/{year} {time}")
        }
    }
}

fn file_transfer_progress(ui: &mut Ui, pal: &Palette, position: u64, total: u64, direction: &str) {
    ui.scope(|ui| {
        ui.visuals_mut().extreme_bg_color = pal.line_br;
        ui.add(
            egui::ProgressBar::new(position.min(total) as f32 / total as f32)
                .desired_width(ui.available_width())
                .desired_height(6.0)
                .corner_radius(3.0)
                .fill(pal.accent),
        )
        .on_hover_text(format!(
            "{} of {} {direction}",
            format_bytes(position),
            format_bytes(total)
        ));
    });
}

fn receiver_file_offer_status(
    transfer: Option<&FileTransferUiState>,
    stopped: bool,
    owner_offline: bool,
    receivers: &[String],
) -> (Option<String>, Option<String>) {
    if stopped {
        return (
            Some("Unavailable".to_owned()),
            Some("Owner stopped sharing this file".to_owned()),
        );
    }
    if let Some(state) = transfer {
        return match (&state.result, &state.phase) {
            (Some(Ok(path)), _) => (
                Some("Saved".to_owned()),
                Some(format!("Saved to {}", path.display())),
            ),
            (Some(Err(error)), _) => (Some("Paused".to_owned()), Some(error.to_string())),
            (_, Some(FileTransferPhase::Paused(reason))) => {
                (Some("Paused".to_owned()), Some(reason.clone()))
            }
            (_, Some(FileTransferPhase::Connecting | FileTransferPhase::Reconnecting))
                if owner_offline =>
            {
                (
                    Some("Waiting for owner".to_owned()),
                    Some("The owner is offline; Wire will keep retrying".to_owned()),
                )
            }
            (_, Some(FileTransferPhase::Connecting)) => (Some("Connecting".to_owned()), None),
            (_, Some(FileTransferPhase::Downloading)) => (Some("Downloading".to_owned()), None),
            (_, Some(FileTransferPhase::Reconnecting)) => (
                Some("Reconnecting".to_owned()),
                Some("Connection stalled; Wire is retrying".to_owned()),
            ),
            (_, Some(FileTransferPhase::Saving)) => (Some("Saving".to_owned()), None),
            _ => (Some("Preparing".to_owned()), None),
        };
    }
    if !receivers.is_empty() {
        return (Some(format!("Received by {}", receivers.join(", "))), None);
    }
    if owner_offline {
        return (Some("Owner offline".to_owned()), None);
    }
    (None, None)
}

fn owner_file_offer_status(
    stopped: bool,
    receivers: &[String],
    active_transfers: usize,
    latest_phase: Option<chat::FileServingPhase>,
) -> Option<String> {
    let receipt = (!receivers.is_empty()).then(|| format!("Received by {}", receivers.join(", ")));
    if stopped {
        return receipt;
    }
    if active_transfers > 0 {
        return Some(if active_transfers == 1 {
            "Sending to 1 peer".to_owned()
        } else {
            format!("Sending to {active_transfers} peers")
        });
    }
    if receipt.is_some() {
        return receipt;
    }
    match latest_phase {
        Some(chat::FileServingPhase::Sent) => Some("Sent to peer".to_owned()),
        Some(chat::FileServingPhase::Interrupted) => Some("Interrupted".to_owned()),
        Some(chat::FileServingPhase::Sending) => Some("Sending to peer".to_owned()),
        None => None,
    }
}

fn messages_share_compact_group(previous: &ChatMessage, current: &ChatMessage) -> bool {
    previous.author_id == current.author_id
        && previous.sent_at.div_euclid(60_000) == current.sent_at.div_euclid(60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_preview_default_fits_small_viewports_without_dipping_below_floor() {
        assert_eq!(
            image_preview_default_size(Vec2::new(1600.0, 900.0)),
            Vec2::new(720.0, 560.0)
        );
        assert_eq!(
            image_preview_default_size(Vec2::new(460.0, 500.0)),
            Vec2::new(414.0, 450.0)
        );
        assert_eq!(
            image_preview_default_size(Vec2::new(200.0, 200.0)),
            IMAGE_PREVIEW_MIN_SIZE
        );
    }

    #[test]
    fn chat_timestamps_read_like_discord() {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        const HOUR: i64 = 60 * 60 * 1000;
        const MINUTE: i64 = 60 * 1000;
        // Epoch day itself.
        assert_eq!(format_chat_timestamp(0, 0), "Today at 00:00");
        assert_eq!(format_chat_timestamp(0, DAY), "Yesterday at 00:00");
        assert_eq!(format_chat_timestamp(0, 2 * DAY), "1/1/1970 00:00");
        // Same-day times stay short; the header only gains a date when old.
        let today = 10 * DAY + 19 * HOUR + 51 * MINUTE;
        assert_eq!(
            format_chat_timestamp(10 * DAY + 8 * HOUR, today),
            "Today at 08:00"
        );
        assert_eq!(
            format_chat_timestamp(9 * DAY + 8 * HOUR, today),
            "Yesterday at 08:00"
        );
        assert_eq!(
            format_chat_timestamp(8 * DAY + 8 * HOUR, today),
            "1/9/1970 08:00"
        );
    }

    #[test]
    fn compact_row_probes_avatar_name_and_body_geometry() {
        // Headless probe mirroring `ui_compact_chat_message`'s skeleton:
        // reports where the avatar allocation, name label, and body label
        // actually land so alignment regressions show up as numbers.
        let context = egui::Context::default();
        let mut tops = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let _ = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.allocate_ui_with_layout(
                        Vec2::new(52.0, 40.0),
                        Layout::left_to_right(Align::Min),
                        |ui| {
                            ui.allocate_ui_with_layout(
                                Vec2::new(40.0, 40.0),
                                Layout::top_down(Align::Min),
                                |ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    // Mirrors the optical nudge in
                                    // `ui_compact_chat_message`: allocation
                                    // stays top-aligned, paint drops 3px.
                                    ui.add_space(3.0);
                                    let (rect, _) = ui.allocate_exact_size(
                                        Vec2::splat(40.0),
                                        egui::Sense::hover(),
                                    );
                                    tops.0 = rect.min.y;
                                    tops.5 = rect.min.x;
                                },
                            );
                            ui.add_space(12.0);
                        },
                    );
                    ui.allocate_ui_with_layout(
                        Vec2::new(300.0, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.with_layout(Layout::left_to_right(Align::Max), |ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                let name_rect = ui
                                    .label(
                                        RichText::new("Macbook")
                                            .strong()
                                            .size(16.0),
                                    )
                                    .rect;
                                tops.1 = name_rect.min.y;
                                tops.4 = name_rect.min.x;
                                ui.label(
                                    RichText::new("8/16/2026 18:17").size(13.0),
                                );
                            });
                            ui.add_space(1.0);
                            let body_rect = ui
                                .add(
                                    egui::Label::new(
                                        RichText::new("jfg").size(15.0),
                                    )
                                    .wrap(),
                                )
                                .rect;
                            tops.2 = body_rect.min.y;
                            tops.3 = body_rect.min.x;
                        },
                    );
                });
            });
        });
        let (avatar_top, name_top, body_top, body_left, name_left, avatar_left) = tops;
        eprintln!("avatar_top={avatar_top} name_top={name_top} body_top={body_top} body_left={body_left} name_left={name_left} avatar_left={avatar_left}");
        assert!(
            (avatar_top - name_top - 3.0).abs() < 0.6,
            "avatar paint must sit 3px below the name row top (cap-top alignment)"
        );
        assert!(
            (name_left - avatar_left - 52.0).abs() < 0.6,
            "content column must start a full 52px gutter after the avatar"
        );
        assert!(
            (body_left - name_left).abs() < 0.6,
            "body must start at the name's x"
        );
        assert!(
            body_top > name_top,
            "body must sit below the name row"
        );
    }

    #[test]
    fn compact_chat_groups_only_same_author_in_same_minute() {
        let message = |author: &str, sent_at| ChatMessage {
            version: 1,
            message_id: format!("{author}-{sent_at}"),
            author_id: author.to_owned(),
            sent_at,
            body: "hello".to_owned(),
            nonce: 0,
            client_version: None,
            author_display_name: None,
            author_avatar_hash: None,
            author_accent_color: None,
            attachments: Vec::new(),
            file_receivers: std::collections::BTreeMap::new(),
            stopped_file_offers: std::collections::BTreeSet::new(),
            deletion: None,
        };

        assert!(messages_share_compact_group(
            &message("alice", 60_001),
            &message("alice", 119_999),
        ));
        assert!(!messages_share_compact_group(
            &message("alice", 119_999),
            &message("alice", 120_000),
        ));
        assert!(!messages_share_compact_group(
            &message("alice", 60_001),
            &message("bob", 60_002),
        ));
    }

    #[test]
    fn composer_grows_for_newlines_and_wrapped_text() {
        assert_eq!(composer_visual_rows("one line", 500.0), 1);
        assert_eq!(composer_visual_rows("one\ntwo\nthree\nfour", 500.0), 4);
        assert!(composer_visual_rows(&"x".repeat(200), 140.0) > 3);
        assert_eq!(composer_visual_rows(&"x".repeat(10_000), 140.0), 8);
    }

    #[test]
    fn stopped_file_offer_prefers_receipt_and_hides_stale_activity() {
        let receivers = vec!["David".to_owned()];
        assert_eq!(
            owner_file_offer_status(true, &receivers, 0, Some(chat::FileServingPhase::Sent),),
            Some("Received by David".to_owned())
        );
        assert_eq!(
            owner_file_offer_status(true, &[], 0, Some(chat::FileServingPhase::Interrupted),),
            None
        );
    }

    #[test]
    fn active_file_offer_status_takes_priority_until_receipt_arrives() {
        let receivers = vec!["David".to_owned()];
        assert_eq!(
            owner_file_offer_status(false, &receivers, 1, Some(chat::FileServingPhase::Sending)),
            Some("Sending to 1 peer".to_owned())
        );
        assert_eq!(
            owner_file_offer_status(false, &receivers, 0, Some(chat::FileServingPhase::Sent)),
            Some("Received by David".to_owned())
        );
    }

    #[test]
    fn receiver_file_offer_status_keeps_recovery_details_out_of_the_primary_row() {
        let transfer = FileTransferUiState {
            path: Some(PathBuf::from("archive.zip")),
            received: 64,
            total: 100,
            phase: Some(FileTransferPhase::Paused("connection lost".to_owned())),
            result: None,
        };
        assert_eq!(
            receiver_file_offer_status(Some(&transfer), false, false, &[]),
            (
                Some("Paused".to_owned()),
                Some("connection lost".to_owned())
            )
        );
        assert_eq!(
            receiver_file_offer_status(Some(&transfer), true, false, &[]),
            (
                Some("Unavailable".to_owned()),
                Some("Owner stopped sharing this file".to_owned())
            )
        );
    }

    #[test]
    fn receiver_file_offer_status_uses_receipt_as_the_idle_fallback() {
        let receivers = vec!["David".to_owned()];
        assert_eq!(
            receiver_file_offer_status(None, false, false, &receivers),
            (Some("Received by David".to_owned()), None)
        );
        assert_eq!(
            receiver_file_offer_status(None, false, true, &[]),
            (Some("Owner offline".to_owned()), None)
        );
    }
}
