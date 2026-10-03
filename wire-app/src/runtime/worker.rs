//! Network, call, chat, and capture orchestration on the worker runtime.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{atomic::AtomicU32, Arc},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use async_channel::Receiver;
use iroh::{protocol::Router, Endpoint, NodeId};
use tokio::{task::JoinSet, time};
use tracing::{debug, info, warn};
use wire::{
    audio::{AudioContext, VolumeHandle},
    rtc::{MediaTrack, RtcConnection, RtcProtocol, TrackKind},
    video::VideoConfig,
};

#[cfg(target_os = "windows")]
use super::trim_process_working_set;
use super::{
    video::{
        await_video_send_stop, request_video_send_stop, run_video_recv, run_video_send,
        stop_video_send, VideoPeerTasks, VideoReceiveControl, VideoSendCommand,
    },
    CallState, Command, Event, EventPublisher, ServiceClient, WorkerHandle,
};
use crate::{
    chat,
    client_status::{
        AllowedPeers, Availability, ClientStatusProtocol, GroupCallAnnouncement,
        CLIENT_STATUS_ALPN, PRESENCE_REFRESH_INTERVAL,
    },
    profile::{self, FetchedProfile, ProfileProtocol, ServedProfile, PROFILE_ALPN},
};

enum CallInfo {
    Calling,
    Connecting(RtcConnection),
    Incoming(RtcConnection),
    Active(RtcConnection),
}

/// Transfer a peer's executable into the staging file, then verify it.
///
/// Verification runs on its own thread: hashing a full release binary on a
/// runtime worker would stall every other task, including audio and video.
#[cfg(windows)]
async fn run_peer_update_download(
    endpoint: Endpoint,
    peer: NodeId,
    offer: crate::peer_update::UpdateOffer,
    event_tx: EventPublisher,
) -> Result<()> {
    let staged = crate::update::StagedUpdate::plan()?;
    let destination = staged.path().to_path_buf();
    let expected_sha256 = offer.sha256.clone();

    let received =
        crate::peer_update::download_update(&endpoint, peer, &destination, |received, total| {
            event_tx.publish(Event::PeerUpdateProgress {
                peer,
                received,
                total,
            });
        })
        .await;

    // A half-written executable next to the running one is worse than none: a
    // later attempt or a stray restart could treat it as installable. Drop it on
    // any failure, including a checksum mismatch.
    let received = match received {
        Ok(offer) => offer,
        Err(error) => {
            let _ = std::fs::remove_file(&destination);
            return Err(error);
        }
    };

    let verify_target = staged.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || verify_target.verify(&expected_sha256))
        .await
        .context("the update verification task did not finish")?
    {
        let _ = std::fs::remove_file(&destination);
        return Err(error);
    }

    // Deliberately the only gap in the chain: between this verification and the
    // relaunch helper, the staged file sits on local disk for as long as the
    // user takes to read the prompt. Closing that fully would mean holding a lock
    // across the handover; anyone who can write to the install directory could
    // replace the executable directly anyway.

    info!(
        peer = %peer.fmt_short(),
        version = %offer.version,
        "verified a peer-to-peer update and staged it for install"
    );
    // `EventPublisher::send` cannot report a gone presenter, so a staged file
    // nobody will install is reaped by the startup sweep instead.
    event_tx
        .send(Event::PeerUpdateReady {
            peer,
            version: received.version,
            staged,
        })
        .await;
    Ok(())
}

pub(super) struct Worker {
    command_rx: Receiver<Command>,
    event_tx: EventPublisher,
    active_calls: BTreeMap<NodeId, CallInfo>,
    volumes: BTreeMap<NodeId, VolumeHandle>,
    stream_volumes: BTreeMap<NodeId, VolumeHandle>,
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
    /// Shared contact gate for presence and executable transfer.
    allowed_peers: AllowedPeers,
    profile_protocol: ProfileProtocol,
    profile_fetches: JoinSet<(NodeId, Result<FetchedProfile>)>,
    /// Peer-to-peer update transfers, one task per in-flight download. Always
    /// present so the select loop does not need a platform-specific arm; only
    /// Windows ever spawns into it.
    peer_updates: JoinSet<(NodeId, Result<()>)>,
    /// Peers with a transfer in flight, so a repeated click cannot start a
    /// second download writing to the same staging file.
    pending_peer_updates: BTreeSet<NodeId>,
    local_group_call: Option<GroupCallAnnouncement>,
    peer_group_calls: BTreeMap<NodeId, Vec<GroupCallAnnouncement>>,
    /// Live per-speaker recording, or `None` when nothing is being recorded.
    recording: Option<crate::recording::RecordingSession>,
    /// Whether the live recording was started by the auto-record preference.
    /// A recording the user asked for by hand outlives that preference.
    recording_automatic: bool,
    /// Record every call without being asked. Off by default: recording is a
    /// deliberate act, not something to discover after the fact.
    record_calls_automatically: bool,
    /// Display names learned from presence, so recordings can be labelled with
    /// something a person recognizes instead of a node id.
    peer_names: BTreeMap<NodeId, String>,
    own_name: String,
    /// Each active peer's voice track and the call generation it belongs to,
    /// kept so recording can be turned on in the middle of a call. Resubscribing
    /// is cheap and does not disturb playback.
    peer_voice_tracks: BTreeMap<NodeId, (u64, MediaTrack)>,
    /// Voice tracks are handed back from the call tasks, which are the only
    /// place a track becomes known. Funnelling both call directions through one
    /// place keeps the recording hook in a single spot.
    voice_track_tx: async_channel::Sender<(NodeId, u64, MediaTrack)>,
    voice_track_rx: async_channel::Receiver<(NodeId, u64, MediaTrack)>,
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

    pub(super) fn spawn() -> (WorkerHandle, ServiceClient) {
        // UI actions must never block the presenter while the worker is busy
        // stopping capture, dialing, or reconciling chat state.
        let (command_tx, command_rx) = async_channel::unbounded();
        let (event_tx, event_rx) = EventPublisher::new();
        let thread_event_tx = event_tx.clone();
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
                    thread_event_tx.send_blocking(Event::WorkerFailed(detail));
                    return;
                }
            };
            rt.block_on(async move {
                let mut worker = match Worker::start(thread_event_tx.clone(), command_rx).await {
                    Ok(worker) => worker,
                    Err(error) => {
                        let detail = format!("Could not start Wire networking: {error:#}");
                        warn!("worker failed to start: {error:#}");
                        thread_event_tx.send(Event::WorkerFailed(detail)).await;
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
        let client = ServiceClient {
            command_tx: command_tx.clone(),
            event_rx,
            event_publisher: event_tx,
        };
        (
            WorkerHandle {
                command_tx,
                thread: Some(thread),
            },
            client,
        )
    }

    async fn emit(&self, event: Event) -> Result<()> {
        self.event_tx.publish(event);
        Ok(())
    }

    async fn start(
        event_tx: EventPublisher,
        command_rx: async_channel::Receiver<Command>,
    ) -> Result<Self> {
        info!("binding Wire networking endpoint");
        let mut alpns = vec![
            iroh_blobs::ALPN.to_vec(),
            iroh_docs::ALPN.to_vec(),
            iroh_gossip::ALPN.to_vec(),
            chat::CHAT_ALPN.to_vec(),
            CLIENT_STATUS_ALPN.to_vec(),
            PROFILE_ALPN.to_vec(),
            wire::remote_logs::LOGS_ALPN.to_vec(),
        ];
        #[cfg(windows)]
        alpns.push(crate::peer_update::PEER_UPDATE_ALPN.to_vec());
        let endpoint = wire::net::bind_endpoint_with_alpns(alpns).await?;
        info!(node = %endpoint.node_id().fmt_short(), "Wire endpoint bound; opening chat storage");
        let handler = RtcProtocol::new(endpoint.clone());
        let logs_protocol = wire::remote_logs::LogsProtocol::new(endpoint.node_id());
        let allowed_peers = AllowedPeers::default();
        let client_status = ClientStatusProtocol::new(endpoint.clone(), allowed_peers.clone());
        // Profiles ride on presence: seed the heartbeat snapshot (and the
        // public fetch protocol + chat snapshots) from local disk.
        let own_profile = profile::load_own_profile();
        let own_avatar = profile::load_avatar_bytes();
        client_status.set_own_profile(own_profile.snapshot());
        let profile_protocol =
            ProfileProtocol::new(ServedProfile::from_own(&own_profile, own_avatar));
        let config_dir = wire::net::config_dir().context("missing Wire config directory")?;
        let mut chat_protocols = chat::ChatService::build(endpoint.clone(), &config_dir).await?;
        chat_protocols.service.set_own_profile(
            (!own_profile.display_name.trim().is_empty()).then(|| own_profile.display_name.clone()),
            own_profile.avatar_hash.clone(),
            own_profile.accent_color.clone(),
        );
        info!("chat storage opened; starting protocol router");
        // Presence and executable transfer share one contact gate, so a single
        // `SetFriends` keeps both in sync.
        #[cfg(windows)]
        let update_protocol = crate::peer_update::PeerUpdateProtocol::new(allowed_peers.clone());
        #[cfg(windows)]
        let router = Router::builder(endpoint.clone())
            .accept(RtcProtocol::ALPN, handler.clone())
            .accept(iroh_blobs::ALPN, chat_protocols.provider.clone())
            .accept(iroh_docs::ALPN, chat_protocols.docs.clone())
            .accept(iroh_gossip::ALPN, chat_protocols.gossip.clone())
            .accept(chat::CHAT_ALPN, chat_protocols.invites.clone())
            .accept(CLIENT_STATUS_ALPN, client_status.clone())
            .accept(PROFILE_ALPN, profile_protocol.clone())
            .accept(wire::remote_logs::LOGS_ALPN, logs_protocol)
            .accept(crate::peer_update::PEER_UPDATE_ALPN, update_protocol);
        #[cfg(not(windows))]
        let router = Router::builder(endpoint.clone())
            .accept(RtcProtocol::ALPN, handler.clone())
            .accept(iroh_blobs::ALPN, chat_protocols.provider.clone())
            .accept(iroh_docs::ALPN, chat_protocols.docs.clone())
            .accept(iroh_gossip::ALPN, chat_protocols.gossip.clone())
            .accept(chat::CHAT_ALPN, chat_protocols.invites.clone())
            .accept(CLIENT_STATUS_ALPN, client_status.clone())
            .accept(PROFILE_ALPN, profile_protocol.clone())
            .accept(wire::remote_logs::LOGS_ALPN, logs_protocol);
        let _router = router.spawn().await?;
        info!("Wire protocol router started");
        let (video_frame_tx, _) = tokio::sync::broadcast::channel(32);
        let (keyframe_tx, _) = tokio::sync::broadcast::channel(16);
        let (voice_track_tx, voice_track_rx) = async_channel::unbounded();
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
            allowed_peers,
            profile_protocol,
            profile_fetches: JoinSet::new(),
            peer_updates: JoinSet::new(),
            pending_peer_updates: Default::default(),
            recording: None,
            recording_automatic: false,
            record_calls_automatically: false,
            peer_names: Default::default(),
            own_name: profile::load_own_profile().display_name,
            peer_voice_tracks: Default::default(),
            voice_track_tx,
            voice_track_rx,
        })
    }

    /// Refresh the served profile from disk (used when the UI edits identity).
    fn refresh_served_profile(&mut self) {
        let own = profile::load_own_profile();
        self.own_name = own.display_name.clone();
        let avatar = profile::load_avatar_bytes();
        self.client_status.set_own_profile(own.snapshot());
        self.chat.set_own_profile(
            Some(own.display_name.clone()),
            own.avatar_hash.clone(),
            own.accent_color.clone(),
        );
        self.profile_protocol
            .set_served(ServedProfile::from_own(&own, avatar));
    }

    /// What to call a speaker in a recording file.
    ///
    /// A profile name is preferred, a short node id beats a blank label, and
    /// the local user is always marked so their file is unambiguous.
    fn speaker_name(&self, node_id: NodeId) -> String {
        if node_id == self.endpoint.node_id() {
            return if self.own_name.trim().is_empty() {
                "You".to_owned()
            } else {
                format!("{} (you)", self.own_name.trim())
            };
        }
        self.peer_names
            .get(&node_id)
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| node_id.fmt_short().to_string())
    }

    /// Start recording, then attach every speaker already in the call.
    async fn start_recording(&mut self, automatic: bool) -> Result<()> {
        if self.recording.is_some() {
            return Ok(());
        }
        let root = crate::recording::recordings_dir().context("no Wire data directory")?;
        let mut session = crate::recording::RecordingSession::start(&root)?;

        // The local microphone is a file of its own and does not depend on any
        // call being connected.
        if let Some(audio) = self.audio_context.clone() {
            let node_id = self.endpoint.node_id();
            let name = self.speaker_name(node_id);
            session
                .attach_microphone(&audio, node_id, name)
                .await
                .context("could not start recording the microphone")?;
        } else {
            warn!("recording started without a microphone: audio is not configured yet");
        }

        // Peers already in the call get their file immediately; anyone who
        // joins later is attached as their track arrives.
        let active: Vec<(NodeId, u64, MediaTrack)> = self
            .peer_voice_tracks
            .iter()
            .map(|(node_id, (generation, track))| (*node_id, *generation, track.clone()))
            .collect();
        for (node_id, generation, track) in active {
            self.attach_recorder_for(node_id, generation, track);
        }

        let dir = session.dir().to_path_buf();
        self.recording = Some(session);
        self.recording_automatic = automatic;
        info!(
            automatic,
            dir = %dir.display(),
            "recording every speaker to a separate file"
        );
        self.emit(Event::CallRecordingToggled {
            active: true,
            dir: Some(dir),
        })
        .await?;
        Ok(())
    }

    /// A peer's voice track arrived. Keep it so recording can start mid-call,
    /// and give the peer a file straight away if it is already running.
    fn on_voice_track(&mut self, node_id: NodeId, generation: u64, track: MediaTrack) {
        // A track from a superseded call must not overwrite the live one, or a
        // recording would keep decoding audio nobody is hearing.
        if !self.call_is_current(node_id, generation) {
            debug!(
                node = %node_id.fmt_short(),
                generation,
                "ignored a voice track from a stale call"
            );
            return;
        }
        self.peer_voice_tracks
            .insert(node_id, (generation, track.clone()));
        self.attach_recorder_for(node_id, generation, track);
    }

    /// Attach one peer to the running recording, if there is one.
    ///
    /// A peer who is already being recorded keeps their single file: a new
    /// track is handed to the existing recorder instead of starting a second one.
    fn attach_recorder_for(&mut self, node_id: NodeId, generation: u64, track: MediaTrack) {
        let name = self.speaker_name(node_id);
        let Some(session) = self.recording.as_mut() else {
            return;
        };
        if session.has_speaker(node_id) {
            session.replace_participant_track(node_id, generation, track);
            return;
        }
        if let Err(error) = session.attach_participant(node_id, name, generation, track) {
            warn!(peer = %node_id.fmt_short(), "could not record this participant: {error:#}");
        }
    }

    /// Stop recording, finalize every file, and report where they landed.
    async fn stop_recording(&mut self) {
        let Some(session) = self.recording.take() else {
            return;
        };
        self.recording_automatic = false;
        let peer_names = self.peer_names.clone();
        // Finalizing renames each file to its display name and writes the
        // manifest, so it must not run on the runtime's async tasks.
        let summary = match tokio::task::spawn_blocking(move || session.finish(&peer_names)).await
        {
            Ok(summary) => summary,
            Err(error) => {
                warn!("the recording finalizer did not finish: {error}");
                return;
            }
        };
        self.emit(Event::CallRecordingToggled {
            active: false,
            dir: None,
        })
        .await
        .ok();
        self.emit(Event::CallRecordingStopped { summary })
            .await
            .ok();
    }

    async fn set_recording(&mut self, enabled: bool) {
        if enabled {
            if let Err(error) = self.start_recording(false).await {
                let detail = format!("Could not start recording: {error:#}");
                warn!("{detail}");
                self.emit(Event::CallRecordingFailed(detail)).await.ok();
            }
        } else {
            self.stop_recording().await;
        }
    }

    /// Auto-record follows the lifetime of a call, so a meeting is captured from
    /// the first word to the last without anyone touching a control.
    async fn maybe_auto_record(&mut self) {
        if self.record_calls_automatically
            && self.recording.is_none()
            && !self.active_calls.is_empty()
        {
            if let Err(error) = self.start_recording(true).await {
                let detail = format!("Could not start recording automatically: {error:#}");
                warn!("{detail}");
                self.emit(Event::CallRecordingFailed(detail)).await.ok();
            }
        }
    }

    /// End an auto-started recording when its call is over.
    ///
    /// A recording the user started by hand is theirs to stop, so switching the
    /// preference off must not cut one short.
    async fn stop_auto_recording(&mut self) {
        if self.record_calls_automatically && self.recording_automatic {
            self.stop_recording().await;
        }
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
                    match command {
                        Err(_) => {
                            info!("application command channel closed; stopping worker");
                            break;
                        }
                        Ok(Command::Shutdown) => {
                            info!("application requested worker shutdown");
                            break;
                        }
                        Ok(command) => {
                            if let Err(err) = self.handle_command(command).await {
                                warn!("command failed: {err}");
                            }
                        }
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
                    self.peer_voice_tracks.remove(&node_id);
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
                    // Names learned here are the best labels available for
                    // recording files, which are named when they are closed.
                    if let Some(name) = status
                        .profile
                        .display_name
                        .as_ref()
                        .map(|name| crate::profile::sanitize_display_name(name))
                        .filter(|name| !name.is_empty())
                    {
                        self.peer_names.insert(status.peer, name);
                    }
                    if matches!(status.availability, Availability::Online) {
                        self.peer_group_calls
                            .insert(status.peer, status.active_group_calls.clone());
                    } else {
                        self.peer_group_calls.remove(&status.peer);
                    }
                    self.emit(Event::ClientStatus(status)).await?;
                }
                Some(joined) = self.peer_updates.join_next(), if !self.peer_updates.is_empty() => {
                    // Dispatched through a method because `select!` arms cannot be
                    // `#[cfg]`-ed: the arm must exist on every platform, while
                    // the events it reports only exist on Windows.
                    self.on_peer_update_finished(joined).await;
                }
                Some(joined) = self.profile_fetches.join_next(), if !self.profile_fetches.is_empty() => {
                    let Ok((peer, result)) = joined else {
                        warn!("profile fetch task was cancelled or panicked");
                        continue;
                    };
                    match result {
                        Ok(fetched) => {
                            debug!(peer = %peer.fmt_short(), "profile fetch succeeded");
                            self.emit(Event::PeerProfile {
                                peer,
                                display_name: fetched.display_name,
                                avatar_hash: fetched.avatar_hash,
                                avatar_bytes: fetched.avatar_bytes,
                                accent_color: fetched.accent_color,
                            })
                            .await?;
                        }
                        Err(error) => {
                            // Warn, not debug: a failing fetch is the only
                            // signal when peer pictures never resolve.
                            warn!(peer = %peer.fmt_short(), "profile fetch failed: {error:#}");
                        }
                    }
                }
                failure = self.capture_failure_rx.recv() => {
                    if let Ok(message) = failure {
                        self.handle_capture_failure(message).await;
                    }
                }
                arrived = self.voice_track_rx.recv() => {
                    if let Ok((node_id, generation, track)) = arrived {
                        self.on_voice_track(node_id, generation, track);
                    }
                }
            }
        }
        info!("Wire worker runtime is stopping");
        self.client_status.broadcast_offline().await;
        self.close_active_call_transports();
        if self.sharing_active {
            self.stop_capture().await;
        }
        // Finalize before the runtime tears down: dropping a session here would
        // leave WAV files without their size header, which most readers reject.
        if self.recording.is_some() {
            self.stop_recording().await;
        }
        Ok(())
    }

    /// Report a finished peer-to-peer update transfer.
    #[cfg(windows)]
    async fn on_peer_update_finished(
        &mut self,
        joined: std::result::Result<(NodeId, Result<()>), tokio::task::JoinError>,
    ) {
        let Ok((peer, result)) = joined else {
            warn!("peer update task was cancelled or panicked");
            return;
        };
        // Clear first: a failure must not leave the peer marked busy, or the UI
        // would refuse every retry until restart.
        self.pending_peer_updates.remove(&peer);
        if let Err(error) = result {
            warn!(peer = %peer.fmt_short(), "peer update failed: {error:#}");
            self.emit(Event::PeerUpdateFailed {
                peer,
                error: format!("{error:#}"),
            })
            .await
            .ok();
        }
    }

    /// Windows-only task set; never populated elsewhere, so there is nothing to
    /// report.
    #[cfg(not(windows))]
    async fn on_peer_update_finished(
        &mut self,
        _joined: std::result::Result<(NodeId, anyhow::Result<()>), tokio::task::JoinError>,
    ) {
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
                self.peer_voice_tracks.remove(&node_id);
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
        self.maybe_auto_record().await;
        let audio_context = self
            .audio_context
            .clone()
            .context("missing audio context")?;

        self.ensure_video_streams(node_id, conn.clone()).await;
        let system_audio_track = self.system_audio.as_ref().map(|share| share.track());

        let audio_conn = conn.clone();
        let voice_track_tx = self.voice_track_tx.clone();
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());

            let fut = async {
                audio_context
                    .play_track_with_volume_and_level(track.clone(), volume.clone(), level.clone())
                    .await?;
                // Hand the voice track back so it can be recorded, now or later.
                let _ = voice_track_tx.send((node_id, generation, track)).await;
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
        self.maybe_auto_record().await;
        let audio_context = self
            .audio_context
            .clone()
            .context("missing audio context")?;

        self.ensure_video_streams(node_id, conn.clone()).await;
        let system_audio_track = self.system_audio.as_ref().map(|share| share.track());

        let audio_conn = conn.clone();
        let voice_track_tx = self.voice_track_tx.clone();
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
                                // Hand the voice track back so it can be recorded,
                                // now or later in the call.
                                let _ = voice_track_tx
                                    .send((node_id, generation, remote_track.clone()))
                                    .await;
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
            let nid = node_id;
            let (recv_control_tx, recv_control_rx) = tokio::sync::mpsc::unbounded_channel();
            let handle = tokio::spawn(async move {
                run_video_recv(recv_conn, nid, event_tx, recv_control_rx).await;
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
        // Auto-recording spans the call, so the last participant leaving is what
        // ends it. A recording the user started by hand is left alone.
        self.stop_auto_recording().await;
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
            Command::Shutdown => {}
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
            Command::SetCallRecording { enabled } => {
                self.set_recording(enabled).await;
            }
            Command::SetRecordCallsAutomatically { enabled } => {
                if self.record_calls_automatically == enabled {
                    return Ok(());
                }
                self.record_calls_automatically = enabled;
                // Turning it on mid-call records from here; turning it off ends
                // only the recordings the preference itself started.
                if enabled {
                    self.maybe_auto_record().await;
                } else {
                    self.stop_auto_recording().await;
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
            Command::OfferChatFiles {
                conversation_id,
                body,
                attachments,
                paths,
            } => {
                self.chat
                    .offer_files(conversation_id, body, attachments, paths);
            }
            Command::ReceiveChatFile {
                conversation_id,
                message_id,
                hash,
                path,
            } => {
                if let Err(error) = self
                    .chat
                    .request_file(conversation_id, message_id.clone(), hash.clone(), path)
                    .await
                {
                    self.emit(Event::Chat(chat::ChatNotification::FileTransfer {
                        message_id,
                        hash,
                        result: Err(format!("{error:#}")),
                    }))
                    .await?;
                }
            }
            Command::CancelChatFile { message_id, hash } => {
                if let Err(error) = self.chat.cancel_file(message_id, hash).await {
                    warn!("could not cancel chat file download: {error:#}");
                }
            }
            Command::SetChatFileServing { hash, serving } => {
                if let Err(error) = self.chat.set_file_serving(hash, serving).await {
                    self.emit(Event::Chat(chat::ChatNotification::Error(format!(
                        "Could not change file sharing: {error:#}"
                    ))))
                    .await?;
                }
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
                self.allowed_peers.replace(friends.clone());
                self.client_status.announce_online(friends);
            }
            Command::SetOwnProfile {
                display_name,
                avatar_hash,
                accent_color,
            } => {
                let snapshot = profile::ProfileSnapshot {
                    display_name: (!display_name.trim().is_empty()).then_some(display_name.clone()),
                    avatar_hash: avatar_hash.clone(),
                    accent_color: accent_color.clone(),
                };
                self.client_status.set_own_profile(snapshot);
                self.chat
                    .set_own_profile(Some(display_name.clone()), avatar_hash, accent_color);
                self.refresh_served_profile();
                // Re-announce so friends learn the new identity immediately.
                self.client_status.refresh_allowed_peers();
            }
            Command::SetChatProfile {
                display_name,
                avatar_hash,
                accent_color,
            } => {
                self.chat
                    .set_own_profile(display_name, avatar_hash, accent_color);
            }
            Command::FetchPeerProfiles { peers } => {
                for peer in peers {
                    // Bound concurrent fetches; the UI de-dupes requests.
                    if self.profile_fetches.len() >= 8 {
                        break;
                    }
                    let endpoint = self.endpoint.clone();
                    self.profile_fetches.spawn(async move {
                        let result = profile::fetch_profile(&endpoint, peer).await;
                        (peer, result)
                    });
                }
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
                scope,
            } => {
                self.chat
                    .restore_message(conversation_id, message_id, scope)
                    .await;
            }
            Command::ClearChatHistory { conversation_id } => {
                self.chat.clear_history(conversation_id).await;
            }
            #[cfg(windows)]
            Command::FetchPeerUpdateOffer { peer } => {
                let endpoint = self.endpoint.clone();
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    let event = match crate::peer_update::fetch_offer(&endpoint, peer).await {
                        Ok(offer) => Event::PeerUpdateOffer { peer, offer },
                        Err(error) => {
                            warn!(peer = %peer.fmt_short(), "peer update offer failed: {error:#}");
                            Event::PeerUpdateFailed {
                                peer,
                                error: format!("{error:#}"),
                            }
                        }
                    };
                    let _ = event_tx.send(event).await;
                });
            }
            #[cfg(windows)]
            Command::DownloadPeerUpdate { peer } => {
                // One transfer per peer: a second click while bytes are moving
                // must not race the staging file.
                if !self.pending_peer_updates.insert(peer) {
                    return Ok(());
                }
                let endpoint = self.endpoint.clone();
                let event_tx = self.event_tx.clone();
                // Cheap first: confirm the peer really offers something newer
                // before writing tens of megabytes to disk.
                let offer = match crate::peer_update::fetch_offer(&endpoint, peer).await {
                    Ok(offer)
                        if offer.is_usable_for(crate::APP_VERSION) && offer.is_runnable_here() =>
                    {
                        offer
                    }
                    Ok(offer) => {
                        self.pending_peer_updates.remove(&peer);
                        // A binary built for another platform cannot run here, so
                        // installing it would break a working install.
                        let error = if !offer.is_runnable_here() {
                            format!(
                                "{} sent a Wire build for {}, which cannot run on {}",
                                peer.fmt_short(),
                                offer.platform,
                                crate::peer_update::current_platform()
                            )
                        } else {
                            format!(
                                "{} is already running v{} or newer",
                                peer.fmt_short(),
                                offer.version
                            )
                        };
                        let _ = event_tx.send(Event::PeerUpdateFailed { peer, error }).await;
                        return Ok(());
                    }
                    Err(error) => {
                        self.pending_peer_updates.remove(&peer);
                        warn!(peer = %peer.fmt_short(), "peer update offer failed: {error:#}");
                        let _ = event_tx
                            .send(Event::PeerUpdateFailed {
                                peer,
                                error: format!("{error:#}"),
                            })
                            .await;
                        return Ok(());
                    }
                };
                self.peer_updates.spawn(async move {
                    let result = run_peer_update_download(endpoint, peer, offer, event_tx).await;
                    (peer, result)
                });
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
