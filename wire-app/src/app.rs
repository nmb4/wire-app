//! Application state, event handling, and UI lifecycle.
//!
//! Child modules render individual screens against the shared `AppState`.

mod calls_ui;
mod chat_ui;
mod chrome;
mod settings_ui;
mod widgets;

#[cfg(windows)]
use self::calls_ui::native_parent_hwnd;
use self::widgets::{ellipsize, track_pane_viewport};
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
use crate::tray::{TrayAction, TrayController};
#[cfg(windows)]
use crate::update::{self, ReleaseInfo};
use crate::{
    activation::ActivationWatcher,
    autostart,
    chat::{
        self, ChatAttachment, ChatConversation, ChatMessage, ChatNotification, DeliveryState,
        RetentionPolicy,
    },
    client_status::{Availability, GroupCallAnnouncement, StatusUpdate},
    dev_pair::DevPairState,
    host::ServiceClient,
    notifications::{NotificationAction, NotificationService},
    persistence,
    resource_monitor::ResourceMonitor,
    runtime::{CallState, Command, Event},
    sounds::{Sound, Sounds},
    theme::{ghost_icon_button, setup_fonts, visuals_for, Palette, Theme, WindowFrameStyle},
    video_decode::DecodedFrameData,
    window_frame,
};
use anyhow::Result;
use eframe::NativeOptions;
use egui::{Frame, Rect, Vec2};
use egui_phosphor::regular as ph;
use iroh::{KeyParsingError, NodeId};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
use tracing::{debug, info, warn};
use wire::{
    audio::{AudioConfig, AudioLevelHandle, AudioQuality, VolumeHandle},
    video::VideoConfig,
};

const DEFAULT: &str = "<default>";

pub struct App {
    is_first_update: bool,
    always_on_top: bool,
    viewport_transparent: Option<bool>,
    close_to_tray: bool,
    quit_requested: bool,
    window_visible: bool,
    hidden_video_nodes: BTreeSet<NodeId>,
    activation_watcher: Option<ActivationWatcher>,
    state: AppState,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    tray: Option<TrayController>,
    #[cfg(windows)]
    global_hotkeys: Option<crate::global_hotkeys::GlobalHotkeys>,
}

/// A locally stored contact, identified by their stable wire node id.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Friend {
    name: String,
    node_id: String,
}

fn friends_path() -> Option<PathBuf> {
    wire::net::config_dir().map(|dir| dir.join("friends.json"))
}

fn load_friends_from(path: &Path) -> Option<Vec<Friend>> {
    match persistence::read_json(path) {
        Ok(friends) => friends,
        Err(error) => {
            warn!(path = %path.display(), "could not load friends: {error:#}");
            None
        }
    }
}

fn load_friends() -> Vec<Friend> {
    if let Some(path) = friends_path() {
        if let Some(friends) = load_friends_from(&path) {
            // An empty current file is intentional. Falling through here used to
            // resurrect contacts from the legacy location on every restart.
            return friends;
        }
    }

    if let Some(legacy_dir) = wire::net::legacy_config_dir() {
        let legacy_path = legacy_dir.join("friends.json");
        if let Some(friends) = load_friends_from(&legacy_path) {
            if !friends.is_empty() {
                save_friends(&friends);
                return friends;
            }
        }
    }

    Vec::new()
}

fn save_friends(friends: &[Friend]) {
    if let Some(path) = friends_path() {
        if let Err(error) = persistence::write_json(&path, friends) {
            warn!(path = %path.display(), "could not save friends: {error:#}");
        }
    }
}

const SEEN_GROUP_CALL_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn seen_group_calls_path() -> Option<PathBuf> {
    wire::net::config_dir().map(|dir| dir.join("seen-group-calls.json"))
}

fn load_seen_group_calls() -> BTreeMap<String, i64> {
    let mut seen: BTreeMap<String, i64> = seen_group_calls_path()
        .and_then(|path| persistence::read_json(&path).ok().flatten())
        .unwrap_or_default();
    let cutoff = chat::now_millis() - SEEN_GROUP_CALL_RETENTION_MS;
    seen.retain(|_, seen_at| *seen_at >= cutoff);
    seen
}

fn save_seen_group_calls(seen: &BTreeMap<String, i64>) {
    if let Some(path) = seen_group_calls_path() {
        if let Err(error) = persistence::write_json(&path, seen) {
            warn!(path = %path.display(), "could not save seen group calls: {error:#}");
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    wire::net::config_dir().map(|dir| dir.join("settings.json"))
}

fn load_settings() -> Option<Settings> {
    let path = settings_path()?;
    match persistence::read_json(&path) {
        Ok(settings) => settings,
        Err(error) => {
            warn!(path = %path.display(), "could not load settings: {error:#}");
            None
        }
    }
}

fn save_settings(settings: &Settings) {
    if let Some(path) = settings_path() {
        if let Err(error) = persistence::write_json(&path, settings) {
            warn!(path = %path.display(), "could not save settings: {error:#}");
        }
    }
}

fn save_start_with_system(enabled: bool) {
    let mut settings = load_settings().unwrap_or_default();
    settings.start_with_system = enabled;
    save_settings(&settings);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamViewMode {
    Normal,
    FillWindow,
    Fullscreen,
}

impl StreamViewMode {
    fn is_fullscreen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum StreamSource {
    Local,
    Remote(NodeId),
}

const STREAM_GRID_GAP: f32 = 6.0;

struct AppState {
    configured: bool,
    show_settings: bool,
    show_contacts: bool,
    /// Last seen healthy viewport rect; floating panes are constrained to it
    /// so degenerate minimize/restore frames cannot shrink their geometry.
    pane_viewport: Rect,
    stream_view_mode: StreamViewMode,
    remote_node_id: Option<Result<NodeId, KeyParsingError>>,
    remote_node_input: String,
    service: ServiceClient,
    our_node_id: Option<NodeId>,
    devices: wire::audio::Devices,
    audio_config: UiAudioConfig,
    video_config: VideoConfig,
    calls: BTreeMap<NodeId, CallState>,
    volumes: BTreeMap<NodeId, VolumeHandle>,
    stream_volumes: BTreeMap<NodeId, VolumeHandle>,
    local_audio_level: Option<AudioLevelHandle>,
    remote_audio_levels: BTreeMap<NodeId, AudioLevelHandle>,
    video_frames: BTreeMap<NodeId, VideoFrameState>,
    video_stream_generations: BTreeMap<NodeId, u64>,
    ended_video_stream_generations: BTreeMap<NodeId, u64>,
    stopped_video_stream_generations: BTreeMap<NodeId, u64>,
    focused_stream: Option<StreamSource>,
    volume_open: BTreeSet<NodeId>,
    sharing_active: bool,
    share_system_audio: bool,
    system_audio_active: bool,
    capture_error: Option<String>,
    show_capture_picker: bool,
    capture_targets: Vec<crate::screen_capture::CaptureTarget>,
    selected_capture_target: Option<usize>,
    preview: Option<PreviewState>,
    friends: Vec<Friend>,
    friend_status: BTreeMap<NodeId, StatusUpdate>,
    group_call_reports: BTreeMap<NodeId, Vec<GroupCallAnnouncement>>,
    local_group_call: Option<GroupCallAnnouncement>,
    seen_group_calls: BTreeMap<String, i64>,
    new_friend_name: String,
    new_friend_id: String,
    theme: Theme,
    window_frame_style: WindowFrameStyle,
    muted: bool,
    deafened: bool,
    ui_sound_volume: f32,
    sounds: Option<Sounds>,
    notifications: NotificationService,
    voluntary_hangups: AtomicU32,
    #[cfg(windows)]
    update_tx: mpsc::Sender<UpdateMessage>,
    #[cfg(windows)]
    update_rx: mpsc::Receiver<UpdateMessage>,
    autostart_tx: mpsc::Sender<AutostartMessage>,
    autostart_rx: mpsc::Receiver<AutostartMessage>,
    #[cfg(windows)]
    update_status: UpdateStatus,
    #[cfg(windows)]
    show_update_prompt: bool,
    resource_monitor: ResourceMonitor,
    dev_pair: Option<DevPairState>,
    dev_auto_share: bool,
    exit_requested: bool,
    app_mode: AppMode,
    chat: ChatUiState,
    chat_notifications_ready: bool,
    chat_retention: RetentionPolicy,
    chat_style: ChatStyle,
    max_image_bytes: Option<u64>,
    start_with_system: bool,
    saved_start_with_system: bool,
    show_system_usage: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Text,
    Calls,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChatStyle {
    #[default]
    Bubbles,
    Compact,
}

#[derive(Default)]
struct ChatUiState {
    conversations: BTreeMap<String, ChatConversation>,
    timelines: BTreeMap<String, Vec<ChatMessage>>,
    delivery: BTreeMap<String, (DeliveryState, Option<String>)>,
    selected: Option<String>,
    unseen: BTreeSet<String>,
    composer: String,
    draft_attachments: Vec<ChatAttachment>,
    attachment_textures: AttachmentTextureCache,
    attachment_requests: BTreeSet<String>,
    conversations_with_older_messages: BTreeSet<String>,
    image_preview: Option<ImagePreview>,
    error: Option<String>,
    service_error: Option<String>,
    show_group_editor: bool,
    show_group_members: bool,
    group_name: String,
    group_members: BTreeSet<NodeId>,
    friend_candidate: Option<NodeId>,
    friend_candidate_name: String,
}

const MAX_ATTACHMENT_TEXTURES: usize = 128;
const MAX_CACHED_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CACHED_TEXTURE_BYTES: usize = 256 * 1024 * 1024;

struct AttachmentTextureEntry {
    texture: egui::TextureHandle,
    last_used: u64,
    data: Option<Arc<Vec<u8>>>,
    texture_bytes: usize,
}

#[derive(Default)]
struct AttachmentTextureCache {
    entries: BTreeMap<String, AttachmentTextureEntry>,
    clock: u64,
    data_bytes: usize,
    texture_bytes: usize,
}

impl AttachmentTextureCache {
    fn get_or_insert(
        &mut self,
        ctx: &egui::Context,
        attachment: &ChatAttachment,
    ) -> Option<&egui::TextureHandle> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(&attachment.id) {
            entry.last_used = self.clock;
        } else {
            self.insert_data(ctx, attachment, attachment.data.as_ref()?.clone())?;
        }
        self.entries.get(&attachment.id).map(|entry| &entry.texture)
    }

    fn insert_data(
        &mut self,
        ctx: &egui::Context,
        attachment: &ChatAttachment,
        data: Arc<Vec<u8>>,
    ) -> Option<()> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(&attachment.id) {
            self.data_bytes = self
                .data_bytes
                .saturating_sub(entry.data.as_ref().map_or(0, |data| data.len()));
            self.data_bytes = self.data_bytes.saturating_add(data.len());
            entry.data = Some(data);
            entry.last_used = self.clock;
        } else {
            let decoded = image::load_from_memory(&data).ok()?.to_rgba8();
            let size = [decoded.width() as usize, decoded.height() as usize];
            let pixels = decoded.into_raw();
            if pixels.len() > MAX_CACHED_TEXTURE_BYTES {
                return None;
            }
            let texture_bytes = pixels.len();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, &pixels);
            let texture = ctx.load_texture(
                format!("chat-image-{}", attachment.id),
                color_image,
                egui::TextureOptions::LINEAR,
            );
            self.data_bytes = self.data_bytes.saturating_add(data.len());
            self.texture_bytes = self.texture_bytes.saturating_add(texture_bytes);
            self.entries.insert(
                attachment.id.clone(),
                AttachmentTextureEntry {
                    texture,
                    last_used: self.clock,
                    data: Some(data),
                    texture_bytes,
                },
            );
        }
        while self.entries.len() > MAX_ATTACHMENT_TEXTURES
            || self.texture_bytes > MAX_CACHED_TEXTURE_BYTES
        {
            let oldest = self.oldest_id(false)?;
            if let Some(entry) = self.entries.remove(&oldest) {
                self.data_bytes = self
                    .data_bytes
                    .saturating_sub(entry.data.as_ref().map_or(0, |data| data.len()));
                self.texture_bytes = self.texture_bytes.saturating_sub(entry.texture_bytes);
            }
        }
        while self.data_bytes > MAX_CACHED_ATTACHMENT_BYTES {
            let oldest = self.oldest_id(true)?;
            let entry = self.entries.get_mut(&oldest)?;
            if let Some(data) = entry.data.take() {
                self.data_bytes = self.data_bytes.saturating_sub(data.len());
            }
        }
        Some(())
    }

    fn oldest_id(&self, require_data: bool) -> Option<String> {
        self.entries
            .iter()
            .filter(|(_, entry)| !require_data || entry.data.is_some())
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(id, _)| id.clone())
    }

    fn data(&self, id: &str) -> Option<Arc<Vec<u8>>> {
        self.entries.get(id)?.data.clone()
    }
}

#[derive(Clone)]
struct ImagePreview {
    attachment: ChatAttachment,
    draft: bool,
    mode: ImagePreviewMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ImagePreviewMode {
    #[default]
    Floating,
    FillWindow,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImagePreviewAction {
    Close,
    Delete,
    SetMode(ImagePreviewMode),
}

#[cfg(windows)]
enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Available(ReleaseInfo),
    Downloading(ReleaseInfo),
    Error(String),
}

#[cfg(windows)]
enum UpdateMessage {
    CheckFinished(anyhow::Result<Option<ReleaseInfo>>),
    DownloadFinished(anyhow::Result<PathBuf>),
}

struct AutostartMessage {
    enabled: bool,
    result: std::result::Result<(), String>,
}

struct PreviewState {
    width: u32,
    height: u32,
    actual_fps: f64,
    encode_time_ms: f64,
    generation: u64,
    data: Arc<Vec<u8>>,
    texture: Option<egui::TextureHandle>,
    uploaded_generation: u64,
    upload_stats: TextureUploadStats,
}

struct VideoFrameState {
    width: u32,
    height: u32,
    stream_generation: u64,
    generation: u64,
    source_height: u32,
    source_fps: u32,
    data: DecodedFrameData,
    texture: Option<egui::TextureHandle>,
    uploaded_generation: u64,
    upload_stats: TextureUploadStats,
    #[cfg(windows)]
    presenter: Option<crate::win_video_presenter::NativeVideoPresenter>,
    #[cfg(windows)]
    native_present_failed: bool,
}

struct TextureUploadStats {
    samples_ms: Vec<f64>,
    frames: u64,
    last_log: std::time::Instant,
}

impl Default for TextureUploadStats {
    fn default() -> Self {
        Self {
            samples_ms: Vec::with_capacity(300),
            frames: 0,
            last_log: std::time::Instant::now(),
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct UiAudioConfig {
    selected_input: String,
    selected_output: String,
    processing_enabled: bool,
    #[serde(default = "enabled_by_default")]
    noise_suppression_enabled: bool,
    quality: AudioQuality,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    audio: UiAudioConfig,
    #[serde(default = "default_ui_sound_volume")]
    ui_sound_volume: f32,
    video: VideoConfig,
    theme: Theme,
    #[serde(default)]
    window_frame_style: WindowFrameStyle,
    configured: bool,
    #[serde(default)]
    chat_retention: RetentionPolicy,
    #[serde(default)]
    chat_style: ChatStyle,
    #[serde(default)]
    max_image_bytes: Option<u64>,
    #[serde(default)]
    start_with_system: bool,
    #[serde(default)]
    show_system_usage: bool,
    #[serde(default = "enabled_by_default")]
    share_system_audio: bool,
}

fn default_ui_sound_volume() -> f32 {
    1.0
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio: UiAudioConfig::default(),
            ui_sound_volume: default_ui_sound_volume(),
            video: VideoConfig::default(),
            theme: Theme::default(),
            window_frame_style: WindowFrameStyle::default(),
            configured: false,
            chat_retention: RetentionPolicy::Unlimited,
            chat_style: ChatStyle::default(),
            max_image_bytes: None,
            start_with_system: false,
            show_system_usage: false,
            share_system_audio: true,
        }
    }
}

impl From<&UiAudioConfig> for AudioConfig {
    fn from(value: &UiAudioConfig) -> Self {
        let input_device = if value.selected_input == DEFAULT {
            None
        } else {
            Some(value.selected_input.to_string())
        };
        let output_device = if value.selected_output == DEFAULT {
            None
        } else {
            Some(value.selected_output.to_string())
        };
        AudioConfig {
            input_device,
            output_device,
            processing_enabled: value.processing_enabled,
            noise_suppression_enabled: value.noise_suppression_enabled,
            quality: value.quality,
        }
    }
}

impl Default for UiAudioConfig {
    fn default() -> Self {
        Self {
            selected_input: DEFAULT.to_string(),
            selected_output: DEFAULT.to_string(),
            processing_enabled: true,
            noise_suppression_enabled: true,
            quality: AudioQuality::default(),
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // The notification viewport shares this clear color with the root viewport.
        // Root panels paint their own opaque background when needed.
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        ctx.style_mut(|style| style.interaction.selectable_labels = false);
        self.process_tray(ctx);
        if self.state.service.take_activation_request() {
            self.show_window(ctx);
        }
        self.handle_close_request(ctx);
        if !self.window_visible {
            // tray-icon wakes egui when possible; this low-frequency fallback
            // also covers window systems that do not deliver repaint requests
            // while the native window is hidden.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        #[cfg(windows)]
        while let Some(action) = self
            .global_hotkeys
            .as_ref()
            .and_then(crate::global_hotkeys::GlobalHotkeys::try_recv)
        {
            match action {
                crate::global_hotkeys::Action::ToggleMute => self.state.toggle_muted(),
                crate::global_hotkeys::Action::ToggleDeafen => self.state.toggle_deafened(),
            }
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            info!("Wire root viewport close requested");
        }
        if self.is_first_update {
            self.is_first_update = false;
            let repaint_ctx = ctx.clone();
            let callback = Arc::new(move || repaint_ctx.request_repaint());
            self.state.service.set_update_callback(callback);
            self.state.service.set_presenter_active(self.window_visible);
            if self.activation_watcher.is_none() {
                let hwnd = {
                    #[cfg(windows)]
                    {
                        native_parent_hwnd(frame).map(|hwnd| hwnd.0 as isize)
                    }
                    #[cfg(not(windows))]
                    {
                        None
                    }
                };
                self.activation_watcher = Some(ActivationWatcher::start(
                    self.state.service.activation_path(),
                    ctx.clone(),
                    hwnd,
                ));
            }
            #[cfg(windows)]
            if let Some(tray) = self.tray.as_ref() {
                if let Some(hwnd) = native_parent_hwnd(frame) {
                    tray.set_wake_window(hwnd.0 as isize);
                }
            }
            #[cfg(windows)]
            if self.state.dev_pair.is_none() {
                self.state.start_update_check(ctx);
            }
        }
        // on android, add some space at the top.
        #[cfg(target_os = "android")]
        egui::TopBottomPanel::top("my_panel")
            .min_height(40.)
            .show(ctx, |_ui| {});

        #[cfg(windows)]
        let parent_hwnd = native_parent_hwnd(frame);
        #[cfg(windows)]
        if let Some(watcher) = self.activation_watcher.as_ref() {
            watcher.set_hwnd(parent_hwnd.map(|hwnd| hwnd.0 as isize));
        }
        self.state.update(
            ctx,
            &mut self.always_on_top,
            &mut self.viewport_transparent,
            self.window_visible,
            #[cfg(windows)]
            parent_hwnd,
        );
        if self.state.exit_requested {
            self.quit_requested = true;
        }
    }
}

impl App {
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if self.quit_requested || !self.close_to_tray {
            return;
        }
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        info!("Wire root viewport close requested; hiding to the system tray");
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if !self.window_visible {
            return;
        }
        self.window_visible = false;
        self.state.service.set_presenter_active(false);
        self.hidden_video_nodes = self.state.pause_remote_video_for_hidden_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        info!("showing Wire from the system tray");
        if !self.window_visible {
            self.window_visible = true;
            let discarded = self.state.service.discard_buffered_media_events();
            if discarded > 0 {
                debug!(
                    discarded,
                    "discarded stale events before restoring the window"
                );
            }
            self.state.service.set_presenter_active(true);
            let hidden_nodes = std::mem::take(&mut self.hidden_video_nodes);
            self.state.resume_hidden_video(hidden_nodes);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn quit(&mut self, ctx: &egui::Context) {
        if self.quit_requested {
            return;
        }
        self.quit_requested = true;
        info!("quit requested from the system tray");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn process_tray(&mut self, ctx: &egui::Context) {
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        while let Some(action) = self.tray.as_ref().and_then(TrayController::try_recv) {
            match action {
                TrayAction::Show => self.show_window(ctx),
                TrayAction::Quit => {
                    self.quit(ctx);
                    break;
                }
            }
        }
    }
}

fn wgpu_surface_error_action(
    error: eframe::wgpu::SurfaceError,
) -> eframe::egui_wgpu::SurfaceErrorAction {
    use eframe::egui_wgpu::SurfaceErrorAction;
    use eframe::wgpu::SurfaceError;

    match error {
        SurfaceError::Outdated => {
            // Windows reports this while a window is minimized. The resize path
            // configures the surface again once the window has a usable size.
            debug!("skipping frame for outdated wgpu surface");
            SurfaceErrorAction::SkipFrame
        }
        SurfaceError::Timeout => {
            warn!("wgpu surface acquisition timed out; skipping frame");
            SurfaceErrorAction::SkipFrame
        }
        SurfaceError::Lost => {
            warn!("wgpu surface was lost; reconfiguring before the next frame");
            SurfaceErrorAction::RecreateSurface
        }
        SurfaceError::Other => {
            warn!("wgpu surface acquisition failed; reconfiguring before the next frame");
            SurfaceErrorAction::RecreateSurface
        }
        SurfaceError::OutOfMemory => {
            tracing::error!("wgpu could not acquire a surface texture because GPU memory is exhausted; skipping frame");
            SurfaceErrorAction::SkipFrame
        }
    }
}

fn handle_uncaptured_wgpu_error(error: eframe::wgpu::Error) {
    let kind = match &error {
        eframe::wgpu::Error::OutOfMemory { .. } => "out-of-memory",
        eframe::wgpu::Error::Validation { .. } => "validation",
        eframe::wgpu::Error::Internal { .. } => "internal",
    };
    tracing::error!(
        kind,
        error = %error,
        details = ?error,
        "uncaptured wgpu device error; suppressing the default panic so the renderer can attempt recovery"
    );
}

#[cfg(test)]
mod wgpu_error_tests {
    use super::*;
    use eframe::{egui_wgpu::SurfaceErrorAction, wgpu::SurfaceError};

    #[test]
    fn reconfigures_surfaces_that_can_recover() {
        assert!(matches!(
            wgpu_surface_error_action(SurfaceError::Lost),
            SurfaceErrorAction::RecreateSurface
        ));
        assert!(matches!(
            wgpu_surface_error_action(SurfaceError::Other),
            SurfaceErrorAction::RecreateSurface
        ));
    }

    #[test]
    fn skips_frames_for_transient_or_resource_errors() {
        for error in [
            SurfaceError::Outdated,
            SurfaceError::Timeout,
            SurfaceError::OutOfMemory,
        ] {
            assert!(matches!(
                wgpu_surface_error_action(error),
                SurfaceErrorAction::SkipFrame
            ));
        }
    }

    #[test]
    fn uncaptured_device_errors_are_logged_without_panicking() {
        handle_uncaptured_wgpu_error(eframe::wgpu::Error::Internal {
            source: Box::new(std::io::Error::other("synthetic GPU error")),
            description: "synthetic GPU error".to_owned(),
        });
    }
}

impl App {
    pub fn initial_window_frame_style() -> WindowFrameStyle {
        load_settings()
            .map(|settings| settings.window_frame_style)
            .unwrap_or_default()
    }

    pub fn run(
        options: NativeOptions,
        service: ServiceClient,
        start_hidden: bool,
    ) -> Result<(), eframe::Error> {
        let mut options = options;
        options.wgpu_options.on_surface_error = Arc::new(wgpu_surface_error_action);
        let devices =
            wire::audio::AudioContext::list_devices_sync().expect("failed to list audio devices");
        let saved_settings = load_settings();
        let dev_fixture = std::env::var_os("WIRE_DEV_PAIR_SESSION").is_some();
        let has_saved_settings = saved_settings
            .as_ref()
            .map(|settings| settings.configured)
            .unwrap_or(false)
            || dev_fixture;
        let settings = saved_settings.unwrap_or_default();
        #[cfg(windows)]
        let (update_tx, update_rx) = mpsc::channel();
        let (autostart_tx, autostart_rx) = mpsc::channel();
        let ui_sound_volume = settings.ui_sound_volume.clamp(0.0, 1.0);
        let sounds = Sounds::try_new(ui_sound_volume);
        if let Some(sounds) = &sounds {
            sounds.play(Sound::Whoosh2);
        }
        let state = AppState {
            configured: has_saved_settings,
            show_settings: !has_saved_settings,
            show_contacts: false,
            pane_viewport: Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1100.0, 720.0)),
            stream_view_mode: StreamViewMode::Normal,
            remote_node_id: Default::default(),
            remote_node_input: String::new(),
            service,
            our_node_id: None,
            devices,
            audio_config: settings.audio,
            video_config: settings.video,
            calls: Default::default(),
            volumes: Default::default(),
            stream_volumes: Default::default(),
            local_audio_level: None,
            remote_audio_levels: Default::default(),
            video_frames: Default::default(),
            video_stream_generations: Default::default(),
            ended_video_stream_generations: Default::default(),
            stopped_video_stream_generations: Default::default(),
            focused_stream: None,
            volume_open: BTreeSet::new(),
            sharing_active: false,
            share_system_audio: settings.share_system_audio,
            system_audio_active: false,
            capture_error: None,
            show_capture_picker: false,
            capture_targets: Vec::new(),
            selected_capture_target: None,
            preview: None,
            friends: load_friends(),
            friend_status: BTreeMap::new(),
            group_call_reports: BTreeMap::new(),
            local_group_call: None,
            seen_group_calls: load_seen_group_calls(),
            new_friend_name: String::new(),
            new_friend_id: String::new(),
            theme: settings.theme,
            window_frame_style: settings.window_frame_style,
            muted: false,
            deafened: false,
            ui_sound_volume,
            sounds,
            notifications: NotificationService::default(),
            voluntary_hangups: AtomicU32::new(0),
            #[cfg(windows)]
            update_tx,
            #[cfg(windows)]
            update_rx,
            autostart_tx,
            autostart_rx,
            #[cfg(windows)]
            update_status: UpdateStatus::Idle,
            #[cfg(windows)]
            show_update_prompt: false,
            resource_monitor: ResourceMonitor::start(),
            dev_pair: DevPairState::from_env(),
            dev_auto_share: std::env::var_os("WIRE_DEV_AUTO_SHARE").is_some(),
            exit_requested: false,
            app_mode: AppMode::Text,
            chat: ChatUiState::default(),
            chat_notifications_ready: false,
            chat_retention: settings.chat_retention,
            chat_style: settings.chat_style,
            max_image_bytes: settings.max_image_bytes,
            start_with_system: settings.start_with_system,
            saved_start_with_system: settings.start_with_system,
            show_system_usage: settings.show_system_usage,
        };

        if has_saved_settings {
            state.cmd(Command::SetAudioConfig {
                audio_config: state.audio_config(),
            });
            state.cmd(Command::SetVideoConfig {
                video_config: state.video_config,
            });
        }
        state.cmd(Command::SetMaxImageBytes {
            max_image_bytes: state.max_image_bytes,
        });
        state.cmd(Command::SetChatRetention {
            retention: state.chat_retention,
        });
        state.sync_friends_with_worker();

        let rounded = window_frame::style_wants_rounded(state.window_frame_style);
        let app = App {
            state,
            is_first_update: true,
            always_on_top: false,
            viewport_transparent: Some(rounded),
            close_to_tray: false,
            quit_requested: false,
            window_visible: !start_hidden,
            hidden_video_nodes: BTreeSet::new(),
            activation_watcher: None,
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            tray: None,
            #[cfg(windows)]
            global_hotkeys: None,
        };
        eframe::run_native(
            "wire",
            options,
            Box::new(move |cc| {
                if let Some(render_state) = &cc.wgpu_render_state {
                    render_state
                        .device
                        .on_uncaptured_error(Arc::new(handle_uncaptured_wgpu_error));
                    let adapter = render_state.adapter.get_info();
                    tracing::info!(
                        backend = ?adapter.backend,
                        adapter = %adapter.name,
                        "initialized UI renderer"
                    );
                }
                setup_fonts(&cc.egui_ctx);
                if start_hidden {
                    // Keep startup genuinely headless even on window systems
                    // that create the native surface before applying the
                    // ViewportBuilder visibility flag.
                    cc.egui_ctx
                        .send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                #[allow(unused_mut)]
                let mut app = app;
                #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
                if !dev_fixture {
                    match TrayController::new(&cc.egui_ctx) {
                        Ok(tray) => {
                            app.tray = Some(tray);
                            app.close_to_tray = true;
                            info!("system tray icon is available");
                        }
                        Err(error) if start_hidden => {
                            return Err(Box::new(std::io::Error::other(error.to_string())));
                        }
                        Err(error) => warn!("system tray icon could not be created: {error:#}"),
                    }
                }
                #[cfg(windows)]
                if !dev_fixture {
                    app.global_hotkeys = Some(crate::global_hotkeys::GlobalHotkeys::start(
                        cc.egui_ctx.clone(),
                    ));
                }
                Ok(Box::new(app))
            }),
        )
    }
}
impl AppState {
    /// Remember the current viewport when it can host floating panes.
    fn track_pane_viewport(&mut self, ctx: &egui::Context) {
        let content = ctx.content_rect();
        let tracked = track_pane_viewport(self.pane_viewport, content);
        if tracked != self.pane_viewport {
            // The viewport settled after a minimize/restore transition; make
            // sure a fresh frame paints the panes at the recovered size.
            self.pane_viewport = tracked;
            ctx.request_repaint();
        }
    }

    fn pane_constrain_rect(&self) -> Rect {
        self.pane_viewport
    }

    fn update(
        &mut self,
        ctx: &egui::Context,
        always_on_top: &mut bool,
        viewport_transparent: &mut Option<bool>,
        window_visible: bool,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        if self.show_system_usage {
            // Keep the optional process resource readout current while the rest of the UI is idle.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if window_visible && self.has_visible_call() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        self.track_pane_viewport(ctx);
        #[cfg(windows)]
        self.process_update_events(ctx);
        self.process_autostart_events();
        let pal = Palette::for_theme(self.theme);
        ctx.set_visuals(visuals_for(&pal));

        self.process_notification_actions(ctx);
        self.process_events(ctx);
        self.mark_visible_conversation_seen(ctx);
        if self.notifications.take_sound_request() {
            self.play_sound(Sound::Notification);
        }
        #[cfg(windows)]
        for frame in self.video_frames.values_mut() {
            if let Some(presenter) = &mut frame.presenter {
                presenter.mark_unused();
            }
        }
        self.handle_view_mode_input(ctx);
        if (self.app_mode == AppMode::Text || !self.has_active_call())
            && self.stream_view_mode != StreamViewMode::Normal
        {
            self.set_stream_view_mode(ctx, StreamViewMode::Normal);
        }
        if self.app_mode != AppMode::Text {
            if self
                .chat
                .image_preview
                .as_ref()
                .is_some_and(|preview| preview.mode == ImagePreviewMode::Fullscreen)
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
            self.chat.image_preview = None;
        }

        if !self.stream_view_mode.is_fullscreen() {
            let rounded = window_frame::effective_rounded(ctx, self.window_frame_style);
            self.ui_with_chrome(
                ctx,
                &pal,
                rounded,
                always_on_top,
                viewport_transparent,
                #[cfg(windows)]
                parent_hwnd,
            );
        } else {
            if *viewport_transparent != Some(false) {
                window_frame::sync_viewport_transparent(ctx, false);
                *viewport_transparent = Some(false);
                ctx.request_repaint();
            }
            egui::CentralPanel::default()
                .frame(Frame::NONE)
                .show(ctx, |ui| {
                    self.ui_stage(
                        ui,
                        ctx,
                        &pal,
                        #[cfg(windows)]
                        parent_hwnd,
                    )
                });
            egui::Area::new(egui::Id::new("fullscreen-mode-switcher"))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(12.0, 12.0))
                .show(ctx, |ui| self.ui_mode_switcher(ui, &pal));
            egui::Area::new(egui::Id::new("fullscreen-contacts"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
                .show(ctx, |ui| {
                    if ghost_icon_button(ui, &pal, ph::ADDRESS_BOOK)
                        .on_hover_text("Contacts and calling")
                        .clicked()
                    {
                        self.show_contacts = true;
                    }
                });
        }

        let contacts_visible =
            self.app_mode == AppMode::Calls && (self.show_contacts || !self.has_active_call());
        if contacts_visible && !self.show_settings && self.configured {
            self.ui_contacts_window(ctx);
        }
        if self.show_settings || !self.configured {
            self.ui_settings_window(ctx);
        }
        if self.show_capture_picker {
            self.ui_capture_picker(ctx, &pal);
        }
        #[cfg(windows)]
        if self.show_update_prompt {
            self.ui_update_prompt(ctx);
        }
        self.notifications.show(ctx, self.theme);
        #[cfg(windows)]
        {
            let force_hide = self.show_settings
                || !self.configured
                || self.show_update_prompt
                || contacts_visible
                || self.show_capture_picker;
            for frame in self.video_frames.values_mut() {
                if let Some(presenter) = &mut frame.presenter {
                    presenter.hide_if_unused(force_hide);
                }
            }
        }
    }

    #[cfg(windows)]
    fn start_update_check(&mut self, ctx: &egui::Context) {
        if matches!(self.update_status, UpdateStatus::Checking) {
            return;
        }
        self.update_status = UpdateStatus::Checking;
        let tx = self.update_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = update::check_for_update();
            let _ = tx.send(UpdateMessage::CheckFinished(result));
            ctx.request_repaint();
        });
    }

    #[cfg(windows)]
    fn start_update_download(&mut self, ctx: &egui::Context, release: ReleaseInfo) {
        if matches!(self.update_status, UpdateStatus::Downloading(_)) {
            return;
        }
        self.update_status = UpdateStatus::Downloading(release.clone());
        self.show_update_prompt = false;
        let tx = self.update_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = update::download_update(&release);
            let _ = tx.send(UpdateMessage::DownloadFinished(result));
            ctx.request_repaint();
        });
    }

    #[cfg(windows)]
    fn process_update_events(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.update_rx.try_recv() {
            match message {
                UpdateMessage::CheckFinished(Ok(Some(release))) => {
                    self.show_update_prompt = true;
                    self.update_status = UpdateStatus::Available(release);
                }
                UpdateMessage::CheckFinished(Ok(None)) => {
                    self.update_status = UpdateStatus::UpToDate;
                }
                UpdateMessage::CheckFinished(Err(error)) => {
                    self.update_status = UpdateStatus::Error(error.to_string());
                }
                UpdateMessage::DownloadFinished(Ok(path)) => {
                    match update::relaunch_after_download(&path) {
                        Ok(_) => {
                            self.exit_requested = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(error) => {
                            self.update_status = UpdateStatus::Error(format!(
                                "Downloaded the update, but could not relaunch it: {error}",
                            ));
                        }
                    }
                }
                UpdateMessage::DownloadFinished(Err(error)) => {
                    self.update_status = UpdateStatus::Error(error.to_string());
                }
            }
        }
    }

    fn process_autostart_events(&mut self) {
        while let Ok(message) = self.autostart_rx.try_recv() {
            match message.result {
                Ok(()) => {
                    self.saved_start_with_system = message.enabled;
                    self.start_with_system = message.enabled;
                    save_start_with_system(message.enabled);
                }
                Err(error) => {
                    self.start_with_system = self.saved_start_with_system;
                    self.notifications.error(
                        "start-with-system-error",
                        "Could not update startup behavior",
                        error,
                    );
                }
            }
        }
    }

    fn process_notification_actions(&mut self, ctx: &egui::Context) {
        while let Some(action) = self.notifications.try_action() {
            match action {
                NotificationAction::OpenConversation(conversation_id) => {
                    if self.chat.conversations.contains_key(&conversation_id) {
                        self.chat.selected = Some(conversation_id);
                        self.app_mode = AppMode::Text;
                        self.show_settings = false;
                        self.set_stream_view_mode(ctx, StreamViewMode::Normal);
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                NotificationAction::OpenCalls => {
                    self.app_mode = AppMode::Calls;
                    self.show_settings = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                NotificationAction::AcceptCall(node_id) => {
                    if let Ok(node_id) = NodeId::from_str(&node_id) {
                        self.accept_incoming_call(node_id);
                    }
                    self.app_mode = AppMode::Calls;
                    self.show_settings = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                NotificationAction::DeclineCall(node_id) => {
                    if let Ok(node_id) = NodeId::from_str(&node_id) {
                        self.cmd(Command::HandleIncoming {
                            node_id,
                            accept: false,
                        });
                    }
                    self.app_mode = AppMode::Calls;
                    self.show_settings = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
        }
    }

    fn process_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.service.try_recv() {
            match event {
                Event::EndpointBound(node_id) => {
                    self.our_node_id = Some(node_id);
                    self.chat.service_error = None;
                    let (dev_call_peers, dev_fixture_peers) = match self.dev_pair.as_mut() {
                        Some(dev_pair) => match dev_pair.register(node_id) {
                            Ok(call_peers) => match dev_pair.discover_fixture_peers(node_id) {
                                Ok(fixture_peers) => (call_peers, fixture_peers),
                                Err(error) => {
                                    warn!("dev fixture discovery failed: {error:#}");
                                    (call_peers.clone(), call_peers)
                                }
                            },
                            Err(error) => {
                                warn!("dev-call rendezvous failed: {error:#}");
                                (Vec::new(), Vec::new())
                            }
                        },
                        None => (Vec::new(), Vec::new()),
                    };
                    let mut contacts_changed = false;
                    for peer in dev_fixture_peers {
                        let node_id = peer.to_string();
                        if !self.friends.iter().any(|friend| friend.node_id == node_id) {
                            self.friends.push(Friend {
                                name: format!("DEV {}", peer.fmt_short()),
                                node_id,
                            });
                            contacts_changed = true;
                        }
                    }
                    if contacts_changed {
                        self.friends
                            .sort_by(|left, right| left.name.cmp(&right.name));
                        save_friends(&self.friends);
                        self.sync_friends_with_worker();
                        info!("added dev fixture peers as local chat contacts");
                    }
                    for peer in dev_call_peers {
                        info!("dev call initiating automatic call to {}", peer.fmt_short());
                        self.cmd(Command::Call { node_id: peer });
                    }
                }
                Event::ClientStatus(update) => {
                    let peer = update.peer;
                    if matches!(update.availability, Availability::Online) {
                        self.group_call_reports
                            .insert(peer, update.active_group_calls.clone());
                    } else {
                        self.group_call_reports.remove(&peer);
                    }
                    #[cfg(windows)]
                    let peer_is_newer = update.client_version.as_deref().is_some_and(|version| {
                        update::is_version_newer(version, crate::APP_VERSION)
                    });
                    self.friend_status.insert(peer, update);
                    #[cfg(windows)]
                    if peer_is_newer {
                        info!(
                            peer = %peer.fmt_short(),
                            "friend advertised a newer Wire version; checking for updates"
                        );
                        self.start_update_check(ctx);
                    }
                }
                Event::GroupCallEntered(call) => {
                    self.mark_group_call_seen(call.call_id.clone());
                    self.local_group_call = Some(call);
                    self.app_mode = AppMode::Calls;
                }
                Event::InitialChatLoaded => self.chat_notifications_ready = true,
                Event::SetCallState(node_id, call_state) => {
                    let auto_accept =
                        self.dev_pair.is_some() && matches!(call_state, CallState::Incoming);
                    let auto_share = self.dev_auto_share
                        && !self.sharing_active
                        && matches!(call_state, CallState::Active);
                    let previous = self.calls.get(&node_id).copied();
                    let call_key = format!("call:{node_id}");
                    if self.dev_pair.is_none()
                        && !matches!(previous, Some(CallState::Incoming))
                        && matches!(call_state, CallState::Incoming)
                    {
                        self.notifications
                            .incoming_call(node_id.to_string(), self.peer_display_name(node_id));
                    }
                    match call_state {
                        CallState::Active => {
                            self.play_sound(Sound::Success);
                            self.notifications.dismiss_key(&call_key);
                            if !matches!(previous, Some(CallState::Active)) {
                                self.notifications.success_with_body(
                                    format!("call-connected:{node_id}"),
                                    "Call connected",
                                    self.peer_display_name(node_id),
                                );
                            }
                        }
                        CallState::Aborted => {
                            self.notifications.dismiss_key(&call_key);
                            if self.voluntary_hangups.load(Ordering::Relaxed) > 0 {
                                self.voluntary_hangups.fetch_sub(1, Ordering::Relaxed);
                            } else if matches!(
                                previous,
                                Some(CallState::Calling) | Some(CallState::Incoming)
                            ) {
                                self.play_sound(Sound::Fail);
                            }
                        }
                        _ => {}
                    }

                    if matches!(call_state, CallState::Aborted) {
                        self.calls.remove(&node_id);
                        self.volumes.remove(&node_id);
                        self.stream_volumes.remove(&node_id);
                        self.remote_audio_levels.remove(&node_id);
                        self.video_frames.remove(&node_id);
                        self.video_stream_generations.remove(&node_id);
                        self.ended_video_stream_generations.remove(&node_id);
                        self.stopped_video_stream_generations.remove(&node_id);
                        self.volume_open.remove(&node_id);
                        if self.focused_stream == Some(StreamSource::Remote(node_id)) {
                            self.focused_stream = None;
                        }
                    } else {
                        self.calls.insert(node_id, call_state);
                    }
                    if self.sharing_active && !self.has_active_call() {
                        self.cmd(Command::ToggleSharing {
                            enabled: false,
                            target: None,
                            share_system_audio: false,
                        });
                    }

                    let has_incoming = self.dev_pair.is_none()
                        && self
                            .calls
                            .values()
                            .any(|state| matches!(state, CallState::Incoming));
                    if let Some(sounds) = &mut self.sounds {
                        sounds.set_incoming_ring(has_incoming);
                    }
                    if auto_accept {
                        info!(
                            "dev call automatically accepting call from {}",
                            node_id.fmt_short()
                        );
                        self.cmd(Command::HandleIncoming {
                            node_id,
                            accept: true,
                        });
                    }
                    if auto_share {
                        info!("dev call automatically starting the explicit test share");
                        self.cmd(Command::ToggleSharing {
                            enabled: true,
                            target: None,
                            share_system_audio: self.share_system_audio,
                        });
                        if let Some(cycles) = std::env::var("WIRE_DEV_SHARE_TOGGLE_CYCLES")
                            .ok()
                            .and_then(|value| value.parse::<u32>().ok())
                            .filter(|cycles| *cycles > 0)
                        {
                            let command_tx = self.service.command_sender().clone();
                            std::thread::spawn(move || {
                                for cycle in 0..cycles {
                                    std::thread::sleep(Duration::from_secs(2));
                                    if command_tx
                                        .send_blocking(Command::ToggleSharing {
                                            enabled: false,
                                            target: None,
                                            share_system_audio: false,
                                        })
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if cycle + 1 < cycles {
                                        std::thread::sleep(Duration::from_secs(1));
                                        if command_tx
                                            .send_blocking(Command::ToggleSharing {
                                                enabled: true,
                                                target: None,
                                                share_system_audio: true,
                                            })
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
                Event::LocalAudioLevel(level) => {
                    self.local_audio_level = Some(level);
                }
                Event::ParticipantAudioHandles {
                    node_id,
                    volume,
                    stream_volume,
                    level,
                } => {
                    self.volumes.insert(node_id, volume);
                    self.stream_volumes.insert(node_id, stream_volume);
                    self.remote_audio_levels.insert(node_id, level);
                }
                Event::VideoStreamAccepted {
                    node_id,
                    generation,
                } => {
                    if !matches!(self.calls.get(&node_id), Some(CallState::Active)) {
                        continue;
                    }
                    if self
                        .video_stream_generations
                        .get(&node_id)
                        .is_some_and(|current| generation < *current)
                    {
                        continue;
                    }
                    self.video_stream_generations.insert(node_id, generation);
                    self.ended_video_stream_generations.remove(&node_id);
                    if self
                        .stopped_video_stream_generations
                        .get(&node_id)
                        .is_some_and(|stopped| generation > *stopped)
                    {
                        self.stopped_video_stream_generations.remove(&node_id);
                    }
                }
                Event::VideoFrame {
                    node_id,
                    generation,
                    frame,
                } => {
                    if !matches!(self.calls.get(&node_id), Some(CallState::Active)) {
                        continue;
                    }
                    if self
                        .video_stream_generations
                        .get(&node_id)
                        .is_some_and(|current| generation < *current)
                        || self
                            .ended_video_stream_generations
                            .get(&node_id)
                            .is_some_and(|ended| generation <= *ended)
                        || self
                            .stopped_video_stream_generations
                            .get(&node_id)
                            .is_some_and(|stopped| generation <= *stopped)
                    {
                        continue;
                    }
                    self.video_stream_generations.insert(node_id, generation);
                    let state =
                        self.video_frames
                            .entry(node_id)
                            .or_insert_with(|| VideoFrameState {
                                width: 0,
                                height: 0,
                                stream_generation: generation,
                                generation: 0,
                                source_height: 0,
                                source_fps: 0,
                                data: DecodedFrameData::Rgba(Arc::new(Vec::new())),
                                texture: None,
                                uploaded_generation: 0,
                                upload_stats: TextureUploadStats::default(),
                                #[cfg(windows)]
                                presenter: None,
                                #[cfg(windows)]
                                native_present_failed: false,
                            });
                    state.stream_generation = generation;
                    state.width = frame.width;
                    state.height = frame.height;
                    state.source_height = frame.source_height;
                    state.source_fps = frame.source_fps;
                    state.data = frame.data;
                    #[cfg(windows)]
                    if matches!(&state.data, DecodedFrameData::D3d11(_)) {
                        state.texture = None;
                    }
                    state.generation += 1;
                }
                Event::VideoStreamEnded {
                    node_id,
                    generation,
                    reason,
                } => {
                    let is_current = self
                        .video_stream_generations
                        .get(&node_id)
                        .is_some_and(|current| *current == generation);
                    if is_current {
                        self.ended_video_stream_generations
                            .insert(node_id, generation);
                        if self
                            .stopped_video_stream_generations
                            .get(&node_id)
                            .is_some_and(|stopped| *stopped == generation)
                        {
                            self.stopped_video_stream_generations.remove(&node_id);
                        }
                        self.video_frames.remove(&node_id);
                        if self.focused_stream == Some(StreamSource::Remote(node_id)) {
                            self.focused_stream = None;
                        }
                        info!(
                            node = %node_id.fmt_short(),
                            generation,
                            reason = ?reason,
                            "cleared ended video stream"
                        );
                    } else {
                        info!(
                            node = %node_id.fmt_short(),
                            generation,
                            reason = ?reason,
                            "ignored stale video stream end"
                        );
                    }
                }
                Event::SharingToggled {
                    active,
                    system_audio,
                } => {
                    self.sharing_active = active;
                    self.system_audio_active = active && system_audio;
                    if active {
                        self.capture_error = None;
                        self.notifications.success(
                            "screen-sharing",
                            if system_audio {
                                "Sharing screen and sound"
                            } else {
                                "Screen sharing started"
                            },
                        );
                    } else {
                        self.notifications
                            .info("screen-sharing", "Screen sharing stopped");
                    }
                    if !active {
                        self.preview = None;
                        if self.focused_stream == Some(StreamSource::Local) {
                            self.focused_stream = None;
                        }
                    }
                }
                Event::SystemAudioToggled(active) => {
                    self.system_audio_active = active && self.sharing_active;
                }
                Event::SystemAudioFailed(message) => {
                    self.system_audio_active = false;
                    self.notifications.error(
                        "system-audio-error",
                        "Could not share computer sound",
                        message,
                    );
                }
                Event::SharingFailed(message) => {
                    self.sharing_active = false;
                    self.system_audio_active = false;
                    self.preview = None;
                    self.notifications.error(
                        "screen-sharing-error",
                        "Could not share your screen",
                        message.clone(),
                    );
                    self.capture_error = Some(message);
                }
                Event::PreviewFrame {
                    width,
                    height,
                    data,
                    actual_fps,
                    encode_time_ms,
                } => {
                    if !self.sharing_active {
                        continue;
                    }
                    let preview = self.preview.get_or_insert_with(|| PreviewState {
                        width: 0,
                        height: 0,
                        actual_fps: 0.0,
                        encode_time_ms: 0.0,
                        generation: 0,
                        data: Arc::new(Vec::new()),
                        texture: None,
                        uploaded_generation: 0,
                        upload_stats: TextureUploadStats::default(),
                    });
                    preview.width = width;
                    preview.height = height;
                    preview.actual_fps = actual_fps;
                    preview.encode_time_ms = encode_time_ms;
                    preview.data = data;
                    preview.generation += 1;
                }
                Event::Chat(notification) => self.apply_chat_notification(notification, ctx),
                Event::WorkerFailed(error) => {
                    warn!("Wire worker unavailable: {error}");
                    self.notifications.error(
                        "wire-worker-error",
                        "Wire is unavailable",
                        error.clone(),
                    );
                    self.chat.service_error = Some(error);
                }
            }
        }
    }

    fn apply_chat_notification(&mut self, notification: ChatNotification, ctx: &egui::Context) {
        match notification {
            ChatNotification::Conversation {
                conversation,
                messages,
                has_more,
            } => {
                let id = conversation.id.clone();
                let conversation_is_new = !self.chat.conversations.contains_key(&id);
                let known_messages = self.chat.timelines.get(&id).map(|timeline| {
                    timeline
                        .iter()
                        .map(|message| message.message_id.clone())
                        .collect::<BTreeSet<_>>()
                });
                let root_focused = ctx.input(|input| input.viewport().focused == Some(true));
                let conversation_is_open = root_focused
                    && self.app_mode == AppMode::Text
                    && !self.show_settings
                    && self.chat.selected.as_deref() == Some(id.as_str());
                let our_node_id = self.our_node_id.map(|node_id| node_id.to_string());
                let new_remote_messages = known_messages
                    .as_ref()
                    .map(|known| {
                        messages
                            .iter()
                            .filter(|message| {
                                !known.contains(&message.message_id)
                                    && message.deletion.is_none()
                                    && our_node_id.as_deref() != Some(message.author_id.as_str())
                            })
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .or_else(|| {
                        self.chat_notifications_ready.then(|| {
                            let recent_cutoff = chat::now_millis() - 10 * 60 * 1000;
                            messages
                                .iter()
                                .filter(|message| {
                                    message.sent_at >= recent_cutoff
                                        && message.deletion.is_none()
                                        && our_node_id.as_deref()
                                            != Some(message.author_id.as_str())
                                })
                                .max_by_key(|message| message.sent_at)
                                .cloned()
                                .into_iter()
                                .collect::<Vec<_>>()
                        })
                    });
                let conversation_title = conversation.title.clone();
                let mark_unseen = should_mark_conversation_unseen(
                    conversation_is_open,
                    new_remote_messages.as_deref(),
                );
                self.chat.conversations.insert(id.clone(), conversation);
                if conversation_is_new {
                    self.sync_friends_with_worker();
                }
                let mut by_id: BTreeMap<_, _> = messages
                    .into_iter()
                    .map(|message| (message.message_id.clone(), message))
                    .collect();
                // Keep only very recent optimistic local sends that the service
                // has not echoed yet. Older pending rows must not survive a
                // replicated history clear (or any authoritative empty snapshot).
                let optimistic_cutoff = chat::now_millis() - 30_000;
                if let Some(existing) = self.chat.timelines.get(&id) {
                    for message in existing {
                        if by_id.contains_key(&message.message_id) {
                            continue;
                        }
                        if message.sent_at < optimistic_cutoff {
                            self.chat.delivery.remove(&message.message_id);
                            continue;
                        }
                        if our_node_id.as_deref() == Some(message.author_id.as_str())
                            && matches!(
                                self.chat.delivery.get(&message.message_id),
                                Some((
                                    DeliveryState::Pending
                                        | DeliveryState::Retrying
                                        | DeliveryState::Queued
                                        | DeliveryState::Failed,
                                    _,
                                ))
                            )
                        {
                            by_id
                                .entry(message.message_id.clone())
                                .or_insert_with(|| message.clone());
                        } else {
                            self.chat.delivery.remove(&message.message_id);
                        }
                    }
                }
                let mut timeline: Vec<_> = by_id.into_values().collect();
                timeline.sort();
                self.chat.timelines.insert(id.clone(), timeline);
                if has_more {
                    self.chat
                        .conversations_with_older_messages
                        .insert(id.clone());
                } else {
                    self.chat.conversations_with_older_messages.remove(&id);
                }
                if self.chat.selected.is_none() {
                    self.chat.selected = Some(id.clone());
                }
                if !conversation_is_open {
                    if mark_unseen {
                        self.chat.unseen.insert(id.clone());
                    }
                    for message in new_remote_messages.into_iter().flatten() {
                        let author = NodeId::from_str(&message.author_id)
                            .ok()
                            .map(|node_id| self.peer_display_name(node_id))
                            .unwrap_or_else(|| "New message".to_owned());
                        self.notifications.message(
                            id.clone(),
                            conversation_title.clone(),
                            author,
                            if message.body.trim().is_empty() {
                                match message.attachments.len() {
                                    1 => "Image".to_owned(),
                                    count => format!("{count} images"),
                                }
                            } else {
                                ellipsize(message.body.trim(), 180)
                            },
                        );
                    }
                }
            }
            ChatNotification::AttachmentData { hash, data } => {
                self.chat.attachment_requests.remove(&hash);
                let attachment = self
                    .chat
                    .timelines
                    .values()
                    .flat_map(|timeline| timeline.iter())
                    .flat_map(|message| message.attachments.iter())
                    .find(|attachment| attachment.hash == hash)
                    .cloned();
                if let Some(attachment) = attachment {
                    let _ = self
                        .chat
                        .attachment_textures
                        .insert_data(ctx, &attachment, data);
                }
            }
            ChatNotification::Delivery {
                message_id,
                state,
                detail,
            } => {
                self.chat.delivery.insert(message_id, (state, detail));
            }
            ChatNotification::Error(error) => {
                self.notifications
                    .error("chat-error", "Messages need attention", error.clone());
                self.chat.error = Some(error);
            }
        }
    }

    fn mark_visible_conversation_seen(&mut self, ctx: &egui::Context) {
        let root_focused = ctx.input(|input| input.viewport().focused == Some(true));
        if root_focused && self.app_mode == AppMode::Text && !self.show_settings {
            if let Some(id) = &self.chat.selected {
                self.chat.unseen.remove(id);
            }
        }
    }

    fn handle_view_mode_input(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.show_capture_picker {
                self.show_capture_picker = false;
            } else if let Some(mode) = self.chat.image_preview.as_ref().map(|preview| preview.mode)
            {
                if mode == ImagePreviewMode::Fullscreen {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
                if mode == ImagePreviewMode::Floating {
                    self.chat.image_preview = None;
                } else if let Some(preview) = &mut self.chat.image_preview {
                    preview.mode = ImagePreviewMode::Floating;
                }
            } else if self.focused_stream.is_some() {
                self.focused_stream = None;
            } else {
                self.set_stream_view_mode(ctx, StreamViewMode::Normal);
            }
        }
    }

    fn local_stream_ready(&self) -> bool {
        self.sharing_active
            && self
                .preview
                .as_ref()
                .is_some_and(|p| p.width > 0 && p.height > 0 && !p.data.is_empty())
    }

    fn has_active_call(&self) -> bool {
        self.calls
            .values()
            .any(|state| matches!(state, CallState::Active))
    }

    fn open_capture_picker(&mut self) {
        match crate::screen_capture::list_capture_targets() {
            Ok(targets) => {
                self.selected_capture_target = targets
                    .iter()
                    .position(|target| target.is_primary)
                    .or_else(|| (!targets.is_empty()).then_some(0));
                self.capture_targets = targets;
                self.capture_error = None;
                self.show_capture_picker = true;
            }
            Err(error) => {
                let message = error.to_string();
                self.capture_error = Some(message.clone());
                self.notifications.error(
                    "screen-sharing-error",
                    "Could not list screens and windows",
                    message,
                );
            }
        }
    }

    fn stop_sharing_from_ui(&mut self) {
        self.play_control_sound(false);
        self.cmd(Command::ToggleSharing {
            enabled: false,
            target: None,
            share_system_audio: false,
        });
    }

    fn set_share_system_audio_from_ui(&mut self, enabled: bool) {
        if self.share_system_audio == enabled && self.system_audio_active == enabled {
            return;
        }
        self.share_system_audio = enabled;
        self.persist_settings();
        if self.sharing_active {
            self.cmd(Command::SetSystemAudio { enabled });
        }
    }

    fn toggle_sharing_from_ui(&mut self) {
        if self.sharing_active {
            self.stop_sharing_from_ui();
        } else {
            self.open_capture_picker();
        }
    }

    fn has_visible_call(&self) -> bool {
        self.local_group_call.is_some()
            || self.calls.values().any(|state| {
                matches!(
                    state,
                    CallState::Incoming | CallState::Calling | CallState::Active
                )
            })
    }

    fn group_call_for(&self, conversation_id: &str) -> Option<GroupCallAnnouncement> {
        let mut merged = self
            .local_group_call
            .iter()
            .chain(self.group_call_reports.values().flatten())
            .filter(|call| call.conversation_id == conversation_id && call.ended_at_ms.is_none())
            .max_by_key(|call| call.started_at_ms)
            .cloned()?;
        let call_id = merged.call_id.clone();
        let mut participants = merged.participants.into_iter().collect::<BTreeSet<_>>();
        for call in self
            .local_group_call
            .iter()
            .chain(self.group_call_reports.values().flatten())
            .filter(|call| call.call_id == call_id)
        {
            participants.extend(call.participants.iter().cloned());
        }
        merged.participants = participants.into_iter().collect();
        Some(merged)
    }

    fn enter_group_call(
        &mut self,
        conversation: &ChatConversation,
        existing: Option<GroupCallAnnouncement>,
    ) {
        let Some(our_node_id) = self.our_node_id else {
            return;
        };
        self.acknowledge_missed_group_calls(&conversation.id);
        let members = conversation
            .members
            .iter()
            .filter_map(|member| NodeId::from_str(member).ok())
            .filter(|member| *member != our_node_id)
            .collect::<Vec<_>>();
        let mut call = existing.unwrap_or_else(|| GroupCallAnnouncement {
            call_id: format!(
                "{}:{}:{}",
                conversation.id,
                our_node_id.fmt_short(),
                chat::now_millis()
            ),
            conversation_id: conversation.id.clone(),
            title: conversation.title.clone(),
            initiator: our_node_id.to_string(),
            started_at_ms: chat::now_millis(),
            ended_at_ms: None,
            participants: Vec::new(),
        });
        if !call
            .participants
            .iter()
            .any(|peer| peer == &our_node_id.to_string())
        {
            call.participants.push(our_node_id.to_string());
        }
        call.participants.sort();
        call.participants.dedup();

        let advertised = call
            .participants
            .iter()
            .filter_map(|peer| NodeId::from_str(peer).ok())
            .collect::<BTreeSet<_>>();
        let targets = members
            .iter()
            .copied()
            .filter(|peer| {
                advertised.contains(peer)
                    || self
                        .friend_status
                        .get(peer)
                        .is_some_and(|status| matches!(status.availability, Availability::Online))
            })
            .collect();
        self.local_group_call = Some(call.clone());
        self.mark_group_call_seen(call.call_id.clone());
        self.app_mode = AppMode::Calls;
        self.cmd(Command::EnterGroupCall {
            call,
            targets,
            notify: members,
        });
    }

    fn leave_group_call(&mut self) {
        let Some(call) = self.local_group_call.take() else {
            return;
        };
        let notify = self
            .chat
            .conversations
            .get(&call.conversation_id)
            .map(|conversation| {
                conversation
                    .members
                    .iter()
                    .filter_map(|member| NodeId::from_str(member).ok())
                    .filter(|member| Some(*member) != self.our_node_id)
                    .collect()
            })
            .unwrap_or_default();
        self.cmd(Command::LeaveGroupCall { notify });
    }

    fn acknowledge_missed_group_calls(&mut self, conversation_id: &str) {
        let call_ids = self
            .group_call_reports
            .values()
            .flatten()
            .filter(|call| call.conversation_id == conversation_id && call.ended_at_ms.is_some())
            .map(|call| call.call_id.clone())
            .collect::<BTreeSet<_>>();
        for call_id in call_ids {
            self.mark_group_call_seen(call_id);
        }
    }

    fn mark_group_call_seen(&mut self, call_id: String) {
        let now = chat::now_millis();
        self.seen_group_calls.insert(call_id, now);
        let cutoff = now - SEEN_GROUP_CALL_RETENTION_MS;
        self.seen_group_calls
            .retain(|_, seen_at| *seen_at >= cutoff);
        save_seen_group_calls(&self.seen_group_calls);
    }

    fn active_stream_sources(&self) -> Vec<StreamSource> {
        let mut sources = Vec::new();
        if self.local_stream_ready() {
            sources.push(StreamSource::Local);
        }
        for node_id in self.video_frames.keys() {
            if self.video_frames[node_id].width > 0
                && self.video_frames[node_id].height > 0
                && match &self.video_frames[node_id].data {
                    DecodedFrameData::Rgba(data) => !data.is_empty(),
                    #[cfg(windows)]
                    DecodedFrameData::D3d11(_) => true,
                }
            {
                sources.push(StreamSource::Remote(*node_id));
            }
        }
        sources
    }

    fn stopped_stream_nodes(&self) -> Vec<NodeId> {
        self.stopped_video_stream_generations
            .iter()
            .filter_map(|(node_id, stopped)| {
                (self.video_stream_generations.get(node_id) == Some(stopped)
                    && matches!(self.calls.get(node_id), Some(CallState::Active)))
                .then_some(*node_id)
            })
            .collect()
    }

    fn pause_remote_video_for_hidden_window(&mut self) -> BTreeSet<NodeId> {
        let nodes: BTreeSet<_> = self
            .video_stream_generations
            .keys()
            .copied()
            .filter(|node_id| {
                matches!(
                    self.calls.get(node_id),
                    Some(CallState::Incoming | CallState::Calling | CallState::Active)
                ) && !self.stopped_video_stream_generations.contains_key(node_id)
            })
            .collect();
        for node_id in &nodes {
            self.stop_watching(*node_id);
        }
        self.preview = None;
        nodes
    }

    fn resume_hidden_video(&mut self, nodes: BTreeSet<NodeId>) {
        for node_id in nodes {
            if matches!(self.calls.get(&node_id), Some(CallState::Active)) {
                self.resume_watching(node_id);
            } else {
                self.stopped_video_stream_generations.remove(&node_id);
            }
        }
    }

    fn stop_watching(&mut self, node_id: NodeId) {
        let Some(generation) = self.video_stream_generations.get(&node_id).copied() else {
            return;
        };
        self.stopped_video_stream_generations
            .insert(node_id, generation);
        self.video_frames.remove(&node_id);
        if self.focused_stream == Some(StreamSource::Remote(node_id)) {
            self.focused_stream = None;
        }
        self.cmd(Command::SetWatching {
            node_id,
            generation,
            watching: false,
        });
    }

    fn resume_watching(&mut self, node_id: NodeId) {
        let Some(generation) = self.stopped_video_stream_generations.remove(&node_id) else {
            return;
        };
        self.cmd(Command::SetWatching {
            node_id,
            generation,
            watching: true,
        });
    }

    fn stream_label(&self, source: StreamSource) -> String {
        match source {
            StreamSource::Local if self.system_audio_active => "You · audio".to_string(),
            StreamSource::Local => "You".to_string(),
            StreamSource::Remote(node_id) => self.peer_display_name(node_id),
        }
    }

    fn stream_aspect_ratio(&self, source: StreamSource) -> f32 {
        let dimensions = match source {
            StreamSource::Local => self
                .preview
                .as_ref()
                .map(|preview| (preview.width, preview.height)),
            StreamSource::Remote(node_id) => self
                .video_frames
                .get(&node_id)
                .map(|frame| (frame.width, frame.height)),
        };
        dimensions
            .filter(|(width, height)| *width > 0 && *height > 0)
            .map(|(width, height)| width as f32 / height as f32)
            .unwrap_or(16.0 / 9.0)
    }

    fn friend_name(&self, node_id: NodeId) -> Option<&str> {
        let node_id = node_id.to_string();
        self.friends
            .iter()
            .find(|friend| friend.node_id.trim() == node_id.as_str())
            .and_then(|friend| {
                let name = friend.name.trim();
                (!name.is_empty() && name != friend.node_id.trim()).then_some(name)
            })
    }

    fn is_friend(&self, node_id: NodeId) -> bool {
        let node_id = node_id.to_string();
        self.friends
            .iter()
            .any(|friend| friend.node_id.trim() == node_id)
    }

    fn peer_display_name(&self, node_id: NodeId) -> String {
        self.friend_name(node_id)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Peer {}", node_id.fmt_short()))
    }

    fn peer_initial(&self, node_id: NodeId) -> String {
        self.friend_name(node_id)
            .and_then(|name| name.chars().find(|c| c.is_alphanumeric()))
            .map(|c| c.to_uppercase().to_string())
            .or_else(|| {
                node_id
                    .fmt_short()
                    .to_string()
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().to_string())
            })
            .unwrap_or_else(|| "?".to_owned())
    }

    fn group_members_for(&self, conversation: &ChatConversation) -> Vec<GroupMemberLabel> {
        group_member_labels(&conversation.members, self.our_node_id, |node| {
            self.friend_name(node).map(str::to_owned)
        })
    }

    fn set_stream_view_mode(&mut self, ctx: &egui::Context, mode: StreamViewMode) {
        if self.stream_view_mode == StreamViewMode::Fullscreen && mode != StreamViewMode::Fullscreen
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        if mode == StreamViewMode::Fullscreen {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
        self.stream_view_mode = mode;
    }

    fn audio_config(&self) -> AudioConfig {
        (&self.audio_config).into()
    }

    fn persist_settings(&self) {
        save_settings(&Settings {
            audio: self.audio_config.clone(),
            ui_sound_volume: self.ui_sound_volume,
            video: self.video_config,
            theme: self.theme,
            window_frame_style: self.window_frame_style,
            configured: true,
            chat_retention: self.chat_retention,
            chat_style: self.chat_style,
            max_image_bytes: self.max_image_bytes,
            start_with_system: self.saved_start_with_system,
            show_system_usage: self.show_system_usage,
            share_system_audio: self.share_system_audio,
        });
    }

    fn update_autostart_in_background(&self, ctx: &egui::Context) {
        let enabled = self.start_with_system;
        let tx = self.autostart_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = autostart::set_enabled(enabled).map_err(|error| error.to_string());
            let _ = tx.send(AutostartMessage { enabled, result });
            ctx.request_repaint();
        });
    }

    fn friend_node_ids(&self) -> BTreeSet<NodeId> {
        let mut peers = self
            .friends
            .iter()
            .filter_map(|friend| NodeId::from_str(friend.node_id.trim()).ok())
            .collect::<BTreeSet<_>>();
        peers.extend(
            self.chat
                .conversations
                .values()
                .flat_map(|conversation| conversation.members.iter())
                .filter_map(|member| NodeId::from_str(member).ok())
                .filter(|member| Some(*member) != self.our_node_id),
        );
        peers
    }

    fn sync_friends_with_worker(&self) {
        self.cmd(Command::SetFriends {
            friends: self.friend_node_ids(),
        });
    }

    fn cmd(&self, command: Command) {
        if self.service.command_sender().try_send(command).is_err() {
            warn!("ignored command because the Wire worker is unavailable");
        }
    }

    fn play_sound(&self, sound: Sound) {
        if let Some(sounds) = &self.sounds {
            sounds.play(sound);
        }
    }

    fn play_control_sound(&self, enabled: bool) {
        self.play_sound(if enabled {
            Sound::Button1
        } else {
            Sound::Button2
        });
    }

    fn toggle_muted(&mut self) {
        self.muted = !self.muted;
        info!(muted = self.muted, "mute toggled");
        self.play_control_sound(self.muted);
        self.cmd(Command::SetMuted { muted: self.muted });
    }

    fn toggle_deafened(&mut self) {
        let (deafened, muted) = next_deafen_audio_state(self.deafened);
        self.deafened = deafened;
        self.muted = muted;
        info!(
            deafened = self.deafened,
            muted = self.muted,
            "deafen toggled"
        );
        self.play_control_sound(self.deafened);
        if self.deafened {
            // Stop capture before silencing playback when deafening.
            self.cmd(Command::SetMuted { muted: true });
            self.cmd(Command::SetDeafened { deafened: true });
        } else {
            // Restore playback before capture when undeafening.
            self.cmd(Command::SetDeafened { deafened: false });
            self.cmd(Command::SetMuted { muted: false });
        }
    }

    fn hang_up_call(&self, node_id: NodeId) {
        self.play_sound(Sound::Whoosh1);
        self.voluntary_hangups.fetch_add(1, Ordering::Relaxed);
        self.cmd(Command::Abort { node_id });
    }

    fn incoming_belongs_to_local_group(&self, node_id: NodeId) -> bool {
        !incoming_call_requires_group_switch(
            self.local_group_call.as_ref(),
            self.group_call_reports.get(&node_id).map(Vec::as_slice),
        ) && self.local_group_call.is_some()
    }

    fn accept_incoming_call(&mut self, node_id: NodeId) {
        let switches_away_from_group =
            self.local_group_call.is_some() && !self.incoming_belongs_to_local_group(node_id);
        if switches_away_from_group {
            let current_peers = self
                .calls
                .keys()
                .copied()
                .filter(|peer| *peer != node_id)
                .collect::<Vec<_>>();
            for peer in current_peers {
                self.voluntary_hangups.fetch_add(1, Ordering::Relaxed);
                self.cmd(Command::Abort { node_id: peer });
            }
            self.leave_group_call();
        }
        self.cmd(Command::HandleIncoming {
            node_id,
            accept: true,
        });
    }

    fn add_friend(&mut self) {
        let node_id = self.new_friend_id.trim().to_string();
        if node_id.is_empty() {
            return;
        }
        if NodeId::from_str(&node_id).is_err() {
            return;
        }
        let name = if self.new_friend_name.trim().is_empty() {
            "Unnamed contact".to_owned()
        } else {
            self.new_friend_name.trim().to_string()
        };
        self.add_friend_record(
            NodeId::from_str(&node_id).expect("node ID was validated"),
            name,
        );
        self.new_friend_name.clear();
        self.new_friend_id.clear();
    }

    fn add_friend_record(&mut self, node_id: NodeId, name: String) {
        if self.is_friend(node_id) {
            return;
        }
        self.play_sound(Sound::Button2);
        self.friends.push(Friend {
            name,
            node_id: node_id.to_string(),
        });
        self.friends
            .sort_by(|left, right| left.name.cmp(&right.name));
        save_friends(&self.friends);
        self.sync_friends_with_worker();
    }
}

fn should_mark_conversation_unseen(
    conversation_is_open: bool,
    new_remote_messages: Option<&[ChatMessage]>,
) -> bool {
    !conversation_is_open && new_remote_messages.is_some_and(|messages| !messages.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupMemberKind {
    You,
    Friend,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupMemberLabel {
    node_id: Option<NodeId>,
    kind: GroupMemberKind,
    text: String,
}

/// Resolve a group's stored member IDs into display labels.
///
/// Order is You, then named friends (A–Z), then unknown IDs (A–Z). Friends use
/// their contact name; everyone else is `Peer {short id}` or a truncated raw
/// string if the ID cannot be parsed.
fn group_member_labels(
    members: &[String],
    our_node_id: Option<NodeId>,
    friend_name: impl Fn(NodeId) -> Option<String>,
) -> Vec<GroupMemberLabel> {
    let mut you = None;
    let mut friends = Vec::new();
    let mut unknown = Vec::new();

    for member in members {
        let raw = member.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(node) = NodeId::from_str(raw) else {
            unknown.push(GroupMemberLabel {
                node_id: None,
                kind: GroupMemberKind::Unknown,
                text: ellipsize(raw, 16),
            });
            continue;
        };
        if our_node_id == Some(node) {
            you = Some(GroupMemberLabel {
                node_id: Some(node),
                kind: GroupMemberKind::You,
                text: "You".to_owned(),
            });
        } else if let Some(name) = friend_name(node) {
            let name = name.trim();
            if name.is_empty() {
                unknown.push(GroupMemberLabel {
                    node_id: Some(node),
                    kind: GroupMemberKind::Unknown,
                    text: format!("Peer {}", node.fmt_short()),
                });
            } else {
                friends.push(GroupMemberLabel {
                    node_id: Some(node),
                    kind: GroupMemberKind::Friend,
                    text: name.to_owned(),
                });
            }
        } else {
            unknown.push(GroupMemberLabel {
                node_id: Some(node),
                kind: GroupMemberKind::Unknown,
                text: format!("Peer {}", node.fmt_short()),
            });
        }
    }

    friends.sort_by(|a, b| {
        a.text
            .to_ascii_lowercase()
            .cmp(&b.text.to_ascii_lowercase())
    });
    unknown.sort_by(|a, b| {
        a.text
            .to_ascii_lowercase()
            .cmp(&b.text.to_ascii_lowercase())
    });

    let mut labels = Vec::with_capacity(usize::from(you.is_some()) + friends.len() + unknown.len());
    if let Some(you) = you {
        labels.push(you);
    }
    labels.extend(friends);
    labels.extend(unknown);
    labels
}

fn format_group_member_summary(members: &[GroupMemberLabel]) -> String {
    if members.is_empty() {
        return "No members".to_owned();
    }
    members
        .iter()
        .map(|member| member.text.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn unknown_direct_conversations(
    conversations: &BTreeMap<String, ChatConversation>,
    known_peers: &BTreeSet<NodeId>,
) -> Vec<(String, NodeId)> {
    let mut unknown = conversations
        .values()
        .filter_map(|conversation| {
            let peer = conversation.direct_peer()?;
            (!known_peers.contains(&peer)).then(|| (conversation.id.clone(), peer))
        })
        .collect::<Vec<_>>();
    unknown.sort_by_key(|(_, peer)| peer.fmt_short().to_string());
    unknown
}

fn next_deafen_audio_state(currently_deafened: bool) -> (bool, bool) {
    let enabled = !currently_deafened;
    (enabled, enabled)
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use crate::chat::ConversationKind;

    #[test]
    fn unseen_indicator_requires_a_new_remote_message_in_a_hidden_conversation() {
        let message = ChatMessage {
            version: 1,
            message_id: "message-1".to_owned(),
            author_id: "alice".to_owned(),
            sent_at: 1,
            body: "hello".to_owned(),
            nonce: 0,
            client_version: None,
            attachments: Vec::new(),
            deletion: None,
        };

        assert!(should_mark_conversation_unseen(
            false,
            Some(std::slice::from_ref(&message))
        ));
        assert!(!should_mark_conversation_unseen(true, Some(&[message])));
        assert!(!should_mark_conversation_unseen(false, Some(&[])));
        assert!(!should_mark_conversation_unseen(false, None));
    }

    #[test]
    fn unknown_direct_conversations_stay_visible_until_the_peer_is_a_friend() {
        let peer = iroh::SecretKey::from_bytes(&[17; 32]).public();
        let direct = ChatConversation {
            id: "unknown-direct".to_owned(),
            title: "Direct".to_owned(),
            kind: ConversationKind::Direct {
                peer_id: peer.to_string(),
            },
            members: Vec::new(),
            document_id: "document".to_owned(),
            history_epoch: 0,
        };
        let conversations = BTreeMap::from([(direct.id.clone(), direct)]);

        assert_eq!(
            unknown_direct_conversations(&conversations, &BTreeSet::new()),
            vec![("unknown-direct".to_owned(), peer)]
        );
        assert!(unknown_direct_conversations(&conversations, &BTreeSet::from([peer])).is_empty());
    }

    #[test]
    fn group_members_label_friends_and_unknown_ids() {
        let you = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let alice = iroh::SecretKey::from_bytes(&[2; 32]).public();
        let bob = iroh::SecretKey::from_bytes(&[3; 32]).public();
        let stranger = iroh::SecretKey::from_bytes(&[4; 32]).public();
        let members = vec![
            stranger.to_string(),
            bob.to_string(),
            you.to_string(),
            alice.to_string(),
            "not-a-node-id".to_owned(),
        ];

        let labels = group_member_labels(&members, Some(you), |node| {
            if node == alice {
                Some("Alice".to_owned())
            } else if node == bob {
                Some("Bob".to_owned())
            } else {
                None
            }
        });

        assert_eq!(labels.len(), 5);
        assert_eq!(labels[0].kind, GroupMemberKind::You);
        assert_eq!(labels[0].text, "You");
        assert_eq!(labels[0].node_id, Some(you));
        assert_eq!(labels[1].kind, GroupMemberKind::Friend);
        assert_eq!(labels[1].text, "Alice");
        assert_eq!(labels[1].node_id, Some(alice));
        assert_eq!(labels[2].kind, GroupMemberKind::Friend);
        assert_eq!(labels[2].text, "Bob");
        assert_eq!(labels[2].node_id, Some(bob));
        assert_eq!(labels[3].kind, GroupMemberKind::Unknown);
        assert_eq!(labels[3].text, "not-a-node-id");
        assert_eq!(labels[3].node_id, None);
        assert_eq!(labels[4].kind, GroupMemberKind::Unknown);
        assert_eq!(labels[4].text, format!("Peer {}", stranger.fmt_short()));
        assert_eq!(labels[4].node_id, Some(stranger));
        assert_eq!(
            format_group_member_summary(&labels),
            format!(
                "You, Alice, Bob, not-a-node-id, Peer {}",
                stranger.fmt_short()
            )
        );
    }

    #[test]
    fn group_member_summary_handles_an_empty_roster() {
        assert_eq!(format_group_member_summary(&[]), "No members");
    }

    #[test]
    fn old_settings_default_to_bubble_chat() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.chat_style, ChatStyle::Bubbles);
        assert!(!settings.start_with_system);
        assert!(!settings.show_system_usage);
        assert!(settings.share_system_audio);
        assert_eq!(settings.ui_sound_volume, 1.0);
    }

    #[test]
    fn old_audio_settings_enable_noise_suppression() {
        let settings: Settings = serde_json::from_str(
            r#"{
                "audio": {
                    "selected_input": "<default>",
                    "selected_output": "<default>",
                    "processing_enabled": true,
                    "quality": "High"
                }
            }"#,
        )
        .unwrap();

        assert!(settings.audio.noise_suppression_enabled);
    }

    #[test]
    fn deafen_and_mute_toggle_together() {
        assert_eq!(next_deafen_audio_state(false), (true, true));
        assert_eq!(next_deafen_audio_state(true), (false, false));
    }

    #[test]
    fn friend_call_button_is_disabled_while_a_call_is_visible() {
        assert!(friend_call_enabled(None));
        assert!(friend_call_enabled(Some(&CallState::Aborted)));
        assert!(!friend_call_enabled(Some(&CallState::Incoming)));
        assert!(!friend_call_enabled(Some(&CallState::Calling)));
        assert!(!friend_call_enabled(Some(&CallState::Active)));
    }

    #[test]
    fn unrelated_incoming_call_requires_leaving_the_group_first() {
        let local = GroupCallAnnouncement {
            call_id: "room-a".to_owned(),
            conversation_id: "group-a".to_owned(),
            title: "Friends".to_owned(),
            initiator: "alice".to_owned(),
            started_at_ms: 1,
            ended_at_ms: None,
            participants: vec!["alice".to_owned()],
        };
        let same_room = GroupCallAnnouncement {
            participants: vec!["bob".to_owned()],
            ..local.clone()
        };
        let other_room = GroupCallAnnouncement {
            call_id: "room-b".to_owned(),
            conversation_id: "group-b".to_owned(),
            ..same_room.clone()
        };

        assert!(!incoming_call_requires_group_switch(
            Some(&local),
            Some(&[same_room]),
        ));
        assert!(incoming_call_requires_group_switch(
            Some(&local),
            Some(&[other_room]),
        ));
        assert!(incoming_call_requires_group_switch(Some(&local), None));
        assert!(!incoming_call_requires_group_switch(None, None));
    }
}

fn incoming_call_requires_group_switch(
    local: Option<&GroupCallAnnouncement>,
    remote_calls: Option<&[GroupCallAnnouncement]>,
) -> bool {
    let Some(local) = local else {
        return false;
    };
    !remote_calls
        .into_iter()
        .flatten()
        .any(|remote| remote.call_id == local.call_id && remote.ended_at_ms.is_none())
}

fn friend_call_enabled(state: Option<&CallState>) -> bool {
    !matches!(
        state,
        Some(CallState::Incoming | CallState::Calling | CallState::Active)
    )
}

#[cfg(target_os = "macos")]
fn open_screen_recording_settings() {
    if let Err(error) = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn()
    {
        warn!("failed to open Screen Recording settings: {error}");
    }
}
