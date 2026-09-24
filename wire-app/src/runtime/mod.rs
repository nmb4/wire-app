//! Background runtime boundary: commands in, events out.
//!
//! The handle owns thread shutdown; worker and stream task details stay private.

mod video;
mod worker;

use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock,
    },
};

use async_channel::{Receiver, Sender};
use iroh::NodeId;
use tracing::warn;
use wire::{
    audio::{AudioConfig, AudioLevelHandle, VolumeHandle},
    video::VideoConfig,
};

use crate::{
    chat::{ChatMessage, ChatNotification, DeleteScope, RetentionPolicy},
    client_status::{GroupCallAnnouncement, StatusUpdate},
    video_decode::DecodedFrame,
};

pub(crate) enum Event {
    EndpointBound(NodeId),
    ClientStatus(StatusUpdate),
    GroupCallEntered(GroupCallAnnouncement),
    InitialChatLoaded,
    Chat(ChatNotification),
    WorkerFailed(String),
    SetCallState(NodeId, CallState),
    LocalAudioLevel(AudioLevelHandle),
    ParticipantAudioHandles {
        node_id: NodeId,
        volume: VolumeHandle,
        stream_volume: VolumeHandle,
        level: AudioLevelHandle,
    },
    VideoStreamAccepted {
        node_id: NodeId,
        generation: u64,
    },
    VideoFrame {
        node_id: NodeId,
        generation: u64,
        frame: DecodedFrame,
    },
    VideoStreamEnded {
        node_id: NodeId,
        generation: u64,
        reason: VideoStreamEndReason,
    },
    SharingToggled {
        active: bool,
        system_audio: bool,
    },
    SystemAudioToggled(bool),
    SystemAudioFailed(String),
    SharingFailed(String),
    PreviewFrame {
        width: u32,
        height: u32,
        data: Arc<Vec<u8>>,
        actual_fps: f64,
        encode_time_ms: f64,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum VideoStreamEndReason {
    CleanEof,
    ReceiveError,
    ReplacementIdle,
    ConnectionClosed,
}

#[derive(strum::Display, Clone, Copy)]
pub(crate) enum CallState {
    Incoming,
    Calling,
    Active,
    Aborted,
}

type UpdateCallback = Arc<dyn Fn() + Send + Sync>;

const EVENT_BUFFER_CAPACITY: usize = 1_024;

#[derive(Clone)]
pub(super) struct EventPublisher {
    sender: Sender<Event>,
    update_callback: Arc<RwLock<Option<UpdateCallback>>>,
    presenter_active: Arc<AtomicBool>,
    dropped_events: Arc<AtomicUsize>,
}

impl EventPublisher {
    fn new() -> (Self, Receiver<Event>) {
        let (sender, receiver) = async_channel::bounded(EVENT_BUFFER_CAPACITY);
        (
            Self {
                sender,
                update_callback: Arc::new(RwLock::new(None)),
                presenter_active: Arc::new(AtomicBool::new(true)),
                dropped_events: Arc::new(AtomicUsize::new(0)),
            },
            receiver,
        )
    }

    pub(super) fn publish(&self, event: Event) {
        // A presenter is optional. Publishing must never backpressure or terminate
        // the service when no client is attached or a client has fallen behind.
        let should_wake_presenter = self.presenter_active.load(Ordering::Acquire)
            && !matches!(
                &event,
                Event::VideoFrame { .. } | Event::PreviewFrame { .. }
            );
        if let Err(async_channel::TrySendError::Full(_)) = self.sender.try_send(event) {
            let dropped = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped <= 5 || dropped.is_power_of_two() {
                warn!(
                    dropped,
                    capacity = EVENT_BUFFER_CAPACITY,
                    "presenter event queue is full; dropping a service event"
                );
            }
        }
        if !should_wake_presenter {
            return;
        }
        let callback = self
            .update_callback
            .read()
            .ok()
            .and_then(|callback| callback.clone());
        if let Some(callback) = callback {
            callback();
        }
    }

    pub(super) async fn send(&self, event: Event) {
        self.publish(event);
    }

    pub(super) fn try_send(&self, event: Event) {
        self.publish(event);
    }

    pub(super) fn send_blocking(&self, event: Event) {
        self.publish(event);
    }

    fn set_update_callback(&self, callback: Option<UpdateCallback>) {
        if let Ok(mut current) = self.update_callback.write() {
            *current = callback;
        }
    }

    fn set_presenter_active(&self, active: bool) {
        self.presenter_active.store(active, Ordering::Release);
    }
}

pub(crate) struct ServiceClient {
    command_tx: Sender<Command>,
    event_rx: Receiver<Event>,
    event_publisher: EventPublisher,
}

impl ServiceClient {
    pub(crate) fn command_sender(&self) -> &Sender<Command> {
        &self.command_tx
    }

    pub(crate) fn set_update_callback(&self, callback: UpdateCallback) {
        self.event_publisher.set_update_callback(Some(callback));
    }

    pub(crate) fn set_presenter_active(&self, active: bool) {
        self.event_publisher.set_presenter_active(active);
    }

    pub(crate) fn try_recv(&self) -> Result<Event, async_channel::TryRecvError> {
        self.event_rx.try_recv()
    }

    pub(crate) fn discard_buffered_media_events(&self) -> usize {
        let mut retained = Vec::new();
        let mut discarded = 0;
        while let Ok(event) = self.event_rx.try_recv() {
            if matches!(
                &event,
                Event::VideoFrame { .. } | Event::PreviewFrame { .. }
            ) {
                discarded += 1;
            } else {
                retained.push(event);
            }
        }
        for event in retained {
            self.event_publisher.try_send(event);
        }
        discarded
    }
}

impl Drop for ServiceClient {
    fn drop(&mut self) {
        self.event_publisher.set_presenter_active(false);
        self.event_publisher.set_update_callback(None);
    }
}

pub(crate) enum Command {
    Shutdown,
    SetAudioConfig {
        audio_config: AudioConfig,
    },
    SetVideoConfig {
        video_config: VideoConfig,
    },
    Call {
        node_id: NodeId,
    },
    EnterGroupCall {
        call: GroupCallAnnouncement,
        targets: Vec<NodeId>,
        notify: Vec<NodeId>,
    },
    LeaveGroupCall {
        notify: Vec<NodeId>,
    },
    HandleIncoming {
        node_id: NodeId,
        accept: bool,
    },
    Abort {
        node_id: NodeId,
    },
    ToggleSharing {
        enabled: bool,
        target: Option<crate::screen_capture::CaptureTarget>,
        share_system_audio: bool,
    },
    SetSystemAudio {
        enabled: bool,
    },
    SetWatching {
        node_id: NodeId,
        generation: u64,
        watching: bool,
    },
    SetMuted {
        muted: bool,
    },
    SetDeafened {
        deafened: bool,
    },
    EnsureDirectChat {
        peer: NodeId,
        title: String,
    },
    CreateGroupChat {
        title: String,
        members: Vec<NodeId>,
    },
    SendChatMessage {
        conversation_id: String,
        message: ChatMessage,
    },
    LoadChatAttachment {
        conversation_id: String,
        hash: String,
        byte_len: u64,
    },
    LoadOlderChatMessages {
        conversation_id: String,
    },
    SetMaxImageBytes {
        max_image_bytes: Option<u64>,
    },
    SetChatRetention {
        retention: RetentionPolicy,
    },
    SetFriends {
        friends: BTreeSet<NodeId>,
    },
    DeleteChatMessage {
        conversation_id: String,
        message_id: String,
        scope: DeleteScope,
    },
    RestoreChatMessage {
        conversation_id: String,
        message_id: String,
    },
    ClearChatHistory {
        conversation_id: String,
    },
}

pub(crate) struct WorkerHandle {
    command_tx: Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WorkerHandle {
    pub(crate) fn shutdown(mut self) {
        let _ = self.command_tx.try_send(Command::Shutdown);
        self.join();
    }

    fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                warn!("Wire worker thread panicked during shutdown");
            }
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Emergency cleanup for startup failures and unwinding. Normal lifecycle
        // shutdown goes through the explicit Shutdown command first.
        self.command_tx.close();
        self.join();
    }
}

pub(crate) fn spawn() -> (WorkerHandle, ServiceClient) {
    worker::Worker::spawn()
}

#[cfg(target_os = "windows")]
fn trim_process_working_set() -> anyhow::Result<()> {
    use anyhow::Context;
    use windows::Win32::System::{ProcessStatus::EmptyWorkingSet, Threading::GetCurrentProcess};

    unsafe { EmptyWorkingSet(GetCurrentProcess()) }
        .ok()
        .context("EmptyWorkingSet failed")
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    };

    use super::{Command, Event, EventPublisher, ServiceClient, WorkerHandle};

    #[test]
    fn publishing_without_a_presenter_never_blocks_or_stops_the_service() {
        let (publisher, receiver) = EventPublisher::new();
        drop(receiver);
        for _ in 0..super::EVENT_BUFFER_CAPACITY * 2 {
            publisher.publish(Event::InitialChatLoaded);
        }
    }

    #[test]
    fn presenter_callback_is_notified_without_owning_service_shutdown() {
        let (publisher, _receiver) = EventPublisher::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        publisher.set_update_callback(Some(Arc::new(move || {
            callback_calls.fetch_add(1, Ordering::Relaxed);
        })));
        publisher.publish(Event::InitialChatLoaded);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn explicit_shutdown_reaches_worker_while_presenter_client_is_alive() {
        let (command_tx, command_rx) = async_channel::unbounded();
        let (publisher, event_rx) = EventPublisher::new();
        let client = ServiceClient {
            command_tx: command_tx.clone(),
            event_rx,
            event_publisher: publisher,
        };
        let (result_tx, result_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let command = command_rx.recv_blocking().unwrap();
            result_tx
                .send(matches!(command, Command::Shutdown))
                .unwrap();
        });
        let host = WorkerHandle {
            command_tx,
            thread: Some(thread),
        };

        assert!(!client.command_tx.is_closed());
        host.shutdown();
        assert!(result_rx.recv().unwrap());
    }
}
