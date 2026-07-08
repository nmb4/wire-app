use std::{
    collections::BTreeMap,
    str::FromStr,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use callme::{
    audio::{AudioConfig, AudioContext, AudioQuality, VolumeHandle},
    rtc::{MediaTrack, RtcConnection, RtcProtocol, TrackKind},
    video::{codec::VideoDecoder, transport, VideoConfig, VideoResolution},
};
use eframe::NativeOptions;
use egui::{Color32, RichText, Ui};
use iroh::{protocol::Router, Endpoint, KeyParsingError, NodeId};
use tokio::task::JoinSet;
use tokio::time;
use tracing::{info, warn};

const DEFAULT: &str = "<default>";

pub struct App {
    is_first_update: bool,
    state: AppState,
}

enum UiSection {
    Config,
    Main,
}

struct AppState {
    section: UiSection,
    remote_node_id: Option<Result<NodeId, KeyParsingError>>,
    worker: WorkerHandle,
    our_node_id: Option<NodeId>,
    devices: callme::audio::Devices,
    audio_config: UiAudioConfig,
    video_config: VideoConfig,
    calls: BTreeMap<NodeId, CallState>,
    volumes: BTreeMap<NodeId, VolumeHandle>,
    rtts: BTreeMap<NodeId, Duration>,
    video_frames: BTreeMap<NodeId, VideoFrameState>,
    selected_video_participant: Option<NodeId>,
    sharing_active: bool,
    preview: Option<PreviewState>,
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

struct UiAudioConfig {
    selected_input: String,
    selected_output: String,
    processing_enabled: bool,
    quality: AudioQuality,
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
            ctx.set_zoom_factor(1.5);
            let ctx = ctx.clone();
            let callback = Arc::new(move || ctx.request_repaint());
            self.state.cmd(Command::SetUpdateCallback { callback });
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
        let state = AppState {
            section: UiSection::Config,
            remote_node_id: Default::default(),
            worker: handle,
            our_node_id: None,
            devices,
            audio_config: Default::default(),
            video_config: Default::default(),
            calls: Default::default(),
            volumes: Default::default(),
            rtts: Default::default(),
            video_frames: Default::default(),
            selected_video_participant: None,
            sharing_active: false,
            preview: None,
        };

        let app = App {
            state,
            is_first_update: true,
        };
        eframe::run_native("callme", options, Box::new(|_cc| Ok(Box::new(app))))
    }
}
impl AppState {
    fn update(&mut self, ctx: &egui::Context) {
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
                    let state = self
                        .video_frames
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
                    if self.selected_video_participant.is_none() {
                        self.selected_video_participant = Some(node_id);
                    }
                }
                Event::SharingToggled(active) => {
                    self.sharing_active = active;
                }
                Event::PreviewFrame {
                    width,
                    height,
                    data,
                    actual_fps,
                    encode_time_ms,
                } => {
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

        egui::CentralPanel::default().show(ctx, |ui| match self.section {
            UiSection::Config => self.ui_section_config(ui),
            UiSection::Main => self.ui_section_call(ui),
        });
    }

    fn audio_config(&self) -> AudioConfig {
        (&self.audio_config).into()
    }

    fn ui_section_call(&mut self, ui: &mut Ui) {
        ui.heading("Call a remote node");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("📋 Paste node id")
                    .on_hover_text("Click to paste")
                    .clicked()
                {
                    #[cfg(not(target_os = "android"))]
                    let pasted = {
                        arboard::Clipboard::new()
                            .expect("failed to access clipboard")
                            .get_text()
                            .expect("failed to get text from clipboard")
                    };

                    #[cfg(target_os = "android")]
                    let pasted = {
                        android_clipboard::get_text().expect("failed to get text from clipboard")
                    };

                    let node_id = NodeId::from_str(&pasted);
                    self.remote_node_id = Some(node_id);
                }
            });
            if let Some(node_id) = self.remote_node_id.as_ref() {
                ui.horizontal(|ui| match node_id {
                    Ok(node_id) => {
                        if ui.button("Call").clicked() {
                            self.cmd(Command::Call { node_id: *node_id });
                        }
                        ui.label(fmt_node_id(&node_id.fmt_short()));
                    }
                    Err(err) => {
                        ui.label(fmt_error(&format!("Invalid node id: {err}")));
                    }
                });
            }
        });

        ui.add_space(8.);
        ui.heading("Accept calls");
        if let Some(node_id) = &self.our_node_id {
            ui.horizontal(|ui| {
                ui.label("Our node id:".to_string());
                ui.label(fmt_node_id(&node_id.fmt_short()));
                if ui
                    .button("📋 Copy")
                    .on_hover_text("Click to copy")
                    .clicked()
                {
                    #[cfg(not(target_os = "android"))]
                    {
                        if let Err(err) = arboard::Clipboard::new()
                            .expect("failed to get clipboard")
                            .set_text(node_id.to_string())
                        {
                            warn!("failed to copy text to clipboard: {err}");
                        }
                    }
                    #[cfg(target_os = "android")]
                    if let Err(err) = android_clipboard::set_text(node_id.to_string()) {
                        warn!("failed to copy text to clipboard: {err}");
                    }
                }
            });
        }

        ui.add_space(8.);
        ui.heading("Screen Sharing");
        ui.horizontal(|ui| {
            if self.sharing_active {
                if ui.button("Stop Sharing").clicked() {
                    self.cmd(Command::ToggleSharing { enabled: false });
                }
            } else if ui.button("Start Sharing").clicked() {
                self.cmd(Command::ToggleSharing { enabled: true });
            }
        });

        if let Some(preview) = &mut self.preview {
            ui.label("Preview:");
            ui.horizontal(|ui| {
                ui.label(format!("{:.0}x{:.0}", preview.width, preview.height));
                ui.separator();
                ui.label(format!("{:.1} FPS", preview.actual_fps));
                ui.separator();
                ui.label(format!("encode {:.1}ms", preview.encode_time_ms));
            });
            if preview.uploaded_generation != preview.generation {
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [preview.width as usize, preview.height as usize],
                    &preview.data,
                );
                if let Some(tex) = &mut preview.texture {
                    tex.set(color_image, egui::TextureOptions::default());
                } else {
                    preview.texture = Some(ui.ctx().load_texture(
                        "preview",
                        color_image,
                        egui::TextureOptions::default(),
                    ));
                }
                preview.uploaded_generation = preview.generation;
            }
            if let Some(tex) = &preview.texture {
                let aspect = preview.width as f32 / preview.height as f32;
                let max_w = ui.available_width().min(400.0);
                ui.add(
                    egui::Image::new(tex)
                        .max_width(max_w)
                        .max_height(max_w / aspect),
                );
            }
        }

        let share_targets: Vec<NodeId> = self.video_frames.keys().copied().collect();
        if !share_targets.is_empty() {
            ui.horizontal(|ui| {
                ui.label("View:");
                let selected = self.selected_video_participant.unwrap_or(share_targets[0]);
                egui::ComboBox::from_id_salt("video_source")
                    .selected_text(
                        share_targets
                            .iter()
                            .find(|n| **n == selected)
                            .map(|n| n.fmt_short())
                            .unwrap_or_default(),
                    )
                    .show_ui(ui, |ui| {
                        for node_id in &share_targets {
                            let label = node_id.fmt_short();
                            if ui.selectable_label(selected == *node_id, &label).clicked() {
                                self.selected_video_participant = Some(*node_id);
                            }
                        }
                    });
            });

            let participant = self
                .selected_video_participant
                .unwrap_or(share_targets[0]);
            if let Some(frame) = self.video_frames.get_mut(&participant) {
                if !frame.data.is_empty() && frame.width > 0 && frame.height > 0 {
                    if frame.uploaded_generation != frame.generation {
                        let color_image = egui::ColorImage::from_rgba_unmultiplied(
                            [frame.width as usize, frame.height as usize],
                            &frame.data,
                        );
                        if let Some(tex) = &mut frame.texture {
                            tex.set(color_image, egui::TextureOptions::default());
                        } else {
                            frame.texture = Some(ui.ctx().load_texture(
                                format!("video-{}", participant),
                                color_image,
                                egui::TextureOptions::default(),
                            ));
                        }
                        frame.uploaded_generation = frame.generation;
                    }
                    if let Some(tex) = &frame.texture {
                        let aspect = frame.width as f32 / frame.height as f32;
                        let max_w = ui.available_width().min(400.0);
                        ui.add(
                            egui::Image::new(tex)
                                .max_width(max_w)
                                .max_height(max_w / aspect),
                        );
                    }
                }
            }
        } else {
            ui.label("No video streams available");
        }

        ui.add_space(8.);
        ui.heading("Active calls");
        ui.vertical(|ui| {
            for (node_id, state) in &self.calls {
                let node_id = *node_id;
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(fmt_node_id(&node_id.fmt_short()));
                        ui.label(format!("{}", state));
                        if matches!(state, CallState::Incoming) {
                            if ui.button("Accept").clicked() {
                                self.cmd(Command::HandleIncoming {
                                    node_id,
                                    accept: true,
                                });
                            }
                            if ui.button("Decline").clicked() {
                                self.cmd(Command::HandleIncoming {
                                    node_id,
                                    accept: false,
                                });
                            }
                        } else if ui.button("Drop").clicked() {
                            self.cmd(Command::Abort { node_id });
                        }
                    });
                    if matches!(state, CallState::Active) {
                        if let Some(volume) = self.volumes.get(&node_id) {
                            let mut vol = f32::from_bits(volume.load(Ordering::Relaxed));
                            ui.horizontal(|ui| {
                                ui.label("Vol:");
                                ui.add(
                                    egui::Slider::new(&mut vol, 0.0..=2.0)
                                        .text("")
                                        .fixed_decimals(1),
                                )
                                .on_hover_text("Adjust volume for this participant");
                            });
                            volume.store(vol.to_bits(), Ordering::Relaxed);
                        }
                        if let Some(rtt) = self.rtts.get(&node_id) {
                            ui.horizontal(|ui| {
                                if rtt.as_millis() < 100 {
                                    ui.colored_label(Color32::GREEN, fmt_rtt(rtt));
                                } else if rtt.as_millis() < 300 {
                                    ui.colored_label(Color32::YELLOW, fmt_rtt(rtt));
                                } else {
                                    ui.colored_label(Color32::LIGHT_RED, fmt_rtt(rtt));
                                }
                            });
                        }
                    }
                });
            }
        });
    }

    fn cmd(&self, command: Command) {
        self.worker
            .command_tx
            .send_blocking(command)
            .expect("worker thread is dead");
    }

    fn ui_section_config(&mut self, ui: &mut Ui) {
        ui.heading("Audio config");
        ui.vertical(|ui| {
            egui::ComboBox::from_label("Capture device")
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
                            .selectable_label(&self.audio_config.selected_input == device, device)
                            .clicked()
                        {
                            self.audio_config.selected_input = device.to_string()
                        }
                    }
                });

            egui::ComboBox::from_label("Playback device")
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
                            .selectable_label(&self.audio_config.selected_output == device, device)
                            .clicked()
                        {
                            self.audio_config.selected_output = device.to_string()
                        }
                    }
                });

            #[cfg(feature = "audio-processing")]
            ui.checkbox(
                &mut self.audio_config.processing_enabled,
                "Enable echo cancellation",
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
                        let label = format!("{} ({})", quality.label(), quality.bandwidth_human());
                        if ui
                            .selectable_label(self.audio_config.quality == *quality, &label)
                            .clicked()
                        {
                            self.audio_config.quality = *quality;
                        }
                    }
                });

            ui.add_space(8.);
            ui.separator();
            ui.heading("Screen Sharing Config");

            egui::ComboBox::from_label("Resolution")
                .selected_text(self.video_config.resolution.label())
                .show_ui(ui, |ui| {
                    for res in VideoResolution::all() {
                        if ui
                            .selectable_label(self.video_config.resolution == *res, res.label())
                            .clicked()
                        {
                            self.video_config.resolution = *res;
                        }
                    }
                });

            egui::ComboBox::from_label("Framerate")
                .selected_text(format!("{} fps", self.video_config.framerate))
                .show_ui(ui, |ui| {
                    for fps in [15u32, 30] {
                        if ui
                            .selectable_label(self.video_config.framerate == fps, format!("{fps} fps"))
                            .clicked()
                        {
                            self.video_config.framerate = fps;
                        }
                    }
                });

            if ui.button("Save & start").clicked() {
                let audio_config = self.audio_config();
                let video_config = self.video_config;
                self.cmd(Command::SetAudioConfig { audio_config });
                self.cmd(Command::SetVideoConfig { video_config });
                self.section = UiSection::Main;
            }
        });
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
    SharingToggled(bool),
    PreviewFrame {
        width: u32,
        height: u32,
        data: Arc<Vec<u8>>,
        actual_fps: f64,
        encode_time_ms: f64,
    },
}

#[derive(strum::Display)]
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
                self.ensure_video_streams(node_id, conn.clone()).await;
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
        self.emit(Event::VolumeHandle(node_id, volume.clone())).await?;
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
        self.emit(Event::VolumeHandle(node_id, volume.clone())).await?;
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
                            audio_context.play_track_with_volume(remote_track, volume.clone()).await?;
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
        let entry = self
            .video_peers
            .entry(node_id)
            .or_insert(VideoPeerTasks {
                send: None,
                recv: None,
            });

        let recv_dead = entry
            .recv
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true);
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
            let send_dead = entry
                .send
                .as_ref()
                .map(|h| h.is_finished())
                .unwrap_or(true);
            if send_dead {
                let send_conn = conn.clone();
                let frame_tx = self.video_frame_tx.clone();
                let keyframe_tx = self.keyframe_tx.clone();
                let nid = node_id;
                let handle = tokio::spawn(async move {
                    run_video_send(send_conn, frame_tx, keyframe_tx, nid).await;
                });
                entry.send = Some(handle);
            }
        }
    }

    async fn attach_video_to_active_calls(&mut self) {
        let active: Vec<_> = self
            .active_calls
            .iter()
            .filter_map(|(id, info)| match info {
                CallInfo::Active(conn) | CallInfo::Connecting(conn) => Some((*id, conn.clone())),
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

    fn stop_all_video_send(&mut self) {
        for tasks in self.video_peers.values_mut() {
            tasks.abort_send();
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
        self.sharing_active = true;
        info!("screen sharing started ({}x{} @ {}fps)", target_w, target_h, config.framerate);
        let event_tx = self.event_tx.clone();
        tokio::task::spawn(async move {
            let _ = event_tx.send(Event::SharingToggled(true)).await;
        });
        Ok(())
    }

    fn stop_capture(&mut self) {
        if let Some(flag) = &self.capture_stop_flag {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(handle) = self.capture_thread.take() {
            let _ = handle.join();
        }
        self.capture_stop_flag = None;
        self.sharing_active = false;
        self.stop_all_video_send();
        let event_tx = self.event_tx.clone();
        tokio::task::spawn(async move {
            let _ = event_tx.send(Event::SharingToggled(false)).await;
        });
    }

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::SetUpdateCallback { callback } => {
                self.update_callback = Some(callback);
            }
            Command::SetAudioConfig { audio_config } => {
                let audio_context = AudioContext::new(audio_config).await?;
                self.audio_context = Some(audio_context);
            }
            Command::SetVideoConfig { video_config } => {
                self.video_config = video_config;
            }
            Command::ToggleSharing { enabled } => {
                if enabled && !self.sharing_active {
                    self.start_capture()?;
                    self.attach_video_to_active_calls().await;
                } else if !enabled && self.sharing_active {
                    self.stop_capture();
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
                self.connect_tasks.spawn(async move {
                    (node_id, handler.connect(node_id).await)
                });
            }
            Command::HandleIncoming { node_id, accept } => {
                let Some(CallInfo::Incoming(conn)) = self.active_calls.remove(&node_id) else {
                    return Ok(());
                };
                if accept {
                    self.accept_from_accept(conn).await?;
                } else {
                    conn.transport().close(0u32.into(), b"bye");
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
            }
            Command::Abort { node_id } => {
                if let Some(state) = self.active_calls.remove(&node_id) {
                    self.volumes.remove(&node_id);
                    self.remove_video_peer(node_id);
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
        tokio::spawn(drain_quic_recv(recv));
        let mut rx = frame_tx.subscribe();
        let _ = keyframe_tx.send(());
        let mut sent = 0u64;
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    transport::send_frame(&mut send, &frame).await?;
                    sent += 1;
                    if sent == 1 {
                        info!(
                            "sent first video frame ({} bytes) to {}",
                            frame.len(),
                            node_id.fmt_short()
                        );
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => return Err(anyhow::anyhow!("broadcast recv: {e}")),
            }
        }
    }
    .await;
    if let Err(e) = result {
        info!("video send to {} stopped: {e:?}", node_id.fmt_short());
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
    let mut decoder = VideoDecoder::new()?;
    let mut decoded = 0u64;
    loop {
        let Some(data) = transport::recv_frame(recv).await? else {
            break;
        };
        match decoder.decode(&data) {
            Ok((rgba, w, h)) => {
                decoded += 1;
                if decoded == 1 {
                    info!(
                        "decoded first video frame from {} ({}x{})",
                        node_id.fmt_short(),
                        w,
                        h
                    );
                }
                if event_tx
                    .try_send(Event::VideoFrame {
                        node_id,
                        data: Arc::new(rgba),
                        width: w,
                        height: h,
                    })
                    .is_ok()
                {
                    if let Some(cb) = callback {
                        cb();
                    }
                }
            }
            Err(e) => {
                info!("video decode error for {}: {e:?}", node_id.fmt_short());
            }
        }
    }
    Ok(())
}
