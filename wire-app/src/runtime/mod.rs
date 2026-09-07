//! Background runtime boundary: commands in, events out.
//!
//! The handle owns thread shutdown; worker and stream task details stay private.

mod video;
mod worker;

use std::{collections::BTreeSet, sync::Arc};

use async_channel::{Receiver, Sender};
use iroh::NodeId;
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

pub(crate) enum Command {
    SetUpdateCallback {
        callback: UpdateCallback,
    },
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
    event_rx: Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        // Wake a worker that is blocked publishing into a full event queue before
        // waiting for its thread. The UI no longer drains this receiver in Drop.
        self.event_rx.close();
        self.command_tx.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl WorkerHandle {
    pub(crate) fn command_sender(&self) -> &Sender<Command> {
        &self.command_tx
    }

    pub(crate) fn try_recv(&self) -> Result<Event, async_channel::TryRecvError> {
        self.event_rx.try_recv()
    }
}

pub(crate) fn spawn() -> WorkerHandle {
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
    use std::{sync::mpsc, time::Duration};

    use super::{Event, WorkerHandle};

    #[test]
    fn shutdown_closes_full_event_queue_and_commands_before_joining() {
        let (command_tx, command_rx) = async_channel::unbounded();
        let retained_sender = command_tx.clone();
        let (event_tx, event_rx) = async_channel::bounded(1);
        event_tx.try_send(Event::InitialChatLoaded).unwrap();
        let (result_tx, result_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            // The UI has stopped draining events. Publishing must unblock when
            // the handle closes, even if another caller retains a command sender.
            let events_closed = event_tx.send_blocking(Event::InitialChatLoaded).is_err();
            let commands_closed = command_rx.recv_blocking().is_err();
            result_tx.send((events_closed, commands_closed)).unwrap();
        });
        let handle = WorkerHandle {
            command_tx,
            event_rx,
            thread: Some(thread),
        };
        let (done_tx, done_rx) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            drop(handle);
            done_tx.send(()).unwrap();
        });

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdown must not wait for the UI to drain events");
        shutdown.join().unwrap();
        assert_eq!(result_rx.recv().unwrap(), (true, true));
        assert!(retained_sender.is_closed());
    }
}
