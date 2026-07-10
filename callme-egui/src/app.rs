use std::{
    collections::BTreeMap,
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use callme::{
    audio::{AudioConfig, AudioContext, AudioQuality, VolumeHandle},
    rtc::{MediaTrack, RtcConnection, RtcProtocol, TrackKind},
    video::{transport, BitratePreset, StreamPreset, VideoConfig},
};
use eframe::NativeOptions;
use egui::{Align, Align2, Color32, CornerRadius, Frame, Layout, RichText, Stroke, Ui, Vec2};
use egui_phosphor::regular as ph;
use iroh::{endpoint::VarInt, protocol::Router, Endpoint, KeyParsingError, NodeId};
use tokio::task::JoinSet;
use tokio::time;
use tracing::{info, warn};

use crate::{
    theme::*,
    update::{self, ReleaseInfo},
    video_decode::VideoDecodeWorker,
};

const DEFAULT: &str = "<default>";
const VIDEO_SEND_LATENCY_BUDGET: Duration = Duration::from_millis(150);
const VIDEO_STREAM_RESET_CODE: VarInt = VarInt::from_u32(0x51);

pub struct App {
    is_first_update: bool,
    state: AppState,
}

/// A locally stored contact, identified by their stable callme node id.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Friend {
    name: String,
    node_id: String,
}

fn friends_path() -> Option<PathBuf> {
    callme::net::config_dir().map(|dir| dir.join("friends.json"))
}

fn load_friends() -> Vec<Friend> {
    if let Some(path) = friends_path() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(friends) = serde_json::from_str::<Vec<Friend>>(&contents) {
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
    callme::net::config_dir().map(|dir| dir.join("settings.json"))
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
    devices: callme::audio::Devices,
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
    muted: bool,
    deafened: bool,
    update_tx: mpsc::Sender<UpdateMessage>,
    update_rx: mpsc::Receiver<UpdateMessage>,
    update_status: UpdateStatus,
    show_update_prompt: bool,
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
}

struct VideoFrameState {
    width: u32,
    height: u32,
    generation: u64,
    data: Arc<Vec<u8>>,
    texture: Option<egui::TextureHandle>,
    uploaded_generation: u64,
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
    configured: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio: UiAudioConfig::default(),
            video: VideoConfig::default(),
            theme: Theme::default(),
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.is_first_update {
            self.is_first_update = false;
            let repaint_ctx = ctx.clone();
            let callback = Arc::new(move || repaint_ctx.request_repaint());
            self.state.cmd(Command::SetUpdateCallback { callback });
            #[cfg(windows)]
            self.state.start_update_check(ctx);
        }
        // on android, add some space at the top.
        #[cfg(target_os = "android")]
        egui::TopBottomPanel::top("my_panel")
            .min_height(40.)
            .show(ctx, |_ui| {});

        self.state.update(ctx);
    }
}

impl App {
    pub fn run(options: NativeOptions) -> Result<(), eframe::Error> {
        let handle = Worker::spawn();
        let devices =
            callme::audio::AudioContext::list_devices_sync().expect("failed to list audio devices");
        let saved_settings = load_settings();
        let has_saved_settings = saved_settings
            .as_ref()
            .map(|settings| settings.configured)
            .unwrap_or(false);
        let settings = saved_settings.unwrap_or_default();
        let (update_tx, update_rx) = mpsc::channel();
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
            muted: false,
            deafened: false,
            update_tx,
            update_rx,
            update_status: UpdateStatus::Idle,
            show_update_prompt: false,
        };

        if has_saved_settings {
            state.cmd(Command::SetAudioConfig {
                audio_config: state.audio_config(),
            });
            state.cmd(Command::SetVideoConfig {
                video_config: state.video_config,
            });
        }

        let app = App {
            state,
            is_first_update: true,
        };
        eframe::run_native(
            "callme",
            options,
            Box::new(|cc| {
                setup_fonts(&cc.egui_ctx);
                Ok(Box::new(app))
            }),
        )
    }
}
impl AppState {
    fn update(&mut self, ctx: &egui::Context) {
        self.process_update_events(ctx);
        if ctx.input(|i| i.key_pressed(egui::Key::T)) {
            self.theme = self.theme.next();
            self.persist_theme();
        }
        let pal = Palette::for_theme(self.theme);
        ctx.set_visuals(visuals_for(&pal));

        self.process_events();
        self.handle_view_mode_input(ctx);

        let immersive = self.stream_view_mode != StreamViewMode::Normal;

        if !self.stream_view_mode.is_fullscreen() {
            self.ui_top_bar(ctx);
        }

        egui::CentralPanel::default()
            .frame(if immersive {
                Frame::NONE
            } else {
                Frame::central_panel(&ctx.style())
                    .fill(pal.bg)
                    .inner_margin(egui::Margin::symmetric(20, 14))
            })
            .show(ctx, |ui| self.ui_stage(ui, ctx, &pal));

        if !self.stream_view_mode.is_fullscreen() {
            self.ui_dock(ctx, &pal);
        }

        if self.show_settings || !self.configured {
            self.ui_settings_window(ctx);
        }
        if self.show_update_prompt {
            self.ui_update_prompt(ctx);
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
                }
                Event::SetCallState(node_id, call_state) => {
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
                }
                Event::VolumeHandle(node_id, volume) => {
                    self.volumes.insert(node_id, volume);
                }
                Event::SetRtt(node_id, rtt) => {
                    self.rtts.insert(node_id, rtt);
                }
                Event::VideoFrame {
                    node_id,
                    data,
                    width,
                    height,
                } => {
                    if !matches!(self.calls.get(&node_id), Some(CallState::Active)) {
                        continue;
                    }
                    let state =
                        self.video_frames
                            .entry(node_id)
                            .or_insert_with(|| VideoFrameState {
                                width: 0,
                                height: 0,
                                generation: 0,
                                data: Arc::new(Vec::new()),
                                texture: None,
                                uploaded_generation: 0,
                            });
                    state.width = width;
                    state.height = height;
                    state.data = data;
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
                && !self.video_frames[node_id].data.is_empty()
            {
                sources.push(StreamSource::Remote(*node_id));
            }
        }
        sources
    }

    fn stream_label(&self, source: StreamSource) -> String {
        match source {
            StreamSource::Local => "You".to_string(),
            StreamSource::Remote(node_id) => node_id.fmt_short().to_string(),
        }
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

    fn ui_top_bar(&mut self, ctx: &egui::Context) {
        let pal = Palette::for_theme(self.theme);
        egui::TopBottomPanel::top("top_bar")
            .frame(
                Frame::new()
                    .fill(pal.bg)
                    .inner_margin(egui::Margin::symmetric(20, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("CALLME")
                            .family(kh_family())
                            .color(pal.text)
                            .size(16.0),
                    );
                    ui.add_space(10.0);
                    v_sep(ui, pal.line);
                    ui.add_space(10.0);
                    if let Some(node_id) = &self.our_node_id {
                        ui.label("Node");
                        ui.label(fmt_node_id(&node_id.fmt_short()));
                    } else {
                        ui.label(RichText::new("Connecting…").weak());
                    }

                    let available_update = match &self.update_status {
                        UpdateStatus::Available(release) => Some(release.clone()),
                        _ => None,
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if action_button(ui, &pal, "Settings", ButtonTone::Secondary)
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
                        if active_calls > 0 {
                            ui.label(
                                RichText::new(format!("{active_calls} active call(s)"))
                                    .color(Color32::from_rgb(100, 200, 120)),
                            );
                        }
                        if self.sharing_active {
                            ui.label(
                                RichText::new("Sharing screen")
                                    .color(Color32::from_rgb(120, 170, 255)),
                            );
                        }
                        ui.add_space(8.0);
                        if theme_badge(ui, &pal, self.theme.name()).clicked() {
                            self.theme = self.theme.next();
                            self.persist_theme();
                        }
                    });
                });
            });
    }

    fn ui_dock(&mut self, ctx: &egui::Context, pal: &Palette) {
        egui::TopBottomPanel::bottom("call_dock")
            .exact_height(86.0)
            .frame(
                Frame::new()
                    .fill(pal.bg)
                    .inner_margin(egui::Margin::symmetric(20, 8)),
            )
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                ui.painter()
                    .hline(rect.x_range(), rect.top() - 8.0, Stroke::new(1.0, pal.line));
                let active_calls = self
                    .calls
                    .values()
                    .filter(|state| matches!(state, CallState::Active))
                    .count();

                let control_width = 58.0;
                let controls_width =
                    control_width * 4.0 + if active_calls > 0 { 100.0 } else { 0.0 };
                let controls_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.right() - controls_width, rect.top()),
                    Vec2::new(controls_width, rect.height()),
                );
                let status_rect = egui::Rect::from_min_max(
                    rect.left_top(),
                    egui::pos2(
                        (controls_rect.left() - 12.0).max(rect.left()),
                        rect.bottom(),
                    ),
                );

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
                            .size(13.0),
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
                                .size(12.0),
                            );
                            if self.sharing_active {
                                ui.add_space(8.0);
                                dot(ui, pal.accent, 5.0);
                                ui.label(RichText::new("sharing").color(pal.dim).size(12.0));
                            }
                        });
                    });
                });

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
                            self.cmd(Command::SetDeafened {
                                deafened: self.deafened,
                            });
                        }
                        ui.add_space(2.0);
                        if dock_control(
                            ui,
                            pal,
                            ph::MONITOR,
                            if self.sharing_active {
                                "Stop share"
                            } else {
                                "Share"
                            },
                            self.sharing_active,
                        )
                        .on_hover_text(if self.sharing_active {
                            "Stop sharing"
                        } else {
                            "Share your screen"
                        })
                        .clicked()
                        {
                            self.cmd(Command::ToggleSharing {
                                enabled: !self.sharing_active,
                            });
                        }
                        ui.add_space(2.0);
                        if dock_control(ui, pal, ph::GEAR_SIX, "Settings", false)
                            .on_hover_text("Audio and screen sharing settings")
                            .clicked()
                        {
                            self.show_settings = true;
                        }
                        if active_calls > 0 {
                            ui.add_space(8.0);
                            v_sep(ui, pal.line);
                            ui.add_space(8.0);
                            if leave_button(ui, pal).clicked() {
                                let peers: Vec<_> = self.calls.keys().copied().collect();
                                for node_id in peers {
                                    self.cmd(Command::Abort { node_id });
                                }
                            }
                        }
                    });
                });
            });
    }

    fn ui_stage(&mut self, ui: &mut Ui, ctx: &egui::Context, pal: &Palette) {
        if self.stream_view_mode != StreamViewMode::Normal {
            self.ui_stream_panel(ui, ctx);
            return;
        }

        let has_live_visual = !self.active_stream_sources().is_empty() || self.sharing_active;
        let stage_height = if has_live_visual {
            (ui.available_height() * 0.45).clamp(160.0, 380.0)
        } else {
            (ui.available_height() * 0.25).clamp(120.0, 150.0)
        };
        let stage_width = ui.available_width();
        ui.allocate_ui_with_layout(
            Vec2::new(stage_width, stage_height),
            Layout::top_down(Align::Min),
            |ui| self.ui_stream_panel(ui, ctx),
        );
        ui.add_space(14.0);

        self.ui_participant_strip(ui, pal);
        ui.add_space(14.0);

        ui.columns(2, |columns| {
            self.ui_identity_card(&mut columns[0]);
            columns[0].add_space(10.0);
            self.ui_dial_card(&mut columns[0]);
            self.ui_friends_card(&mut columns[1]);
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
            ui.label(RichText::new("live call status").color(pal.dim2).size(12.0));
        });
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if self.calls.is_empty() {
                self.ui_empty_peer_tile(ui, pal);
            } else {
                let calls: Vec<_> = self
                    .calls
                    .iter()
                    .map(|(node_id, state)| (*node_id, *state))
                    .collect();
                for (node_id, state) in calls {
                    self.ui_peer_tile(ui, pal, node_id, state);
                    ui.add_space(10.0);
                }
            }
        });
    }

    fn ui_empty_peer_tile(&self, ui: &mut Ui, pal: &Palette) {
        Frame::new()
            .fill(pal.panel)
            .stroke(Stroke::new(1.0, pal.line))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_min_width(220.0);
                ui.label(
                    RichText::new("NO ACTIVE PEERS")
                        .family(kh_family())
                        .color(pal.text2)
                        .size(12.0),
                );
                ui.label(
                    RichText::new("Call a saved contact or paste a node ID below.")
                        .color(pal.dim)
                        .size(12.0),
                );
            });
    }

    fn ui_peer_tile(&mut self, ui: &mut Ui, pal: &Palette, node_id: NodeId, state: CallState) {
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
                ui.set_min_width(208.0);
                ui.horizontal(|ui| {
                    circle_avatar(ui, pal, "P", 28.0);
                    ui.vertical(|ui| {
                        ui.label(
                            fmt_node_id(&node_id.fmt_short())
                                .color(pal.text)
                                .monospace()
                                .size(12.0),
                        );
                        let (label, color) = match state {
                            CallState::Incoming => ("incoming", pal.accent),
                            CallState::Calling => ("connecting", pal.accent),
                            CallState::Active => ("connected", pal.ok),
                            CallState::Aborted => ("ended", pal.err),
                        };
                        ui.label(RichText::new(label).color(color).size(12.0));
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
                            ui.label(rtt_label(*rtt).color(pal.dim).monospace().size(11.0));
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
                                self.cmd(Command::Abort { node_id });
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
                            self.cmd(Command::Call { node_id });
                        }
                    }
                });
                match &self.remote_node_id {
                    Some(Ok(node_id)) => {
                        ui.label(fmt_node_id(&node_id.fmt_short()));
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
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(friend.name.clone());
                        match &parsed {
                            Ok(id) => {
                                ui.label(fmt_node_id(&id.fmt_short()));
                            }
                            Err(_) => {
                                ui.label(RichText::new("invalid id").small().weak());
                            }
                        };
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
                self.cmd(Command::Call { node_id: id });
            }
            if let Some(idx) = remove_idx {
                self.friends.remove(idx);
                save_friends(&self.friends);
            }

            ui.separator();
            ui.label(RichText::new("Add a friend").strong());
            ui.add(
                egui::TextEdit::singleline(&mut self.new_friend_name).hint_text("Name (optional)"),
            );
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.new_friend_id).hint_text("Their node ID"),
                );
                if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    self.add_friend();
                }
            });
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
            node_id.clone()
        } else {
            self.new_friend_name.trim().to_string()
        };
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
                            ui.label(fmt_node_id(&node_id.fmt_short()));
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
                                    self.cmd(Command::Abort { node_id });
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
                        self.cmd(Command::ToggleSharing { enabled: false });
                    }
                    ui.label(RichText::new("Live").color(Color32::from_rgb(100, 200, 120)));
                } else if action_button(ui, &pal, "Start sharing", ButtonTone::Primary).clicked() {
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

    fn ui_stream_panel(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let pal = Palette::for_theme(self.theme);
        let immersive = self.stream_view_mode != StreamViewMode::Normal;
        let streams = self.active_stream_sources();
        let has_stream = !streams.is_empty();

        if immersive {
            self.ui_stream_toolbar(ui, ctx, streams.len(), has_stream, true);
            ui.add_space(4.0);
        } else {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("STAGE")
                        .family(kh_family())
                        .color(pal.text)
                        .size(16.0),
                );
                ui.label(
                    RichText::new({
                        if has_stream {
                            if self.focused_stream.is_some() {
                                "focused stream".to_string()
                            } else if streams.len() == 1 {
                                "1 stream".to_string()
                            } else {
                                format!("{} streams", streams.len())
                            }
                        } else if self.sharing_active {
                            "starting share…".to_string()
                        } else {
                            "secure screen sharing".to_string()
                        }
                    })
                    .color(pal.dim)
                    .size(13.0),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.ui_stream_toolbar(ui, ctx, streams.len(), has_stream, false);
                });
            });
            ui.add_space(8.0);
        }

        let available = ui.available_size();
        let (area, _) = ui.allocate_exact_size(available, egui::Sense::hover());

        if !has_stream {
            if !immersive {
                ui.painter()
                    .rect_filled(area, CornerRadius::same(10), pal.panel);
                ui.painter().rect_stroke(
                    area,
                    CornerRadius::same(10),
                    Stroke::new(1.0, pal.line),
                    egui::StrokeKind::Inside,
                );
            }
            ui.painter().text(
                area.center(),
                Align2::CENTER_CENTER,
                if self.sharing_active {
                    "Starting screen share…"
                } else {
                    "No active streams"
                },
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
            return;
        }

        if let Some(focused) = self
            .focused_stream
            .filter(|source| streams.contains(source))
        {
            ui.allocate_new_ui(egui::UiBuilder::new().max_rect(area), |ui| {
                self.ui_stream_tile(ui, &pal, focused, true, immersive);
            });
            return;
        }

        let count = streams.len();
        let (cols, rows) = stream_grid_dims(count);
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
                self.ui_stream_tile(ui, &pal, *source, false, immersive);
            });
        }
    }

    fn ui_stream_tile(
        &mut self,
        ui: &mut Ui,
        pal: &Palette,
        source: StreamSource,
        expanded: bool,
        immersive: bool,
    ) {
        let available = ui.available_size();
        let (tile_rect, _tile_response) = ui.allocate_exact_size(available, egui::Sense::hover());
        let corner = if immersive { 0.0 } else { 10.0 };
        let corner_radius = CornerRadius::same(corner as u8);
        let btn_size = 30.0;
        let btn_rect = egui::Rect::from_min_size(
            tile_rect.right_top() + egui::vec2(-btn_size - 8.0, 8.0),
            Vec2::splat(btn_size),
        );
        let pointer_over = ui.ctx().pointer_hover_pos().is_some_and(|pos| {
            tile_rect.contains(pos) || btn_rect.contains(pos)
        });
        let show_controls = expanded || pointer_over;

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

        let label = self.stream_label(source);
        let (width, height, texture_id) = match source {
            StreamSource::Local => {
                let Some(preview) = &mut self.preview else {
                    return;
                };
                let width = preview.width;
                let height = preview.height;
                sync_rgba_texture(
                    ui,
                    "preview-stage",
                    width,
                    height,
                    &preview.data,
                    preview.generation,
                    &mut preview.uploaded_generation,
                    &mut preview.texture,
                );
                let texture_id = preview.texture.as_ref().map(|tex| tex.id());
                (width, height, texture_id)
            }
            StreamSource::Remote(node_id) => {
                let Some(frame) = self.video_frames.get_mut(&node_id) else {
                    return;
                };
                let width = frame.width;
                let height = frame.height;
                sync_rgba_texture(
                    ui,
                    &format!("video-{node_id}"),
                    width,
                    height,
                    &frame.data,
                    frame.generation,
                    &mut frame.uploaded_generation,
                    &mut frame.texture,
                );
                let texture_id = frame.texture.as_ref().map(|tex| tex.id());
                (width, height, texture_id)
            }
        };

        let Some(texture_id) = texture_id else {
            ui.painter().text(
                tile_rect.center(),
                Align2::CENTER_CENTER,
                "Waiting for video…",
                sans(13.0),
                pal.dim,
            );
            return;
        };

        let aspect = width as f32 / height.max(1) as f32;
        let image_rect = egui::Rect::from_center_size(
            tile_rect.center(),
            video_display_size(tile_rect.shrink(4.0).size(), aspect, expanded),
        );
        ui.painter().image(
            texture_id,
            image_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        let name_bg = egui::Rect::from_min_size(
            tile_rect.left_bottom() + egui::vec2(8.0, -30.0),
            egui::vec2(120.0, 22.0),
        );
        ui.painter()
            .rect_filled(name_bg, CornerRadius::same(4), Color32::from_rgba_unmultiplied(0, 0, 0, 170));
        ui.painter().text(
            name_bg.left_center() + egui::vec2(8.0, 0.0),
            Align2::LEFT_CENTER,
            &label,
            sans(11.0),
            pal.text,
        );

        if show_controls && !expanded {
            ui.painter().rect_filled(
                tile_rect,
                corner_radius,
                Color32::from_rgba_unmultiplied(0, 0, 0, 50),
            );
        }

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
        if show_controls {
            if ui
                .put(
                    btn_rect,
                    egui::Button::new(RichText::new(icon).size(15.0))
                        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 190))
                        .stroke(Stroke::new(1.0, pal.line_br)),
                )
                .on_hover_text(tooltip)
                .clicked()
            {
                if expanded {
                    self.focused_stream = None;
                } else {
                    self.focused_stream = Some(source);
                }
            }
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
        if stream_count > 1 {
            ui.label(
                RichText::new(format!("{stream_count} streams"))
                    .color(Color32::from_rgb(0x91, 0x8e, 0x8a))
                    .size(12.0),
            );
        }

        if has_stream {
            let fill_selected = self.stream_view_mode == StreamViewMode::FillWindow;
            if ui
                .selectable_label(fill_selected, if compact { "Fill" } else { "Fill window" })
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
            if ui
                .selectable_label(
                    fs_selected,
                    if compact { "Fullscreen" } else { "Fullscreen" },
                )
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
            if ui
                .button("Exit")
                .on_hover_text("Return to normal layout (Esc)")
                .clicked()
            {
                self.set_stream_view_mode(ctx, StreamViewMode::Normal);
            }
        }
    }

    fn ui_settings_window(&mut self, ctx: &egui::Context) {
        let can_close = self.configured;
        egui::Window::new("Settings")
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new("Audio").strong());
                ui.add_space(4.0);

                egui::ComboBox::from_label("Microphone")
                    .selected_text(&self.audio_config.selected_input)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(self.audio_config.selected_input == DEFAULT, DEFAULT)
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
                                self.audio_config.selected_input = device.to_string();
                            }
                        }
                    });

                egui::ComboBox::from_label("Speakers")
                    .selected_text(&self.audio_config.selected_output)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(self.audio_config.selected_output == DEFAULT, DEFAULT)
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
                                self.audio_config.selected_output = device.to_string();
                            }
                        }
                    });

                #[cfg(feature = "audio-processing")]
                ui.checkbox(
                    &mut self.audio_config.processing_enabled,
                    "Echo cancellation",
                );

                egui::ComboBox::from_label("Audio quality")
                    .selected_text(self.audio_config.quality.label())
                    .show_ui(ui, |ui| {
                        for quality in &[
                            AudioQuality::Low,
                            AudioQuality::Medium,
                            AudioQuality::High,
                            AudioQuality::Ultra,
                        ] {
                            let label =
                                format!("{} ({})", quality.label(), quality.bandwidth_human());
                            if ui
                                .selectable_label(self.audio_config.quality == *quality, &label)
                                .clicked()
                            {
                                self.audio_config.quality = *quality;
                            }
                        }
                    });

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("Screen sharing").strong());
                ui.add_space(4.0);

                let preset_label = StreamPreset::matches(&self.video_config)
                    .map(|p| p.label)
                    .unwrap_or("Custom");
                egui::ComboBox::from_label("Stream quality")
                    .selected_text(preset_label)
                    .show_ui(ui, |ui| {
                        for preset in StreamPreset::all() {
                            let selected = self.video_config.resolution == preset.resolution
                                && self.video_config.framerate == preset.framerate;
                            if ui.selectable_label(selected, preset.label).clicked() {
                                self.video_config.resolution = preset.resolution;
                                self.video_config.framerate = preset.framerate;
                            }
                        }
                    });

                ui.label(
                    RichText::new(format!(
                        "{}×{} @ {} fps",
                        self.video_config.resolution.width(),
                        self.video_config.resolution.height(),
                        self.video_config.framerate
                    ))
                    .small()
                    .weak(),
                );

                let bitrate_label = BitratePreset::from_config(&self.video_config).label();
                egui::ComboBox::from_label("Bitrate")
                    .selected_text(bitrate_label)
                    .show_ui(ui, |ui| {
                        for preset in BitratePreset::all() {
                            let selected =
                                BitratePreset::from_config(&self.video_config) == *preset;
                            if ui.selectable_label(selected, preset.label()).clicked() {
                                self.video_config.bitrate_bps = preset.bps();
                            }
                        }
                    });

                ui.label(
                    RichText::new(format!(
                        "Effective bitrate: {} Mbps",
                        self.video_config.effective_bitrate() / 1_000_000
                    ))
                    .small()
                    .weak(),
                );

                #[cfg(windows)]
                {
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(RichText::new("Updates").strong());
                    ui.label(
                        RichText::new(format!("Installed version: v{}", env!("CARGO_PKG_VERSION")))
                            .small()
                            .weak(),
                    );

                    let mut check_clicked = false;
                    let mut download = None;
                    match &self.update_status {
                        UpdateStatus::Idle => {
                            check_clicked = ui.button("Check for updates").clicked();
                        }
                        UpdateStatus::Checking => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Checking for updates...");
                            });
                        }
                        UpdateStatus::UpToDate => {
                            ui.horizontal(|ui| {
                                ui.label("You are up to date.");
                                check_clicked = ui.button("Check again").clicked();
                            });
                        }
                        UpdateStatus::Available(release) => {
                            ui.label(format!("Version v{} is available.", release.version));
                            if ui.button("Download to Desktop and relaunch").clicked() {
                                download = Some(release.clone());
                            }
                        }
                        UpdateStatus::Downloading(release) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(format!("Downloading v{}...", release.version));
                            });
                        }
                        UpdateStatus::Error(error) => {
                            ui.colored_label(Color32::from_rgb(220, 100, 90), error);
                            check_clicked = ui.button("Try again").clicked();
                        }
                    }
                    if check_clicked {
                        self.start_update_check(ctx);
                    }
                    if let Some(release) = download {
                        self.start_update_download(ctx, release);
                    }
                }

                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let audio_config = self.audio_config();
                        let video_config = self.video_config;
                        self.cmd(Command::SetAudioConfig { audio_config });
                        self.cmd(Command::SetVideoConfig { video_config });
                        self.persist_settings();
                        self.configured = true;
                        self.show_settings = false;
                    }
                    if can_close && ui.button("Cancel").clicked() {
                        self.show_settings = false;
                    }
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
                    "Callme v{} is available. You are running v{}.",
                    release.version,
                    env!("CARGO_PKG_VERSION")
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
            ui.add_space(8.0);
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

fn stream_grid_dims(count: usize) -> (usize, usize) {
    match count {
        0 => (1, 1),
        1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        5 | 6 => (3, 2),
        7 | 8 | 9 => (3, 3),
        _ => {
            let cols = (count as f32).sqrt().ceil() as usize;
            let rows = count.div_ceil(cols);
            (cols, rows)
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
) {
    if *uploaded_generation == generation || width == 0 || height == 0 || data.is_empty() {
        return;
    }
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], data);
    let options = egui::TextureOptions::LINEAR;
    if let Some(tex) = texture {
        tex.set(color_image, options);
    } else {
        *texture = Some(ui.ctx().load_texture(id.to_string(), color_image, options));
    }
    *uploaded_generation = generation;
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

enum Event {
    EndpointBound(NodeId),
    SetCallState(NodeId, CallState),
    VolumeHandle(NodeId, VolumeHandle),
    SetRtt(NodeId, Duration),
    VideoFrame {
        node_id: NodeId,
        width: u32,
        height: u32,
        data: Arc<Vec<u8>>,
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
    video_frame_tx: tokio::sync::broadcast::Sender<Arc<Vec<u8>>>,
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
                let mut worker = Worker::start(event_tx, command_rx)
                    .await
                    .expect("worker failed to start");
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
        let endpoint = callme::net::bind_endpoint().await?;
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
        if !matches!(
            self.active_calls.get(&node_id),
            Some(CallInfo::Active(_))
        ) {
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
    frame_tx: tokio::sync::broadcast::Sender<Arc<Vec<u8>>>,
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
        let mut resets = 0u64;
        loop {
            let frame = match rx.recv().await {
                Ok(frame) => frame,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let mut latest = frame;
            let mut source_closed = false;
            loop {
                match rx.try_recv() {
                    Ok(frame) => latest = frame,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        source_closed = true;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                }
            }
            if source_closed {
                break;
            }
            let send_start = std::time::Instant::now();
            if let Err(e) = transport::send_frame(&mut send, &latest).await {
                info!("video send to {} failed: {e:?}", node_id.fmt_short());
                break;
            }
            let send_ms = send_start.elapsed().as_secs_f64() * 1000.0;
            if send_ms > 1000.0 {
                resets += 1;
                if resets <= 3 || resets % 30 == 0 {
                    warn!(
                        "video send to {} took {:.0}ms for {} bytes (backpressured by receiver/network)",
                        node_id.fmt_short(),
                        send_ms,
                        latest.len()
                    );
                }
            }
            sent += 1;
            if sent == 1 {
                info!(
                    "sent first video frame ({} bytes) to {}",
                    latest.len(),
                    node_id.fmt_short()
                );
            } else if send_start.elapsed() > Duration::from_millis(75) {
                info!(
                    "video send to {} took {:.0}ms for {} bytes",
                    node_id.fmt_short(),
                    send_start.elapsed().as_secs_f64() * 1000.0,
                    latest.len()
                );
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
                        if let Err(e) =
                            recv_video_on_stream(&mut recv, node_id, &event_tx, callback.as_ref())
                                .await
                        {
                            info!("video stream from {} ended: {e:?}", node_id.fmt_short());
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
}

async fn recv_video_on_stream(
    recv: &mut (impl tokio::io::AsyncRead + Unpin),
    node_id: NodeId,
    event_tx: &async_channel::Sender<Event>,
    callback: Option<&UpdateCallback>,
) -> Result<()> {
    let frame_event_tx = event_tx.clone();
    let ended_event_tx = event_tx.clone();
    let frame_callback = callback.cloned();
    let ended_callback = callback.cloned();
    let worker = VideoDecodeWorker::spawn(move |frame| {
        if frame_event_tx
            .try_send(Event::VideoFrame {
                node_id,
                data: frame.data,
                width: frame.width,
                height: frame.height,
            })
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
    let mut last_stats_log = std::time::Instant::now();

    loop {
        match transport::recv_frame(recv).await {
            Ok(Some((data, sent_at_micros))) => {
                received += 1;
                received_bytes += data.len() as u64;
                if let Some(age_ms) = transport::frame_age_ms(sent_at_micros) {
                    received_age_ms += age_ms;
                    max_age_ms = f64::max(max_age_ms, age_ms);
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
                    info!(
                        "video receive pipeline from {}: {:.1} fps, {:.1} KiB/frame, {:.0}ms avg age, {:.0}ms max age",
                        node_id.fmt_short(),
                        recv_fps,
                        avg_packet_kb,
                        avg_age_ms,
                        max_age_ms
                    );
                    last_stats_log = std::time::Instant::now();
                    received = 0;
                    received_bytes = 0;
                    received_age_ms = 0.0;
                    max_age_ms = 0.0;
                }
                worker.submit(data);
            }
            Ok(None) => break,
            Err(e) => {
                info!(
                    "video stream from {} ended with error: {e:?}",
                    node_id.fmt_short()
                );
                break;
            }
        }
    }

    drop(worker);
    let _ = ended_event_tx.try_send(Event::VideoStreamEnded(node_id));
    if let Some(cb) = ended_callback {
        cb();
    }
    Ok(())
}
