//! Confirmation step for destructive actions that cannot be undone.

use super::{
    widgets::{dialog_body, dialog_footer, dialog_window, floating_dialog_header},
    AppState,
};
use crate::theme::{action_button, ui_font_size, ButtonTone, Palette};
use egui::RichText;

/// A destructive action waiting for the user to confirm it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PendingConfirm {
    /// Deletes the conversation's history for every member.
    ClearChatHistory {
        conversation_id: String,
        title: String,
    },
    /// Removes a saved friend (by the stored node ID string).
    RemoveFriend { node_id: String, name: String },
}

impl PendingConfirm {
    fn copy(&self) -> (&'static str, String, &'static str) {
        match self {
            Self::ClearChatHistory { title, .. } => (
                "CLEAR HISTORY",
                format!(
                    "All messages and attachments in “{title}” are deleted for everyone in the \
                     conversation. This cannot be undone."
                ),
                "Clear history",
            ),
            Self::RemoveFriend { name, .. } => (
                "REMOVE FRIEND",
                format!(
                    "{name} is removed from your contacts. Your chat history stays, and you can \
                     add them again later with their node ID."
                ),
                "Remove",
            ),
        }
    }
}

impl AppState {
    pub(super) fn ui_confirm_dialog(&mut self, ctx: &egui::Context, pal: &Palette) {
        let Some(pending) = self.pending_confirm.clone() else {
            return;
        };
        let (title, body, confirm_label) = pending.copy();
        let mut confirm = false;
        let mut cancel = false;
        let (window, width) =
            dialog_window("confirm-dialog", pal, self.pane_constrain_rect(), 380.0);
        window.show(ctx, |ui| {
            ui.set_width(width);
            cancel |= floating_dialog_header(ui, pal, title, "", Some("Cancel"));
            dialog_body(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(body)
                            .color(pal.text2)
                            .size(ui_font_size(12.0)),
                    )
                    .wrap(),
                );
                dialog_footer(ui, |ui| {
                    if action_button(ui, pal, confirm_label, ButtonTone::Danger).clicked() {
                        confirm = true;
                    }
                    if action_button(ui, pal, "Cancel", ButtonTone::Secondary).clicked() {
                        cancel = true;
                    }
                });
            });
        });
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if confirm {
            self.pending_confirm = None;
            match pending {
                PendingConfirm::ClearChatHistory {
                    conversation_id, ..
                } => self.clear_chat_history(&conversation_id),
                PendingConfirm::RemoveFriend { node_id, .. } => self.remove_friend(&node_id),
            }
        } else if cancel {
            self.pending_confirm = None;
        }
    }
}
