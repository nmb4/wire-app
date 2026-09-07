//! Network, call, chat, and capture orchestration on the worker runtime.

use std::{
    collections::BTreeMap,
    sync::{atomic::AtomicU32, Arc},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use iroh::{protocol::Router, Endpoint, NodeId};
use tokio::{task::JoinSet, time};
use tracing::{debug, info, warn};
use wire::{
    audio::{AudioContext, VolumeHandle},
    rtc::{MediaTrack, RtcConnection, RtcProtocol, TrackKind},
    video::VideoConfig,
};

use crate::{
    chat,
    client_status::{
        Availability, ClientStatusProtocol, GroupCallAnnouncement, CLIENT_STATUS_ALPN,
        PRESENCE_REFRESH_INTERVAL,
    },
};

#[cfg(target_os = "windows")]
use super::trim_process_working_set;
use super::video::{
    await_video_send_stop, request_video_send_stop, run_video_recv, run_video_send,
    stop_video_send, VideoPeerTasks, VideoReceiveControl, VideoSendCommand,
};
use super::{CallState, Command, Event, UpdateCallback, WorkerHandle};

enum CallInfo {
    Calling,
    Connecting(RtcConnection),
    Incoming(RtcConnection),
    Active(RtcConnection),
}

pub(super) struct Worker {
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
    active_calls: BTreeMap<NodeId, CallInfo>,
    volumes: BTreeMap<NodeId, VolumeHandle>,
    stream_volumes: BTreeMap<NodeId, VolumeHandle>,
    update_callback: Option<UpdateCallback>,
    endpoint: Endpoint,
    handler: RtcProtocol,
    call_tasks: JoinSet<(NodeId, u64, Result<()>)>,
    connect_tasks: JoinSet<(NodeId, u64, Result<RtcConnection>)>,
    track_tasks: JoinSet<(NodeId, u64, Result<MediaTrack>)>,
    incoming_tasks: JoinSet<(NodeId, u64)>,
    connect_aborts: BTreeMap<NodeId, (u64, tokio::task::AbortHandle)>,
    call_generations: BTreeMap<NodeId, u64>,
    next_call_generation: u64,
    _router: Router,
    audio_context: Option<AudioContext>,
    presence_interval: time::Interval,
    video_config: VideoConfig,
    video_frame_tx: tokio::sync::broadcast::Sender<Arc<wire::video::transport::EncodedVideoFrame>>,
    keyframe_tx: tokio::sync::broadcast::Sender<()>,
    video_peers: BTreeMap<NodeId, VideoPeerTasks>,
    capture_thread: Option<std::thread::JoinHandle<()>>,
    capture_stop_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    capture_preview_task: Option<tokio::task::JoinHandle<()>>,
    capture_failure_task: Option<tokio::task::JoinHandle<()>>,
    capture_failure_tx: async_channel::Sender<String>,
    capture_failure_rx: async_channel::Receiver<String>,
    capture_idle_trim_task: Option<tokio::task::JoinHandle<()>>,
    sharing_active: bool,
    system_audio: Option<crate::system_audio::SystemAudioShare>,
    capture_target: Option<crate::screen_capture::CaptureTarget>,
    muted: bool,
    deafened: bool,
    chat: chat::ChatService,
    client_status: ClientStatusProtocol,
    local_group_call: Option<GroupCallAnnouncement>,
    peer_group_calls: BTreeMap<NodeId, Vec<GroupCallAnnouncement>>,
}

impl Worker {
    fn begin_call(&mut self, node_id: NodeId) -> u64 {
        self.next_call_generation = self.next_call_generation.wrapping_add(1).max(1);
        let generation = self.next_call_generation;
        self.call_generations.insert(node_id, generation);
        generation
    }

    fn call_is_current(&self, node_id: NodeId, generation: u64) -> bool {
        self.call_generations.get(&node_id) == Some(&generation)
    }

    pub(super) fn spawn() -> WorkerHandle {
        // UI actions must never block the egui thread while the worker is busy
        // stopping capture, dialing, or reconciling chat state.
        let (command_tx, command_rx) = async_channel::unbounded();
        let (event_tx, event_rx) = async_channel::bounded(64);
        let thread = std::thread::spawn(move || {
            info!("Wire worker thread starting");
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let detail = format!("Could not start the background runtime: {error}");
                    warn!("{detail}");
                    let _ = event_tx.send_blocking(Event::WorkerFailed(detail));
                    return;
                }
            };
            rt.block_on(async move {
                let mut worker = match Worker::start(event_tx.clone(), command_rx).await {
                    Ok(worker) => worker,
                    Err(error) => {
                        let detail = format!("Could not start Wire networking: {error:#}");
                        warn!("worker failed to start: {error:#}");
                        let _ = event_tx.send(Event::WorkerFailed(detail)).await;
                        return;
                    }
                };
                if let Err(err) = worker.run().await {
                    warn!("worker stopped with error: {err:?}");
                    let detail = format!("Wire networking stopped: {err:#}");
                    let _ = worker.emit(Event::WorkerFailed(detail)).await;
                }
            });
        });
        WorkerHandle {
            event_rx,
            command_tx,
            thread: Some(thread),
        }
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
        info!("binding Wire networking endpoint");
        let endpoint = wire::net::bind_endpoint_with_alpns([
            iroh_blobs::ALPN.to_vec(),
            iroh_docs::ALPN.to_vec(),
            iroh_gossip::ALPN.to_vec(),
            chat::CHAT_ALPN.to_vec(),
            CLIENT_STATUS_ALPN.to_vec(),
            wire::remote_logs::LOGS_ALPN.to_vec(),
        ])
        .await?;
        info!(node = %endpoint.node_id().fmt_short(), "Wire endpoint bound; opening chat storage");
        let handler = RtcProtocol::new(endpoint.clone());
        let logs_protocol = wire::remote_logs::LogsProtocol::new(endpoint.node_id());
        let client_status = ClientStatusProtocol::new(endpoint.clone());
        let config_dir = wire::net::config_dir().context("missing Wire config directory")?;
        let chat_protocols = chat::ChatService::build(endpoint.clone(), &config_dir).await?;
        info!("chat storage opened; starting protocol router");
        let _router = Router::builder(endpoint.clone())
            .accept(RtcProtocol::ALPN, handler.clone())
            .accept(iroh_blobs::ALPN, chat_protocols.blobs.clone())
            .accept(iroh_docs::ALPN, chat_protocols.docs.clone())
            .accept(iroh_gossip::ALPN, chat_protocols.gossip.clone())
            .accept(chat::CHAT_ALPN, chat_protocols.invites.clone())
            .accept(CLIENT_STATUS_ALPN, client_status.clone())
            .accept(wire::remote_logs::LOGS_ALPN, logs_protocol)
            .spawn()
            .await?;
        info!("Wire protocol router started");
        let (video_frame_tx, _) = tokio::sync::broadcast::channel(32);
        let (keyframe_tx, _) = tokio::sync::broadcast::channel(16);
        let (capture_failure_tx, capture_failure_rx) = async_channel::unbounded();
        Ok(Self {
            command_rx,
            event_tx,
            active_calls: Default::default(),
            volumes: Default::default(),
            stream_volumes: Default::default(),
            call_tasks: JoinSet::new(),
            connect_tasks: JoinSet::new(),
            track_tasks: JoinSet::new(),
            incoming_tasks: JoinSet::new(),
            connect_aborts: BTreeMap::new(),
            call_generations: BTreeMap::new(),
            next_call_generation: 0,
            local_group_call: None,
            peer_group_calls: BTreeMap::new(),
            endpoint,
            handler,
            _router,
            audio_context: None,
            update_callback: None,
            presence_interval: time::interval(PRESENCE_REFRESH_INTERVAL),
            video_config: VideoConfig::default(),
            video_frame_tx,
            keyframe_tx,
            video_peers: Default::default(),
            capture_thread: None,
            capture_stop_flag: None,
            capture_preview_task: None,
            capture_failure_task: None,
            capture_failure_tx,
            capture_failure_rx,
            capture_idle_trim_task: None,
            sharing_active: false,
            system_audio: None,
            capture_target: None,
            muted: false,
            deafened: false,
            chat: chat_protocols.service,
            client_status,
        })
    }

    async fn run(&mut self) -> Result<()> {
        self.emit(Event::EndpointBound(self.endpoint.node_id()))
            .await?;
        let mut initial_chat_loaded = false;
        loop {
            if let Some(notification) = self.chat.pop_notification() {
                self.emit(Event::Chat(notification)).await?;
                continue;
            }
            if !initial_chat_loaded {
                initial_chat_loaded = true;
                self.emit(Event::InitialChatLoaded).await?;
                continue;
            }
            tokio::select! {
                command = self.command_rx.recv() => {
                    let Ok(command) = command else {
                        info!("app command channel closed; stopping worker");
                        self.client_status.broadcast_offline().await;
                        self.close_active_call_transports();
                        if self.sharing_active {
                            self.stop_capture().await;
                        }
                        break;
                    };
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
                Some(joined) = self.call_tasks.join_next(), if !self.call_tasks.is_empty() => {
                    let Ok((node_id, generation, res)) = joined else {
                        warn!("call task was cancelled or panicked");
                        continue;
                    };
                    if !self.call_is_current(node_id, generation) {
                        debug!(node = %node_id.fmt_short(), generation, "ignored stale call completion");
                        continue;
                    }
                    if let Err(err) = res {
                        warn!("connection with {} closed: {err:?}", node_id.fmt_short());
                    } else {
                        info!("connection with {} closed", node_id.fmt_short());
                    }
                    self.call_generations.remove(&node_id);
                    self.active_calls.remove(&node_id);
                    self.volumes.remove(&node_id);
                    self.stream_volumes.remove(&node_id);
                    self.remove_video_peer(node_id).await;
                    self.cleanup_after_call_end().await;
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
                Some(joined) = self.connect_tasks.join_next(), if !self.connect_tasks.is_empty() => {
                    let Ok((node_id, generation, res)) = joined else {
                        warn!("connect task was cancelled or panicked");
                        continue;
                    };
                    if self.connect_aborts.get(&node_id).is_some_and(|(current, _)| *current == generation) {
                        self.connect_aborts.remove(&node_id);
                    }
                    self.handle_quic_connected(node_id, generation, res).await?;
                }
                Some(joined) = self.track_tasks.join_next(), if !self.track_tasks.is_empty() => {
                    let Ok((node_id, generation, res)) = joined else {
                        warn!("track task was cancelled or panicked");
                        continue;
                    };
                    self.handle_track_received(node_id, generation, res).await?;
                }
                Some(joined) = self.incoming_tasks.join_next(), if !self.incoming_tasks.is_empty() => {
                    let Ok((node_id, generation)) = joined else {
                        continue;
                    };
                    if self.call_is_current(node_id, generation)
                        && matches!(self.active_calls.get(&node_id), Some(CallInfo::Incoming(_)))
                    {
                        info!(node = %node_id.fmt_short(), generation, "incoming caller disconnected before acceptance");
                        self.call_generations.remove(&node_id);
                        self.active_calls.remove(&node_id);
                        self.emit(Event::SetCallState(node_id, CallState::Aborted)).await?;
                    }
                }
                _ = self.presence_interval.tick() => {
                    self.client_status.refresh_allowed_peers();
                }
                input = self.chat.wait_input() => {
                    if let Some(notification) = self.chat.process_input(input).await {
                        self.emit(Event::Chat(notification)).await?;
                    }
                }
                status = self.client_status.next_update() => {
                    let status = status?;
                    if matches!(status.availability, Availability::Online) {
                        self.peer_group_calls
                            .insert(status.peer, status.active_group_calls.clone());
                    } else {
                        self.peer_group_calls.remove(&status.peer);
                    }
                    self.emit(Event::ClientStatus(status)).await?;
                }
                failure = self.capture_failure_rx.recv() => {
                    if let Ok(message) = failure {
                        self.handle_capture_failure(message).await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_incoming(&mut self, conn: RtcConnection) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        if self.active_calls.contains_key(&node_id) {
            info!(node = %node_id.fmt_short(), "rejecting duplicate incoming call");
            conn.transport().close(1u32.into(), b"call already active");
            return Ok(());
        }
        let generation = self.begin_call(node_id);
        let same_group_room = self.local_group_call.as_ref().is_some_and(|local| {
            self.peer_group_calls.get(&node_id).is_some_and(|calls| {
                calls
                    .iter()
                    .any(|remote| remote.call_id == local.call_id && remote.ended_at_ms.is_none())
            })
        });
        if same_group_room {
            info!(
                node = %node_id.fmt_short(),
                "automatically accepting participant joining our group call"
            );
            self.accept_from_accept(conn, generation).await?;
            return Ok(());
        }
        info!("incoming connection from {}", node_id.fmt_short());
        let monitor = conn.clone();
        self.incoming_tasks.spawn(async move {
            let _ = monitor.transport().closed().await;
            (node_id, generation)
        });
        self.active_calls.insert(node_id, CallInfo::Incoming(conn));
        self.emit(Event::SetCallState(node_id, CallState::Incoming))
            .await?;
        Ok(())
    }

    async fn handle_quic_connected(
        &mut self,
        node_id: NodeId,
        generation: u64,
        conn: Result<RtcConnection>,
    ) -> Result<()> {
        if !self.call_is_current(node_id, generation)
            || !matches!(self.active_calls.get(&node_id), Some(CallInfo::Calling))
        {
            if let Ok(conn) = conn {
                conn.transport().close(0u32.into(), b"stale call attempt");
            }
            debug!(node = %node_id.fmt_short(), generation, "ignored stale connect completion");
            return Ok(());
        }
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
                    (node_id, generation, res)
                });
            }
            Err(err) => {
                warn!("connection to {} failed: {err:?}", node_id.fmt_short());
                self.active_calls.remove(&node_id);
                self.call_generations.remove(&node_id);
                self.volumes.remove(&node_id);
                self.stream_volumes.remove(&node_id);
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
        generation: u64,
        track: Result<MediaTrack>,
    ) -> Result<()> {
        if !self.call_is_current(node_id, generation) {
            debug!(node = %node_id.fmt_short(), generation, "ignored stale track completion");
            return Ok(());
        }
        let Some(CallInfo::Connecting(conn)) = self.active_calls.remove(&node_id) else {
            return Ok(());
        };
        match track {
            Ok(track) => self.accept_from_connect(conn, track, generation).await?,
            Err(err) => {
                warn!(
                    "failed to receive audio track from {}: {err:?}",
                    node_id.fmt_short()
                );
                self.remove_video_peer(node_id).await;
                self.cleanup_after_call_end().await;
                conn.transport().close(0u32.into(), b"bye");
                self.call_generations.remove(&node_id);
                self.emit(Event::SetCallState(node_id, CallState::Aborted))
                    .await?;
            }
        }
        Ok(())
    }

    async fn accept_from_connect(
        &mut self,
        conn: RtcConnection,
        track: MediaTrack,
        generation: u64,
    ) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let stream_volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        self.volumes.insert(node_id, volume.clone());
        self.stream_volumes.insert(node_id, stream_volume.clone());
        self.emit(Event::ParticipantAudioHandles {
            node_id,
            volume: volume.clone(),
            stream_volume: stream_volume.clone(),
            level: level.clone(),
        })
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
        let system_audio_track = self.system_audio.as_ref().map(|share| share.track());

        let audio_conn = conn.clone();
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());

            let fut = async {
                audio_context
                    .play_track_with_volume_and_level(track, volume.clone(), level.clone())
                    .await?;
                let capture_track = audio_context.capture_track().await?;
                audio_conn.send_track(capture_track).await?;
                if let Some(system_audio_track) = system_audio_track {
                    audio_conn.send_track(system_audio_track).await?;
                }
                while let Some(remote_track) = audio_conn.recv_track().await? {
                    info!(
                        "new remote track: {:?} {:?}",
                        remote_track.kind(),
                        remote_track.codec()
                    );
                    match remote_track.kind() {
                        TrackKind::Audio => {
                            audio_context
                                .play_track_with_volume(remote_track, stream_volume.clone())
                                .await?;
                        }
                        TrackKind::Video => {
                            warn!(
                                node = %node_id.fmt_short(),
                                "ignored unexpected RTC video track"
                            );
                        }
                    }
                }
                anyhow::Ok(())
            };
            let res = fut.await;
            info!("connection with {} closed: {:?}", node_id.fmt_short(), res);
            (node_id, generation, res)
        });
        Ok(())
    }

    async fn accept_from_accept(&mut self, conn: RtcConnection, generation: u64) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let stream_volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        self.volumes.insert(node_id, volume.clone());
        self.stream_volumes.insert(node_id, stream_volume.clone());
        self.emit(Event::ParticipantAudioHandles {
            node_id,
            volume: volume.clone(),
            stream_volume: stream_volume.clone(),
            level: level.clone(),
        })
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
        let system_audio_track = self.system_audio.as_ref().map(|share| share.track());

        let audio_conn = conn.clone();
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());

            let fut = async {
                let capture_track = audio_context.capture_track().await?;
                audio_conn.send_track(capture_track).await?;
                if let Some(system_audio_track) = system_audio_track {
                    audio_conn.send_track(system_audio_track).await?;
                }
                info!("added capture track to rtc connection");
                let mut first_audio = true;
                while let Some(remote_track) = audio_conn.recv_track().await? {
                    info!(
                        "new remote track: {:?} {:?}",
                        remote_track.kind(),
                        remote_track.codec()
                    );
                    match remote_track.kind() {
                        TrackKind::Audio => {
                            if first_audio {
                                first_audio = false;
                                audio_context
                                    .play_track_with_volume_and_level(
                                        remote_track,
                                        volume.clone(),
                                        level.clone(),
                                    )
                                    .await?;
                            } else {
                                audio_context
                                    .play_track_with_volume(remote_track, stream_volume.clone())
                                    .await?;
                            }
                        }
                        // Video uses the dedicated framed transport below. A
                        // malformed/older peer must not crash the call worker by
                        // advertising video as a generic RTC media track.
                        TrackKind::Video => {
                            warn!(node = %node_id.fmt_short(), "ignored unexpected RTC video track");
                        }
                    }
                }
                anyhow::Ok(())
            };
            let res = fut.await;
            info!("connection with {} closed: {:?}", node_id.fmt_short(), res);
            (node_id, generation, res)
        });
        Ok(())
    }

    async fn ensure_video_streams(&mut self, node_id: NodeId, conn: RtcConnection) {
        if !matches!(self.active_calls.get(&node_id), Some(CallInfo::Active(_))) {
            return;
        }

        self.video_peers
            .entry(node_id)
            .or_insert_with(VideoPeerTasks::new);

        let recv_dead = self
            .video_peers
            .get(&node_id)
            .and_then(|tasks| tasks.recv.as_ref())
            .map(|h| h.is_finished())
            .unwrap_or(true);
        if recv_dead {
            let recv_conn = conn.clone();
            let event_tx = self.event_tx.clone();
            let callback = self.update_callback.clone();
            let nid = node_id;
            let (recv_control_tx, recv_control_rx) = tokio::sync::mpsc::unbounded_channel();
            let handle = tokio::spawn(async move {
                run_video_recv(recv_conn, nid, event_tx, callback, recv_control_rx).await;
            });
            let entry = self
                .video_peers
                .get_mut(&node_id)
                .expect("video peer was initialized");
            entry.recv = Some(handle);
            entry.recv_control = Some(recv_control_tx);
        }

        if self.sharing_active {
            let (old_stop, old_send) = {
                let entry = self
                    .video_peers
                    .get_mut(&node_id)
                    .expect("video peer was initialized");
                (entry.send_stop.take(), entry.send.take())
            };
            stop_video_send(node_id, old_stop, old_send, VideoSendCommand::Replace).await;
            let send_conn = conn.clone();
            let frame_tx = self.video_frame_tx.clone();
            let keyframe_tx = self.keyframe_tx.clone();
            let nid = node_id;
            let (stop_tx, stop_rx) = tokio::sync::watch::channel(VideoSendCommand::Running);
            info!("starting video send task for {}", node_id.fmt_short());
            let handle = tokio::spawn(async move {
                run_video_send(send_conn, frame_tx, keyframe_tx, nid, stop_rx).await;
            });
            let entry = self
                .video_peers
                .get_mut(&node_id)
                .expect("video peer was initialized");
            entry.send = Some(handle);
            entry.send_stop = Some(stop_tx);
        }
    }

    async fn finish_all_video_send(&mut self) {
        let tasks: Vec<_> = self
            .video_peers
            .iter_mut()
            .map(|(node_id, tasks)| (*node_id, tasks.send_stop.take(), tasks.send.take()))
            .collect();
        for (node_id, stop, _) in &tasks {
            request_video_send_stop(*node_id, stop.as_ref(), VideoSendCommand::Finish);
        }
        for (node_id, _, send) in tasks {
            await_video_send_stop(node_id, send).await;
        }
    }

    async fn start_system_audio(&mut self) {
        if self.system_audio.is_some() {
            return;
        }
        match crate::system_audio::SystemAudioShare::start() {
            Ok(share) => {
                if let Err(error) = self.send_system_audio_track(&share.track()).await {
                    warn!("could not send system audio to active calls: {error:#}");
                    self.emit(Event::SystemAudioFailed(error.to_string()))
                        .await
                        .ok();
                    return;
                }
                self.system_audio = Some(share);
            }
            Err(error) => {
                warn!("system audio capture failed to start: {error:#}");
                self.emit(Event::SystemAudioFailed(error.to_string()))
                    .await
                    .ok();
            }
        }
    }

    async fn send_system_audio_track(&self, track: &MediaTrack) -> Result<()> {
        let active: Vec<_> = self
            .active_calls
            .values()
            .filter_map(|info| match info {
                CallInfo::Active(conn) => Some(conn.clone()),
                _ => None,
            })
            .collect();
        for conn in active {
            conn.send_track(track.clone()).await?;
        }
        Ok(())
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

    async fn remove_video_peer(&mut self, node_id: NodeId) {
        if let Some(mut tasks) = self.video_peers.remove(&node_id) {
            stop_video_send(
                node_id,
                tasks.send_stop.take(),
                tasks.send.take(),
                VideoSendCommand::Finish,
            )
            .await;
            if let Some(recv) = tasks.recv.take() {
                recv.abort();
                let _ = recv.await;
            }
        }
    }

    async fn cleanup_after_call_end(&mut self) {
        if self.active_calls.is_empty() && self.sharing_active {
            self.stop_capture().await;
        }
    }

    fn close_active_call_transports(&self) {
        for (node_id, call) in &self.active_calls {
            let connection = match call {
                CallInfo::Connecting(connection)
                | CallInfo::Incoming(connection)
                | CallInfo::Active(connection) => Some(connection),
                CallInfo::Calling => None,
            };
            if let Some(connection) = connection {
                info!(
                    node = %node_id.fmt_short(),
                    "closing active call transport during app shutdown"
                );
                connection
                    .transport()
                    .close(0u32.into(), b"wire app shutdown");
            }
        }
    }

    fn start_capture(&mut self) -> Result<()> {
        if let Some(trim_task) = self.capture_idle_trim_task.take() {
            trim_task.abort();
        }
        let config = self.video_config;
        let target_w = config.resolution.width();
        let target_h = config.resolution.height();

        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (preview_tx, preview_rx) =
            async_channel::bounded::<crate::screen_capture::PreviewUpdate>(4);
        let event_tx = self.event_tx.clone();
        let callback = self.update_callback.clone();
        if let Some(stale_task) = self.capture_preview_task.take() {
            stale_task.abort();
        }

        let pipeline = crate::screen_capture::start(
            config,
            self.capture_target.clone(),
            stop_flag.clone(),
            self.video_frame_tx.clone(),
            preview_tx,
            self.keyframe_tx.clone(),
        )?;

        self.capture_thread = Some(pipeline.thread);
        self.capture_stop_flag = Some(stop_flag);
        if let Some(stale_task) = self.capture_failure_task.take() {
            stale_task.abort();
        }
        let failure_tx = self.capture_failure_tx.clone();
        self.capture_failure_task = Some(tokio::spawn(async move {
            if let Ok(message) = pipeline.failure_rx.recv().await {
                let _ = failure_tx.send(message).await;
            }
        }));
        self.capture_preview_task = Some(tokio::task::spawn(async move {
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
        }));
        info!(
            "screen capture started ({}x{} @ {}fps, {} kbps, {} active call(s))",
            target_w,
            target_h,
            config.framerate,
            config.effective_bitrate() / 1000,
            self.video_peers.len()
        );
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

    async fn stop_capture_pipeline(&mut self) {
        self.stop_capture_thread();
        if let Some(preview_task) = self.capture_preview_task.take() {
            preview_task.abort();
            let _ = preview_task.await;
        }
        if let Some(failure_task) = self.capture_failure_task.take() {
            failure_task.abort();
            let _ = failure_task.await;
        }
        info!("screen capture pipeline fully stopped");
    }

    async fn handle_capture_failure(&mut self, message: String) {
        if !self.sharing_active {
            return;
        }
        warn!("screen capture pipeline failed after startup: {message}");
        self.sharing_active = false;
        self.system_audio = None;
        self.capture_target = None;
        self.finish_all_video_send().await;
        self.stop_capture_pipeline().await;
        self.schedule_idle_working_set_trim();
        let _ = self.emit(Event::SharingFailed(message)).await;
    }

    async fn stop_capture(&mut self) {
        self.sharing_active = false;
        self.system_audio = None;
        self.capture_target = None;
        self.finish_all_video_send().await;
        let _ = self
            .emit(Event::SharingToggled {
                active: false,
                system_audio: false,
            })
            .await;
        self.stop_capture_pipeline().await;
        self.schedule_idle_working_set_trim();
    }

    fn schedule_idle_working_set_trim(&mut self) {
        const IDLE_TRIM_DELAY: Duration = Duration::from_secs(5);

        if let Some(trim_task) = self.capture_idle_trim_task.take() {
            trim_task.abort();
        }
        self.capture_idle_trim_task = Some(tokio::spawn(async move {
            tokio::time::sleep(IDLE_TRIM_DELAY).await;
            #[cfg(target_os = "windows")]
            match trim_process_working_set() {
                Ok(()) => info!(
                    "released inactive screen-sharing pages after {:?} idle",
                    IDLE_TRIM_DELAY
                ),
                Err(error) => warn!("could not release inactive screen-sharing pages: {error:#}"),
            }
        }));
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
                self.emit(Event::LocalAudioLevel(audio_context.capture_level()))
                    .await?;
                self.audio_context = Some(audio_context);
            }
            Command::SetVideoConfig { video_config } => {
                let restart_capture = self.sharing_active;
                if restart_capture {
                    self.stop_capture_pipeline().await;
                }
                self.video_config = video_config;
                if restart_capture {
                    if let Err(error) = self.start_capture() {
                        warn!("screen capture restart failed: {error:#}");
                        self.sharing_active = false;
                        self.system_audio = None;
                        self.capture_target = None;
                        self.finish_all_video_send().await;
                        self.emit(Event::SharingFailed(error.to_string())).await?;
                    }
                }
            }
            Command::ToggleSharing {
                enabled,
                target,
                share_system_audio,
            } => {
                if enabled && !self.sharing_active {
                    if let Err(error) = crate::screen_capture::ensure_capture_permission() {
                        warn!("screen sharing permission unavailable: {error:#}");
                        self.emit(Event::SharingFailed(error.to_string())).await?;
                        return Ok(());
                    }
                    self.capture_target = target;
                    self.sharing_active = true;
                    if let Err(error) = self.start_capture() {
                        warn!("screen capture failed to start: {error:#}");
                        self.sharing_active = false;
                        self.system_audio = None;
                        self.capture_target = None;
                        self.emit(Event::SharingFailed(error.to_string())).await?;
                        return Ok(());
                    }
                    let _ = self.keyframe_tx.send(());
                    self.attach_video_to_active_calls().await;
                    if share_system_audio {
                        self.start_system_audio().await;
                    } else {
                        self.system_audio = None;
                    }
                    self.emit(Event::SharingToggled {
                        active: true,
                        system_audio: self.system_audio.is_some(),
                    })
                    .await?;
                } else if !enabled && self.sharing_active {
                    self.stop_capture().await;
                }
            }
            Command::SetSystemAudio { enabled } => {
                if !self.sharing_active {
                    self.system_audio = None;
                    self.emit(Event::SystemAudioToggled(false)).await?;
                    return Ok(());
                }
                if enabled {
                    if self.system_audio.is_none() {
                        self.start_system_audio().await;
                    }
                    self.emit(Event::SystemAudioToggled(self.system_audio.is_some()))
                        .await?;
                } else if self.system_audio.take().is_some() {
                    self.emit(Event::SystemAudioToggled(false)).await?;
                }
            }
            Command::SetWatching {
                node_id,
                generation,
                watching,
            } => {
                if let Some(control) = self
                    .video_peers
                    .get(&node_id)
                    .and_then(|tasks| tasks.recv_control.as_ref())
                {
                    let _ = control.send(VideoReceiveControl {
                        generation,
                        watching,
                    });
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
            Command::EnsureDirectChat { peer, title } => {
                self.chat.ensure_direct(peer, title).await?;
            }
            Command::CreateGroupChat { title, members } => {
                self.chat.create_group(title, members).await?;
            }
            Command::SendChatMessage {
                conversation_id,
                message,
            } => {
                self.chat.send_message(conversation_id, message).await;
            }
            Command::LoadChatAttachment {
                conversation_id,
                hash,
                byte_len,
            } => {
                if let Err(error) = self
                    .chat
                    .load_attachment_data(&conversation_id, &hash, byte_len)
                    .await
                {
                    warn!("could not load chat attachment {hash}: {error:#}");
                }
            }
            Command::LoadOlderChatMessages { conversation_id } => {
                if let Err(error) = self.chat.load_older_messages(&conversation_id).await {
                    warn!("could not load older chat messages: {error:#}");
                }
            }
            Command::SetMaxImageBytes { max_image_bytes } => {
                self.chat.set_max_image_bytes(max_image_bytes);
            }
            Command::SetChatRetention { retention } => {
                if let Err(error) = self.chat.set_retention_policy(retention).await {
                    warn!("could not apply chat retention policy: {error:#}");
                }
            }
            Command::SetFriends { friends } => {
                self.client_status.replace_peers(friends.clone());
                self.client_status.announce_online(friends);
            }
            Command::DeleteChatMessage {
                conversation_id,
                message_id,
                scope,
            } => {
                self.chat
                    .delete_message(conversation_id, message_id, scope)
                    .await;
            }
            Command::RestoreChatMessage {
                conversation_id,
                message_id,
            } => {
                self.chat.restore_message(conversation_id, message_id).await;
            }
            Command::ClearChatHistory { conversation_id } => {
                self.chat.clear_history(conversation_id).await;
            }
            Command::Call { node_id } => {
                if self.active_calls.contains_key(&node_id) {
                    return Ok(());
                }
                let generation = self.begin_call(node_id);
                self.active_calls.insert(node_id, CallInfo::Calling);
                self.emit(Event::SetCallState(node_id, CallState::Calling))
                    .await?;

                let handler = self.handler.clone();
                let abort = self
                    .connect_tasks
                    .spawn(async move { (node_id, generation, handler.connect(node_id).await) });
                self.connect_aborts.insert(node_id, (generation, abort));
            }
            Command::EnterGroupCall {
                call,
                targets,
                notify,
            } => {
                self.local_group_call = Some(call.clone());
                self.client_status
                    .set_active_group_calls(vec![call.clone()]);
                // This immediate beat is the ring/catch-up signal. The regular
                // heartbeat keeps the room discoverable for members who start
                // later and withdraws it when we leave.
                self.client_status.announce_online(notify);
                for node_id in targets {
                    if self.active_calls.contains_key(&node_id) {
                        continue;
                    }
                    let generation = self.begin_call(node_id);
                    self.active_calls.insert(node_id, CallInfo::Calling);
                    self.emit(Event::SetCallState(node_id, CallState::Calling))
                        .await?;
                    let handler = self.handler.clone();
                    let abort = self.connect_tasks.spawn(async move {
                        // Give the lightweight room advertisement a short head
                        // start so peers already in this call can recognize and
                        // auto-accept a Join instead of showing another ring.
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        (node_id, generation, handler.connect(node_id).await)
                    });
                    self.connect_aborts.insert(node_id, (generation, abort));
                }
            }
            Command::LeaveGroupCall { notify } => {
                let recently_ended = self.local_group_call.take().and_then(|mut call| {
                    if call.initiator != self.endpoint.node_id().to_string() {
                        return None;
                    }
                    for report in self.peer_group_calls.values().flatten() {
                        if report.call_id == call.call_id {
                            call.participants
                                .extend(report.participants.iter().cloned());
                        }
                    }
                    call.participants.sort();
                    call.participants.dedup();
                    call.ended_at_ms = Some(chat::now_millis());
                    Some(call)
                });
                self.client_status
                    .set_active_group_calls(recently_ended.into_iter().collect());
                self.client_status.announce_online(notify);
            }
            Command::HandleIncoming { node_id, accept } => {
                let Some(CallInfo::Incoming(conn)) = self.active_calls.remove(&node_id) else {
                    return Ok(());
                };
                let Some(generation) = self.call_generations.get(&node_id).copied() else {
                    conn.transport().close(0u32.into(), b"stale incoming call");
                    return Ok(());
                };
                if accept {
                    if self.local_group_call.is_none() {
                        if let Some(mut call) = self
                            .peer_group_calls
                            .get(&node_id)
                            .and_then(|calls| {
                                calls
                                    .iter()
                                    .filter(|call| call.ended_at_ms.is_none())
                                    .max_by_key(|call| call.started_at_ms)
                            })
                            .cloned()
                        {
                            let us = self.endpoint.node_id().to_string();
                            if !call.participants.contains(&us) {
                                call.participants.push(us);
                                call.participants.sort();
                                call.participants.dedup();
                            }
                            self.local_group_call = Some(call.clone());
                            self.client_status
                                .set_active_group_calls(vec![call.clone()]);
                            self.client_status.refresh_allowed_peers();
                            self.emit(Event::GroupCallEntered(call)).await?;
                        }
                    }
                    self.accept_from_accept(conn, generation).await?;
                } else {
                    self.call_generations.remove(&node_id);
                    self.remove_video_peer(node_id).await;
                    conn.transport().close(0u32.into(), b"bye");
                    self.cleanup_after_call_end().await;
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
            }
            Command::Abort { node_id } => {
                if let Some(state) = self.active_calls.remove(&node_id) {
                    let generation = self.call_generations.remove(&node_id);
                    if let Some((task_generation, abort)) = self.connect_aborts.remove(&node_id) {
                        if generation == Some(task_generation) {
                            abort.abort();
                        }
                    }
                    self.volumes.remove(&node_id);
                    self.stream_volumes.remove(&node_id);
                    self.remove_video_peer(node_id).await;
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
