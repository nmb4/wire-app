//! Personal profile state + Discord-style self card + avatar widgets.
//!
//! This sits on top of the presence system: display names / avatar hashes
//! arrive via `StatusUpdate.profile`, chat message snapshots, and invites,
//! while avatar *bytes* are fetched on demand through `wire/profile/1`.

use super::{AppState, Friend};
use crate::{
    profile::{self, PeerProfile},
    runtime::Command,
    theme::{kh_family, ui_font_size, Palette},
};
use egui::{ColorImage, CornerRadius, FontId, Stroke, TextureHandle, Vec2};
use iroh::NodeId;
use std::collections::BTreeMap;

/// Cool-down between automatic profile fetches for the same peer.
const FETCH_COOLDOWN_MS: i64 = 60_000;

/// Decode stored avatar PNG bytes into an egui image.
pub(super) fn decode_avatar_image(bytes: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    Some(ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw()))
}

fn persist_peer_profiles(state: &BTreeMap<NodeId, PeerProfile>) {
    let stored: BTreeMap<String, PeerProfile> = state
        .iter()
        .map(|(peer, profile)| (peer.to_string(), profile.clone()))
        .collect();
    profile::save_peer_profiles(&stored);
}

impl AppState {
    /// Our chosen display name, or empty when unset.
    pub fn own_display_name(&self) -> &str {
        self.own_profile_name.trim()
    }

    /// Label for self shown across the UI ("Name" or "You").
    pub fn own_label(&self) -> String {
        if self.own_display_name().is_empty() {
            "You".to_owned()
        } else {
            self.own_display_name().to_owned()
        }
    }

    /// Remote profile display name, if we learned one.
    pub fn peer_profile_name(&self, peer: NodeId) -> Option<&str> {
        self.peer_profiles
            .get(&peer)
            .and_then(|profile| profile.display_name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// Learn a display-name / avatar-hash snapshot from presence, chat, or an
    /// invite. Persists the cache and triggers an avatar fetch when we see a
    /// hash we have no bytes for.
    pub fn learn_peer_snapshot(
        &mut self,
        peer: NodeId,
        display_name: Option<String>,
        avatar_hash: Option<String>,
        _source: &str,
    ) {
        if Some(peer) == self.our_node_id {
            return;
        }
        let cleaned_name = display_name
            .map(|name| profile::sanitize_display_name(&name))
            .filter(|name| !name.is_empty());
        let cleaned_hash = avatar_hash
            .map(|hash| hash.trim().to_owned())
            .filter(|hash| !hash.is_empty());
        if cleaned_name.is_none() && cleaned_hash.is_none() {
            return;
        }
        let entry = self.peer_profiles.entry(peer).or_default();
        let mut changed = false;
        if let Some(name) = cleaned_name {
            if entry.display_name.as_deref() != Some(name.as_str()) {
                entry.display_name = Some(name);
                changed = true;
            }
        }
        if let Some(hash) = cleaned_hash {
            if entry.avatar_hash.as_deref() != Some(hash.as_str()) {
                entry.avatar_hash = Some(hash);
                changed = true;
            }
        }
        if changed {
            entry.touch();
            persist_peer_profiles(&self.peer_profiles);
        } else {
            entry.touch();
        }
        // Fetch avatar bytes when the hash is new to us.
        let want_bytes = self
            .peer_profiles
            .get(&peer)
            .and_then(|profile| profile.avatar_hash.clone());
        if let Some(hash) = want_bytes {
            let have_matching = self
                .peer_avatar_bytes
                .get(&peer)
                .map(|bytes| profile::avatar_hash_for(bytes) == hash)
                .unwrap_or(false);
            if !have_matching {
                // Prefer a cached file from a previous run before dialing.
                if profile::load_peer_avatar_bytes(peer)
                    .as_deref()
                    .map(profile::avatar_hash_for)
                    .as_deref()
                    == Some(hash.as_str())
                {
                    if let Some(bytes) = profile::load_peer_avatar_bytes(peer) {
                        self.peer_avatar_bytes.insert(peer, bytes);
                    }
                } else {
                    self.request_peer_profile(peer);
                }
            }
        }
    }

    /// Apply a full fetched profile (name + optional avatar bytes).
    pub fn apply_fetched_profile(
        &mut self,
        peer: NodeId,
        display_name: String,
        avatar_hash: Option<String>,
        avatar_bytes: Option<Vec<u8>>,
    ) {
        if Some(peer) == self.our_node_id {
            return;
        }
        self.pending_profile_fetches.remove(&peer);
        let cleaned_name = profile::sanitize_display_name(&display_name);
        let entry = self.peer_profiles.entry(peer).or_default();
        if !cleaned_name.is_empty() {
            entry.display_name = Some(cleaned_name);
        }
        if let Some(hash) = avatar_hash
            .map(|hash| hash.trim().to_owned())
            .filter(|hash| !hash.is_empty())
        {
            entry.avatar_hash = Some(hash);
        } else if let Some(bytes) = avatar_bytes.as_deref() {
            entry.avatar_hash = Some(profile::avatar_hash_for(bytes));
        }
        entry.touch();
        if let Some(bytes) = avatar_bytes {
            if bytes.is_empty() {
                self.peer_avatar_bytes.remove(&peer);
                self.peer_avatar_textures.remove(&peer);
            } else {
                profile::save_peer_avatar_bytes(peer, &bytes);
                self.peer_avatar_textures.remove(&peer);
                self.peer_avatar_bytes.insert(peer, bytes);
            }
        }
        persist_peer_profiles(&self.peer_profiles);
    }

    /// Queue a background fetch of a peer's public profile (cool-down
    /// guarded so presence storms cannot dial-loop).
    pub fn request_peer_profile(&mut self, peer: NodeId) {
        if Some(peer) == self.our_node_id {
            return;
        }
        let now = chat_now_millis();
        if let Some(last) = self.pending_profile_fetches.get(&peer) {
            if now.saturating_sub(*last) < FETCH_COOLDOWN_MS {
                return;
            }
        }
        self.pending_profile_fetches.insert(peer, now);
        self.cmd(Command::FetchPeerProfiles { peers: vec![peer] });
    }

    /// Push our current identity to the worker (presence + chat + fetch
    /// protocol) after startup or an edit.
    pub fn sync_own_profile_to_worker(&self) {
        let name = self.own_profile_name.clone();
        self.cmd(Command::SetChatProfile {
            display_name: (!name.trim().is_empty()).then_some(name.clone()),
            avatar_hash: self.own_avatar_hash.clone(),
        });
        self.cmd(Command::SetOwnProfile {
            display_name: name,
            avatar_hash: self.own_avatar_hash.clone(),
        });
    }

    /// Persist an edit of our own profile and advertise it immediately.
    pub fn save_own_profile_edit(&mut self) {
        let name = profile::sanitize_display_name(&self.profile_edit_name);
        self.own_profile_name = name.clone();
        self.profile_edit_name = name.clone();
        self.profile_edit_error = None;
        profile::save_own_profile(&profile::OwnProfile {
            display_name: self.own_profile_name.clone(),
            avatar_hash: self.own_avatar_hash.clone(),
        });
        // File bytes are written at pick time; just refresh the texture.
        self.own_avatar_texture = None;
        self.sync_own_profile_to_worker();
        self.play_control_sound(true);
    }

    /// User picked an image file: normalize to a square PNG, store it, and
    /// advertise the new hash.
    pub fn set_own_avatar_from_file(&mut self, path: &std::path::Path) {
        match std::fs::read(path) {
            Ok(raw) => match profile::process_avatar_bytes(&raw) {
                Ok((png, hash)) => {
                    profile::save_avatar_bytes(&png);
                    self.own_avatar_bytes = Some(png);
                    self.own_avatar_hash = Some(hash);
                    self.own_avatar_texture = None;
                    profile::save_own_profile(&profile::OwnProfile {
                        display_name: self.own_profile_name.clone(),
                        avatar_hash: self.own_avatar_hash.clone(),
                    });
                    self.sync_own_profile_to_worker();
                    self.profile_edit_error = None;
                }
                Err(error) => {
                    self.profile_edit_error = Some(format!("Could not use image: {error:#}"));
                }
            },
            Err(error) => {
                self.profile_edit_error = Some(format!("Could not read image: {error}"));
            }
        }
    }

    pub fn clear_own_avatar(&mut self) {
        profile::remove_avatar_file();
        self.own_avatar_bytes = None;
        self.own_avatar_hash = None;
        self.own_avatar_texture = None;
        profile::save_own_profile(&profile::OwnProfile {
            display_name: self.own_profile_name.clone(),
            avatar_hash: None,
        });
        self.sync_own_profile_to_worker();
    }

    /// Cached texture for our own avatar, decoded on demand.
    pub fn own_avatar_texture(&mut self, ctx: &egui::Context) -> Option<TextureHandle> {
        if self.own_avatar_texture.is_none() {
            // Load from disk once per process if the in-memory copy is empty
            // (e.g. avatar set by a previous run).
            if self.own_avatar_bytes.is_none() {
                self.own_avatar_bytes = profile::load_avatar_bytes();
            }
            if let Some(bytes) = self.own_avatar_bytes.clone() {
                if let Some(image) = decode_avatar_image(&bytes) {
                    self.own_avatar_texture =
                        Some(ctx.load_texture("own-avatar", image, Default::default()));
                }
            }
        }
        self.own_avatar_texture.clone()
    }

    /// Cached texture for a peer's avatar, decoded on demand. Triggers a
    /// background fetch when we know a hash but have no bytes.
    pub fn peer_avatar_texture(
        &mut self,
        ctx: &egui::Context,
        peer: NodeId,
    ) -> Option<TextureHandle> {
        if let Some(texture) = self.peer_avatar_textures.get(&peer) {
            return Some(texture.clone());
        }
        if self.peer_avatar_bytes.get(&peer).is_none() {
            if let Some(bytes) = profile::load_peer_avatar_bytes(peer) {
                self.peer_avatar_bytes.insert(peer, bytes);
            }
        }
        if let Some(bytes) = self.peer_avatar_bytes.get(&peer).cloned() {
            if let Some(image) = decode_avatar_image(&bytes) {
                let texture =
                    ctx.load_texture(format!("peer-avatar-{peer}"), image, Default::default());
                self.peer_avatar_textures.insert(peer, texture.clone());
                return Some(texture);
            }
        }
        // Opportunistic fetch: we know a hash (or want a name) but have no
        // bytes yet.
        if self.peer_profiles.contains_key(&peer) {
            self.request_peer_profile(peer);
        }
        None
    }

    /// Ensure we have attempted a profile fetch for every peer we display but
    /// know nothing about (unknown senders / callers).
    pub fn ensure_peer_profiles(&mut self, peers: impl IntoIterator<Item = NodeId>) {
        for peer in peers {
            if Some(peer) == self.our_node_id {
                continue;
            }
            if self.peer_profiles.contains_key(&peer) {
                // Still fetch avatars for known names with unknown bytes.
                let missing_avatar = self
                    .peer_profiles
                    .get(&peer)
                    .and_then(|profile| profile.avatar_hash.clone())
                    .is_some_and(|_| !self.peer_avatar_bytes.contains_key(&peer));
                if missing_avatar {
                    self.request_peer_profile(peer);
                }
                continue;
            }
            self.request_peer_profile(peer);
        }
    }

    /// Discord-style self card for the bottom control bar: avatar (click to
    /// edit profile), display name, online dot.
    pub fn ui_self_user_card(&mut self, ui: &mut egui::Ui, pal: &Palette, ctx: &egui::Context) {
        let name = self.own_label();
        let avatar = self.own_avatar_texture(ctx);
        let initials = profile::display_name_initial(&name).unwrap_or_else(|| "Y".to_owned());
        let response = egui::Frame::new()
            .fill(pal.panel)
            .stroke(Stroke::new(1.0_f32, pal.line))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(8, 5))
            .show(ui, |ui| {
                ui.set_height(44.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    paint_profile_avatar(ui, pal, avatar, &initials, 32.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            egui::RichText::new(truncate_name(&name, 16))
                                .color(pal.text)
                                .size(ui_font_size(12.5)),
                        );
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(7.0), egui::Sense::hover());
                            ui.painter().circle_filled(rect.center(), 3.5, pal.ok);
                            ui.label(
                                egui::RichText::new("Online")
                                    .color(pal.dim)
                                    .size(ui_font_size(10.5)),
                            );
                        });
                    });
                });
            })
            .response
            .interact(egui::Sense::click());
        if response
            .on_hover_text("Edit your profile (name + picture)")
            .clicked()
        {
            self.profile_edit_name = self.own_profile_name.clone();
            self.profile_edit_error = None;
            self.show_profile_editor = true;
        }
    }

    /// Modal editor for display name + avatar picture.
    pub fn ui_profile_editor(&mut self, ctx: &egui::Context) {
        if !self.show_profile_editor {
            return;
        }
        let pal = Palette::for_theme(self.theme);
        let pane_rect = self.pane_constrain_rect();
        let dialog_width = (pane_rect.width() - 60.0).clamp(360.0, 440.0);
        let mut open = true;
        let mut pick_avatar = false;
        let mut remove_avatar = false;
        let mut save = false;
        egui::Window::new("profile-editor")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .constrain_to(pane_rect)
            .default_width(dialog_width)
            .min_width(dialog_width)
            .max_width(dialog_width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                egui::Frame::new()
                    .fill(pal.bg)
                    .stroke(Stroke::new(1.0_f32, pal.line_br))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(0.0),
            )
            .show(ctx, |ui| {
                ui.set_width(dialog_width);
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("YOUR PROFILE")
                                .family(kh_family())
                                .color(pal.text2)
                                .size(13.0),
                        );
                        ui.label(
                            egui::RichText::new("Shown to everyone you message or call.")
                                .color(pal.dim)
                                .size(ui_font_size(11.0)),
                        );
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            let avatar = self.own_avatar_texture(ctx);
                            let preview_name = profile::sanitize_display_name(
                                &self.profile_edit_name,
                            );
                            let preview_name =
                                if preview_name.is_empty() { "You".to_owned() } else { preview_name };
                            let initials =
                                profile::display_name_initial(&preview_name).unwrap_or_else(|| "?".to_owned());
                            paint_profile_avatar(ui, &pal, avatar, &initials, 56.0);
                            ui.vertical(|ui| {
                                if ui
                                    .button("Choose picture…")
                                    .on_hover_text("PNG, JPEG, GIF or WebP up to 8 MiB")
                                    .clicked()
                                {
                                    pick_avatar = true;
                                }
                                if self.own_avatar_hash.is_some()
                                    && ui.button("Remove picture").clicked()
                                {
                                    remove_avatar = true;
                                }
                            });
                        });
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new("Display name")
                                .color(pal.text2)
                                .size(ui_font_size(12.0)),
                        );
                        let changed = ui
                            .add(
                                egui::TextEdit::singleline(&mut self.profile_edit_name)
                                    .hint_text("e.g. Ada Lovelace")
                                    .desired_width(f32::INFINITY),
                            )
                            .changed();
                        if changed {
                            self.profile_edit_error = None;
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{}/32",
                                profile::sanitize_display_name(&self.profile_edit_name)
                                    .chars()
                                    .count()
                            ))
                            .color(pal.dim)
                            .size(ui_font_size(10.5)),
                        );
                        if let Some(error) = &self.profile_edit_error {
                            ui.label(
                                egui::RichText::new(error)
                                    .color(pal.err)
                                    .size(ui_font_size(11.5)),
                            );
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                save = true;
                            }
                            if ui.button("Cancel").clicked() {
                                open = false;
                            }
                        });
                    });
            });
        if pick_avatar {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Choose profile picture")
                .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                .pick_file()
            {
                self.set_own_avatar_from_file(&path);
            }
        }
        if remove_avatar {
            self.clear_own_avatar();
        }
        if save {
            if profile::sanitize_display_name(&self.profile_edit_name).is_empty() {
                self.profile_edit_error =
                    Some("Enter a display name (or Cancel to keep browsing as a peer ID).".to_owned());
            } else {
                self.save_own_profile_edit();
                open = false;
            }
        }
        self.show_profile_editor = open;
    }
}

/// Circular avatar: profile picture when available, initial letter otherwise.
pub fn paint_profile_avatar(
    ui: &mut egui::Ui,
    pal: &Palette,
    texture: Option<TextureHandle>,
    initial: &str,
    size: f32,
) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    if let Some(texture) = texture {
        paint_circular_image(ui, rect, &texture);
        ui.painter()
            .circle_stroke(rect.center(), size / 2.0, Stroke::new(1.0_f32, pal.line_br));
    } else {
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
}

/// Avatar badge pinned to a stream tile corner, deliberately overflowing the
/// frame so stream ownership is obvious at a glance.
pub fn paint_stream_owner_avatar(
    ui: &mut egui::Ui,
    pal: &Palette,
    tile_rect: egui::Rect,
    texture: Option<TextureHandle>,
    initial: &str,
    name: &str,
) {
    const SIZE: f32 = 30.0;
    // Top-left, half outside the tile frame.
    let center = tile_rect.left_top() + Vec2::new(10.0, -2.0);
    let rect = egui::Rect::from_center_size(center, Vec2::splat(SIZE));
    // Halo so the badge reads on top of video content.
    ui.painter()
        .circle_filled(rect.center(), SIZE / 2.0 + 2.0, pal.bg);
    if let Some(texture) = texture {
        paint_circular_image(ui, rect, &texture);
        ui.painter()
            .circle_stroke(rect.center(), SIZE / 2.0, Stroke::new(1.5_f32, pal.line_br));
    } else {
        ui.painter()
            .circle_filled(rect.center(), SIZE / 2.0, pal.panel2);
        ui.painter()
            .circle_stroke(rect.center(), SIZE / 2.0, Stroke::new(1.5_f32, pal.line_br));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            initial,
            FontId::new(SIZE * 0.42, kh_family()),
            pal.text,
        );
    }
    // Tooltip for screen readers / hover.
    let _ = ui
        .interact(rect, ui.id().with(("stream-owner", name)), egui::Sense::hover())
        .on_hover_text(format!("{name}'s stream"));
}

/// Paint a texture clipped to a true circle via a triangle-fan mesh. The
/// stored avatars are square PNGs, so a centered 0..1 UV mapping is correct.
fn paint_circular_image(ui: &egui::Ui, rect: egui::Rect, texture: &TextureHandle) {
    const SEGMENTS: usize = 40;
    let center = rect.center();
    let radius = rect.width().min(rect.height()) / 2.0;
    let mut mesh = egui::Mesh::with_texture(texture.id());
    mesh.vertices.push(egui::epaint::Vertex {
        pos: center,
        uv: egui::pos2(0.5, 0.5),
        color: egui::Color32::WHITE,
    });
    for i in 0..=SEGMENTS {
        let angle = (i as f32 / SEGMENTS as f32) * std::f32::consts::TAU;
        let (sin, cos) = angle.sin_cos();
        mesh.vertices.push(egui::epaint::Vertex {
            pos: center + Vec2::new(cos * radius, sin * radius),
            uv: egui::pos2(0.5 + cos * 0.5, 0.5 + sin * 0.5),
            color: egui::Color32::WHITE,
        });
    }
    for i in 1..SEGMENTS as u32 + 1 {
        mesh.add_triangle(0, i, i + 1);
    }
    ui.painter().add(egui::Shape::mesh(mesh));
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_owned();
    }
    let truncated: String = name.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn chat_now_millis() -> i64 {
    crate::chat::now_millis()
}

#[allow(dead_code)]
pub(super) fn example_friend(name: &str, node: &str) -> Friend {
    Friend {
        name: name.to_owned(),
        node_id: node.to_owned(),
    }
}

/// IDs whose display we attempted to resolve this frame (for fetch batching).
#[allow(dead_code)]
pub type PeerSet = BTreeMap<NodeId, ()>;
