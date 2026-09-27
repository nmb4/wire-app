//! Personal profiles: display name + avatar picture.
//!
//! Profiles sit on top of the identity/presence system. Each user picks a
//! display name and an optional avatar picture, persisted locally. The
//! display name + avatar hash ride along with presence heartbeats
//! (`client_status`), message snapshots (`chat`), and a lightweight public
//! profile fetch protocol so unknown peers (message/call senders you never
//! saved) still resolve to a proper identity instead of a raw peer ID.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

/// Public profile fetch protocol: anyone (including strangers) may query it.
pub const PROFILE_ALPN: &[u8] = b"wire/profile/1";
const MAX_PROFILE_REQUEST_BYTES: usize = 1024;
const MAX_PROFILE_RESPONSE_BYTES: usize = 768 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

/// Maximum display-name length in Unicode scalar values (Discord uses 32).
pub const MAX_DISPLAY_NAME_LEN: usize = 32;
/// Reject raw avatar uploads larger than this before decoding.
pub const MAX_AVATAR_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
/// Stored avatar edge length in pixels (square cover).
pub const AVATAR_EDGE: u32 = 256;

/// Trim, collapse interior whitespace, and cap length. Returns an empty
/// string when nothing usable remains (callers fall back to peer IDs).
pub fn sanitize_display_name(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let name: String = trimmed.chars().take(MAX_DISPLAY_NAME_LEN).collect();
    name.trim().to_owned()
}

/// Short, stable fallback initial for a display name (first alphanumeral).
pub fn display_name_initial(name: &str) -> Option<String> {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
}

/// Preset name-color palette offered in the profile editor.
pub const ACCENT_PRESETS: [&str; 10] = [
    "#5865F2", // blurple
    "#57F287", // green
    "#FEE75C", // yellow
    "#EB459E", // fuchsia
    "#ED4245", // red
    "#E67E22", // orange
    "#1ABC9C", // teal
    "#9B59B6", // purple
    "#3498DB", // blue
    "#E8E6E3", // near-white
];

/// Normalize a user-supplied accent color to `#RRGGBB` (uppercase). Accepts
/// `#RGB`, `#RRGGBB`, with or without the leading `#`. Returns `None` for
/// anything else (callers fall back to the theme text color).
pub fn sanitize_accent_color(raw: &str) -> Option<String> {
    let hex = raw.trim().strip_prefix('#').unwrap_or(raw.trim());
    let expanded = match hex.len() {
        3 if hex.chars().all(|c| c.is_ascii_hexdigit()) => hex
            .chars()
            .flat_map(|c| [c, c])
            .collect::<String>(),
        6 if hex.chars().all(|c| c.is_ascii_hexdigit()) => hex.to_owned(),
        _ => return None,
    };
    Some(format!("#{}", expanded.to_ascii_uppercase()))
}

/// Decode a sanitized (or raw) accent color into RGB bytes.
pub fn accent_rgb(raw: &str) -> Option<(u8, u8, u8)> {
    let normalized = sanitize_accent_color(raw)?;
    let hex = &normalized[1..];
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

fn config_dir() -> Option<PathBuf> {
    wire::net::config_dir()
}

fn profile_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("profile.json"))
}

fn avatar_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("avatar.png"))
}

fn peer_profiles_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("peer_profiles.json"))
}

fn peer_avatar_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("peer_avatars"))
}

fn peer_avatar_path(peer: NodeId) -> Option<PathBuf> {
    peer_avatar_dir().map(|dir| dir.join(format!("{peer}.png")))
}

/// The local user's persisted profile.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OwnProfile {
    #[serde(default)]
    pub display_name: String,
    /// Hex sha256 of the stored `avatar.png`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_hash: Option<String>,
    /// Name accent color as `#RRGGBB`, if the user picked one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
}

impl OwnProfile {
    pub fn snapshot(&self) -> ProfileSnapshot {
        let display_name = if self.display_name.trim().is_empty() {
            None
        } else {
            Some(self.display_name.clone())
        };
        ProfileSnapshot {
            display_name,
            avatar_hash: self.avatar_hash.clone(),
            accent_color: self.accent_color.clone(),
        }
    }
}

/// Cached remote peer profile (learned via presence, chat snapshots, or fetch).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PeerProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<i64>,
}

impl PeerProfile {
    pub fn touch(&mut self) {
        self.last_seen_ms = Some(now_millis());
    }
}

/// Tiny identity snapshot exchanged inside presence heartbeats and chat
/// messages. Avatar *bytes* are never embedded here; they are fetched on
/// demand via [`PROFILE_ALPN`] once the hash is known.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
}

impl ProfileSnapshot {
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.display_name {
            if name.chars().count() > MAX_DISPLAY_NAME_LEN || name.trim().is_empty() {
                bail!("profile display name exceeds safety limit");
            }
        }
        if let Some(hash) = &self.avatar_hash {
            if hash.len() > 128 || hash.trim().is_empty() {
                bail!("profile avatar hash exceeds safety limit");
            }
        }
        if let Some(color) = &self.accent_color {
            if sanitize_accent_color(color).is_none() {
                bail!("profile accent color is not a valid hex color");
            }
        }
        Ok(())
    }
}

pub fn load_own_profile() -> OwnProfile {
    let Some(path) = profile_path() else {
        return OwnProfile::default();
    };
    match crate::persistence::read_json::<OwnProfile>(&path) {
        Ok(Some(mut profile)) => {
            profile.display_name = sanitize_display_name(&profile.display_name);
            profile.accent_color = profile
                .accent_color
                .as_deref()
                .and_then(sanitize_accent_color);
            // Drop a hash that no longer has bytes behind it (user deleted the
            // file out of band, fresh config dir, ...).
            if profile.avatar_hash.is_some() && load_avatar_bytes().is_none() {
                profile.avatar_hash = None;
            }
            profile
        }
        Ok(None) => OwnProfile::default(),
        Err(error) => {
            warn!(path = %path.display(), "could not load profile: {error:#}");
            OwnProfile::default()
        }
    }
}

pub fn save_own_profile(profile: &OwnProfile) {
    if let Some(path) = profile_path() {
        if let Err(error) = crate::persistence::write_json(&path, profile) {
            warn!(path = %path.display(), "could not save profile: {error:#}");
        }
    }
}

pub fn load_avatar_bytes() -> Option<Vec<u8>> {
    let path = avatar_path()?;
    std::fs::read(&path).ok().filter(|bytes| !bytes.is_empty())
}

/// Decode an arbitrary user-supplied image, cover-resize it to a square, and
/// re-encode as PNG. Returns `(png_bytes, hash)`. Kept for the non-interactive
/// path and tests; the UI goes through the crop editor + `crop_avatar_image`.
#[allow(dead_code)]
pub fn process_avatar_bytes(raw: &[u8]) -> Result<(Vec<u8>, String)> {
    if raw.is_empty() || raw.len() > MAX_AVATAR_UPLOAD_BYTES {
        bail!("image is empty or larger than 8 MiB");
    }
    let img = image::load_from_memory(raw).context("could not decode image")?;
    finalize_avatar_image(img)
}

/// Final shared avatar output: cover-resize any image to the stored square
/// PNG. Used both for direct uploads and for user-cropped selections.
pub fn finalize_avatar_image(img: image::DynamicImage) -> Result<(Vec<u8>, String)> {
    let square = img.resize_to_fill(
        AVATAR_EDGE,
        AVATAR_EDGE,
        image::imageops::FilterType::Lanczos3,
    );
    let mut png = Vec::new();
    square
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("could not encode avatar")?;
    let hash = hex_bytes(&Sha256::digest(&png));
    Ok((png, hash))
}

/// Crop a square region (source pixels: x, y, edge) out of an image and run
/// it through the standard avatar output. Lets users choose the framing
/// instead of accepting a blind center-cover.
pub fn crop_avatar_image(
    img: &image::DynamicImage,
    x: u32,
    y: u32,
    edge: u32,
) -> Result<(Vec<u8>, String)> {
    let (width, height) = (img.width(), img.height());
    if edge == 0 || width == 0 || height == 0 {
        bail!("image is empty");
    }
    let edge = edge.min(width).min(height);
    let x = x.min(width.saturating_sub(edge));
    let y = y.min(height.saturating_sub(edge));
    let cropped = img.crop_imm(x, y, edge, edge);
    finalize_avatar_image(cropped)
}

// ---------------------------------------------------------------------------
// Interactive crop math (display-space <-> source-pixel mapping).
//
// The editor shows the image scaled uniformly by `scale` (display pixels per
// source pixel) with its center at `area_center + offset`. A fixed crop
// square of `crop_size` display pixels sits centered in the area. Zoom is
// expressed relative to the minimum cover scale so the crop square is always
// fully covered by image content.
// ---------------------------------------------------------------------------

/// Minimum scale so the crop square is fully covered by the image.
pub fn crop_min_scale(src_w: u32, src_h: u32, crop_size: f32) -> f32 {
    let smallest = src_w.min(src_h).max(1) as f32;
    (crop_size / smallest).max(0.01)
}

/// Clamp a pan offset so the crop square never leaves the image.
pub fn clamp_crop_offset(
    offset: (f32, f32),
    src_w: u32,
    src_h: u32,
    scale: f32,
    crop_size: f32,
) -> (f32, f32) {
    let clamp_axis = |displayed: f32, value: f32| {
        let slack = (displayed - crop_size).max(0.0) / 2.0;
        value.clamp(-slack, slack)
    };
    (
        clamp_axis(src_w as f32 * scale, offset.0),
        clamp_axis(src_h as f32 * scale, offset.1),
    )
}

/// Map the centered crop square back to source pixels: `(x, y, edge)`.
pub fn crop_source_rect(
    src_w: u32,
    src_h: u32,
    scale: f32,
    offset: (f32, f32),
    crop_size: f32,
) -> (u32, u32, u32) {
    let edge = ((crop_size / scale).round() as u32).max(1).min(src_w).min(src_h);
    // Center of the crop square in source pixels.
    let center_x = src_w as f32 / 2.0 - offset.0 / scale;
    let center_y = src_h as f32 / 2.0 - offset.1 / scale;
    let x = (center_x - edge as f32 / 2.0)
        .round()
        .clamp(0.0, src_w.saturating_sub(edge) as f32) as u32;
    let y = (center_y - edge as f32 / 2.0)
        .round()
        .clamp(0.0, src_h.saturating_sub(edge) as f32) as u32;
    (x, y, edge)
}

pub fn avatar_hash_for(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

pub fn save_avatar_bytes(png: &[u8]) {
    if let Some(path) = avatar_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&path, png) {
            warn!(path = %path.display(), "could not save avatar: {error:#}");
        }
    }
}

pub fn remove_avatar_file() {
    if let Some(path) = avatar_path() {
        let _ = std::fs::remove_file(&path);
    }
}

pub fn load_peer_profiles() -> BTreeMap<String, PeerProfile> {
    let Some(path) = peer_profiles_path() else {
        return BTreeMap::new();
    };
    match crate::persistence::read_json::<BTreeMap<String, PeerProfile>>(&path) {
        Ok(Some(profiles)) => profiles,
        Ok(None) => BTreeMap::new(),
        Err(error) => {
            warn!(path = %path.display(), "could not load peer profiles: {error:#}");
            BTreeMap::new()
        }
    }
}

pub fn save_peer_profiles(profiles: &BTreeMap<String, PeerProfile>) {
    if let Some(path) = peer_profiles_path() {
        if let Err(error) = crate::persistence::write_json(&path, profiles) {
            warn!(path = %path.display(), "could not save peer profiles: {error:#}");
        }
    }
}

pub fn load_peer_avatar_bytes(peer: NodeId) -> Option<Vec<u8>> {
    let path = peer_avatar_path(peer)?;
    std::fs::read(&path).ok().filter(|bytes| !bytes.is_empty())
}

pub fn save_peer_avatar_bytes(peer: NodeId, png: &[u8]) {
    if let Some(path) = peer_avatar_path(peer) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(error) = std::fs::write(&path, png) {
            warn!(peer = %peer.fmt_short(), "could not save peer avatar: {error:#}");
        }
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

// ---------------------------------------------------------------------------
// Profile fetch protocol (public: strangers may query it)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProfileRequest {
    version: u8,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProfileResponse {
    version: u8,
    #[serde(default)]
    display_name: String,
    /// Base64-encoded stored avatar PNG, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avatar_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    avatar_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accent_color: Option<String>,
}

/// Data served to anyone asking for our profile.
#[derive(Clone, Debug, Default)]
pub struct ServedProfile {
    pub display_name: String,
    pub avatar_png: Option<Vec<u8>>,
    pub avatar_hash: Option<String>,
    pub accent_color: Option<String>,
}

impl ServedProfile {
    pub fn from_own(profile: &OwnProfile, avatar_bytes: Option<Vec<u8>>) -> Self {
        Self {
            display_name: profile.display_name.clone(),
            avatar_hash: profile.avatar_hash.clone(),
            accent_color: profile.accent_color.clone(),
            avatar_png: avatar_bytes,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProfileProtocol {
    served: Arc<RwLock<ServedProfile>>,
}

impl ProfileProtocol {
    pub fn new(served: ServedProfile) -> Self {
        Self {
            served: Arc::new(RwLock::new(served)),
        }
    }

    pub fn set_served(&self, served: ServedProfile) {
        if let Ok(mut current) = self.served.write() {
            *current = served;
        }
    }

    fn response(&self) -> ProfileResponse {
        let served = self.served.read().map(|s| s.clone()).unwrap_or_default();
        let avatar_base64 = served.avatar_png.as_deref().map(base64_encode);
        ProfileResponse {
            version: 1,
            display_name: served.display_name,
            avatar_hash: served.avatar_hash,
            accent_color: served.accent_color,
            avatar_base64,
        }
    }
}

impl ProtocolHandler for ProfileProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let protocol = self.clone();
        async move {
            let connection = connecting.await?;
            let peer_label = connection
                .remote_node_id()
                .map(|peer| peer.fmt_short().to_string())
                .unwrap_or_else(|_| "?".to_owned());
            let (mut send, mut recv) = connection.accept_bi().await?;
            let _request: ProfileRequest =
                read_json_packet(&mut recv, MAX_PROFILE_REQUEST_BYTES).await?;
            let response = protocol.response();
            write_json_packet(&mut send, &response, MAX_PROFILE_RESPONSE_BYTES).await?;
            send.finish()?;
            debug!(peer = %peer_label, "served profile");
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        async move {}.boxed()
    }
}

/// A profile fetched from a remote peer.
#[derive(Clone, Debug, Default)]
pub struct FetchedProfile {
    pub display_name: String,
    pub avatar_bytes: Option<Vec<u8>>,
    pub avatar_hash: Option<String>,
    pub accent_color: Option<String>,
}

pub async fn fetch_profile(endpoint: &Endpoint, peer: NodeId) -> Result<FetchedProfile> {
    tokio::time::timeout(FETCH_TIMEOUT, async {
        let connection: Connection = endpoint
            .connect(NodeAddr::from(peer), PROFILE_ALPN)
            .await
            .with_context(|| format!("connect to {} for profile", peer.fmt_short()))?;
        let (mut send, mut recv) = connection.open_bi().await?;
        write_json_packet(
            &mut send,
            &ProfileRequest { version: 1 },
            MAX_PROFILE_REQUEST_BYTES,
        )
        .await?;
        send.finish()?;
        let response: ProfileResponse =
            read_json_packet(&mut recv, MAX_PROFILE_RESPONSE_BYTES).await?;
        if response.version != 1 {
            bail!("unsupported profile protocol version {}", response.version);
        }
        let display_name = sanitize_display_name(&response.display_name);
        let avatar_bytes = response
            .avatar_base64
            .as_deref()
            .map(base64_decode)
            .transpose()
            .context("invalid profile avatar")?;
        if let Some(bytes) = &avatar_bytes {
            if bytes.len() > MAX_PROFILE_RESPONSE_BYTES {
                bail!("profile avatar exceeds safety limit");
            }
        }
        // A hash mismatch just means "treat bytes as authoritative for this
        // fetch".
        if let (Some(hash), Some(bytes)) = (&response.avatar_hash, &avatar_bytes) {
            let actual = avatar_hash_for(bytes);
            if &actual != hash {
                debug!(peer = %peer.fmt_short(), "profile avatar hash mismatch; using fetched bytes");
            }
        }
        let avatar_hash = match (response.avatar_hash, avatar_bytes.as_deref()) {
            (Some(hash), _) => Some(hash),
            (None, Some(bytes)) => Some(avatar_hash_for(bytes)),
            (None, None) => None,
        };
        let accent_color = response.accent_color.as_deref().and_then(sanitize_accent_color);
        Ok(FetchedProfile {
            display_name,
            avatar_hash,
            avatar_bytes,
            accent_color,
        })
    })
    .await
    .context("profile fetch timed out")?
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits: u8 = 0;
    for char in input.chars() {
        let value = match char {
            'A'..='Z' => char as u32 - 'A' as u32,
            'a'..='z' => char as u32 - 'a' as u32 + 26,
            '0'..='9' => char as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            _ if char.is_whitespace() => continue,
            _ => bail!("invalid base64 character"),
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
            if bits > 0 {
                buffer &= (1 << bits) - 1;
            } else {
                buffer = 0;
            }
        }
    }
    Ok(output)
}

async fn write_json_packet<T: Serialize>(
    send: &mut iroh::endpoint::SendStream,
    packet: &T,
    max_bytes: usize,
) -> Result<()> {
    let bytes = serde_json::to_vec(packet)?;
    if bytes.is_empty() || bytes.len() > max_bytes {
        bail!("invalid profile packet length {}", bytes.len());
    }
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_json_packet<T: serde::de::DeserializeOwned>(
    recv: &mut iroh::endpoint::RecvStream,
    max_bytes: usize,
) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > max_bytes {
        bail!("invalid profile packet length {len}");
    }
    let mut bytes = vec![0; len];
    recv.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_trimmed_collapsed_and_capped() {
        assert_eq!(sanitize_display_name("  Ada   Lovelace  "), "Ada Lovelace");
        assert_eq!(sanitize_display_name("   "), "");
        assert_eq!(sanitize_display_name(&"a".repeat(64)).chars().count(), 32);
        assert_eq!(sanitize_display_name("a\t\nb"), "a b");
    }

    #[test]
    fn snapshot_rejects_oversized_names() {
        let mut snapshot = ProfileSnapshot {
            display_name: Some("a".repeat(64)),
            avatar_hash: None,
            accent_color: None,
        };
        assert!(snapshot.validate().is_err());
        snapshot.display_name = Some("Ada".to_owned());
        assert!(snapshot.validate().is_ok());
        snapshot.display_name = Some("   ".to_owned());
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn accent_colors_normalize_and_reject_garbage() {
        assert_eq!(
            sanitize_accent_color("#5865f2").as_deref(),
            Some("#5865F2")
        );
        assert_eq!(sanitize_accent_color("f27").as_deref(), Some("#FF2277"));
        assert_eq!(sanitize_accent_color("  #1abc9c ").as_deref(), Some("#1ABC9C"));
        assert_eq!(sanitize_accent_color("not a color"), None);
        assert_eq!(sanitize_accent_color("#12345"), None);
        assert_eq!(sanitize_accent_color(""), None);
        assert_eq!(accent_rgb("#FF0000"), Some((255, 0, 0)));
        assert_eq!(accent_rgb("bogus"), None);
        let mut snapshot = ProfileSnapshot {
            display_name: None,
            avatar_hash: None,
            accent_color: Some("#ZZZZZZ".to_owned()),
        };
        assert!(snapshot.validate().is_err());
        snapshot.accent_color = Some("#5865F2".to_owned());
        assert!(snapshot.validate().is_ok());
    }

    #[test]
    fn crop_math_covers_and_round_trips() {
        // Wide panorama: min scale makes the 240px crop fit the 100px height.
        let min = crop_min_scale(400, 100, 240.0);
        assert!((min - 2.4).abs() < 1e-5);
        // Center offset maps to a centered source square.
        let (x, y, edge) = crop_source_rect(400, 100, min, (0.0, 0.0), 240.0);
        assert_eq!(edge, 100);
        assert!((x as i32 - 150).abs() <= 1);
        assert_eq!(y, 0);
        // Panning is clamped so the crop never leaves the image.
        let clamped = clamp_crop_offset((10_000.0, -10_000.0), 400, 100, min, 240.0);
        let expected_x = (400.0 * min - 240.0) / 2.0;
        assert!((clamped.0 - expected_x).abs() < 1e-3);
        assert!(clamped.1.abs() < 1e-3);
        // Cropping the computed rect yields a square avatar.
        let img = image::DynamicImage::new_rgba8(400, 100);
        let (png, _) = crop_avatar_image(&img, x, y, edge).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (256, 256));
    }

    #[test]
    fn base64_round_trips_binary() {
        let bytes: Vec<u8> = (0..255).collect();
        let encoded = base64_encode(&bytes);
        assert_eq!(base64_decode(&encoded).unwrap(), bytes);
    }

    #[test]
    fn avatar_processing_normalizes_to_a_square_png() {
        let mut buf = Vec::new();
        image::DynamicImage::new_rgba8(64, 32)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let (png, hash) = process_avatar_bytes(&buf).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (256, 256));
        assert_eq!(hash, avatar_hash_for(&png));
    }
}
