use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use eframe::NativeOptions;
use egui::{Align, Align2, Color32, CornerRadius, Frame, Layout, RichText, Stroke, Ui, Vec2};
use egui_phosphor::regular as ph;
use iroh::{endpoint::VarInt, protocol::Router, Endpoint, KeyParsingError, NodeId};
use lucide_icons::Icon;
use tokio::task::JoinSet;
use tokio::time;
use tracing::{info, warn};
use wire::{
    audio::{AudioConfig, AudioContext, AudioQuality, VolumeHandle},
    rtc::{MediaTrack, RtcConnection, RtcProtocol, TrackKind},
    video::{transport, BitratePreset, StreamPreset, VideoConfig},
};

use crate::{
    dev_pair::DevPairState,
    resource_monitor::ResourceMonitor,
    sounds::{Sound, Sounds},
    theme::*,
    title_bar,
    update::{self, ReleaseInfo},
    video_decode::{DecodedFrame, DecodedFrameData, VideoDecodeWorker},
    window_frame,
};

const DEFAULT: &str = "<default>";
const VIDEO_STREAM_RESET_CODE: VarInt = VarInt::from_u32(0x51);

pub struct App {
    is_first_update: bool,
    always_on_top: bool,
    viewport_transparent: Option<bool>,
    state: AppState,
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
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn load_friends() -> Vec<Friend> {
    if let Some(path) = friends_path() {
        if let Some(friends) = load_friends_from(&path) {
            if !friends.is_empty() {
                return friends;
            }
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
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = serde_json::to_string_pretty(friends) {
            let _ = std::fs::write(&path, contents);
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    wire::net::config_dir().map(|dir| dir.join("settings.json"))
}

fn load_settings() -> Option<Settings> {
    let path = settings_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn save_settings(settings: &Settings) {
    if let Some(path) = settings_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = serde_json::to_string_pretty(settings) {
            let _ = std::fs::write(path, contents);
        }
    }
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
    stream_view_mode: StreamViewMode,
    remote_node_id: Option<Result<NodeId, KeyParsingError>>,
    remote_node_input: String,
    worker: WorkerHandle,
    our_node_id: Option<NodeId>,
    devices: wire::audio::Devices,
    audio_config: UiAudioConfig,
    video_config: VideoConfig,
    calls: BTreeMap<NodeId, CallState>,
    volumes: BTreeMap<NodeId, VolumeHandle>,
    rtts: BTreeMap<NodeId, Duration>,
    video_frames: BTreeMap<NodeId, VideoFrameState>,
    focused_stream: Option<StreamSource>,
    sharing_active: bool,
    preview: Option<PreviewState>,
    friends: Vec<Friend>,
    new_friend_name: String,
    new_friend_id: String,
    theme: Theme,
    window_frame_style: WindowFrameStyle,
    muted: bool,
    deafened: bool,
    sounds: Option<Sounds>,
    voluntary_hangups: AtomicU32,
    update_tx: mpsc::Sender<UpdateMessage>,
    update_rx: mpsc::Receiver<UpdateMessage>,
    update_status: UpdateStatus,
    show_update_prompt: bool,
    reset_home_scroll: bool,
    resource_monitor: ResourceMonitor,
    dev_pair: Option<DevPairState>,
    dev_auto_share: bool,
}

enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Available(ReleaseInfo),
    Downloading(ReleaseInfo),
    Error(String),
}

enum UpdateMessage {
    CheckFinished(anyhow::Result<Option<ReleaseInfo>>),
    DownloadFinished(anyhow::Result<PathBuf>),
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
    generation: u64,
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
    quality: AudioQuality,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    audio: UiAudioConfig,
    video: VideoConfig,
    theme: Theme,
    #[serde(default)]
    window_frame_style: WindowFrameStyle,
    configured: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio: UiAudioConfig::default(),
            video: VideoConfig::default(),
            theme: Theme::default(),
            window_frame_style: WindowFrameStyle::default(),
            configured: false,
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
            quality: AudioQuality::default(),
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.viewport_transparent == Some(true) {
            egui::Rgba::TRANSPARENT.to_array()
        } else {
            let pal = Palette::for_theme(self.state.theme);
            egui::Rgba::from(pal.bg).to_array()
        }
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.is_first_update {
            self.is_first_update = false;
            let repaint_ctx = ctx.clone();
            let callback = Arc::new(move || repaint_ctx.request_repaint());
            self.state.cmd(Command::SetUpdateCallback { callback });
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
        self.state.update(
            ctx,
            &mut self.always_on_top,
            &mut self.viewport_transparent,
            #[cfg(windows)]
            parent_hwnd,
        );
    }
}

impl App {
    pub fn initial_window_frame_style() -> WindowFrameStyle {
        load_settings()
            .map(|settings| settings.window_frame_style)
            .unwrap_or_default()
    }

    pub fn run(options: NativeOptions) -> Result<(), eframe::Error> {
        let handle = Worker::spawn();
        let devices =
            wire::audio::AudioContext::list_devices_sync().expect("failed to list audio devices");
        let saved_settings = load_settings();
        let has_saved_settings = saved_settings
            .as_ref()
            .map(|settings| settings.configured)
            .unwrap_or(false);
        let settings = saved_settings.unwrap_or_default();
        let (update_tx, update_rx) = mpsc::channel();
        let sounds = Sounds::try_new();
        if let Some(sounds) = &sounds {
            sounds.play(Sound::Whoosh2);
        }
        let state = AppState {
            configured: has_saved_settings,
            show_settings: !has_saved_settings,
            stream_view_mode: StreamViewMode::Normal,
            remote_node_id: Default::default(),
            remote_node_input: String::new(),
            worker: handle,
            our_node_id: None,
            devices,
            audio_config: settings.audio,
            video_config: settings.video,
            calls: Default::default(),
            volumes: Default::default(),
            rtts: Default::default(),
            video_frames: Default::default(),
            focused_stream: None,
            sharing_active: false,
            preview: None,
            friends: load_friends(),
            new_friend_name: String::new(),
            new_friend_id: String::new(),
            theme: settings.theme,
            window_frame_style: settings.window_frame_style,
            muted: false,
            deafened: false,
            sounds,
            voluntary_hangups: AtomicU32::new(0),
            update_tx,
            update_rx,
            update_status: UpdateStatus::Idle,
            show_update_prompt: false,
            reset_home_scroll: false,
            resource_monitor: ResourceMonitor::start(),
            dev_pair: DevPairState::from_env(),
            dev_auto_share: std::env::var_os("WIRE_DEV_AUTO_SHARE").is_some(),
        };

        if has_saved_settings {
            state.cmd(Command::SetAudioConfig {
                audio_config: state.audio_config(),
            });
            state.cmd(Command::SetVideoConfig {
                video_config: state.video_config,
            });
        }

        let rounded = window_frame::style_wants_rounded(state.window_frame_style);
        let app = App {
            state,
            is_first_update: true,
            always_on_top: false,
            viewport_transparent: Some(rounded),
        };
        eframe::run_native(
            "wire",
            options,
            Box::new(|cc| {
                setup_fonts(&cc.egui_ctx);
                Ok(Box::new(app))
            }),
        )
    }
}
impl AppState {
    fn update(
        &mut self,
        ctx: &egui::Context,
        always_on_top: &mut bool,
        viewport_transparent: &mut Option<bool>,
        #[cfg(windows)] parent_hwnd: Option<windows::Win32::Foundation::HWND>,
    ) {
        // Keep the process resource readout current even while the rest of the UI is idle.
        ctx.request_repaint_after(Duration::from_secs(1));
        self.process_update_events(ctx);
        if ctx.input(|i| i.key_pressed(egui::Key::T)) {
            self.theme = self.theme.next();
            self.persist_theme();
        }
        let pal = Palette::for_theme(self.theme);
        ctx.set_visuals(visuals_for(&pal));

        self.process_events();
        #[cfg(windows)]
        for frame in self.video_frames.values_mut() {
            if let Some(presenter) = &mut frame.presenter {
                presenter.mark_unused();
            }
        }
        self.handle_view_mode_input(ctx);

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
        }

        if self.show_settings || !self.configured {
            self.ui_settings_window(ctx);
        }
        if self.show_update_prompt {
            self.ui_update_prompt(ctx);
        }
        #[cfg(windows)]
        {
            let force_hide = self.show_settings || !self.configured || self.show_update_prompt;
            for frame in self.video_frames.values_mut() {
                if let Some(presenter) = &mut frame.presenter {
                    presenter.hide_if_unused(force_hide);
                }
            }
        }
    }

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
                        Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
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

    fn process_events(&mut self) {
        while let Ok(event) = self.worker.event_rx.try_recv() {
            match event {
                Event::EndpointBound(node_id) => {
                    self.our_node_id = Some(node_id);
                    let dev_peers = match self.dev_pair.as_mut() {
                        Some(dev_pair) => match dev_pair.register(node_id) {
                            Ok(peers) => peers,
                            Err(error) => {
                                warn!("dev-call rendezvous failed: {error:#}");
                                Vec::new()
                            }
                        },
                        None => Vec::new(),
                    };
                    for peer in dev_peers {
                        info!("dev call initiating automatic call to {}", peer.fmt_short());
                        self.cmd(Command::Call { node_id: peer });
                    }
                }
                Event::SetCallState(node_id, call_state) => {
                    let auto_accept =
                        self.dev_pair.is_some() && matches!(call_state, CallState::Incoming);
                    let auto_share = self.dev_auto_share
                        && !self.sharing_active
                        && matches!(call_state, CallState::Active);
                    let previous = self.calls.get(&node_id).copied();
                    match call_state {
                        CallState::Active => {
                            self.play_sound(Sound::Success);
                        }
                        CallState::Aborted => {
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
                        self.rtts.remove(&node_id);
                        self.video_frames.remove(&node_id);
                        if self.focused_stream == Some(StreamSource::Remote(node_id)) {
                            self.focused_stream = None;
                        }
                    } else {
                        self.calls.insert(node_id, call_state);
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
                        self.cmd(Command::ToggleSharing { enabled: true });
                    }
                }
                Event::VolumeHandle(node_id, volume) => {
                    self.volumes.insert(node_id, volume);
                }
                Event::SetRtt(node_id, rtt) => {
                    self.rtts.insert(node_id, rtt);
                }
                Event::VideoFrame { node_id, frame } => {
                    if !matches!(self.calls.get(&node_id), Some(CallState::Active)) {
                        continue;
                    }
                    if !self.video_frames.contains_key(&node_id) {
                        self.reset_home_scroll = true;
                    }
                    let state =
                        self.video_frames
                            .entry(node_id)
                            .or_insert_with(|| VideoFrameState {
                                width: 0,
                                height: 0,
                                generation: 0,
                                data: DecodedFrameData::Rgba(Arc::new(Vec::new())),
                                texture: None,
                                uploaded_generation: 0,
                                upload_stats: TextureUploadStats::default(),
                                #[cfg(windows)]
                                presenter: None,
                                #[cfg(windows)]
                                native_present_failed: false,
                            });
                    state.width = frame.width;
                    state.height = frame.height;
                    state.data = frame.data;
                    #[cfg(windows)]
                    if matches!(&state.data, DecodedFrameData::D3d11(_)) {
                        state.texture = None;
                    }
                    state.generation += 1;
                }
                Event::VideoStreamEnded(node_id) => {
                    self.video_frames.remove(&node_id);
                    if self.focused_stream == Some(StreamSource::Remote(node_id)) {
                        self.focused_stream = None;
                    }
                }
                Event::SharingToggled(active) => {
                    self.sharing_active = active;
                    self.reset_home_scroll = true;
                    if !active {
                        self.preview = None;
                        if self.focused_stream == Some(StreamSource::Local) {
                            self.focused_stream = None;
                        }
                    }
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
            }
        }
    }

    fn handle_view_mode_input(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.focused_stream.is_some() {
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

    fn stream_label(&self, source: StreamSource) -> String {
        match source {
            StreamSource::Local => "You".to_string(),
            StreamSource::Remote(node_id) => self.peer_display_name(node_id),
        }
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
            video: self.video_config,
            theme: self.theme,
            window_frame_style: self.window_frame_style,
            configured: true,
        });
    }

    fn persist_theme(&self) {
        let mut settings = load_settings().unwrap_or_default();
        settings.theme = self.theme;
        save_settings(&settings);
    }

    fn cmd(&self, command: Command) {
        self.worker
            .command_tx
            .send_blocking(command)
            .expect("worker thread is dead");
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

    fn hang_up_call(&self, node_id: NodeId) {
        self.play_sound(Sound::Whoosh1);
        self.voluntary_hangups.fetch_add(1, Ordering::Relaxed);
        self.cmd(Command::Abort { node_id });
    }

    fn ui_with_chrome(
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
                        .map(|dev_pair| format!("Wire · DEV {}", dev_pair.session()))
                        .unwrap_or_else(|| "Wire".to_owned());
                    title_bar::ui(
                        ui,
                        title_bar_rect,
                        pal,
                        &title,
                        self.resource_monitor.snapshot(),
                        always_on_top,
                        rounded,
                    );

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
                    window_frame::resize_edges(ui, ctx.screen_rect());
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
        const TOP_BAR_HEIGHT: f32 = 54.0;
        const DOCK_HEIGHT: f32 = 86.0;
        let immersive = self.stream_view_mode != StreamViewMode::Normal;

        let top_rect = egui::Rect::from_min_max(
            body.min,
            egui::pos2(body.max.x, body.min.y + TOP_BAR_HEIGHT),
        );
        let dock_rect =
            egui::Rect::from_min_max(egui::pos2(body.min.x, body.max.y - DOCK_HEIGHT), body.max);
        let stage_rect = egui::Rect::from_min_max(
            egui::pos2(body.min.x, top_rect.max.y),
            egui::pos2(body.max.x, dock_rect.min.y),
        );

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(top_rect), |ui| {
            Frame::new()
                .inner_margin(egui::Margin::symmetric(20, 0))
                .show(ui, |ui| self.ui_top_bar_content(ui, ctx, pal));
        });

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(dock_rect), |ui| {
            Frame::new()
                .inner_margin(egui::Margin::symmetric(20, 8))
                .show(ui, |ui| self.ui_dock_content(ui, pal));
        });

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(stage_rect), |ui| {
            let frame = if immersive {
                Frame::NONE
            } else {
                Frame::new().inner_margin(egui::Margin::symmetric(20, 14))
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
    }

    fn ui_top_bar_content(&mut self, ui: &mut Ui, ctx: &egui::Context, pal: &Palette) {
        let show_context_status = ui.max_rect().width() >= 760.0;
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if self.sharing_active {
                dot(ui, pal.accent, 6.0);
                ui.label(
                    RichText::new("Sharing screen")
                        .color(pal.text2)
                        .size(ui_font_size(12.0)),
                );
            } else if self.our_node_id.is_some() {
                dot(ui, pal.ok, 6.0);
                ui.label(
                    RichText::new("Ready")
                        .color(pal.text2)
                        .size(ui_font_size(12.0)),
                );
            } else {
                ui.label(RichText::new("Connecting…").weak());
            }

            let available_update = match &self.update_status {
                UpdateStatus::Available(release) => Some(release.clone()),
                _ => None,
            };
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_icon_button(ui, pal, ph::GEAR_SIX)
                    .on_hover_text("Audio and screen sharing options")
                    .clicked()
                {
                    self.show_settings = true;
                }
                if let Some(release) = available_update {
                    if action_button(
                        ui,
                        &pal,
                        &format!("Update v{}", release.version),
                        ButtonTone::Primary,
                    )
                    .on_hover_text("Download the update to Desktop and relaunch")
                    .clicked()
                    {
                        self.start_update_download(ctx, release);
                    }
                }
                let active_calls = self
                    .calls
                    .values()
                    .filter(|s| matches!(s, CallState::Active))
                    .count();
                if show_context_status && active_calls > 0 {
                    ui.label(
                        RichText::new(if active_calls == 1 {
                            "1 active call".to_owned()
                        } else {
                            format!("{active_calls} active calls")
                        })
                        .color(pal.ok)
                        .size(ui_font_size(12.0)),
                    );
                }
            });
        });
    }

    fn ui_dock_content(&mut self, ui: &mut Ui, pal: &Palette) {
        let rect = ui.max_rect();
        ui.painter()
            .hline(rect.x_range(), rect.top() - 8.0, Stroke::new(1.0, pal.line));
        let active_calls = self
            .calls
            .values()
            .filter(|state| matches!(state, CallState::Active))
            .count();

        let controls_width = if active_calls > 0 { 320.0 } else { 210.0 };
        let show_status = rect.width() >= controls_width + 180.0;
        let controls_left = if show_status {
            rect.right() - controls_width
        } else {
            rect.center().x - controls_width / 2.0
        };
        let controls_rect = egui::Rect::from_min_size(
            egui::pos2(controls_left.max(rect.left()), rect.top()),
            Vec2::new(controls_width, rect.height()),
        );
        let status_rect = egui::Rect::from_min_max(
            rect.left_top(),
            egui::pos2(
                (controls_rect.left() - 12.0).max(rect.left()),
                rect.bottom(),
            ),
        );

        if show_status {
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(status_rect), |ui| {
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(if active_calls == 1 {
                            "1 peer in session".to_owned()
                        } else {
                            format!("{active_calls} peers in session")
                        })
                        .color(pal.text2)
                        .size(ui_font_size(13.0)),
                    );
                    ui.horizontal(|ui| {
                        dot(ui, if active_calls > 0 { pal.ok } else { pal.dim2 }, 5.0);
                        ui.label(
                            RichText::new(if active_calls > 0 {
                                "direct connection"
                            } else {
                                "ready to connect"
                            })
                            .color(pal.dim)
                            .size(ui_font_size(12.0)),
                        );
                    });
                });
            });
        }

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if active_calls > 0 {
                    ui.add_space(4.0);
                }
                if dock_control(
                    ui,
                    pal,
                    if self.muted {
                        ph::MICROPHONE_SLASH
                    } else {
                        ph::MICROPHONE
                    },
                    if self.muted { "Muted" } else { "Mute" },
                    self.muted,
                )
                .on_hover_text("Mute or unmute your microphone")
                .clicked()
                {
                    self.muted = !self.muted;
                    self.play_control_sound(self.muted);
                    self.cmd(Command::SetMuted { muted: self.muted });
                }
                ui.add_space(2.0);
                if dock_control(
                    ui,
                    pal,
                    ph::HEADPHONES,
                    if self.deafened { "Deafened" } else { "Deafen" },
                    self.deafened,
                )
                .on_hover_text("Silence or restore all incoming call audio")
                .clicked()
                {
                    self.deafened = !self.deafened;
                    self.play_control_sound(self.deafened);
                    self.cmd(Command::SetDeafened {
                        deafened: self.deafened,
                    });
                }
                ui.add_space(2.0);
                if dock_control(
                    ui,
                    pal,
                    ph::MONITOR,
                    if self.sharing_active { "Stop" } else { "Share" },
                    self.sharing_active,
                )
                .on_hover_text(if self.sharing_active {
                    "Stop sharing"
                } else {
                    "Share your screen"
                })
                .clicked()
                {
                    let enabled = !self.sharing_active;
                    self.play_control_sound(enabled);
                    self.cmd(Command::ToggleSharing { enabled });
                }
                if active_calls > 0 {
                    ui.add_space(8.0);
                    v_sep(ui, pal.line);
                    ui.add_space(8.0);
                    if leave_button(ui, pal).clicked() {
                        let peers: Vec<_> = self.calls.keys().copied().collect();
                        if !peers.is_empty() {
                            self.play_sound(Sound::Whoosh1);
                        }
                        for node_id in peers {
                            self.voluntary_hangups.fetch_add(1, Ordering::Relaxed);
                            self.cmd(Command::Abort { node_id });
                        }
                    }
                }
            });
        });
    }

    fn ui_stage(
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

        let has_live_visual = !self.active_stream_sources().is_empty() || self.sharing_active;
        let available_height = ui.available_height();
        let stage_height = if has_live_visual {
            (available_height * 0.62)
                .clamp(220.0, 720.0)
                .min(available_height.max(120.0))
        } else {
            (available_height * 0.32)
                .clamp(150.0, 220.0)
                .min(available_height.max(110.0))
        };
        let stage_width = ui.available_width();
        ui.allocate_ui_with_layout(
            Vec2::new(stage_width, stage_height),
            Layout::top_down(Align::Min),
            |ui| {
                self.ui_stream_panel(
                    ui,
                    ctx,
                    #[cfg(windows)]
                    parent_hwnd,
                )
            },
        );
        ui.add_space(8.0);

        self.ui_participant_strip(ui, pal);
        ui.add_space(8.0);

        let mut scroll_area = egui::ScrollArea::vertical()
            .id_salt("home-cards-scroll")
            .auto_shrink([false, false]);
        if self.reset_home_scroll {
            scroll_area = scroll_area.vertical_scroll_offset(0.0);
            self.reset_home_scroll = false;
        }
        scroll_area.show(ui, |ui| {
            if ui.available_width() >= 760.0 {
                ui.columns(2, |columns| {
                    self.ui_identity_card(&mut columns[0]);
                    columns[0].add_space(10.0);
                    self.ui_dial_card(&mut columns[0]);
                    self.ui_friends_card(&mut columns[1]);
                });
            } else {
                self.ui_identity_card(ui);
                ui.add_space(10.0);
                self.ui_dial_card(ui);
                ui.add_space(10.0);
                self.ui_friends_card(ui);
            }
        });
    }

    fn ui_participant_strip(&mut self, ui: &mut Ui, pal: &Palette) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("PEERS")
                    .family(kh_family())
                    .color(pal.dim)
                    .size(12.0),
            );
            ui.label(
                RichText::new("live call status")
                    .color(pal.dim2)
                    .size(ui_font_size(12.0)),
            );
        });
        ui.add_space(2.0);
        if self.calls.is_empty() {
            self.ui_empty_peer_tile(ui, pal);
        } else {
            let calls: Vec<_> = self
                .calls
                .iter()
                .map(|(node_id, state)| (*node_id, *state))
                .collect();
            let column_count = participant_grid_columns(ui.available_width(), calls.len());
            let tile_width =
                participant_tile_width(ui.available_width(), column_count, PARTICIPANT_GRID_GAP);

            for (row_index, row) in calls.chunks(column_count).enumerate() {
                if row_index > 0 {
                    ui.add_space(PARTICIPANT_GRID_GAP);
                }
                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.x = PARTICIPANT_GRID_GAP;
                    for &(node_id, state) in row {
                        ui.allocate_ui_with_layout(
                            Vec2::new(tile_width, 0.0),
                            Layout::top_down(Align::Min),
                            |ui| self.ui_peer_tile(ui, pal, node_id, state, tile_width),
                        );
                    }
                });
            }
        }
    }

    fn ui_empty_peer_tile(&self, ui: &mut Ui, pal: &Palette) {
        Frame::new()
            .fill(pal.panel)
            .stroke(Stroke::new(1.0, pal.line))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (icon_rect, _) =
                        ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(icon_rect.center(), 17.0, pal.panel2);
                    ui.painter().circle_stroke(
                        icon_rect.center(),
                        17.0,
                        Stroke::new(1.0, pal.line_br),
                    );
                    ui.painter().text(
                        icon_rect.center(),
                        Align2::CENTER_CENTER,
                        ph::USER_PLUS,
                        sans(15.0),
                        pal.dim,
                    );
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("No one else is here")
                                .color(pal.text2)
                                .size(ui_font_size(13.0)),
                        );
                        ui.label(
                            RichText::new(
                                "Start a call from a saved contact or enter a node ID below.",
                            )
                            .color(pal.dim)
                            .size(ui_font_size(11.5)),
                        );
                    });
                });
            });
    }

    fn ui_peer_tile(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        node_id: NodeId,
        state: CallState,
        tile_width: f32,
    ) {
        Frame::new()
            .fill(pal.panel)
            .stroke(Stroke::new(
                1.0,
                if matches!(state, CallState::Active) {
                    pal.line_br
                } else {
                    pal.line
                },
            ))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width((tile_width - 28.0).max(1.0));
                let peer_name = self.peer_display_name(node_id);
                let peer_initial = self.peer_initial(node_id);
                ui.horizontal(|ui| {
                    circle_avatar(ui, pal, &peer_initial, 28.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(peer_name)
                                .color(pal.text)
                                .size(ui_font_size(13.0)),
                        )
                        .on_hover_text(format!("Node {}…", node_id.fmt_short()));
                        let (label, color) = match state {
                            CallState::Incoming => ("incoming", pal.accent),
                            CallState::Calling => ("connecting", pal.accent),
                            CallState::Active => ("connected", pal.ok),
                            CallState::Aborted => ("ended", pal.err),
                        };
                        ui.label(RichText::new(label).color(color).size(ui_font_size(11.5)));
                    });
                });
                ui.add_space(7.0);
                ui.horizontal(|ui| match state {
                    CallState::Incoming => {
                        if action_button(ui, pal, "Accept", ButtonTone::Primary).clicked() {
                            self.cmd(Command::HandleIncoming {
                                node_id,
                                accept: true,
                            });
                        }
                        if action_button(ui, pal, "Decline", ButtonTone::Danger).clicked() {
                            self.cmd(Command::HandleIncoming {
                                node_id,
                                accept: false,
                            });
                        }
                    }
                    CallState::Calling | CallState::Active => {
                        if let Some(rtt) = self.rtts.get(&node_id) {
                            ui.label(
                                rtt_label(*rtt)
                                    .color(pal.dim)
                                    .monospace()
                                    .size(ui_font_size(11.0)),
                            );
                        }
                        if let Some(volume) = self.volumes.get(&node_id) {
                            let mut value = f32::from_bits(volume.load(Ordering::Relaxed));
                            ui.add_sized(
                                [56.0, 18.0],
                                egui::Slider::new(&mut value, 0.0..=2.0)
                                    .show_value(false)
                                    .max_decimals(1),
                            );
                            volume.store(value.to_bits(), Ordering::Relaxed);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if action_button(ui, pal, "End", ButtonTone::Danger).clicked() {
                                self.hang_up_call(node_id);
                            }
                        });
                    }
                    CallState::Aborted => {}
                });
            });
    }

    fn ui_sidebar(&mut self, ui: &mut Ui) {
        self.ui_identity_card(ui);
        ui.add_space(12.0);
        self.ui_dial_card(ui);
        ui.add_space(12.0);
        self.ui_friends_card(ui);
        ui.add_space(12.0);
        self.ui_calls_card(ui);
        ui.add_space(12.0);
        self.ui_sharing_card(ui);
    }

    fn ui_identity_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "Your identity", |ui| {
            if let Some(node_id) = &self.our_node_id {
                ui.horizontal_wrapped(|ui| {
                    ui.label(fmt_node_id(&node_id.fmt_short()));
                    if action_button(ui, &pal, "Copy", ButtonTone::Secondary).clicked() {
                        copy_to_clipboard(&node_id.to_string());
                    }
                });
                ui.label(
                    RichText::new("This is your stable ID (saved locally). Share it so friends can add and call you.")
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
        section_card(ui, &pal, "Place a call", |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.remote_node_input)
                        .hint_text("Paste remote node ID")
                        .desired_width(ui.available_width() - 64.0),
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

    fn ui_friends_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "Friends", |ui| {
            let mut call: Option<NodeId> = None;
            let mut remove_idx: Option<usize> = None;

            if self.friends.is_empty() {
                ui.label(RichText::new("No friends yet. Add one below.").weak());
            }
            for (idx, friend) in self.friends.iter().enumerate() {
                let parsed = NodeId::from_str(&friend.node_id);
                let display_name = if friend.name.trim().is_empty()
                    || friend.name.trim() == friend.node_id.trim()
                {
                    "Unnamed contact"
                } else {
                    friend.name.trim()
                };
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(display_name)
                                .color(pal.text)
                                .size(ui_font_size(13.0)),
                        );
                        if parsed.is_err() {
                            ui.label(RichText::new("invalid id").small().weak());
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if action_button(ui, &pal, "Remove", ButtonTone::Danger).clicked() {
                            remove_idx = Some(idx);
                        }
                        if action_button(ui, &pal, "Call", ButtonTone::Primary).clicked() {
                            if let Ok(id) = parsed {
                                call = Some(id);
                            }
                        }
                    });
                });
                ui.add_space(6.0);
            }

            if let Some(id) = call {
                self.play_sound(Sound::Button2);
                self.cmd(Command::Call { node_id: id });
            }
            if let Some(idx) = remove_idx {
                self.friends.remove(idx);
                save_friends(&self.friends);
            }

            ui.separator();
            ui.label(RichText::new("Add a friend").strong());
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
            if action_button_full(ui, &pal, "Add friend", ButtonTone::Primary).clicked() {
                self.add_friend();
            }
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
        self.play_sound(Sound::Button2);
        if !self.friends.iter().any(|f| f.node_id == node_id) {
            self.friends.push(Friend {
                name,
                node_id: node_id.clone(),
            });
            save_friends(&self.friends);
        }
        self.new_friend_name.clear();
        self.new_friend_id.clear();
    }

    fn ui_calls_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "Calls", |ui| {
            if self.calls.is_empty() {
                ui.label(RichText::new("No active calls").weak());
                return;
            }

            let calls: Vec<_> = self.calls.iter().collect();
            for (node_id, state) in calls {
                let node_id = *node_id;
                let peer_name = self.peer_display_name(node_id);
                Frame::new()
                    .fill(ui.visuals().widgets.noninteractive.bg_fill)
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(10.0)
                    .stroke(Stroke::new(
                        1.0,
                        ui.visuals().widgets.noninteractive.bg_stroke.color,
                    ))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(peer_name)
                                    .color(pal.text)
                                    .size(ui_font_size(13.0)),
                            )
                            .on_hover_text(format!("Node {}…", node_id.fmt_short()));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                call_state_badge(ui, state);
                            });
                        });

                        ui.add_space(4.0);
                        ui.horizontal(|ui| match state {
                            CallState::Incoming => {
                                if action_button(ui, &pal, "Accept", ButtonTone::Primary).clicked()
                                {
                                    self.cmd(Command::HandleIncoming {
                                        node_id,
                                        accept: true,
                                    });
                                }
                                if action_button(ui, &pal, "Decline", ButtonTone::Danger).clicked()
                                {
                                    self.cmd(Command::HandleIncoming {
                                        node_id,
                                        accept: false,
                                    });
                                }
                            }
                            CallState::Calling | CallState::Active => {
                                if action_button(ui, &pal, "End", ButtonTone::Danger).clicked() {
                                    self.hang_up_call(node_id);
                                }
                            }
                            CallState::Aborted => {}
                        });

                        if matches!(state, CallState::Active) {
                            if let Some(volume) = self.volumes.get(&node_id) {
                                let mut vol = f32::from_bits(volume.load(Ordering::Relaxed));
                                ui.horizontal(|ui| {
                                    ui.label("Volume");
                                    ui.add(
                                        egui::Slider::new(&mut vol, 0.0..=2.0)
                                            .show_value(false)
                                            .fixed_decimals(1),
                                    );
                                });
                                volume.store(vol.to_bits(), Ordering::Relaxed);
                            }
                            if let Some(rtt) = self.rtts.get(&node_id) {
                                ui.label(rtt_label(*rtt));
                            }
                        }
                    });
                ui.add_space(8.0);
            }
        });
    }

    fn ui_sharing_card(&mut self, ui: &mut Ui) {
        let pal = Palette::for_theme(self.theme);
        section_card(ui, &pal, "Screen sharing", |ui| {
            ui.horizontal(|ui| {
                if self.sharing_active {
                    if action_button(ui, &pal, "Stop sharing", ButtonTone::Danger).clicked() {
                        self.play_control_sound(false);
                        self.cmd(Command::ToggleSharing { enabled: false });
                    }
                    ui.label(RichText::new("Live").color(Color32::from_rgb(100, 200, 120)));
                } else if action_button(ui, &pal, "Start sharing", ButtonTone::Primary).clicked() {
                    self.play_control_sound(true);
                    self.cmd(Command::ToggleSharing { enabled: true });
                }
            });

            if let Some(preview) = &mut self.preview {
                ui.add_space(8.0);
                ui.label(RichText::new("Outgoing preview").small().weak());
                ui.horizontal(|ui| {
                    ui.label(format!("{:.0}×{:.0}", preview.width, preview.height));
                    ui.separator();
                    ui.label(format!("{:.0} fps", preview.actual_fps));
                    ui.separator();
                    ui.label(format!("{:.0} ms encode", preview.encode_time_ms));
                });
                sync_rgba_texture(
                    ui,
                    "preview",
                    preview.width,
                    preview.height,
                    &preview.data,
                    preview.generation,
                    &mut preview.uploaded_generation,
                    &mut preview.texture,
                    &mut preview.upload_stats,
                );
                if let Some(tex) = &preview.texture {
                    let max_w = ui.available_width();
                    let aspect = preview.width as f32 / preview.height as f32;
                    ui.add(
                        egui::Image::new(tex)
                            .max_width(max_w)
                            .max_height(max_w / aspect),
                    );
                }
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
                self.ui_stream_toolbar(ui, ctx, streams.len(), has_stream, true);
            });
            ui.add_space(4.0);
        } else {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("STAGE")
                        .family(kh_family())
                        .color(pal.text)
                        .size(16.0),
                );
                let stage_detail = if has_stream {
                    Some(if self.focused_stream.is_some() {
                        "focused stream".to_string()
                    } else if streams.len() == 1 {
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
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.ui_stream_toolbar(ui, ctx, streams.len(), has_stream, false);
                });
            });
            ui.add_space(4.0);
        }

        let available = ui.available_size();
        let (area, _) = ui.allocate_exact_size(available, egui::Sense::hover());

        if !has_stream {
            self.ui_empty_stream_state(ui, &pal, area, immersive);
            return;
        }

        if let Some(focused) = self
            .focused_stream
            .filter(|source| streams.contains(source))
        {
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(area), |ui| {
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
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(cell_rect), |ui| {
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
        ui.painter().rect_filled(area, corner_radius, pal.panel);
        if !immersive {
            ui.painter().rect_stroke(
                area,
                corner_radius,
                Stroke::new(1.0, pal.line),
                egui::StrokeKind::Inside,
            );
        }

        let roomy = area.height() >= 150.0;
        let block_height = if roomy { 142.0 } else { 70.0 };
        let block_size = Vec2::new((area.width() - 24.0).min(440.0), block_height);
        let block_rect = egui::Rect::from_center_size(area.center(), block_size);

        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(block_rect), |ui| {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                let (icon_rect, _) =
                    ui.allocate_exact_size(Vec2::splat(40.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(icon_rect.center(), 20.0, pal.panel2);
                ui.painter()
                    .circle_stroke(icon_rect.center(), 20.0, Stroke::new(1.0, pal.line_br));
                ui.painter().text(
                    icon_rect.center(),
                    Align2::CENTER_CENTER,
                    if self.sharing_active {
                        ph::SPINNER_GAP
                    } else {
                        ph::MONITOR
                    },
                    sans(17.0),
                    if self.sharing_active {
                        pal.accent
                    } else {
                        pal.dim
                    },
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(if self.sharing_active {
                        "Starting your screen share"
                    } else {
                        "Nothing is being shared"
                    })
                    .color(pal.text2)
                    .size(ui_font_size(14.0)),
                );

                if roomy {
                    ui.label(
                        RichText::new(if self.sharing_active {
                            "Preparing the first frame. This usually takes a moment."
                        } else {
                            "Shared screens and incoming video will appear here."
                        })
                        .color(pal.dim)
                        .size(ui_font_size(11.5)),
                    );
                    ui.add_space(6.0);
                    let (label, tone) = if self.sharing_active {
                        ("Stop sharing", ButtonTone::Secondary)
                    } else {
                        ("Share your screen", ButtonTone::Primary)
                    };
                    if action_button(ui, pal, label, tone).clicked() {
                        let enabled = !self.sharing_active;
                        self.play_control_sound(enabled);
                        self.cmd(Command::ToggleSharing { enabled });
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
            CornerRadius::same(10)
        };
        ui.painter()
            .rect_filled(tile_rect, corner_radius, Color32::from_rgb(12, 12, 14));
        if !immersive {
            ui.painter().rect_stroke(
                tile_rect,
                corner_radius,
                Stroke::new(1.0, pal.line),
                egui::StrokeKind::Inside,
            );
        }

        // Native child windows necessarily sit above the parent's OpenGL
        // surface. Keep controls in a small egui-owned header and limit the
        // child to the image rectangle so it never obscures interaction.
        let header_height = if tile_rect.height() >= 80.0 {
            34.0
        } else {
            0.0
        };
        let header_rect = egui::Rect::from_min_max(
            tile_rect.min,
            egui::pos2(tile_rect.max.x, tile_rect.min.y + header_height),
        );
        let label = ellipsize(&self.stream_label(source), 30);
        if header_height > 0.0 {
            ui.painter().text(
                header_rect.left_center() + egui::vec2(10.0, 0.0),
                Align2::LEFT_CENTER,
                &label,
                sans(11.0),
                pal.text,
            );
            let button_rect = egui::Rect::from_min_size(
                header_rect.right_top() + egui::vec2(-34.0, 2.0),
                Vec2::splat(30.0),
            );
            let icon = if expanded {
                ph::SQUARES_FOUR
            } else {
                ph::ARROWS_OUT_SIMPLE
            };
            let tooltip = if expanded {
                "Show all streams"
            } else {
                "Focus this stream"
            };
            if ui
                .put(
                    button_rect,
                    egui::Button::new(RichText::new(icon).size(ui_font_size(15.0)))
                        .fill(Color32::from_rgb(20, 20, 23))
                        .stroke(Stroke::new(1.0, pal.line_br)),
                )
                .on_hover_text(tooltip)
                .clicked()
            {
                self.focused_stream = if expanded { None } else { Some(source) };
            }
        }

        let content_rect = egui::Rect::from_min_max(
            egui::pos2(tile_rect.min.x + 4.0, tile_rect.min.y + header_height + 2.0),
            tile_rect.max - egui::vec2(4.0, 4.0),
        );
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
        if width == 0 || height == 0 {
            return;
        }
        let aspect = width as f32 / height as f32;
        let image_rect = egui::Rect::from_center_size(
            content_rect.center(),
            video_display_size(content_rect.size(), aspect, expanded),
        );

        let mut texture_id = None;
        let mut native_presented = false;
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
                #[cfg(windows)]
                let allow_native =
                    self.configured && !self.show_settings && !self.show_update_prompt;
                let Some(frame) = self.video_frames.get_mut(&node_id) else {
                    return;
                };
                #[cfg(windows)]
                if allow_native && matches!(&frame.data, DecodedFrameData::D3d11(_)) {
                    let rect = physical_video_rect(image_rect, ui.ctx().pixels_per_point());
                    match present_native_video(frame, parent_hwnd, rect) {
                        Ok(presented) => native_presented = presented,
                        Err(error) => warn!("native video fallback failed: {error:#}"),
                    }
                }
                if let DecodedFrameData::Rgba(data) = &frame.data {
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
            }
        }

        if let Some(texture_id) = texture_id {
            ui.painter().image(
                texture_id,
                image_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
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
    }

    fn ui_stream_toolbar(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        stream_count: usize,
        has_stream: bool,
        compact: bool,
    ) {
        let pal = Palette::for_theme(self.theme);
        if stream_count > 1 {
            ui.label(
                RichText::new(format!("{stream_count} streams"))
                    .color(Color32::from_rgb(0x91, 0x8e, 0x8a))
                    .size(ui_font_size(12.0)),
            );
        }

        if has_stream {
            let fill_selected = self.stream_view_mode == StreamViewMode::FillWindow;
            if toolbar_button(
                ui,
                &pal,
                Icon::Expand,
                if compact { "Fill" } else { "Fill window" },
                fill_selected,
            )
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
            if toolbar_button(ui, &pal, Icon::Fullscreen, "Fullscreen", fs_selected)
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

        if compact && self.stream_view_mode != StreamViewMode::Normal {
            if toolbar_button(ui, &pal, Icon::Minimize2, "Exit", false)
                .on_hover_text("Return to normal layout (Esc)")
                .clicked()
            {
                self.set_stream_view_mode(ctx, StreamViewMode::Normal);
            }
        }
    }

    fn ui_settings_window(&mut self, ctx: &egui::Context) {
        let can_close = self.configured;
        let pal = Palette::for_theme(self.theme);
        let screen_rect = ctx.screen_rect();
        let dialog_width = (screen_rect.width() - 40.0).clamp(420.0, 500.0);
        let body_height = (screen_rect.height() - 130.0).clamp(360.0, 840.0);
        egui::Window::new("settings-dialog")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(dialog_width)
            .min_width(dialog_width)
            .max_width(dialog_width)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(
                Frame::new()
                    .fill(pal.bg)
                    .stroke(Stroke::new(1.0, pal.line_br))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(0.0),
            )
            .show(ctx, |ui| {
                ui.set_width(dialog_width);
                Frame::new()
                    .fill(pal.panel)
                    .inner_margin(egui::Margin::symmetric(18, 12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("SETTINGS")
                                    .family(kh_family())
                                    .color(pal.text)
                                    .size(16.0),
                            );
                            ui.label(
                                RichText::new("appearance, audio, video and updates")
                                    .color(pal.dim)
                                    .size(ui_font_size(11.5)),
                            );
                        });
                    });
                Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 16))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("settings-scroll")
                            .max_height(body_height)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
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

                                #[cfg(feature = "audio-processing")]
                                {
                                    ui.add_space(8.0);
                                    Frame::new()
                                        .fill(pal.panel2)
                                        .stroke(Stroke::new(1.0, pal.line))
                                        .corner_radius(CornerRadius::same(7))
                                        .inner_margin(egui::Margin::symmetric(10, 5))
                                        .show(ui, |ui| {
                                            ui.checkbox(
                                                &mut self.audio_config.processing_enabled,
                                                RichText::new("Echo cancellation")
                                                    .color(pal.text2)
                                                    .size(ui_font_size(12.0)),
                                            );
                                        });
                                }

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
                                        self.persist_settings();
                                        self.configured = true;
                                        self.show_settings = false;
                                    }
                                    if can_close
                                        && action_button(ui, &pal, "Cancel", ButtonTone::Secondary)
                                            .clicked()
                                    {
                                        self.show_settings = false;
                                    }
                                });
                            });
                    });
            });
    }

    fn ui_update_prompt(&mut self, ctx: &egui::Context) {
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
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "Wire v{} is available. You are running v{}.",
                    release.version,
                    crate::APP_VERSION
                ));
                ui.label("The new executable will be verified and placed on your Desktop.");
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Download and relaunch").clicked() {
                        download = true;
                    }
                    if ui.button("Later").clicked() {
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

fn section_card<R>(
    ui: &mut Ui,
    pal: &Palette,
    title: &str,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    Frame::new()
        .fill(pal.panel)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(12.0)
        .stroke(Stroke::new(1.0, pal.line))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title.to_uppercase())
                    .family(kh_family())
                    .color(pal.dim)
                    .size(11.0),
            );
            ui.add_space(4.0);
            add_contents(ui)
        })
        .inner
}

fn call_state_badge(ui: &mut Ui, state: &CallState) {
    let (text, color) = match state {
        CallState::Incoming => ("Incoming", Color32::from_rgb(255, 200, 80)),
        CallState::Calling => ("Calling…", Color32::from_rgb(120, 170, 255)),
        CallState::Active => ("Active", Color32::from_rgb(100, 200, 120)),
        CallState::Aborted => ("Ended", Color32::GRAY),
    };
    ui.label(RichText::new(text).color(color).small());
}

fn rtt_label(rtt: Duration) -> RichText {
    let text = fmt_rtt(&rtt);
    let color = if rtt.as_millis() < 100 {
        Color32::GREEN
    } else if rtt.as_millis() < 300 {
        Color32::YELLOW
    } else {
        Color32::LIGHT_RED
    };
    RichText::new(text).color(color).small()
}

const PARTICIPANT_TILE_MIN_WIDTH: f32 = 360.0;
const PARTICIPANT_GRID_GAP: f32 = 10.0;

fn participant_grid_columns(available_width: f32, participant_count: usize) -> usize {
    if participant_count == 0 {
        return 1;
    }
    let fitting_columns = ((available_width + PARTICIPANT_GRID_GAP)
        / (PARTICIPANT_TILE_MIN_WIDTH + PARTICIPANT_GRID_GAP))
        .floor()
        .max(1.0) as usize;
    fitting_columns.min(participant_count).min(3)
}

fn participant_tile_width(available_width: f32, columns: usize, gap: f32) -> f32 {
    let columns = columns.max(1);
    ((available_width - gap * columns.saturating_sub(1) as f32) / columns as f32).max(1.0)
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
fn native_parent_hwnd(frame: &eframe::Frame) -> Option<windows::Win32::Foundation::HWND> {
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

fn video_display_size(available: Vec2, aspect: f32, _fill_window: bool) -> Vec2 {
    if available.x <= 0.0 || available.y <= 0.0 || aspect <= 0.0 {
        return available;
    }
    if available.x / available.y > aspect {
        Vec2::new(available.y * aspect, available.y)
    } else {
        Vec2::new(available.x, available.x / aspect)
    }
}

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

fn copy_to_clipboard(text: &str) {
    #[cfg(not(target_os = "android"))]
    {
        if let Err(err) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text.to_string())) {
            warn!("failed to copy to clipboard: {err}");
        }
    }
    #[cfg(target_os = "android")]
    if let Err(err) = android_clipboard::set_text(text.to_string()) {
        warn!("failed to copy to clipboard: {err}");
    }
}

fn read_clipboard() -> Option<String> {
    #[cfg(not(target_os = "android"))]
    {
        arboard::Clipboard::new()
            .ok()
            .and_then(|mut c| c.get_text().ok())
    }
    #[cfg(target_os = "android")]
    {
        android_clipboard::get_text().ok()
    }
}

fn fmt_node_id(text: &str) -> RichText {
    let text = format!("{text}…");
    egui::RichText::new(text)
        .underline()
        .family(egui::FontFamily::Monospace)
}

fn fmt_error(text: &str) -> RichText {
    egui::RichText::new(text).color(Color32::LIGHT_RED)
}

fn fmt_rtt(dur: &Duration) -> String {
    format!("{}ms", dur.as_millis())
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    let mut shortened: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn stream_grid_tracks_the_stage_shape() {
        assert_eq!(stream_grid_dims(2, Vec2::new(1200.0, 400.0)), (2, 1));
        assert_eq!(stream_grid_dims(2, Vec2::new(400.0, 1200.0)), (1, 2));
        assert_eq!(stream_grid_dims(4, Vec2::new(800.0, 800.0)), (2, 2));
    }

    #[test]
    fn participant_grid_keeps_every_peer_on_screen() {
        assert_eq!(participant_grid_columns(1570.0, 2), 2);
        assert_eq!(participant_grid_columns(1570.0, 3), 3);
        assert_eq!(participant_grid_columns(700.0, 2), 1);

        let width = participant_tile_width(1570.0, 2, PARTICIPANT_GRID_GAP);
        assert!((width - 780.0).abs() < f32::EPSILON);
        assert_eq!(
            width * 2.0 + PARTICIPANT_GRID_GAP,
            1570.0,
            "tiles and gap must consume exactly the visible width"
        );
    }

    #[test]
    fn video_fit_preserves_aspect_ratio() {
        let wide = video_display_size(Vec2::new(1000.0, 400.0), 16.0 / 9.0, false);
        assert!((wide.x - 711.1111).abs() < 0.01);
        assert_eq!(wide.y, 400.0);

        let narrow = video_display_size(Vec2::new(400.0, 1000.0), 16.0 / 9.0, false);
        assert_eq!(narrow.x, 400.0);
        assert!((narrow.y - 225.0).abs() < 0.01);
    }

    #[test]
    fn long_stream_names_are_clipped_cleanly() {
        assert_eq!(ellipsize("Ada", 8), "Ada");
        assert_eq!(ellipsize("Long display name", 8), "Long di…");
    }

    #[test]
    fn recognizes_only_the_reserved_video_replacement_reset() {
        assert!(is_video_stream_replacement_error(&anyhow::anyhow!(
            "stream reset by peer: error 81"
        )));
        assert!(!is_video_stream_replacement_error(&anyhow::anyhow!(
            "stream reset by peer: error 12"
        )));
        assert!(!is_video_stream_replacement_error(&anyhow::anyhow!(
            "invalid video frame length: 81 bytes"
        )));
    }
}

enum Event {
    EndpointBound(NodeId),
    SetCallState(NodeId, CallState),
    VolumeHandle(NodeId, VolumeHandle),
    SetRtt(NodeId, Duration),
    VideoFrame {
        node_id: NodeId,
        frame: DecodedFrame,
    },
    VideoStreamEnded(NodeId),
    SharingToggled(bool),
    PreviewFrame {
        width: u32,
        height: u32,
        data: Arc<Vec<u8>>,
        actual_fps: f64,
        encode_time_ms: f64,
    },
}

#[derive(strum::Display, Clone, Copy)]
enum CallState {
    Incoming,
    Calling,
    Active,
    Aborted,
}

enum CallInfo {
    Calling,
    Connecting(RtcConnection),
    Incoming(RtcConnection),
    Active(RtcConnection),
}

struct VideoPeerTasks {
    send: Option<tokio::task::JoinHandle<()>>,
    recv: Option<tokio::task::JoinHandle<()>>,
}

impl VideoPeerTasks {
    fn abort_all(&mut self) {
        if let Some(h) = self.send.take() {
            h.abort();
        }
        if let Some(h) = self.recv.take() {
            h.abort();
        }
    }

    fn abort_send(&mut self) {
        if let Some(h) = self.send.take() {
            h.abort();
        }
    }
}

type UpdateCallback = Arc<dyn Fn() + Send + Sync>;

enum Command {
    SetUpdateCallback { callback: UpdateCallback },
    SetAudioConfig { audio_config: AudioConfig },
    SetVideoConfig { video_config: VideoConfig },
    Call { node_id: NodeId },
    HandleIncoming { node_id: NodeId, accept: bool },
    Abort { node_id: NodeId },
    ToggleSharing { enabled: bool },
    SetMuted { muted: bool },
    SetDeafened { deafened: bool },
}

struct Worker {
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
    active_calls: BTreeMap<NodeId, CallInfo>,
    volumes: BTreeMap<NodeId, VolumeHandle>,
    update_callback: Option<UpdateCallback>,
    endpoint: Endpoint,
    handler: RtcProtocol,
    call_tasks: JoinSet<(NodeId, Result<()>)>,
    connect_tasks: JoinSet<(NodeId, Result<RtcConnection>)>,
    track_tasks: JoinSet<(NodeId, Result<MediaTrack>)>,
    _router: Router,
    audio_context: Option<AudioContext>,
    rtt_interval: time::Interval,
    video_config: VideoConfig,
    video_frame_tx: tokio::sync::broadcast::Sender<Arc<wire::video::transport::EncodedVideoFrame>>,
    keyframe_tx: tokio::sync::broadcast::Sender<()>,
    video_peers: BTreeMap<NodeId, VideoPeerTasks>,
    capture_thread: Option<std::thread::JoinHandle<()>>,
    capture_stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    sharing_active: bool,
    muted: bool,
    deafened: bool,
}

struct WorkerHandle {
    command_tx: Sender<Command>,
    event_rx: Receiver<Event>,
}

impl Worker {
    pub fn spawn() -> WorkerHandle {
        let (command_tx, command_rx) = async_channel::bounded(16);
        let (event_tx, event_rx) = async_channel::bounded(64);
        let handle = WorkerHandle {
            event_rx,
            command_tx,
        };
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to start tokio runtime");
            rt.block_on(async move {
                let mut worker = match Worker::start(event_tx, command_rx).await {
                    Ok(worker) => worker,
                    Err(error) => {
                        warn!("worker failed to start: {error:#}");
                        return;
                    }
                };
                if let Err(err) = worker.run().await {
                    warn!("worker stopped with error: {err:?}");
                }
            });
        });
        handle
    }

    async fn emit(&self, event: Event) -> Result<()> {
        self.event_tx.send(event).await?;
        if let Some(callback) = &self.update_callback {
            callback();
        }
        Ok(())
    }

    async fn start(
        event_tx: async_channel::Sender<Event>,
        command_rx: async_channel::Receiver<Command>,
    ) -> Result<Self> {
        let endpoint = wire::net::bind_endpoint().await?;
        let handler = RtcProtocol::new(endpoint.clone());
        let _router = Router::builder(endpoint.clone())
            .accept(RtcProtocol::ALPN, handler.clone())
            .spawn()
            .await?;
        let (video_frame_tx, _) = tokio::sync::broadcast::channel(32);
        let (keyframe_tx, _) = tokio::sync::broadcast::channel(16);
        Ok(Self {
            command_rx,
            event_tx,
            active_calls: Default::default(),
            volumes: Default::default(),
            call_tasks: JoinSet::new(),
            connect_tasks: JoinSet::new(),
            track_tasks: JoinSet::new(),
            endpoint,
            handler,
            _router,
            audio_context: None,
            update_callback: None,
            rtt_interval: time::interval(Duration::from_secs(1)),
            video_config: VideoConfig::default(),
            video_frame_tx,
            keyframe_tx,
            video_peers: Default::default(),
            capture_thread: None,
            capture_stop_flag: None,
            sharing_active: false,
            muted: false,
            deafened: false,
        })
    }

    async fn run(&mut self) -> Result<()> {
        self.emit(Event::EndpointBound(self.endpoint.node_id()))
            .await?;
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let command = command?;
                    if let Err(err) = self.handle_command(command).await {
                        warn!("command failed: {err}");
                    }
                }
                conn = self.handler.accept() => {
                    let Some(conn) = conn? else {
                        break;
                    };
                    self.handle_incoming(conn).await?;
                }
                Some(res) = self.call_tasks.join_next(), if !self.call_tasks.is_empty() => {
                    let (node_id, res) = res.expect("connection task panicked");
                    if let Err(err) = res {
                        warn!("connection with {} closed: {err:?}", node_id.fmt_short());
                    } else {
                        info!("connection with {} closed", node_id.fmt_short());
                    }
                    self.active_calls.remove(&node_id);
                    self.volumes.remove(&node_id);
                    self.remove_video_peer(node_id);
                    self.cleanup_after_call_end().await;
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
                Some(res) = self.connect_tasks.join_next(), if !self.connect_tasks.is_empty() => {
                    let (node_id, res) = res.expect("connect task panicked");
                    self.handle_quic_connected(node_id, res).await?;
                }
                Some(res) = self.track_tasks.join_next(), if !self.track_tasks.is_empty() => {
                    let (node_id, res) = res.expect("track task panicked");
                    self.handle_track_received(node_id, res).await?;
                }
                _ = self.rtt_interval.tick() => {
                    self.query_rtts().await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_incoming(&mut self, conn: RtcConnection) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        info!("incoming connection from {}", node_id.fmt_short());
        self.active_calls.insert(node_id, CallInfo::Incoming(conn));
        self.emit(Event::SetCallState(node_id, CallState::Incoming))
            .await?;
        Ok(())
    }

    async fn handle_quic_connected(
        &mut self,
        node_id: NodeId,
        conn: Result<RtcConnection>,
    ) -> Result<()> {
        match conn {
            Ok(conn) => {
                info!("quic connected to {}", node_id.fmt_short());
                self.active_calls
                    .insert(node_id, CallInfo::Connecting(conn.clone()));
                self.track_tasks.spawn(async move {
                    let res: Result<MediaTrack> = async {
                        conn.recv_track()
                            .await?
                            .ok_or_else(|| anyhow!("connection closed without receiving a track"))
                    }
                    .await;
                    (node_id, res)
                });
            }
            Err(err) => {
                warn!("connection to {} failed: {err:?}", node_id.fmt_short());
                self.active_calls.remove(&node_id);
                self.volumes.remove(&node_id);
                self.cleanup_after_call_end().await;
                self.emit(Event::SetCallState(node_id, CallState::Aborted))
                    .await?;
            }
        }
        Ok(())
    }

    async fn handle_track_received(
        &mut self,
        node_id: NodeId,
        track: Result<MediaTrack>,
    ) -> Result<()> {
        let Some(CallInfo::Connecting(conn)) = self.active_calls.remove(&node_id) else {
            return Ok(());
        };
        match track {
            Ok(track) => self.accept_from_connect(conn, track).await?,
            Err(err) => {
                warn!(
                    "failed to receive audio track from {}: {err:?}",
                    node_id.fmt_short()
                );
                self.remove_video_peer(node_id);
                self.cleanup_after_call_end().await;
                conn.transport().close(0u32.into(), b"bye");
                self.emit(Event::SetCallState(node_id, CallState::Aborted))
                    .await?;
            }
        }
        Ok(())
    }

    async fn accept_from_connect(&mut self, conn: RtcConnection, track: MediaTrack) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        self.volumes.insert(node_id, volume.clone());
        self.emit(Event::VolumeHandle(node_id, volume.clone()))
            .await?;
        self.active_calls
            .insert(node_id, CallInfo::Active(conn.clone()));
        self.emit(Event::SetCallState(node_id, CallState::Active))
            .await?;
        let audio_context = self
            .audio_context
            .clone()
            .context("missing audio context")?;

        self.ensure_video_streams(node_id, conn.clone()).await;

        let audio_conn = conn.clone();
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());

            let fut = async {
                audio_context.play_track_with_volume(track, volume).await?;
                let capture_track = audio_context.capture_track().await?;
                audio_conn.send_track(capture_track).await?;
                #[allow(clippy::redundant_pattern_matching)]
                while let Some(_) = audio_conn.recv_track().await? {}
                anyhow::Ok(())
            };
            let res = fut.await;
            info!("connection with {} closed: {:?}", node_id.fmt_short(), res);
            (node_id, res)
        });
        Ok(())
    }

    async fn accept_from_accept(&mut self, conn: RtcConnection) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        self.volumes.insert(node_id, volume.clone());
        self.emit(Event::VolumeHandle(node_id, volume.clone()))
            .await?;
        self.active_calls
            .insert(node_id, CallInfo::Active(conn.clone()));
        self.emit(Event::SetCallState(node_id, CallState::Active))
            .await?;
        let audio_context = self
            .audio_context
            .clone()
            .context("missing audio context")?;

        self.ensure_video_streams(node_id, conn.clone()).await;

        let audio_conn = conn.clone();
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());

            let fut = async {
                let capture_track = audio_context.capture_track().await?;
                audio_conn.send_track(capture_track).await?;
                info!("added capture track to rtc connection");
                while let Some(remote_track) = audio_conn.recv_track().await? {
                    info!(
                        "new remote track: {:?} {:?}",
                        remote_track.kind(),
                        remote_track.codec()
                    );
                    match remote_track.kind() {
                        TrackKind::Audio => {
                            audio_context
                                .play_track_with_volume(remote_track, volume.clone())
                                .await?;
                        }
                        TrackKind::Video => unimplemented!(),
                    }
                }
                anyhow::Ok(())
            };
            let res = fut.await;
            info!("connection with {} closed: {:?}", node_id.fmt_short(), res);
            (node_id, res)
        });
        Ok(())
    }

    async fn ensure_video_streams(&mut self, node_id: NodeId, conn: RtcConnection) {
        if !matches!(self.active_calls.get(&node_id), Some(CallInfo::Active(_))) {
            return;
        }

        let entry = self.video_peers.entry(node_id).or_insert(VideoPeerTasks {
            send: None,
            recv: None,
        });

        let recv_dead = entry.recv.as_ref().map(|h| h.is_finished()).unwrap_or(true);
        if recv_dead {
            let recv_conn = conn.clone();
            let event_tx = self.event_tx.clone();
            let callback = self.update_callback.clone();
            let nid = node_id;
            let handle = tokio::spawn(async move {
                run_video_recv(recv_conn, nid, event_tx, callback).await;
            });
            entry.recv = Some(handle);
        }

        if self.sharing_active {
            if let Some(h) = entry.send.take() {
                h.abort();
                let _ = h.await;
            }
            let send_conn = conn.clone();
            let frame_tx = self.video_frame_tx.clone();
            let keyframe_tx = self.keyframe_tx.clone();
            let nid = node_id;
            info!("starting video send task for {}", node_id.fmt_short());
            let handle = tokio::spawn(async move {
                run_video_send(send_conn, frame_tx, keyframe_tx, nid).await;
            });
            entry.send = Some(handle);
        }
    }

    async fn finish_all_video_send(&mut self) {
        for tasks in self.video_peers.values_mut() {
            if let Some(h) = tasks.send.take() {
                h.abort();
                let _ = h.await;
            }
        }
    }

    async fn attach_video_to_active_calls(&mut self) {
        let active: Vec<_> = self
            .active_calls
            .iter()
            .filter_map(|(id, info)| match info {
                CallInfo::Active(conn) => Some((*id, conn.clone())),
                _ => None,
            })
            .collect();
        for (node_id, conn) in active {
            self.ensure_video_streams(node_id, conn).await;
        }
    }

    fn remove_video_peer(&mut self, node_id: NodeId) {
        if let Some(mut tasks) = self.video_peers.remove(&node_id) {
            tasks.abort_all();
        }
    }

    async fn cleanup_after_call_end(&mut self) {
        if self.active_calls.is_empty() && self.sharing_active {
            self.stop_capture().await;
        }
    }

    async fn query_rtts(&mut self) -> Result<()> {
        for (node_id, info) in &self.active_calls {
            if let Some(rtt) = match info {
                CallInfo::Active(conn) => Some(conn.transport().rtt()),
                _ => None,
            } {
                self.emit(Event::SetRtt(*node_id, rtt)).await?;
            }
        }
        Ok(())
    }

    fn start_capture(&mut self) -> Result<()> {
        let config = self.video_config;
        let target_w = config.resolution.width();
        let target_h = config.resolution.height();

        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (preview_tx, preview_rx) =
            async_channel::bounded::<crate::screen_capture::PreviewUpdate>(4);
        let event_tx = self.event_tx.clone();
        let callback = self.update_callback.clone();
        tokio::task::spawn(async move {
            while let Ok(update) = preview_rx.recv().await {
                let _ = event_tx
                    .send(Event::PreviewFrame {
                        width: update.width,
                        height: update.height,
                        data: update.data,
                        actual_fps: update.actual_fps,
                        encode_time_ms: update.encode_time_ms,
                    })
                    .await;
                if let Some(cb) = &callback {
                    cb();
                }
            }
        });

        let thread = crate::screen_capture::start(
            config,
            stop_flag.clone(),
            self.video_frame_tx.clone(),
            preview_tx,
            self.keyframe_tx.clone(),
        );

        self.capture_thread = Some(thread);
        self.capture_stop_flag = Some(stop_flag);
        info!(
            "screen capture started ({}x{} @ {}fps, {} kbps, {} active call(s))",
            target_w,
            target_h,
            config.framerate,
            config.effective_bitrate() / 1000,
            self.video_peers.len()
        );
        let event_tx = self.event_tx.clone();
        tokio::task::spawn(async move {
            let _ = event_tx.send(Event::SharingToggled(true)).await;
        });
        Ok(())
    }

    fn stop_capture_thread(&mut self) {
        if let Some(flag) = &self.capture_stop_flag {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
        self.capture_stop_flag = None;
    }

    async fn stop_capture(&mut self) {
        self.stop_capture_thread();
        self.sharing_active = false;
        self.finish_all_video_send().await;
        let event_tx = self.event_tx.clone();
        let _ = event_tx.send(Event::SharingToggled(false)).await;
    }

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::SetUpdateCallback { callback } => {
                self.update_callback = Some(callback);
            }
            Command::SetAudioConfig { audio_config } => {
                let audio_context = AudioContext::new(audio_config).await?;
                audio_context.set_muted(self.muted);
                audio_context.set_deafened(self.deafened);
                self.audio_context = Some(audio_context);
            }
            Command::SetVideoConfig { video_config } => {
                let restart_capture = self.sharing_active;
                if restart_capture {
                    self.stop_capture_thread();
                }
                self.video_config = video_config;
                if restart_capture {
                    self.start_capture()?;
                }
            }
            Command::ToggleSharing { enabled } => {
                if enabled && !self.sharing_active {
                    self.sharing_active = true;
                    let _ = self.keyframe_tx.send(());
                    self.attach_video_to_active_calls().await;
                    self.start_capture()?;
                } else if !enabled && self.sharing_active {
                    self.stop_capture().await;
                }
            }
            Command::SetMuted { muted } => {
                self.muted = muted;
                if let Some(audio_context) = &self.audio_context {
                    audio_context.set_muted(muted);
                }
            }
            Command::SetDeafened { deafened } => {
                self.deafened = deafened;
                if let Some(audio_context) = &self.audio_context {
                    audio_context.set_deafened(deafened);
                }
            }
            Command::Call { node_id } => {
                if self.active_calls.contains_key(&node_id) {
                    return Ok(());
                }
                self.active_calls.insert(node_id, CallInfo::Calling);
                self.emit(Event::SetCallState(node_id, CallState::Calling))
                    .await?;

                let handler = self.handler.clone();
                self.connect_tasks
                    .spawn(async move { (node_id, handler.connect(node_id).await) });
            }
            Command::HandleIncoming { node_id, accept } => {
                let Some(CallInfo::Incoming(conn)) = self.active_calls.remove(&node_id) else {
                    return Ok(());
                };
                if accept {
                    self.accept_from_accept(conn).await?;
                } else {
                    self.remove_video_peer(node_id);
                    conn.transport().close(0u32.into(), b"bye");
                    self.cleanup_after_call_end().await;
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
            }
            Command::Abort { node_id } => {
                if let Some(state) = self.active_calls.remove(&node_id) {
                    self.volumes.remove(&node_id);
                    self.remove_video_peer(node_id);
                    self.cleanup_after_call_end().await;
                    match state {
                        CallInfo::Calling => {}
                        CallInfo::Connecting(conn) => {
                            conn.transport().close(0u32.into(), b"bye");
                        }
                        CallInfo::Active(conn) => {
                            conn.transport().close(0u32.into(), b"bye");
                        }
                        CallInfo::Incoming(conn) => {
                            conn.transport().close(0u32.into(), b"bye");
                        }
                    }
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
            }
        }
        Ok(())
    }
}

async fn run_video_send(
    conn: RtcConnection,
    frame_tx: tokio::sync::broadcast::Sender<Arc<wire::video::transport::EncodedVideoFrame>>,
    keyframe_tx: tokio::sync::broadcast::Sender<()>,
    node_id: NodeId,
) {
    let result: Result<()> = async {
        info!("opening video stream to {}", node_id.fmt_short());
        let (mut send, recv) = conn.transport().open_bi().await?;
        let _ = send.set_priority(10);
        tokio::spawn(drain_quic_recv(recv));
        let mut rx = frame_tx.subscribe();
        let _ = keyframe_tx.send(());
        let mut sent = 0u64;
        let mut skipped = 0u64;
        let mut resyncs = 0u64;
        let mut keyframe_gate = transport::KeyframeGate::waiting();
        let mut replace_stream = false;
        let mut window_sent = 0u64;
        let mut window_bytes = 0u64;
        let mut window_send_ms = Vec::with_capacity(300);
        let mut last_stats_log = std::time::Instant::now();
        loop {
            let frame = match rx.recv().await {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    skipped += count;
                    keyframe_gate.require_keyframe();
                    replace_stream = true;
                    let _ = keyframe_tx.send(());
                    warn!(
                        "video send to {} lagged by {} frame(s); waiting for IDR",
                        node_id.fmt_short(),
                        count
                    );
                    continue;
                }
                Err(_) => break,
            };
            let was_waiting = keyframe_gate.is_waiting();
            if !keyframe_gate.accept(&frame) {
                skipped += 1;
                continue;
            }
            if was_waiting {
                resyncs += 1;
            }
            if replace_stream {
                // Open the replacement before resetting the old stream. If the
                // connection cannot allocate a new stream, the peer keeps the
                // old one instead of being stranded on a permanent last frame.
                let (new_send, recv) = conn.transport().open_bi().await?;
                let _ = new_send.set_priority(10);
                tokio::spawn(drain_quic_recv(recv));
                let mut old_send = std::mem::replace(&mut send, new_send);
                let _ = old_send.reset(VIDEO_STREAM_RESET_CODE);
                replace_stream = false;
                info!(
                    "replaced video stream to {} at IDR sequence {} ({} skipped, {} resyncs)",
                    node_id.fmt_short(),
                    frame.sequence,
                    skipped,
                    resyncs
                );
            }
            let send_start = std::time::Instant::now();
            if let Err(e) = transport::send_frame(&mut send, frame.as_ref()).await {
                info!("video send to {} failed: {e:?}", node_id.fmt_short());
                break;
            }
            let send_elapsed = send_start.elapsed();
            window_sent += 1;
            window_bytes += frame.data.len() as u64;
            window_send_ms.push(send_elapsed.as_secs_f64() * 1000.0);
            let latency_budget = std::cmp::max(
                Duration::from_millis(250),
                conn.transport().rtt().saturating_mul(3),
            );
            if send_elapsed > latency_budget {
                keyframe_gate.require_keyframe();
                replace_stream = true;
                let _ = keyframe_tx.send(());
                warn!(
                    "video send to {} took {:.0}ms for {} bytes (budget {:.0}ms); scheduling frame-boundary resync",
                    node_id.fmt_short(),
                    send_elapsed.as_secs_f64() * 1000.0,
                    frame.data.len(),
                    latency_budget.as_secs_f64() * 1000.0,
                );
            }
            sent += 1;
            if sent == 1 {
                info!(
                    "sent first video frame ({} bytes) to {}",
                    frame.data.len(),
                    node_id.fmt_short()
                );
            } else if send_elapsed > Duration::from_millis(75) {
                info!(
                    "video send to {} took {:.0}ms for {} bytes",
                    node_id.fmt_short(),
                    send_elapsed.as_secs_f64() * 1000.0,
                    frame.data.len()
                );
            }
            if last_stats_log.elapsed() >= Duration::from_secs(5) {
                let elapsed = last_stats_log.elapsed().as_secs_f64();
                let avg = window_send_ms.iter().sum::<f64>() / window_send_ms.len() as f64;
                window_send_ms.sort_by(f64::total_cmp);
                let p95 = window_send_ms
                    [((window_send_ms.len() - 1) as f64 * 0.95).round() as usize];
                info!(
                    "video send pipeline to {}: {:.1} fps, {:.1} Mbps, {:.1} ms avg / {:.1} ms p95, {} skipped, {} resyncs",
                    node_id.fmt_short(),
                    window_sent as f64 / elapsed,
                    window_bytes as f64 * 8.0 / elapsed / 1_000_000.0,
                    avg,
                    p95,
                    skipped,
                    resyncs
                );
                window_sent = 0;
                window_bytes = 0;
                window_send_ms.clear();
                last_stats_log = std::time::Instant::now();
            }
        }
        let _ = send.reset(VIDEO_STREAM_RESET_CODE);
        Ok(())
    }
    .await;
    if let Err(e) = result {
        info!("video send to {} stopped: {e:?}", node_id.fmt_short());
    } else {
        info!("video send to {} ended", node_id.fmt_short());
    }
}

async fn drain_quic_recv(mut recv: impl tokio::io::AsyncRead + Unpin + Send + 'static) {
    let mut buf = [0u8; 256];
    loop {
        match tokio::io::AsyncReadExt::read(&mut recv, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

async fn run_video_recv(
    conn: RtcConnection,
    node_id: NodeId,
    event_tx: async_channel::Sender<Event>,
    callback: Option<UpdateCallback>,
) {
    loop {
        let accept = conn.transport().accept_bi();
        tokio::select! {
            closed = conn.transport().closed() => {
                match closed {
                    iroh::endpoint::ConnectionError::LocallyClosed => {}
                    err => info!(
                        "connection closed while waiting for video from {}: {err:?}",
                        node_id.fmt_short()
                    ),
                }
                break;
            }
            stream = accept => {
                match stream {
                    Ok((_send, mut recv)) => {
                        info!("receiving video from {}", node_id.fmt_short());
                        match recv_video_on_stream(
                            &mut recv,
                            node_id,
                            &event_tx,
                            callback.as_ref(),
                        )
                        .await
                        {
                            Ok(()) => {
                                info!(
                                    "video stream from {} ended cleanly; waiting for another share",
                                    node_id.fmt_short()
                                );
                                notify_video_stream_ended(&event_tx, callback.as_ref(), node_id);
                            }
                            Err(error) if is_video_stream_replacement_error(&error) => {
                                info!(
                                    "video stream from {} was replaced at a frame boundary; waiting for the resync stream",
                                    node_id.fmt_short()
                                );
                            }
                            Err(error) => {
                                warn!(
                                    "video stream from {} failed: {error:?}; waiting for a replacement stream",
                                    node_id.fmt_short()
                                );
                                notify_video_stream_ended(&event_tx, callback.as_ref(), node_id);
                            }
                        }
                    }
                    Err(e) => {
                        info!("video accept_bi from {} failed: {e:?}", node_id.fmt_short());
                        break;
                    }
                }
            }
        }
    }
    notify_video_stream_ended(&event_tx, callback.as_ref(), node_id);
}

fn notify_video_stream_ended(
    event_tx: &async_channel::Sender<Event>,
    callback: Option<&UpdateCallback>,
    node_id: NodeId,
) {
    let _ = event_tx.try_send(Event::VideoStreamEnded(node_id));
    if let Some(callback) = callback {
        callback();
    }
}

fn is_video_stream_replacement_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("stream reset by peer")
        && (message.contains("error 81") || message.contains("error 0x51"))
}

async fn recv_video_on_stream(
    recv: &mut (impl tokio::io::AsyncRead + Unpin),
    node_id: NodeId,
    event_tx: &async_channel::Sender<Event>,
    callback: Option<&UpdateCallback>,
) -> Result<()> {
    let frame_event_tx = event_tx.clone();
    let frame_callback = callback.cloned();
    let worker = VideoDecodeWorker::spawn(move |frame| {
        if frame_event_tx
            .try_send(Event::VideoFrame { node_id, frame })
            .is_ok()
        {
            if let Some(cb) = &frame_callback {
                cb();
            }
        }
    })?;
    let mut received = 0u64;
    let mut received_bytes = 0u64;
    let mut received_age_ms = 0.0;
    let mut max_age_ms = 0.0;
    let mut age_samples = Vec::with_capacity(300);
    let mut last_sequence: Option<u64> = None;
    let mut last_stats_log = std::time::Instant::now();

    loop {
        match transport::recv_frame(recv).await {
            Ok(Some(frame)) => {
                if let Some(previous) = last_sequence {
                    let expected = previous.wrapping_add(1);
                    if frame.sequence != expected {
                        warn!(
                            "video sequence gap from {}: expected {}, received {} (keyframe={})",
                            node_id.fmt_short(),
                            expected,
                            frame.sequence,
                            frame.keyframe
                        );
                    }
                }
                last_sequence = Some(frame.sequence);
                received += 1;
                received_bytes += frame.data.len() as u64;
                if let Some(age_ms) = transport::frame_age_ms(frame.sent_at_micros) {
                    received_age_ms += age_ms;
                    max_age_ms = f64::max(max_age_ms, age_ms);
                    age_samples.push(age_ms);
                }
                if last_stats_log.elapsed() >= Duration::from_secs(5) {
                    let elapsed = last_stats_log.elapsed().as_secs_f64();
                    let recv_fps = if elapsed > 0.0 {
                        received as f64 / elapsed
                    } else {
                        0.0
                    };
                    let avg_packet_kb = if received > 0 {
                        received_bytes as f64 / received as f64 / 1024.0
                    } else {
                        0.0
                    };
                    let avg_age_ms = if received > 0 {
                        received_age_ms / received as f64
                    } else {
                        0.0
                    };
                    age_samples.sort_by(f64::total_cmp);
                    let p95_age_ms = if age_samples.is_empty() {
                        0.0
                    } else {
                        age_samples[((age_samples.len() - 1) as f64 * 0.95).round() as usize]
                    };
                    info!(
                        "video receive pipeline from {}: {:.1} fps, {:.1} KiB/frame, {:.0}ms avg / {:.0}ms p95 / {:.0}ms max age",
                        node_id.fmt_short(),
                        recv_fps,
                        avg_packet_kb,
                        avg_age_ms,
                        p95_age_ms,
                        max_age_ms
                    );
                    last_stats_log = std::time::Instant::now();
                    received = 0;
                    received_bytes = 0;
                    received_age_ms = 0.0;
                    max_age_ms = 0.0;
                    age_samples.clear();
                }
                worker.submit(frame.data, frame.keyframe);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(e);
            }
        }
    }

    drop(worker);
    Ok(())
}
