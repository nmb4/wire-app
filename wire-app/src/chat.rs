use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::{Bound, RangeBounds},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod blob_provider;
use blob_provider::{is_stopped, set_stopped, GuardedBlobProvider, StoppedHashes};

use crate::persistence;
use anyhow::{bail, Context, Result};
use futures_lite::StreamExt;
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use iroh_blobs::{
    downloader::DownloadRequest,
    get::db::{BlobId, DownloadProgress},
    net_protocol::Blobs,
    store::{
        fs::Store as BlobStore, ExportMode, GcConfig, ImportMode, ImportProgress, Map, MapMut,
        ReadableStore, Store,
    },
    util::progress::{AsyncChannelProgressSender, IgnoreProgressSender},
    BlobFormat, Hash, HashAndFormat, Tag,
};
use iroh_docs::{
    engine::LiveEvent,
    protocol::Docs,
    rpc::{
        client::docs::{MemClient, ShareMode},
        AddrInfoOptions,
    },
    store::{Query, SortBy, SortDirection},
    AuthorId, DocTicket, NamespaceId,
};
use iroh_gossip::net::Gossip;
use iroh_io::AsyncSliceReader;
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, trace, warn};

use tokio::sync::watch;

pub const CHAT_ALPN: &[u8] = b"wire/chat-invite/1";
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_INVITE_BYTES: usize = 256 * 1024;
const MAX_ATTACHMENT_PUSH_BYTES: u64 = 1024 * 1024 * 1024;
const MESSAGE_PREFIX: &[u8] = b"message/";
const DELETION_PREFIX: &[u8] = b"deletion/";
const RECEIPT_PREFIX: &[u8] = b"receipt/";
const FILE_RECEIPT_PREFIX: &[u8] = b"file-receipt/";
const FILE_SHARING_PREFIX: &[u8] = b"file-sharing/";
const RETRY_TICK: Duration = Duration::from_secs(1);
const MAX_RETRY_SECONDS: u64 = 60;
/// Keep chat-plane QUIC sessions warm for bursty back-and-forth.
/// See `docs/chat-keepalive-sessions.md`.
const CHAT_SESSION_IDLE: Duration = Duration::from_secs(60);
const CHAT_SESSION_POOL_CAP: usize = 8;
const CHAT_PEER_GATE_CAP: usize = 64;
/// Half-open pooled sessions must fail fast — logs showed a hard ~3s floor on
/// every send while waiting out a full stream timeout on a dead reuse.
const CHAT_REUSE_TIMEOUT: Duration = Duration::from_millis(400);
const CHAT_STREAM_TIMEOUT: Duration = Duration::from_secs(3);
const CHAT_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// First delivery re-probe after send (receipt / docs pull may still be in flight).
const CHAT_FIRST_RETRY: Duration = Duration::from_millis(400);
/// Min gap between receipt-driven wakes (avoids connect storms that block sends).
const CHAT_RECEIPT_WAKE_COOLDOWN: Duration = Duration::from_secs(2);
/// Retry missing attachment blobs independently of document changes. A peer can
/// publish the message metadata before its blob address is usable, so one
/// failed fetch must not leave the UI on "Loading image…" forever.
const ATTACHMENT_RETRY_MAX: Duration = Duration::from_secs(60);
/// A downloader intent can remain pending while it retries an address that is
/// no longer usable. Bound each intent so a fresh attempt can pick up address
/// information learned from later chat/status connections.
const ATTACHMENT_DOWNLOAD_TIMEOUT_MIN: Duration = Duration::from_secs(10);
const ATTACHMENT_DOWNLOAD_TIMEOUT_MAX: Duration = Duration::from_secs(120);
const CHAT_TIMELINE_PAGE: usize = 250;
/// After this many consecutive failed probes, show Queued and switch to slow
/// offline probes (see `delivery_retry_delay(..., offline=true)`).
const CHAT_QUEUE_AFTER_FAILURES: u8 = 5;
/// Leave headroom under MAX_INVITE_BYTES for ticket + framing on chat ALPN.
const CHAT_WAKE_PAYLOAD_BUDGET: usize = 200 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionPolicy {
    #[default]
    Unlimited,
    Days(u32),
}

impl RetentionPolicy {
    pub fn includes(self, sent_at: i64, now: i64) -> bool {
        match self {
            Self::Unlimited => true,
            Self::Days(days) => {
                let keep_ms = i64::from(days).saturating_mul(24 * 60 * 60 * 1000);
                sent_at >= now.saturating_sub(keep_ms)
            }
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Unlimited => "Unlimited".to_owned(),
            Self::Days(days) => format!("Last {days} days"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub version: u8,
    pub message_id: String,
    pub author_id: String,
    pub sent_at: i64,
    pub body: String,
    pub nonce: u64,
    /// Wire GUI build that authored this message (e.g. "0.4.7"). Absent on
    /// older peers / historical entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// Identity snapshot so receivers show a display name (and can fetch the
    /// matching avatar) even for senders they never saved as contacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_avatar_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_accent_color: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ChatAttachment>,
    /// Receipt identities are reconstructed from the replicated document.
    #[serde(skip)]
    pub file_receivers: BTreeMap<String, BTreeSet<String>>,
    /// Locally reconstructed from owner-authored file-sharing records.
    #[serde(skip)]
    pub stopped_file_offers: BTreeSet<String>,
    #[serde(skip)]
    pub deletion: Option<MessageDeletion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatAttachment {
    #[serde(default)]
    pub kind: AttachmentKind,
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub byte_len: u64,
    pub width: u32,
    pub height: u32,
    pub hash: String,
    /// Present only on the local UI side or after the referenced blob was
    /// downloaded. The original bytes are never embedded in message metadata.
    #[serde(skip)]
    pub data: Option<Arc<Vec<u8>>>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    #[default]
    Image,
    FileOffer,
    InlineFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDeletion {
    Local,
    Everyone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteScope {
    Local,
    Everyone,
}

impl ChatMessage {
    #[cfg(test)]
    pub fn new(author_id: NodeId, body: String) -> Self {
        Self::new_with_attachments(author_id, body, Vec::new())
    }

    pub fn new_with_attachments(
        author_id: NodeId,
        body: String,
        attachments: Vec<ChatAttachment>,
    ) -> Self {
        let sent_at = now_millis();
        let nonce = next_nonce();
        let mut hasher = Sha256::new();
        hasher.update(author_id.as_bytes());
        hasher.update(sent_at.to_be_bytes());
        hasher.update(nonce.to_be_bytes());
        hasher.update(body.as_bytes());
        for attachment in &attachments {
            hasher.update(attachment.hash.as_bytes());
            hasher.update(attachment.byte_len.to_be_bytes());
        }
        let message_id = hex_bytes(&hasher.finalize());
        Self {
            version: 1,
            message_id,
            author_id: author_id.to_string(),
            sent_at,
            body,
            nonce,
            client_version: Some(crate::APP_VERSION.to_owned()),
            author_display_name: None,
            author_avatar_hash: None,
            author_accent_color: None,
            attachments,
            file_receivers: BTreeMap::new(),
            stopped_file_offers: BTreeSet::new(),
            deletion: None,
        }
    }

    pub fn entry_key(&self) -> String {
        format!(
            "message/{:020}/{}/{:016x}",
            self.sent_at, self.author_id, self.nonce
        )
    }

    /// Attach the sender's current profile snapshot before publishing.
    pub fn with_author_profile(
        mut self,
        display_name: Option<String>,
        avatar_hash: Option<String>,
        accent_color: Option<String>,
    ) -> Self {
        let name = display_name
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .map(|name| name.chars().take(32).collect::<String>().trim().to_owned())
            .filter(|name| !name.is_empty());
        self.author_display_name = name;
        self.author_avatar_hash = avatar_hash.filter(|hash| !hash.trim().is_empty());
        self.author_accent_color = accent_color
            .as_deref()
            .and_then(crate::profile::sanitize_accent_color);
        self
    }

    pub fn visible_under(&self, retention: RetentionPolicy, now: i64) -> bool {
        retention.includes(self.sent_at, now) || !self.attachments.is_empty()
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported message version {}", self.version);
        }
        if self.body.trim().is_empty() && self.attachments.is_empty() {
            bail!("message is empty");
        }
        if self.body.len() > MAX_MESSAGE_BYTES {
            bail!("message is larger than the 1 MiB safety cap");
        }
        if !is_message_id(&self.message_id) || self.author_id.is_empty() {
            bail!("message identity is incomplete");
        }
        if self.attachments.len() > 128 {
            bail!("message has more than 128 attachments");
        }
        for attachment in &self.attachments {
            if attachment.id.is_empty()
                || attachment.name.is_empty()
                || attachment.name.len() > 1024
                || attachment.media_type.len() > 128
                || attachment.byte_len == 0
                || (attachment.kind == AttachmentKind::Image
                    && (attachment.width == 0 || attachment.height == 0))
                || (attachment.kind == AttachmentKind::InlineFile
                    && (attachment.byte_len > 64 * 1024 || attachment.media_type != "text/plain"))
                || Hash::from_str(&attachment.hash).is_err()
            {
                bail!("attachment metadata is incomplete");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplicatedDeletion {
    version: u8,
    message_id: String,
    deleted_at: i64,
}

impl ReplicatedDeletion {
    fn new(message_id: String) -> Self {
        Self {
            version: 1,
            message_id,
            deleted_at: now_millis(),
        }
    }

    fn entry_key(&self) -> String {
        format!("deletion/{}", self.message_id)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported deletion version {}", self.version);
        }
        if !is_message_id(&self.message_id) {
            bail!("deletion has an invalid message id");
        }
        Ok(())
    }
}

/// A recipient-authored durable acknowledgement.  The message data stays in
/// Iroh Docs; this just lets the sender distinguish a local commit from an
/// observed remote replica.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplicatedReceipt {
    version: u8,
    message_id: String,
    delivered_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileReceipt {
    message_id: String,
    hash: String,
    receiver_id: String,
    received_at: i64,
}

impl FileReceipt {
    fn entry_key(&self) -> String {
        format!(
            "file-receipt/{}/{}/{}",
            self.message_id, self.hash, self.receiver_id
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileSharingState {
    message_id: String,
    hash: String,
    serving: bool,
    changed_at: i64,
}

impl FileSharingState {
    fn entry_key(&self) -> String {
        format!("file-sharing/{}/{}", self.message_id, self.hash)
    }
}

impl ReplicatedReceipt {
    fn new(message_id: String) -> Self {
        Self {
            version: 1,
            message_id,
            delivered_at: now_millis(),
        }
    }

    fn entry_key(&self) -> String {
        format!("receipt/{}", self.message_id)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported receipt version {}", self.version);
        }
        if !is_message_id(&self.message_id) {
            bail!("receipt has an invalid message id");
        }
        Ok(())
    }
}

impl Ord for ChatMessage {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.sent_at, &self.author_id, self.nonce, &self.message_id).cmp(&(
            other.sent_at,
            &other.author_id,
            other.nonce,
            &other.message_id,
        ))
    }
}

impl PartialOrd for ChatMessage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConversationKind {
    Direct { peer_id: String },
    Group,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatConversation {
    pub id: String,
    pub title: String,
    pub kind: ConversationKind,
    pub members: Vec<String>,
    pub document_id: String,
    /// Bumped when history is hard-deleted and the conversation rotates onto a
    /// fresh empty document. Higher epoch always wins on invite.
    #[serde(default)]
    pub history_epoch: u64,
}

impl ChatConversation {
    pub fn direct_peer(&self) -> Option<NodeId> {
        match &self.kind {
            ConversationKind::Direct { peer_id } => NodeId::from_str(peer_id).ok(),
            ConversationKind::Group => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Peer was reachable; waiting on remote receipt.
    Pending,
    /// Active delivery attempts still running.
    Retrying,
    /// Saved locally; no member reachable right now — not aggressively retried.
    Queued,
    Delivered,
    Failed,
}

#[derive(Debug, Clone)]
pub enum ChatNotification {
    Conversation {
        conversation: ChatConversation,
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    AttachmentData {
        hash: String,
        data: Arc<Vec<u8>>,
    },
    FileTransfer {
        message_id: String,
        hash: String,
        result: std::result::Result<PathBuf, String>,
    },
    FileTransferUpdate {
        message_id: String,
        hash: String,
        path: PathBuf,
        received: u64,
        total: u64,
        phase: FileTransferPhase,
    },
    FileTransferCancelled {
        message_id: String,
        hash: String,
    },
    FileServing {
        hash: String,
        connection_id: u64,
        request_id: u64,
        position: u64,
        total: u64,
        phase: FileServingPhase,
    },
    FileOfferPrepared,
    RetentionSweep,
    /// Identity snapshot learned from an invite/message fast-path, for a peer
    /// we may never have saved as a contact.
    PeerIdentity {
        peer: String,
        display_name: Option<String>,
        avatar_hash: Option<String>,
        accent_color: Option<String>,
    },
    Delivery {
        message_id: String,
        state: DeliveryState,
        detail: Option<String>,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransferPhase {
    Connecting,
    Downloading,
    Reconnecting,
    Saving,
    Paused(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileServingPhase {
    Sending,
    Sent,
    Interrupted,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoppedFileIndex {
    #[serde(default)]
    hashes: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingFileDownload {
    conversation_id: String,
    message_id: String,
    hash: String,
    path: PathBuf,
    byte_len: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PendingFileDownloadIndex {
    downloads: Vec<PendingFileDownload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConversation {
    #[serde(flatten)]
    public: ChatConversation,
    ticket: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatIndex {
    #[serde(default)]
    conversations: BTreeMap<String, StoredConversation>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LocalDeletionIndex {
    #[serde(default)]
    conversations: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingAttachmentDelivery {
    message_id: String,
    hash: String,
    byte_len: u64,
    #[serde(default)]
    attachment_kind: AttachmentKind,
    #[serde(default)]
    sent_at: i64,
    pending_peers: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingDeletionDelivery {
    deletion: ReplicatedDeletion,
    pending_peers: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingInviteDelivery {
    history_epoch: u64,
    pending_peers: BTreeSet<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ReliableControlIndex {
    #[serde(default)]
    attachments: BTreeMap<String, BTreeMap<String, PendingAttachmentDelivery>>,
    #[serde(default)]
    deletions: BTreeMap<String, BTreeMap<String, PendingDeletionDelivery>>,
    #[serde(default)]
    invites: BTreeMap<String, PendingInviteDelivery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatInvite {
    version: u8,
    conversation: ChatConversation,
    ticket: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inviter_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inviter_avatar_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inviter_accent_color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ChatProtocolMessage {
    Invite(ChatInvite),
    AttachmentPush(AttachmentPush),
    SyncRequest(SyncRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentPush {
    kind: String,
    version: u8,
    conversation_id: String,
    hash: String,
    byte_len: u64,
    #[serde(default)]
    attachment_kind: AttachmentKind,
    #[serde(default)]
    sent_at: i64,
    #[serde(skip)]
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncRequest {
    kind: String,
    version: u8,
    conversation_id: String,
    /// Fresh write ticket (includes current relay/direct addrs). Optional for
    /// backward compatibility with older peers. Lets the receiver start_sync
    /// toward the sender even when their endpoint address book is stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ticket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_version: Option<String>,
    /// Optional message bodies for chat-ALPN fast path. Older peers ignore this
    /// field and still pull via docs; new peers insert immediately so delivery
    /// does not wait on outbound docs `Connect(DirectJoin)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    messages: Vec<ChatMessage>,
    /// Optional receipts so the sender can mark Delivered without waiting for
    /// a docs pull of the recipient's receipt entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    receipts: Vec<ReplicatedReceipt>,
    /// Optional author tombstones for immediate delete-for-everyone updates.
    /// The durable source remains Iroh Docs; this fast path covers peers that
    /// can receive our chat connection but cannot dial our Docs endpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deletions: Vec<ReplicatedDeletion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachment_acks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    deletion_acks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepted_history_epoch: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct WakePayload {
    messages: Vec<ChatMessage>,
    receipts: Vec<ReplicatedReceipt>,
    deletions: Vec<ReplicatedDeletion>,
    attachment_acks: Vec<String>,
    deletion_acks: Vec<String>,
    accepted_history_epoch: Option<u64>,
    attachments: Vec<PendingAttachmentDelivery>,
}

#[derive(Debug)]
pub(crate) struct IncomingInvite {
    remote: NodeId,
    message: ChatProtocolMessage,
}

#[derive(Clone)]
pub struct ChatInviteProtocol {
    tx: async_channel::Sender<IncomingInvite>,
    sessions: ChatSessionPool,
}

impl std::fmt::Debug for ChatInviteProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatInviteProtocol").finish_non_exhaustive()
    }
}

impl ChatInviteProtocol {
    fn new(sessions: ChatSessionPool) -> (Self, async_channel::Receiver<IncomingInvite>) {
        let (tx, rx) = async_channel::bounded(64);
        (Self { tx, sessions }, rx)
    }
}

impl ProtocolHandler for ChatInviteProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let tx = self.tx.clone();
        let sessions = self.sessions.clone();
        async move {
            let connection = connecting.await?;
            let remote = connection.remote_node_id()?;
            info!(peer = %remote.fmt_short(), "received chat protocol connection");
            // Teach the endpoint this peer's dialable address so docs/gossip can
            // Connect — chat ALPN often works while docs DirectJoin still has no
            // usable NodeAddr (see docs/chat-delivery-asymmetry.md).
            sessions.remember_connection(remote, &connection);
            let pooled = sessions.insert(remote, connection.clone()).await;
            // Keep accepting bi-streams until idle so rapid chatter reuses this
            // session instead of dialing again (see docs/chat-keepalive-sessions.md).
            let mut idle = Box::pin(tokio::time::sleep(CHAT_SESSION_IDLE));
            loop {
                tokio::select! {
                    bi = connection.accept_bi() => {
                        match bi {
                            Ok((mut send, mut recv)) => {
                                sessions.touch(remote).await;
                                sessions.remember_connection(remote, &connection);
                                idle.as_mut().reset(
                                    tokio::time::Instant::now() + CHAT_SESSION_IDLE,
                                );
                                match tokio::time::timeout(
                                    CHAT_STREAM_TIMEOUT,
                                    accept_chat_stream(&mut send, &mut recv),
                                )
                                .await
                                {
                                    Ok(Ok(message)) => {
                                        if tx
                                            .send(IncomingInvite { remote, message })
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    Ok(Err(error)) => {
                                        warn!(
                                            peer = %remote.fmt_short(),
                                            "chat session stream failed: {error:#}"
                                        );
                                        break;
                                    }
                                    Err(_) => {
                                        warn!(
                                            peer = %remote.fmt_short(),
                                            "chat session stream timed out"
                                        );
                                        break;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = &mut idle => {
                        debug!(peer = %remote.fmt_short(), "chat session idle timeout");
                        break;
                    }
                    _ = connection.closed() => break,
                }
            }
            // Forget the pool entry but do not force-close — the peer may still
            // be using this QUIC session for in-flight docs/blob traffic.
            if pooled {
                sessions.forget_if(remote, &connection).await;
            }
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        async move {}.boxed()
    }
}

/// Shared outbound/inbound chat QUIC sessions kept warm for bursty messaging.
#[derive(Clone)]
struct ChatSessionPool {
    endpoint: Endpoint,
    inner: Arc<tokio::sync::Mutex<SessionPoolState>>,
}

struct SessionPoolState {
    sessions: BTreeMap<NodeId, HotSession>,
    /// Serialize dial/send per peer so concurrent wakes don't kill each other's
    /// fresh connections (seen as rapid reused=false + multi-second gaps).
    peer_gates: BTreeMap<NodeId, Arc<tokio::sync::Mutex<()>>>,
}

struct HotSession {
    connection: Connection,
    last_used: tokio::time::Instant,
}

impl ChatSessionPool {
    fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            inner: Arc::new(tokio::sync::Mutex::new(SessionPoolState {
                sessions: BTreeMap::new(),
                peer_gates: BTreeMap::new(),
            })),
        }
    }

    async fn peer_gate(&self, peer: NodeId) -> Arc<tokio::sync::Mutex<()>> {
        let mut guard = self.inner.lock().await;
        if guard.peer_gates.len() >= CHAT_PEER_GATE_CAP {
            guard
                .peer_gates
                .retain(|_, gate| Arc::strong_count(gate) > 1);
        }
        guard
            .peer_gates
            .entry(peer)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn remember_connection(&self, peer: NodeId, _connection: &Connection) {
        // After a successful chat ALPN connect/accept, magicsock knows how to
        // reach this peer. Re-inject that RemoteInfo so docs/gossip DirectJoin
        // can use the same path instead of timing out with no addresses.
        let Some(info) = self.endpoint.remote_info(peer) else {
            return;
        };
        let node_addr: NodeAddr = info.into();
        if node_addr.direct_addresses.is_empty() && node_addr.relay_url.is_none() {
            return;
        }
        if let Err(error) = self.endpoint.add_node_addr(node_addr) {
            trace!(peer = %peer.fmt_short(), "failed to record chat peer address: {error:#}");
        }
    }

    async fn insert(&self, peer: NodeId, connection: Connection) -> bool {
        self.remember_connection(peer, &connection);
        let mut guard = self.inner.lock().await;
        Self::sweep_locked(&mut guard.sessions);
        // Already have a warm session for this peer. Keep it and leave the new
        // connection alone — closing "dup" inbound/outbound sessions was killing
        // accept loops (`closed by peer: chat-replaced` / `chat-dup-dial`).
        if guard.sessions.contains_key(&peer) {
            if let Some(session) = guard.sessions.get_mut(&peer) {
                session.last_used = tokio::time::Instant::now();
            }
            return false;
        }
        guard.sessions.insert(
            peer,
            HotSession {
                connection,
                last_used: tokio::time::Instant::now(),
            },
        );
        Self::evict_locked(&mut guard.sessions);
        true
    }

    async fn touch(&self, peer: NodeId) {
        let mut guard = self.inner.lock().await;
        if let Some(session) = guard.sessions.get_mut(&peer) {
            session.last_used = tokio::time::Instant::now();
        }
    }

    async fn get(&self, peer: NodeId) -> Option<Connection> {
        let mut guard = self.inner.lock().await;
        Self::sweep_locked(&mut guard.sessions);
        guard
            .sessions
            .get(&peer)
            .map(|session| session.connection.clone())
    }

    /// Drop a pooled handle without closing the QUIC connection.
    async fn forget_if(&self, peer: NodeId, connection: &Connection) {
        let mut guard = self.inner.lock().await;
        if guard
            .sessions
            .get(&peer)
            .is_some_and(|session| session.connection.stable_id() == connection.stable_id())
        {
            guard.sessions.remove(&peer);
        }
    }

    fn sweep_locked(sessions: &mut BTreeMap<NodeId, HotSession>) {
        let now = tokio::time::Instant::now();
        sessions.retain(|_, session| now.duration_since(session.last_used) < CHAT_SESSION_IDLE);
    }

    fn evict_locked(sessions: &mut BTreeMap<NodeId, HotSession>) {
        while sessions.len() > CHAT_SESSION_POOL_CAP {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, session)| session.last_used)
                .map(|(peer, _)| *peer);
            let Some(peer) = oldest else {
                break;
            };
            sessions.remove(&peer);
        }
    }

    async fn dial(&self, peer: NodeId) -> Result<Connection> {
        if let Some(existing) = self.get(peer).await {
            return Ok(existing);
        }
        let connection = tokio::time::timeout(
            CHAT_CONNECT_TIMEOUT,
            self.endpoint.connect(NodeAddr::from(peer), CHAT_ALPN),
        )
        .await
        .context("chat connect timed out")?
        .context("chat connect failed")?;
        self.remember_connection(peer, &connection);
        self.insert(peer, connection.clone()).await;
        // Another task may have won the insert race — prefer pooled session.
        Ok(self.get(peer).await.unwrap_or(connection))
    }
}

pub struct ChatProtocols {
    pub provider: GuardedBlobProvider,
    pub docs: Docs<BlobStore>,
    pub gossip: Gossip,
    pub invites: ChatInviteProtocol,
    pub service: ChatService,
}

pub struct ChatService {
    endpoint: Endpoint,
    sessions: ChatSessionPool,
    docs: MemClient,
    blobs: BlobStore,
    author: AuthorId,
    root: PathBuf,
    index: ChatIndex,
    local_deletions: LocalDeletionIndex,
    reliable_control: ReliableControlIndex,
    our_node_id: NodeId,
    invite_rx: async_channel::Receiver<IncomingInvite>,
    doc_event_tx: async_channel::Sender<DocumentSignal>,
    doc_event_rx: async_channel::Receiver<DocumentSignal>,
    wake_tx: async_channel::Sender<ChatInput>,
    wake_rx: async_channel::Receiver<ChatInput>,
    provider_event_rx: async_channel::Receiver<iroh_blobs::provider::Event>,
    subscriptions: BTreeMap<String, String>,
    queued: VecDeque<ChatNotification>,
    retry_tick: tokio::time::Interval,
    retry_state: BTreeMap<String, ConversationRetry>,
    pending_deliveries: BTreeMap<String, String>,
    /// Full outbound bodies for pending deliveries (chat-ALPN fast path).
    pending_outbound: BTreeMap<String, ChatMessage>,
    /// Inbound bodies received over chat ALPN before docs sync lands them.
    /// Keyed conversation_id → message_id → message. Never written under our
    /// docs author (that would forge peer entries); merged into the timeline
    /// until `load_messages` sees the real replica.
    staged_inbound: BTreeMap<String, BTreeMap<String, ChatMessage>>,
    /// Author-validated tombstones received over the chat ALPN before Docs
    /// replication lands them. conversation_id -> message_id -> author_id.
    staged_deletions: BTreeMap<String, BTreeMap<String, String>>,
    /// In-flight background wakes per conversation — prevents a losing race
    /// from parking deliveries as Queued right after a successful wake.
    wake_inflight: BTreeMap<String, u32>,
    /// Last time we spawned a receipt-driven wake for a conversation.
    last_receipt_wake: BTreeMap<String, tokio::time::Instant>,
    /// Consecutive failed wake probes per conversation.
    wake_failures: BTreeMap<String, u8>,
    max_image_bytes: Option<u64>,
    retention: RetentionPolicy,
    /// Own identity snapshot stamped onto outbound messages + invites.
    own_display_name: Option<String>,
    own_avatar_hash: Option<String>,
    own_accent_color: Option<String>,
    next_retention_sweep: tokio::time::Instant,
    attachment_downloads: BTreeSet<Hash>,
    attachment_retries: BTreeMap<Hash, AttachmentRetry>,
    recorded_file_receipts: BTreeSet<String>,
    timeline_limits: BTreeMap<String, usize>,
    control_retries: BTreeMap<String, ControlRetry>,
    blob_downloader: iroh_blobs::downloader::Downloader,
    pending_file_downloads: BTreeMap<(String, String), PendingFileDownload>,
    active_file_downloads: BTreeMap<(String, String), (u64, watch::Sender<bool>)>,
    stopped_file_index: StoppedFileIndex,
    stopped_hashes: StoppedHashes,
    offered_file_sizes: BTreeMap<Hash, u64>,
    serving_requests: BTreeMap<(u64, u64), ServingRequest>,
    #[cfg(test)]
    invite_attempts: AtomicU64,
    #[cfg(test)]
    direct_deletions_accepted: AtomicU64,
}

#[derive(Debug)]
struct ConversationRetry {
    attempts: u8,
    next_attempt: tokio::time::Instant,
    messages: BTreeSet<String>,
}

#[derive(Debug)]
struct AttachmentRetry {
    conversation_id: String,
    attempts: u8,
    next_attempt: tokio::time::Instant,
}

struct ServingRequest {
    hash: Hash,
    total: u64,
    position: u64,
    last_emit: tokio::time::Instant,
}

#[derive(Debug)]
struct ControlRetry {
    attempts: u8,
    next_attempt: tokio::time::Instant,
}

pub(crate) enum ChatInput {
    Invite(Box<IncomingInvite>),
    Document(DocumentSignal),
    /// Background wake finished (send path does not block on dial/reuse).
    WakeFinished {
        conversation_id: String,
        reached: bool,
    },
    AttachmentDownloadFinished {
        conversation_id: String,
        hash: Hash,
        byte_len: u64,
        succeeded: bool,
    },
    FilesImported {
        conversation_id: String,
        body: String,
        attachments: Vec<ChatAttachment>,
        result: std::result::Result<Vec<ChatAttachment>, String>,
    },
    FileDownloadFinished {
        conversation_id: String,
        message_id: String,
        hash: String,
        attempt: u64,
        result: std::result::Result<PathBuf, String>,
    },
    FileDownloadProgress {
        message_id: String,
        hash: String,
        attempt: u64,
        path: PathBuf,
        received: u64,
        total: u64,
        phase: FileTransferPhase,
    },
    BlobProviderEvent(iroh_blobs::provider::Event),
    Retry,
}

#[derive(Debug)]
pub(crate) enum DocumentSignal {
    Changed {
        conversation_id: String,
        /// True when the live event came from a remote peer (or a sync finish).
        /// Used to resume queued offline deliveries without treating local
        /// inserts as “peer is online”.
        from_remote: bool,
    },
    SubscriptionEnded {
        conversation_id: String,
        document_id: String,
    },
}

impl ChatService {
    pub async fn build(endpoint: Endpoint, config_dir: &Path) -> Result<ChatProtocols> {
        let root = config_dir.join("chat");
        info!(storage = %root.display(), "opening text chat storage");
        tokio::fs::create_dir_all(&root).await?;
        let blobs_path = root.join("blobs");
        tokio::fs::create_dir_all(&blobs_path).await?;
        let docs_path = root.join("docs");
        tokio::fs::create_dir_all(&docs_path).await?;
        let blob_store = BlobStore::load(&blobs_path).await?;
        debug!("chat blob store opened");
        let stopped_file_index: StoppedFileIndex =
            load_local_json(&root.join("stopped-files.json"), "stopped file index");
        let stopped_hashes: StoppedHashes = Arc::new(RwLock::new(
            stopped_file_index
                .hashes
                .iter()
                .filter_map(|hash| Hash::from_str(hash).ok())
                .collect(),
        ));
        let (provider_event_tx, provider_event_rx) = async_channel::bounded(512);
        let blobs = Blobs::builder(blob_store.clone())
            .events(GuardedBlobProvider::events(provider_event_tx))
            .build(&endpoint);
        let provider = GuardedBlobProvider::new(blobs.clone(), stopped_hashes.clone());
        let blob_downloader = blobs.downloader().clone();
        let gossip = Gossip::builder().spawn(endpoint.clone()).await?;
        let docs = Docs::persistent(docs_path).spawn(&blobs, &gossip).await?;
        // Docs entries store their values in the shared blob store. Register
        // those hashes before starting GC or message bodies will be collected
        // while their document metadata remains intact.
        blobs.add_protected(docs.protect_cb())?;
        blobs.start_gc(GcConfig {
            period: Duration::from_secs(30 * 60),
            done_callback: None,
        })?;
        debug!("persistent Iroh Docs engine opened");
        let sessions = ChatSessionPool::new(endpoint.clone());
        let (invites, invite_rx) = ChatInviteProtocol::new(sessions.clone());
        let client = docs.client().clone();
        let author = client.authors().default().await?;
        let (doc_event_tx, doc_event_rx) = async_channel::bounded(256);
        let (wake_tx, wake_rx) = async_channel::bounded(64);
        let index = load_index(&root.join("index.json"));
        let local_deletions = load_local_deletions(&root.join("local-deletions.json"));
        let reliable_control = load_reliable_control(&root.join("reliable-control.json"));
        let pending_file_downloads = load_pending_file_downloads(&root.join("file-downloads.json"));
        let conversation_count = index.conversations.len();
        let mut service = Self {
            endpoint: endpoint.clone(),
            sessions,
            docs: client,
            blobs: blob_store,
            author,
            root,
            index,
            local_deletions,
            reliable_control,
            our_node_id: endpoint.node_id(),
            invite_rx,
            doc_event_tx,
            doc_event_rx,
            wake_tx,
            wake_rx,
            provider_event_rx,
            subscriptions: BTreeMap::new(),
            queued: VecDeque::new(),
            retry_tick: tokio::time::interval_at(
                tokio::time::Instant::now() + RETRY_TICK,
                RETRY_TICK,
            ),
            retry_state: BTreeMap::new(),
            pending_deliveries: BTreeMap::new(),
            pending_outbound: BTreeMap::new(),
            staged_inbound: BTreeMap::new(),
            staged_deletions: BTreeMap::new(),
            wake_inflight: BTreeMap::new(),
            last_receipt_wake: BTreeMap::new(),
            wake_failures: BTreeMap::new(),
            max_image_bytes: None,
            retention: RetentionPolicy::Unlimited,
            own_display_name: None,
            own_avatar_hash: None,
            own_accent_color: None,
            next_retention_sweep: tokio::time::Instant::now() + Duration::from_secs(60 * 60),
            attachment_downloads: BTreeSet::new(),
            attachment_retries: BTreeMap::new(),
            recorded_file_receipts: BTreeSet::new(),
            timeline_limits: BTreeMap::new(),
            control_retries: BTreeMap::new(),
            blob_downloader,
            pending_file_downloads,
            active_file_downloads: BTreeMap::new(),
            stopped_file_index,
            stopped_hashes,
            offered_file_sizes: BTreeMap::new(),
            serving_requests: BTreeMap::new(),
            #[cfg(test)]
            invite_attempts: AtomicU64::new(0),
            #[cfg(test)]
            direct_deletions_accepted: AtomicU64::new(0),
        };
        service.initialize().await;
        info!(
            node = %service.our_node_id.fmt_short(),
            conversations = conversation_count,
            storage = %service.root.display(),
            "text chat service ready"
        );
        Ok(ChatProtocols {
            provider,
            docs,
            gossip,
            invites,
            service,
        })
    }

    async fn initialize(&mut self) {
        let ids: Vec<_> = self.index.conversations.keys().cloned().collect();
        for id in ids {
            if let Err(error) = self.open_and_publish(&id).await {
                warn!(conversation = %log_id(&id), "failed to open persisted chat: {error:#}");
                self.queued.push_back(ChatNotification::Error(format!(
                    "Could not open chat {id}: {error:#}"
                )));
            }
        }
        self.restore_pending_delivery_retries().await;
        self.initialize_invite_deliveries();
        self.restore_control_retries();
        self.retry_due_controls();
        self.restore_file_downloads().await;
    }

    async fn restore_pending_delivery_retries(&mut self) {
        let conversations: Vec<_> = self
            .index
            .conversations
            .iter()
            .map(|(id, stored)| (id.clone(), stored.clone()))
            .collect();
        let mut pending_by_conversation: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (conversation_id, stored) in conversations {
            let Ok(messages) = self.load_messages(&stored).await else {
                continue;
            };
            let Ok(delivered) = self.load_delivered_message_ids(&stored).await else {
                continue;
            };
            let our_node_id = self.our_node_id.to_string();
            for message in messages.into_iter().filter(|message| {
                message.author_id == our_node_id && !delivered.contains(&message.message_id)
            }) {
                self.pending_deliveries
                    .insert(message.message_id.clone(), conversation_id.clone());
                self.pending_outbound
                    .insert(message.message_id.clone(), message.clone());
                pending_by_conversation
                    .entry(conversation_id.clone())
                    .or_default()
                    .push(message.message_id);
            }
        }
        for (conversation_id, message_ids) in pending_by_conversation {
            // Do not dial during startup — that blocked the worker for seconds
            // per conversation when peers were offline.
            for message_id in &message_ids {
                self.schedule_delivery_retry(&conversation_id, message_id, true);
                self.queued.push_back(ChatNotification::Delivery {
                    message_id: message_id.clone(),
                    state: DeliveryState::Pending,
                    detail: Some("delivering".to_owned()),
                });
            }
            self.spawn_wake(&conversation_id);
            self.spawn_doc_sync(&conversation_id);
        }
    }

    #[allow(dead_code)]
    pub async fn next_notification(&mut self) -> ChatNotification {
        loop {
            if let Some(notification) = self.pop_notification() {
                return notification;
            }
            let input = self.wait_input().await;
            if let Some(notification) = self.process_input(input).await {
                return notification;
            }
        }
    }

    pub(crate) fn pop_notification(&mut self) -> Option<ChatNotification> {
        self.queued.pop_front()
    }

    pub(crate) async fn wait_input(&mut self) -> ChatInput {
        tokio::select! {
            incoming = self.invite_rx.recv() => ChatInput::Invite(Box::new(
                incoming.expect("chat invitation protocol channel closed")
            )),
            changed = self.doc_event_rx.recv() => ChatInput::Document(
                changed.expect("chat document event channel closed")
            ),
            wake = self.wake_rx.recv() => wake.expect("chat wake channel closed"),
            provider = self.provider_event_rx.recv(), if !self.provider_event_rx.is_closed() => {
                match provider {
                    Ok(event) => ChatInput::BlobProviderEvent(event),
                    Err(_) => ChatInput::Retry,
                }
            },
            _ = self.retry_tick.tick() => ChatInput::Retry,
        }
    }

    pub(crate) async fn process_input(&mut self, input: ChatInput) -> Option<ChatNotification> {
        match input {
            ChatInput::Invite(incoming) => match incoming.message {
                ChatProtocolMessage::Invite(invite) => {
                    let remote = incoming.remote;
                    let conversation_id = invite.conversation.id.clone();
                    let history_epoch = invite.conversation.history_epoch;
                    note_peer_client_version(remote, invite.client_version.as_deref(), "invite");
                    if let Err(error) = self.accept_invite(remote, invite).await {
                        return Some(ChatNotification::Error(format!(
                            "Chat invitation failed: {error:#}"
                        )));
                    }
                    self.reliable_control.invites.insert(
                        conversation_id.clone(),
                        PendingInviteDelivery {
                            history_epoch,
                            pending_peers: BTreeSet::new(),
                        },
                    );
                    self.save_reliable_control();
                    self.spawn_control_ack(
                        &conversation_id,
                        remote,
                        vec![],
                        vec![],
                        Some(history_epoch),
                    );
                    self.resume_deliveries_for_peer(remote).await;
                }
                ChatProtocolMessage::SyncRequest(request) => {
                    if request.kind != "sync-request" || request.version != 1 {
                        return Some(ChatNotification::Error(format!(
                            "Unsupported chat sync request version {}",
                            request.version
                        )));
                    }
                    note_peer_client_version(
                        incoming.remote,
                        request.client_version.as_deref(),
                        "sync-request",
                    );
                    if self.is_conversation_member(&request.conversation_id, incoming.remote) {
                        if let Err(error) = self.apply_sync_request(incoming.remote, &request).await
                        {
                            warn!(
                                conversation = %log_id(&request.conversation_id),
                                peer = %incoming.remote.fmt_short(),
                                "failed to apply chat sync request: {error:#}"
                            );
                        }
                        self.resume_deliveries_for_peer(incoming.remote).await;
                    } else {
                        warn!(
                            conversation = %log_id(&request.conversation_id),
                            peer = %incoming.remote.fmt_short(),
                            "ignored chat sync request from a non-member"
                        );
                    }
                }
                ChatProtocolMessage::AttachmentPush(push) => {
                    let remote = incoming.remote;
                    if let Err(error) = self.apply_attachment_push(remote, push).await {
                        warn!(
                            peer = %remote.fmt_short(),
                            "ignored chat attachment push: {error:#}"
                        );
                    }
                }
            },
            ChatInput::Document(signal) => match signal {
                DocumentSignal::Changed {
                    conversation_id,
                    from_remote,
                } => {
                    // Only resume parked sends on real remote inserts — not on
                    // every SyncFinished (that caused a wake/resume storm).
                    if from_remote {
                        self.resume_queued_deliveries(&conversation_id).await;
                    }
                    if let Err(error) = self.publish_timeline(&conversation_id).await {
                        return Some(ChatNotification::Error(format!(
                            "Could not refresh chat: {error:#}"
                        )));
                    }
                }
                DocumentSignal::SubscriptionEnded {
                    conversation_id,
                    document_id,
                } => {
                    if self.subscriptions.get(&conversation_id) == Some(&document_id) {
                        self.subscriptions.remove(&conversation_id);
                        warn!(
                            conversation = %log_id(&conversation_id),
                            "chat document subscription stopped; reattaching"
                        );
                        if let Err(error) = self.open_and_publish(&conversation_id).await {
                            return Some(ChatNotification::Error(format!(
                                "Could not restore live chat updates: {error:#}"
                            )));
                        }
                    }
                }
            },
            ChatInput::WakeFinished {
                conversation_id,
                reached,
            } => {
                self.apply_wake_finished(&conversation_id, reached).await;
            }
            ChatInput::AttachmentDownloadFinished {
                conversation_id,
                hash,
                byte_len,
                succeeded,
            } => {
                self.attachment_downloads.remove(&hash);
                if succeeded {
                    self.attachment_retries.remove(&hash);
                    if let Err(error) = self.queue_attachment_data(hash, byte_len).await {
                        return Some(ChatNotification::Error(format!(
                            "Could not refresh downloaded image: {error:#}"
                        )));
                    }
                    if let Err(error) = self.publish_timeline(&conversation_id).await {
                        warn!("could not record received attachment: {error:#}");
                    }
                } else {
                    self.schedule_attachment_retry(&conversation_id, hash);
                }
            }
            ChatInput::FilesImported {
                conversation_id,
                body,
                mut attachments,
                result,
            } => {
                match result {
                    Ok(files) => {
                        attachments.extend(files);
                        let mut message =
                            ChatMessage::new_with_attachments(self.our_node_id, body, attachments);
                        self.stamp_own_profile(&mut message);
                        if !self.send_message(conversation_id, message).await {
                            self.queued.push_back(ChatNotification::Error(
                                "Could not publish file offer".to_owned(),
                            ));
                        }
                    }
                    Err(error) => self.queued.push_back(ChatNotification::Error(format!(
                        "Could not offer file: {error}"
                    ))),
                }
                self.queued.push_back(ChatNotification::FileOfferPrepared);
            }
            ChatInput::FileDownloadFinished {
                conversation_id,
                message_id,
                hash,
                attempt,
                mut result,
            } => {
                let key = (message_id.clone(), hash.clone());
                if !self
                    .active_file_downloads
                    .get(&key)
                    .is_some_and(|(current, _)| *current == attempt)
                {
                    return None;
                }
                self.active_file_downloads.remove(&key);
                if result.is_ok() {
                    if let Err(error) = self
                        .record_file_receipt(&conversation_id, &message_id, &hash)
                        .await
                    {
                        warn!("file saved but receipt could not be recorded: {error:#}");
                        result = Err(format!(
                            "file saved, but its receive receipt could not be recorded: {error:#}"
                        ));
                    } else {
                        self.pending_file_downloads.remove(&key);
                        if let Err(error) = self.save_pending_file_downloads() {
                            warn!("could not save completed file download: {error:#}");
                        }
                        if let Err(error) = self.release_download_pin(&hash).await {
                            warn!("could not release completed file download pin: {error:#}");
                        }
                        self.spawn_doc_sync(&conversation_id);
                    }
                    if let Err(error) = self.publish_timeline(&conversation_id).await {
                        warn!("could not refresh file receipt: {error:#}");
                    }
                }
                self.queued.push_back(ChatNotification::FileTransfer {
                    message_id,
                    hash,
                    result,
                });
            }
            ChatInput::FileDownloadProgress {
                message_id,
                hash,
                attempt,
                path,
                received,
                total,
                phase,
            } => {
                if self
                    .active_file_downloads
                    .get(&(message_id.clone(), hash.clone()))
                    .is_some_and(|(current, _)| *current == attempt)
                {
                    self.queued.push_back(ChatNotification::FileTransferUpdate {
                        message_id,
                        hash,
                        path,
                        received,
                        total,
                        phase,
                    });
                }
            }
            ChatInput::BlobProviderEvent(event) => self.handle_blob_provider_event(event),
            ChatInput::Retry => {
                if self.next_retention_sweep <= tokio::time::Instant::now() {
                    self.next_retention_sweep =
                        tokio::time::Instant::now() + Duration::from_secs(60 * 60);
                    if let Err(error) = self.release_expired_attachment_tags().await {
                        warn!("could not release expired attachment blobs: {error:#}");
                    }
                    if self.retention != RetentionPolicy::Unlimited {
                        self.queued.push_back(ChatNotification::RetentionSweep);
                    }
                }
                self.retry_due_attachment_downloads().await;
                self.retry_due_deliveries().await;
                self.retry_due_controls();
            }
        }
        self.pop_notification()
    }

    pub async fn ensure_direct(&mut self, peer: NodeId, title: String) -> Result<String> {
        let id = direct_conversation_id(self.our_node_id, peer);
        if !self.index.conversations.contains_key(&id) {
            let members = sorted_members([self.our_node_id, peer]);
            let stored = self
                .create_conversation(
                    id.clone(),
                    title,
                    ConversationKind::Direct {
                        peer_id: peer.to_string(),
                    },
                    members,
                    0,
                )
                .await?;
            self.index.conversations.insert(id.clone(), stored);
            self.persist_index()?;
            self.open_and_publish(&id).await?;
            info!(
                conversation = %log_id(&id),
                peer = %peer.fmt_short(),
                "created direct-message replica"
            );
            // Await invite so an immediate first send's SyncRequest is not
            // ignored as non-member on the peer.
            self.invite_members_wait(&id).await;
        }
        Ok(id)
    }

    pub async fn create_group(&mut self, title: String, members: Vec<NodeId>) -> Result<String> {
        let title = title.trim();
        if title.is_empty() {
            bail!("group name is empty");
        }
        let mut all_members = members;
        all_members.push(self.our_node_id);
        all_members.sort();
        all_members.dedup();
        if all_members.len() < 2 {
            bail!("choose at least one other group member");
        }
        let member_count = all_members.len();
        let id = format!(
            "group/{}/{:020}/{:016x}",
            self.our_node_id,
            now_millis(),
            next_nonce()
        );
        let stored = self
            .create_conversation(
                id.clone(),
                title.chars().take(128).collect(),
                ConversationKind::Group,
                sorted_members(all_members),
                0,
            )
            .await?;
        self.index.conversations.insert(id.clone(), stored);
        self.persist_index()?;
        self.open_and_publish(&id).await?;
        info!(
            conversation = %log_id(&id),
            members = member_count,
            "created group-chat replica"
        );
        self.invite_members_wait(&id).await;
        Ok(id)
    }

    pub async fn send_message(&mut self, conversation_id: String, message: ChatMessage) -> bool {
        let mut message = message;
        self.stamp_own_profile(&mut message);
        if let Err(error) = self.import_attachment_bytes(&message).await {
            self.queued.push_back(ChatNotification::Delivery {
                message_id: message.message_id.clone(),
                state: DeliveryState::Failed,
                detail: Some(error.to_string()),
            });
            return false;
        }
        let message_id = message.message_id.clone();
        let body_bytes = message.body.len();
        match self.insert_message(&conversation_id, &message).await {
            Ok(()) => {
                if let Some(stored) = self.index.conversations.get(&conversation_id) {
                    let stopped_offers: Vec<_> = message
                        .attachments
                        .iter()
                        .filter(|attachment| {
                            attachment.kind == AttachmentKind::FileOffer
                                && self.stopped_file_index.hashes.contains(&attachment.hash)
                        })
                        .collect();
                    if !stopped_offers.is_empty() {
                        let result: Result<()> = async {
                            let doc_id = NamespaceId::from_str(&stored.public.document_id)?;
                            let doc = self
                                .docs
                                .open(doc_id)
                                .await?
                                .context("conversation document unavailable")?;
                            for attachment in stopped_offers {
                                let state = FileSharingState {
                                    message_id: message_id.clone(),
                                    hash: attachment.hash.clone(),
                                    serving: false,
                                    changed_at: now_millis(),
                                };
                                doc.set_bytes(
                                    self.author,
                                    state.entry_key(),
                                    serde_json::to_vec(&state)?,
                                )
                                .await?;
                            }
                            Ok(())
                        }
                        .await;
                        if let Err(error) = result {
                            warn!("could not publish stopped state for new file offer: {error:#}");
                            self.queued.push_back(ChatNotification::Error(format!(
                                "File offer was published, but sharing state could not sync: {error:#}"
                            )));
                        }
                    }
                }
                info!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    bytes = body_bytes,
                    "message committed to local chat replica"
                );
                self.pending_deliveries
                    .insert(message_id.clone(), conversation_id.clone());
                self.pending_outbound
                    .insert(message_id.clone(), message.clone());
                self.register_attachment_deliveries(&conversation_id, &message);
                // Optimistic pending — do not block the chat worker on dial/reuse.
                // A background wake nudges peers; WakeFinished parks as Queued if
                // nobody is reachable. See docs/chat-delivery-asymmetry.md.
                self.schedule_delivery_retry(&conversation_id, &message_id, false);
                self.queued.push_back(ChatNotification::Delivery {
                    message_id: message_id.clone(),
                    state: DeliveryState::Pending,
                    detail: Some("delivering".to_owned()),
                });
                self.spawn_doc_sync(&conversation_id);
                self.spawn_wake(&conversation_id);
                if let Err(error) = self.publish_timeline(&conversation_id).await {
                    warn!(
                        conversation = %log_id(&conversation_id),
                        "failed to refresh timeline after send: {error:#}"
                    );
                }
                true
            }
            Err(error) => {
                warn!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    "failed to commit chat message: {error:#}"
                );
                self.queued.push_back(ChatNotification::Delivery {
                    message_id,
                    state: DeliveryState::Failed,
                    detail: Some(error.to_string()),
                });
                false
            }
        }
    }

    pub fn offer_files(
        &self,
        conversation_id: String,
        body: String,
        attachments: Vec<ChatAttachment>,
        paths: Vec<PathBuf>,
    ) {
        let blobs = self.blobs.clone();
        let tx = self.wake_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut imported = Vec::new();
                for path in paths {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("file name is not valid Unicode")?
                        .to_owned();
                    let (tag, byte_len) = blobs
                        .import_file(
                            path,
                            ImportMode::Copy,
                            BlobFormat::Raw,
                            IgnoreProgressSender::<ImportProgress>::default(),
                        )
                        .await?;
                    if byte_len == 0 {
                        bail!("empty files cannot be offered");
                    }
                    imported.push((tag, byte_len, name));
                }
                let mut files = Vec::new();
                for (tag, byte_len, name) in imported {
                    let hash = *tag.hash();
                    blobs
                        .set_tag(attachment_blob_tag(hash), Some(HashAndFormat::raw(hash)))
                        .await?;
                    files.push(ChatAttachment {
                        kind: AttachmentKind::FileOffer,
                        id: hash.to_string(),
                        name,
                        media_type: "application/octet-stream".to_owned(),
                        byte_len,
                        width: 0,
                        height: 0,
                        hash: hash.to_string(),
                        data: None,
                    });
                }
                Ok::<_, anyhow::Error>(files)
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx
                .send(ChatInput::FilesImported {
                    conversation_id,
                    body,
                    attachments,
                    result,
                })
                .await;
        });
    }

    pub async fn request_file(
        &mut self,
        conversation_id: String,
        message_id: String,
        hash_text: String,
        path: PathBuf,
    ) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(&conversation_id)
            .context("unknown conversation")?;
        let messages = self.load_messages(stored).await?;
        let message = messages
            .iter()
            .find(|message| message.message_id == message_id && message.deletion.is_none())
            .or_else(|| {
                self.staged_inbound
                    .get(&conversation_id)
                    .and_then(|staged| staged.get(&message_id))
            })
            .context("file offer is unavailable")?;
        let attachment = message
            .attachments
            .iter()
            .find(|attachment| {
                attachment.hash == hash_text
                    && matches!(
                        attachment.kind,
                        AttachmentKind::FileOffer | AttachmentKind::Image
                    )
            })
            .context("attachment is unavailable")?;
        if attachment.kind == AttachmentKind::FileOffer
            && message.stopped_file_offers.contains(&hash_text)
        {
            bail!("file owner stopped sharing this file");
        }
        let hash = Hash::from_str(&hash_text)?;
        let owner = NodeId::from_str(&message.author_id)?;
        let providers = if owner == self.our_node_id {
            Vec::new()
        } else {
            self.attachment_providers(&conversation_id, Some(owner))
                .into_iter()
                .filter(|address| address.node_id == owner)
                .collect()
        };
        let byte_len = attachment.byte_len;
        let key = (message_id.clone(), hash_text.clone());
        if self.active_file_downloads.contains_key(&key) {
            bail!("file download is already active");
        }
        let pending = PendingFileDownload {
            conversation_id: conversation_id.clone(),
            message_id: message_id.clone(),
            hash: hash_text.clone(),
            path: path.clone(),
            byte_len,
        };
        let previous = self.pending_file_downloads.insert(key.clone(), pending);
        if let Err(error) = self.save_pending_file_downloads() {
            if let Some(previous) = previous {
                self.pending_file_downloads.insert(key, previous);
            } else {
                self.pending_file_downloads.remove(&key);
            }
            return Err(error);
        }
        let hash_and_format = HashAndFormat::raw(hash);
        self.blobs
            .set_tag(download_blob_tag(hash), Some(hash_and_format))
            .await?;
        let received = partial_download_bytes(&self.blobs, hash, byte_len).await;
        let attempt = next_nonce();
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        self.active_file_downloads.insert(key, (attempt, cancel_tx));
        self.queued.push_back(ChatNotification::FileTransferUpdate {
            message_id: message_id.clone(),
            hash: hash_text.clone(),
            path: path.clone(),
            received,
            total: byte_len,
            phase: FileTransferPhase::Connecting,
        });
        let blobs = self.blobs.clone();
        let downloader = self.blob_downloader.clone();
        let tx = self.wake_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let available = blobs
                    .get(&hash)
                    .await?
                    .is_some_and(|blob| blob.is_complete());
                if !available {
                    if providers.is_empty() {
                        bail!("file owner is unavailable");
                    }
                    let (progress_tx, progress_rx) = async_channel::bounded(128);
                    let request = DownloadRequest::new(hash_and_format, providers)
                        .progress_sender(AsyncChannelProgressSender::new(progress_tx));
                    let mut handle = downloader.queue(request).await;
                    let deadline = tokio::time::sleep(
                        attachment_download_timeout(byte_len).max(Duration::from_secs(30 * 60)),
                    );
                    tokio::pin!(deadline);
                    let mut tick = tokio::time::interval(Duration::from_secs(1));
                    let mut base_received = received;
                    let mut current_offset = 0;
                    let mut last_activity = tokio::time::Instant::now();
                    let mut last_report = tokio::time::Instant::now();
                    let mut phase = FileTransferPhase::Connecting;
                    let mut progress_open = true;
                    let updates = tx.clone();
                    let report = |received: u64, phase: FileTransferPhase| {
                        let _ = updates.try_send(ChatInput::FileDownloadProgress {
                            message_id: message_id.clone(),
                            hash: hash_text.clone(),
                            attempt,
                            path: path.clone(),
                            received: received.min(byte_len),
                            total: byte_len,
                            phase,
                        });
                    };
                    loop {
                        tokio::select! {
                            outcome = &mut handle => {
                                outcome?;
                                break;
                            }
                            _ = &mut deadline => {
                                downloader.cancel(handle).await;
                                bail!("file download timed out");
                            }
                            changed = cancel_rx.changed() => {
                                if changed.is_err() || *cancel_rx.borrow() {
                                    downloader.cancel(handle).await;
                                    bail!("file download cancelled");
                                }
                            }
                            event = progress_rx.recv(), if progress_open => {
                                match event {
                                    Ok(DownloadProgress::FoundLocal { child: BlobId::Root, size, valid_ranges, .. }) => {
                                        base_received = verified_range_bytes(&valid_ranges, size.value().min(byte_len));
                                        current_offset = 0;
                                        report(base_received.saturating_add(current_offset), phase.clone());
                                    }
                                    Ok(DownloadProgress::InitialState(state)) => {
                                        if let (Some(size), Some(ranges)) = (state.root.size, state.root.local_ranges.as_ref()) {
                                            base_received = verified_range_bytes(ranges, size.value().min(byte_len));
                                        }
                                        if let iroh_blobs::get::progress::BlobProgress::Progressing(offset) = state.root.progress {
                                            current_offset = offset;
                                        }
                                        report(base_received.saturating_add(current_offset), phase.clone());
                                    }
                                    Ok(DownloadProgress::Connected) => {
                                        phase = FileTransferPhase::Downloading;
                                        last_activity = tokio::time::Instant::now();
                                        report(base_received.saturating_add(current_offset), phase.clone());
                                    }
                                    Ok(DownloadProgress::Progress { offset, .. }) => {
                                        current_offset = current_offset.max(offset);
                                        last_activity = tokio::time::Instant::now();
                                        if phase != FileTransferPhase::Downloading
                                            || last_report.elapsed() >= Duration::from_millis(250)
                                        {
                                            phase = FileTransferPhase::Downloading;
                                            report(base_received.saturating_add(current_offset), phase.clone());
                                            last_report = tokio::time::Instant::now();
                                        }
                                    }
                                    Ok(DownloadProgress::Done { .. }) => {
                                        report(byte_len, FileTransferPhase::Saving);
                                    }
                                    Ok(_) => {}
                                    Err(_) => progress_open = false,
                                }
                            }
                            _ = tick.tick() => {
                                if last_activity.elapsed() >= Duration::from_secs(90) {
                                    downloader.cancel(handle).await;
                                    bail!("connection stalled for 90 seconds; resume when the owner is online");
                                }
                                if last_activity.elapsed() >= Duration::from_secs(10)
                                    && phase != FileTransferPhase::Reconnecting
                                {
                                    phase = FileTransferPhase::Reconnecting;
                                    report(base_received.saturating_add(current_offset), phase.clone());
                                }
                            }
                        }
                    }
                    blobs
                        .set_tag(attachment_blob_tag(hash), Some(hash_and_format))
                        .await?;
                }
                let _ = tx.try_send(ChatInput::FileDownloadProgress {
                    message_id: message_id.clone(),
                    hash: hash_text.clone(),
                    attempt,
                    path: path.clone(),
                    received: byte_len,
                    total: byte_len,
                    phase: FileTransferPhase::Saving,
                });
                if *cancel_rx.borrow() {
                    bail!("file download cancelled");
                }
                let partial = path.with_file_name(format!(".wire-download-{}.part", next_nonce()));
                let export = blobs
                    .export(
                        hash,
                        partial.clone(),
                        ExportMode::Copy,
                        Box::new(|_| Ok(())),
                    )
                    .await;
                if let Err(error) = export {
                    let _ = std::fs::remove_file(&partial);
                    return Err(error.into());
                }
                if *cancel_rx.borrow() {
                    let _ = std::fs::remove_file(&partial);
                    bail!("file download cancelled");
                }
                if let Err(error) = persistence::replace_file(&partial, &path) {
                    let _ = std::fs::remove_file(&partial);
                    return Err(error.into());
                }
                Ok::<_, anyhow::Error>(path)
            }
            .await
            .map_err(|error| format!("{error:#}"));
            let _ = tx
                .send(ChatInput::FileDownloadFinished {
                    conversation_id,
                    message_id,
                    hash: hash_text,
                    attempt,
                    result,
                })
                .await;
        });
        Ok(())
    }

    pub async fn cancel_file(&mut self, message_id: String, hash: String) -> Result<()> {
        let key = (message_id.clone(), hash.clone());
        if !self.active_file_downloads.contains_key(&key)
            && !self.pending_file_downloads.contains_key(&key)
        {
            return Ok(());
        }
        let previous = self.pending_file_downloads.remove(&key);
        if let Err(error) = self.save_pending_file_downloads() {
            if let Some(previous) = previous {
                self.pending_file_downloads.insert(key, previous);
            }
            return Err(error);
        }
        if let Some((_, cancel)) = self.active_file_downloads.remove(&key) {
            let _ = cancel.send(true);
        }
        self.queued
            .push_back(ChatNotification::FileTransferCancelled { message_id, hash });
        self.release_download_pin(&key.1).await?;
        Ok(())
    }

    pub async fn set_file_serving(&mut self, hash_text: String, serving: bool) -> Result<()> {
        let hash = Hash::from_str(&hash_text)?;
        let mut offers = Vec::new();
        for (conversation_id, stored) in &self.index.conversations {
            for message in self.load_messages(stored).await? {
                if message.author_id == self.our_node_id.to_string()
                    && message.deletion.is_none()
                    && message.attachments.iter().any(|attachment| {
                        attachment.kind == AttachmentKind::FileOffer && attachment.hash == hash_text
                    })
                {
                    offers.push((conversation_id.clone(), message.message_id));
                }
            }
        }
        if offers.is_empty() {
            bail!("no file offer from this device matches this file");
        }
        let was_stopped = is_stopped(&self.stopped_hashes, &hash);
        set_stopped(&self.stopped_hashes, hash, !serving);
        if serving {
            self.stopped_file_index.hashes.remove(&hash_text);
        } else {
            self.stopped_file_index.hashes.insert(hash_text.clone());
        }
        if let Err(error) = persistence::write_json(
            &self.root.join("stopped-files.json"),
            &self.stopped_file_index,
        ) {
            set_stopped(&self.stopped_hashes, hash, was_stopped);
            if was_stopped {
                self.stopped_file_index.hashes.insert(hash_text);
            } else {
                self.stopped_file_index.hashes.remove(&hash_text);
            }
            return Err(error);
        }
        let mut changed_conversations = BTreeSet::new();
        for (conversation_id, message_id) in offers {
            let stored = &self.index.conversations[&conversation_id];
            let doc_id = NamespaceId::from_str(&stored.public.document_id)?;
            let doc = self
                .docs
                .open(doc_id)
                .await?
                .context("conversation document unavailable")?;
            let state = FileSharingState {
                message_id,
                hash: hash_text.clone(),
                serving,
                changed_at: now_millis(),
            };
            doc.set_bytes(self.author, state.entry_key(), serde_json::to_vec(&state)?)
                .await?;
            changed_conversations.insert(conversation_id);
        }
        for conversation_id in changed_conversations {
            self.spawn_doc_sync(&conversation_id);
            self.publish_timeline(&conversation_id).await?;
        }
        Ok(())
    }

    fn handle_blob_provider_event(&mut self, event: iroh_blobs::provider::Event) {
        use iroh_blobs::provider::Event;
        match event {
            Event::GetRequestReceived {
                connection_id,
                request_id,
                hash,
            } => {
                let Some(total) = self.offered_file_sizes.get(&hash).copied() else {
                    return;
                };
                self.serving_requests.insert(
                    (connection_id, request_id),
                    ServingRequest {
                        hash,
                        total,
                        position: 0,
                        last_emit: tokio::time::Instant::now(),
                    },
                );
                self.queued.push_back(ChatNotification::FileServing {
                    hash: hash.to_string(),
                    connection_id,
                    request_id,
                    position: 0,
                    total,
                    phase: FileServingPhase::Sending,
                });
            }
            Event::TransferProgress {
                connection_id,
                request_id,
                hash,
                end_offset,
            } => {
                let Some(request) = self.serving_requests.get_mut(&(connection_id, request_id))
                else {
                    return;
                };
                if request.hash != hash {
                    return;
                }
                request.position = request.position.max(end_offset.min(request.total));
                if request.last_emit.elapsed() >= Duration::from_millis(200)
                    || request.position == request.total
                {
                    request.last_emit = tokio::time::Instant::now();
                    self.queued.push_back(ChatNotification::FileServing {
                        hash: hash.to_string(),
                        connection_id,
                        request_id,
                        position: request.position,
                        total: request.total,
                        phase: FileServingPhase::Sending,
                    });
                }
            }
            Event::TransferCompleted {
                connection_id,
                request_id,
                ..
            }
            | Event::TransferAborted {
                connection_id,
                request_id,
                ..
            } => {
                let phase = if matches!(event, Event::TransferCompleted { .. }) {
                    FileServingPhase::Sent
                } else {
                    FileServingPhase::Interrupted
                };
                if let Some(request) = self.serving_requests.remove(&(connection_id, request_id)) {
                    self.queued.push_back(ChatNotification::FileServing {
                        hash: request.hash.to_string(),
                        connection_id,
                        request_id,
                        position: request.position,
                        total: request.total,
                        phase,
                    });
                }
            }
            _ => {}
        }
    }

    fn save_pending_file_downloads(&self) -> Result<()> {
        let index = PendingFileDownloadIndex {
            downloads: self.pending_file_downloads.values().cloned().collect(),
        };
        persistence::write_json(&self.root.join("file-downloads.json"), &index)
    }

    async fn release_download_pin(&self, hash: &str) -> Result<()> {
        if self
            .pending_file_downloads
            .values()
            .any(|pending| pending.hash == hash)
        {
            return Ok(());
        }
        self.blobs
            .set_tag(download_blob_tag(Hash::from_str(hash)?), None)
            .await?;
        Ok(())
    }

    async fn restore_file_downloads(&mut self) {
        let pending: Vec<_> = self.pending_file_downloads.values().cloned().collect();
        let mut discarded = Vec::new();
        for item in pending {
            let Ok(hash) = Hash::from_str(&item.hash) else {
                warn!("ignored invalid persisted file download hash");
                discarded.push((item.message_id, item.hash));
                continue;
            };
            if !self.index.conversations.contains_key(&item.conversation_id) {
                discarded.push((item.message_id, item.hash));
                continue;
            }
            if let Err(error) = self
                .blobs
                .set_tag(download_blob_tag(hash), Some(HashAndFormat::raw(hash)))
                .await
            {
                warn!("could not restore file download pin: {error:#}");
            }
            let received = partial_download_bytes(&self.blobs, hash, item.byte_len).await;
            self.queued.push_back(ChatNotification::FileTransferUpdate {
                message_id: item.message_id,
                hash: item.hash,
                path: item.path,
                received,
                total: item.byte_len,
                phase: FileTransferPhase::Paused("Wire was closed during this download".to_owned()),
            });
        }
        if !discarded.is_empty() {
            for key in &discarded {
                self.pending_file_downloads.remove(key);
            }
            if let Err(error) = self.save_pending_file_downloads() {
                warn!("could not discard orphaned file downloads: {error:#}");
            }
            for (_, hash) in discarded {
                if Hash::from_str(&hash).is_ok() {
                    if let Err(error) = self.release_download_pin(&hash).await {
                        warn!("could not release orphaned file download pin: {error:#}");
                    }
                }
            }
        }
    }

    async fn record_file_receipt(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        hash: &str,
    ) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(conversation_id)
            .context("unknown conversation")?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = self
            .docs
            .open(document_id)
            .await?
            .context("conversation document is unavailable")?;
        let receipt = FileReceipt {
            message_id: message_id.to_owned(),
            hash: hash.to_owned(),
            receiver_id: self.our_node_id.to_string(),
            received_at: now_millis(),
        };
        let key = receipt.entry_key();
        if self.recorded_file_receipts.contains(&key) {
            return Ok(());
        }
        doc.set_bytes(self.author, key.clone(), serde_json::to_vec(&receipt)?)
            .await?;
        self.recorded_file_receipts.insert(key);
        Ok(())
    }

    pub fn set_max_image_bytes(&mut self, max_image_bytes: Option<u64>) {
        self.max_image_bytes = max_image_bytes;
        let ids: Vec<_> = self.index.conversations.keys().cloned().collect();
        for id in ids {
            if let Err(error) = self.doc_event_tx.try_send(DocumentSignal::Changed {
                conversation_id: id,
                from_remote: false,
            }) {
                trace!("could not queue image-limit refresh: {error}");
            }
        }
    }

    pub async fn set_retention_policy(&mut self, retention: RetentionPolicy) -> Result<()> {
        self.retention = retention;
        self.release_expired_attachment_tags().await?;
        let ids: Vec<_> = self.index.conversations.keys().cloned().collect();
        for id in ids {
            self.publish_timeline(&id).await?;
        }
        Ok(())
    }

    /// Remember our current identity so outbound messages and invites carry
    /// a snapshot for peers that never saved us as contacts.
    pub fn set_own_profile(
        &mut self,
        display_name: Option<String>,
        avatar_hash: Option<String>,
        accent_color: Option<String>,
    ) {
        self.own_display_name = display_name
            .map(|name| {
                name.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .chars()
                    .take(32)
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .filter(|name| !name.is_empty());
        self.own_avatar_hash = avatar_hash.filter(|hash| !hash.trim().is_empty());
        self.own_accent_color = accent_color
            .as_deref()
            .and_then(crate::profile::sanitize_accent_color);
    }

    fn stamp_own_profile(&self, message: &mut ChatMessage) {
        if message.author_display_name.is_none() {
            message.author_display_name = self.own_display_name.clone();
        }
        if message.author_avatar_hash.is_none() {
            message.author_avatar_hash = self.own_avatar_hash.clone();
        }
        if message.author_accent_color.is_none() {
            message.author_accent_color = self.own_accent_color.clone();
        }
    }

    async fn release_expired_attachment_tags(&self) -> Result<()> {
        let RetentionPolicy::Days(_) = self.retention else {
            return Ok(());
        };
        let now = now_millis();
        let mut expired = BTreeSet::new();
        let mut retained = BTreeSet::new();
        for stored in self.index.conversations.values() {
            for message in self.load_messages(stored).await? {
                for attachment in message.attachments {
                    if attachment.kind == AttachmentKind::FileOffer
                        && message.author_id == self.our_node_id.to_string()
                    {
                        retained.insert(Hash::from_str(&attachment.hash)?);
                        continue;
                    }
                    if let Ok(hash) = Hash::from_str(&attachment.hash) {
                        let target = if self.retention.includes(message.sent_at, now) {
                            &mut retained
                        } else {
                            &mut expired
                        };
                        target.insert(hash);
                    }
                }
            }
        }
        for hash in expired.difference(&retained).copied() {
            self.blobs.set_tag(attachment_blob_tag(hash), None).await?;
        }
        Ok(())
    }

    pub async fn load_older_messages(&mut self, conversation_id: &str) -> Result<()> {
        let limit = self
            .timeline_limits
            .entry(conversation_id.to_owned())
            .or_insert(CHAT_TIMELINE_PAGE);
        *limit = limit.saturating_add(CHAT_TIMELINE_PAGE);
        self.publish_timeline(conversation_id).await
    }

    pub async fn load_attachment_data(
        &mut self,
        conversation_id: &str,
        hash: &str,
        byte_len: u64,
    ) -> Result<()> {
        let hash = Hash::from_str(hash).context("invalid chat attachment hash")?;
        if self.queue_attachment_data(hash, byte_len).await? {
            return Ok(());
        }
        self.schedule_attachment_retry(conversation_id, hash);
        Ok(())
    }

    async fn queue_attachment_data(&mut self, hash: Hash, byte_len: u64) -> Result<bool> {
        let Some(blob) = self.blobs.get(&hash).await? else {
            return Ok(false);
        };
        if !blob.is_complete() {
            return Ok(false);
        }
        let len = usize::try_from(byte_len).context("chat attachment is too large")?;
        let mut reader = blob.data_reader();
        let bytes = reader.read_at(0, len).await?;
        self.queued.push_back(ChatNotification::AttachmentData {
            hash: hash.to_string(),
            data: Arc::new(bytes.to_vec()),
        });
        Ok(true)
    }

    async fn import_attachment_bytes(&self, message: &ChatMessage) -> Result<()> {
        for attachment in &message.attachments {
            let expected = Hash::from_str(&attachment.hash)?;
            if let Some(data) = &attachment.data {
                if data.len() as u64 != attachment.byte_len
                    || Hash::new(data.as_slice()) != expected
                {
                    bail!("image attachment changed before it was sent");
                }
                let tag = self
                    .blobs
                    .import_bytes(data.as_slice().to_vec().into(), BlobFormat::Raw)
                    .await?;
                if *tag.hash() != expected {
                    bail!("image attachment hash mismatch");
                }
                self.blobs
                    .set_tag(
                        attachment_blob_tag(expected),
                        Some(HashAndFormat::raw(expected)),
                    )
                    .await?;
            } else if !self
                .blobs
                .get(&expected)
                .await?
                .is_some_and(|blob| blob.is_complete())
            {
                bail!("image attachment bytes are unavailable");
            }
        }
        Ok(())
    }

    fn is_conversation_member(&self, conversation_id: &str, node: NodeId) -> bool {
        self.index
            .conversations
            .get(conversation_id)
            .is_some_and(|stored| {
                stored
                    .public
                    .members
                    .iter()
                    .any(|member| member == &node.to_string())
            })
    }

    fn schedule_delivery_retry(
        &mut self,
        conversation_id: &str,
        message_id: &str,
        immediate: bool,
    ) {
        let now = tokio::time::Instant::now();
        let existed = self.retry_state.contains_key(conversation_id);
        let state = self
            .retry_state
            .entry(conversation_id.to_owned())
            .or_insert_with(|| ConversationRetry {
                attempts: 0,
                // Short first probe — long backoff only after real attempts.
                next_attempt: now + CHAT_FIRST_RETRY,
                messages: BTreeSet::new(),
            });
        state.messages.insert(message_id.to_owned());
        // Only pull the timer forward for a brand-new conversation schedule.
        // Re-arming to `now` on every resume caused a wake every retry tick.
        if immediate && !existed {
            state.next_attempt = now;
        }
    }

    fn save_reliable_control(&self) {
        if let Err(error) = save_reliable_control(
            &self.root.join("reliable-control.json"),
            &self.reliable_control,
        ) {
            warn!("failed to persist reliable chat control queue: {error:#}");
        }
    }

    fn schedule_control_retry(&mut self, conversation_id: &str, immediate: bool) {
        let now = tokio::time::Instant::now();
        let retry = self
            .control_retries
            .entry(conversation_id.to_owned())
            .or_insert(ControlRetry {
                attempts: 0,
                next_attempt: now + CHAT_FIRST_RETRY,
            });
        if immediate {
            retry.next_attempt = now;
        }
    }

    fn conversation_has_pending_control(&self, conversation_id: &str) -> bool {
        self.reliable_control
            .attachments
            .get(conversation_id)
            .is_some_and(|entries| {
                entries
                    .values()
                    .any(|entry| !entry.pending_peers.is_empty())
            })
            || self
                .reliable_control
                .deletions
                .get(conversation_id)
                .is_some_and(|entries| {
                    entries
                        .values()
                        .any(|entry| !entry.pending_peers.is_empty())
                })
            || self
                .reliable_control
                .invites
                .get(conversation_id)
                .is_some_and(|entry| !entry.pending_peers.is_empty())
    }

    fn conversation_peer_strings(&self, conversation_id: &str) -> BTreeSet<String> {
        self.index
            .conversations
            .get(conversation_id)
            .map(|stored| {
                self.other_members(stored)
                    .into_iter()
                    .map(|peer| peer.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn register_attachment_deliveries(&mut self, conversation_id: &str, message: &ChatMessage) {
        let pending_peers = self.conversation_peer_strings(conversation_id);
        if pending_peers.is_empty() || message.attachments.is_empty() {
            return;
        }
        let entries = self
            .reliable_control
            .attachments
            .entry(conversation_id.to_owned())
            .or_default();
        for attachment in &message.attachments {
            if attachment.kind == AttachmentKind::FileOffer {
                continue;
            }
            entries.insert(
                attachment.hash.clone(),
                PendingAttachmentDelivery {
                    message_id: message.message_id.clone(),
                    hash: attachment.hash.clone(),
                    byte_len: attachment.byte_len,
                    attachment_kind: attachment.kind,
                    sent_at: message.sent_at,
                    pending_peers: pending_peers.clone(),
                },
            );
        }
        self.save_reliable_control();
        self.schedule_control_retry(conversation_id, false);
    }

    fn register_deletion_delivery(&mut self, conversation_id: &str, deletion: ReplicatedDeletion) {
        let pending_peers = self.conversation_peer_strings(conversation_id);
        if pending_peers.is_empty() {
            return;
        }
        self.reliable_control
            .deletions
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(
                deletion.message_id.clone(),
                PendingDeletionDelivery {
                    deletion,
                    pending_peers,
                },
            );
        self.save_reliable_control();
        self.schedule_control_retry(conversation_id, false);
    }

    fn register_invite_delivery(&mut self, conversation_id: &str) {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return;
        };
        let history_epoch = stored.public.history_epoch;
        let pending_peers = self.conversation_peer_strings(conversation_id);
        self.reliable_control.invites.insert(
            conversation_id.to_owned(),
            PendingInviteDelivery {
                history_epoch,
                pending_peers,
            },
        );
        self.save_reliable_control();
        self.schedule_control_retry(conversation_id, true);
    }

    fn initialize_invite_deliveries(&mut self) {
        let conversations: Vec<_> = self
            .index
            .conversations
            .iter()
            .map(|(id, stored)| (id.clone(), stored.public.history_epoch))
            .collect();
        let mut changed = false;
        for (conversation_id, history_epoch) in conversations {
            let current = self.reliable_control.invites.get(&conversation_id);
            if current.is_some_and(|pending| pending.history_epoch == history_epoch) {
                continue;
            }
            let pending_peers = self.conversation_peer_strings(&conversation_id);
            self.reliable_control.invites.insert(
                conversation_id.clone(),
                PendingInviteDelivery {
                    history_epoch,
                    pending_peers,
                },
            );
            changed = true;
        }
        if changed {
            self.save_reliable_control();
        }
    }

    fn restore_control_retries(&mut self) {
        let conversations: Vec<_> = self
            .index
            .conversations
            .keys()
            .filter(|id| self.conversation_has_pending_control(id))
            .cloned()
            .collect();
        for conversation_id in conversations {
            self.schedule_control_retry(&conversation_id, true);
        }
    }

    fn apply_control_acks(&mut self, conversation_id: &str, remote: NodeId, request: &SyncRequest) {
        let remote = remote.to_string();
        let mut changed = false;
        if let Some(entries) = self.reliable_control.attachments.get_mut(conversation_id) {
            for hash in &request.attachment_acks {
                if let Some(entry) = entries.get_mut(hash) {
                    changed |= entry.pending_peers.remove(&remote);
                }
            }
            entries.retain(|_, entry| !entry.pending_peers.is_empty());
        }
        if let Some(entries) = self.reliable_control.deletions.get_mut(conversation_id) {
            for message_id in &request.deletion_acks {
                if let Some(entry) = entries.get_mut(message_id) {
                    changed |= entry.pending_peers.remove(&remote);
                }
            }
            entries.retain(|_, entry| !entry.pending_peers.is_empty());
        }
        if let (Some(epoch), Some(invite)) = (
            request.accepted_history_epoch,
            self.reliable_control.invites.get_mut(conversation_id),
        ) {
            if invite.history_epoch == epoch {
                changed |= invite.pending_peers.remove(&remote);
            }
        }
        if changed {
            self.save_reliable_control();
            if !self.conversation_has_pending_control(conversation_id) {
                self.control_retries.remove(conversation_id);
            }
        }
    }

    fn wake_payload_for(&self, conversation_id: &str) -> WakePayload {
        let mut messages = Vec::new();
        let mut used = 0usize;
        for (message_id, pending_conversation) in &self.pending_deliveries {
            if pending_conversation != conversation_id {
                continue;
            }
            let Some(message) = self.pending_outbound.get(message_id) else {
                continue;
            };
            let size = message.body.len().saturating_add(256);
            if !messages.is_empty() && used.saturating_add(size) > CHAT_WAKE_PAYLOAD_BUDGET {
                break;
            }
            if size > CHAT_WAKE_PAYLOAD_BUDGET {
                continue;
            }
            used = used.saturating_add(size);
            messages.push(message.clone());
        }
        let deletions = self
            .reliable_control
            .deletions
            .get(conversation_id)
            .into_iter()
            .flat_map(BTreeMap::values)
            .filter(|pending| !pending.pending_peers.is_empty())
            .map(|pending| pending.deletion.clone())
            .collect();
        let attachments = self
            .reliable_control
            .attachments
            .get(conversation_id)
            .into_iter()
            .flat_map(BTreeMap::values)
            .filter(|pending| {
                !pending.pending_peers.is_empty()
                    && self.retention.includes(pending.sent_at, now_millis())
            })
            .cloned()
            .collect();
        WakePayload {
            messages,
            receipts: Vec::new(),
            deletions,
            attachment_acks: Vec::new(),
            deletion_acks: Vec::new(),
            accepted_history_epoch: None,
            attachments,
        }
    }

    fn spawn_wake(&mut self, conversation_id: &str) {
        let payload = self.wake_payload_for(conversation_id);
        self.spawn_wake_with(conversation_id, payload);
    }

    fn spawn_wake_with(&mut self, conversation_id: &str, payload: WakePayload) {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return;
        };
        let peers = self.other_members(stored);
        if peers.is_empty() {
            return;
        }
        *self
            .wake_inflight
            .entry(conversation_id.to_owned())
            .or_insert(0) += 1;
        let sessions = self.sessions.clone();
        let docs = self.docs.clone();
        let blobs = self.blobs.clone();
        let stored = stored.clone();
        let endpoint = self.endpoint.clone();
        let wake_tx = self.wake_tx.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            let ticket = refresh_share_ticket(&docs, &stored, &endpoint)
                .await
                .ok()
                .or_else(|| Some(stored.ticket.clone()));
            push_pending_attachments(
                blobs,
                sessions.clone(),
                &peers,
                &conversation_id,
                &payload.attachments,
            )
            .await;
            let reached = wake_peers(
                sessions,
                peers,
                &conversation_id,
                ticket.as_deref(),
                payload,
            )
            .await;
            let _ = wake_tx
                .send(ChatInput::WakeFinished {
                    conversation_id,
                    reached,
                })
                .await;
        });
    }

    fn spawn_doc_sync(&self, conversation_id: &str) {
        let Some(stored) = self.index.conversations.get(conversation_id).cloned() else {
            return;
        };
        let docs = self.docs.clone();
        let our_node_id = self.our_node_id;
        let endpoint = self.endpoint.clone();
        tokio::spawn(async move {
            if let Err(error) = nudge_doc_sync(&docs, &stored, our_node_id, &endpoint, None).await {
                trace!(
                    conversation = %log_id(&stored.public.id),
                    "doc sync nudge failed: {error:#}"
                );
            }
        });
    }

    fn spawn_receipt_wake(&mut self, conversation_id: &str, receipts: Vec<ReplicatedReceipt>) {
        let now = tokio::time::Instant::now();
        if self
            .last_receipt_wake
            .get(conversation_id)
            .is_some_and(|at| now.duration_since(*at) < CHAT_RECEIPT_WAKE_COOLDOWN)
        {
            return;
        }
        self.last_receipt_wake
            .insert(conversation_id.to_owned(), now);
        let mut payload = self.wake_payload_for(conversation_id);
        payload.receipts = receipts;
        self.spawn_wake_with(conversation_id, payload);
    }

    fn spawn_control_ack(
        &self,
        conversation_id: &str,
        peer: NodeId,
        attachment_acks: Vec<String>,
        deletion_acks: Vec<String>,
        accepted_history_epoch: Option<u64>,
    ) {
        let Some(stored) = self.index.conversations.get(conversation_id).cloned() else {
            return;
        };
        let payload = WakePayload {
            attachment_acks,
            deletion_acks,
            accepted_history_epoch,
            ..WakePayload::default()
        };
        let sessions = self.sessions.clone();
        let docs = self.docs.clone();
        let endpoint = self.endpoint.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            let ticket = refresh_share_ticket(&docs, &stored, &endpoint)
                .await
                .ok()
                .or_else(|| Some(stored.ticket.clone()));
            if let Err(error) =
                send_sync_request(sessions, peer, &conversation_id, ticket.as_deref(), payload)
                    .await
            {
                trace!(
                    conversation = %log_id(&conversation_id),
                    peer = %peer.fmt_short(),
                    "chat control acknowledgement did not reach peer: {error:#}"
                );
            }
        });
    }

    fn conversation_is_queued(&self, conversation_id: &str) -> bool {
        self.wake_failures
            .get(conversation_id)
            .is_some_and(|failures| *failures >= CHAT_QUEUE_AFTER_FAILURES)
    }

    async fn apply_wake_finished(&mut self, conversation_id: &str, reached: bool) {
        if let Some(count) = self.wake_inflight.get_mut(conversation_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.wake_inflight.remove(conversation_id);
            }
        }
        let inflight = self
            .wake_inflight
            .get(conversation_id)
            .copied()
            .unwrap_or(0);
        let pending: Vec<_> = self
            .pending_deliveries
            .iter()
            .filter(|(_, cid)| *cid == conversation_id)
            .map(|(mid, _)| mid.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        if reached {
            self.wake_failures.remove(conversation_id);
            for message_id in &pending {
                self.schedule_delivery_retry(conversation_id, message_id, false);
            }
            // Do not publish_timeline here — that re-entered doc events and
            // spawned more wakes. Receipts arrive via InsertRemote / ALPN.
            self.spawn_doc_sync(conversation_id);
            return;
        }
        // Always count this failed probe (even if another wake is still in
        // flight). Skipping the count while stacked let offline peers never
        // reach Queued and stay on "delivery retry N" forever.
        let failures = {
            let entry = self
                .wake_failures
                .entry(conversation_id.to_owned())
                .or_insert(0);
            *entry = entry.saturating_add(1);
            *entry
        };
        let offline = failures >= CHAT_QUEUE_AFTER_FAILURES;
        if offline {
            for message_id in &pending {
                self.queued.push_back(ChatNotification::Delivery {
                    message_id: message_id.clone(),
                    state: DeliveryState::Queued,
                    detail: Some("waiting for peer".to_owned()),
                });
            }
            if failures == CHAT_QUEUE_AFTER_FAILURES || failures.is_multiple_of(10) {
                info!(
                    conversation = %log_id(conversation_id),
                    messages = pending.len(),
                    failures,
                    "peer unreachable; queued until they come online"
                );
            }
        }
        // Another wake still running — let it finish before arming the next.
        if inflight > 0 {
            return;
        }
        for message_id in &pending {
            self.schedule_delivery_retry(conversation_id, message_id, false);
        }
        // Stretch the timer when offline so we do not hammer Connect every few
        // seconds (online receipt wait still uses the fast schedule).
        if let Some(state) = self.retry_state.get_mut(conversation_id) {
            let now = tokio::time::Instant::now();
            state.next_attempt =
                now + delivery_retry_delay(conversation_id, state.attempts, offline);
        }
    }

    async fn retry_due_deliveries(&mut self) {
        let now = tokio::time::Instant::now();
        let due: Vec<_> = self
            .retry_state
            .iter()
            .filter_map(|(id, state)| (state.next_attempt <= now).then_some(id.clone()))
            .collect();
        for conversation_id in due {
            let message_ids = self
                .retry_state
                .get(&conversation_id)
                .map(|state| state.messages.clone())
                .unwrap_or_default();
            if message_ids.is_empty() {
                self.retry_state.remove(&conversation_id);
                continue;
            }
            // Never stack dials: retry interval used to be shorter than the
            // connect timeout, so offline peers accumulated parallel wakes and
            // never settled into Queued.
            if self
                .wake_inflight
                .get(&conversation_id)
                .copied()
                .unwrap_or(0)
                > 0
            {
                if let Some(state) = self.retry_state.get_mut(&conversation_id) {
                    state.next_attempt = now + CHAT_FIRST_RETRY;
                }
                continue;
            }
            let offline = self.conversation_is_queued(&conversation_id);
            // Never await dials on the service loop — that blocked all chat
            // processing for multi-second connect timeouts and starved sends.
            self.spawn_wake(&conversation_id);
            // Docs DirectJoin is useless while the peer is offline and only
            // adds noise; resume path will nudge when they return.
            if !offline {
                self.spawn_doc_sync(&conversation_id);
            }
            let Some(state) = self.retry_state.get_mut(&conversation_id) else {
                continue;
            };
            if state.messages.is_empty() {
                continue;
            }
            let attempts = {
                state.attempts = state.attempts.saturating_add(1);
                state.next_attempt =
                    now + delivery_retry_delay(&conversation_id, state.attempts, offline);
                state.attempts
            };
            // Keep Queued in the UI while offline — do not overwrite with
            // "delivery retry 34" every few seconds.
            if offline {
                continue;
            }
            for message_id in message_ids {
                if self.pending_deliveries.contains_key(&message_id) {
                    self.queued.push_back(ChatNotification::Delivery {
                        message_id,
                        state: DeliveryState::Retrying,
                        detail: Some(format!("delivery retry {attempts}")),
                    });
                }
            }
        }
    }

    fn retry_due_controls(&mut self) {
        let now = tokio::time::Instant::now();
        let due: Vec<_> = self
            .control_retries
            .iter()
            .filter_map(|(id, retry)| (retry.next_attempt <= now).then_some(id.clone()))
            .collect();
        for conversation_id in due {
            if !self.conversation_has_pending_control(&conversation_id) {
                self.control_retries.remove(&conversation_id);
                continue;
            }
            if self
                .wake_inflight
                .get(&conversation_id)
                .copied()
                .unwrap_or(0)
                > 0
            {
                if let Some(retry) = self.control_retries.get_mut(&conversation_id) {
                    retry.next_attempt = now + CHAT_FIRST_RETRY;
                }
                continue;
            }
            self.send_pending_invites(&conversation_id);
            if self
                .reliable_control
                .attachments
                .get(&conversation_id)
                .is_some_and(|entries| !entries.is_empty())
                || self
                    .reliable_control
                    .deletions
                    .get(&conversation_id)
                    .is_some_and(|entries| !entries.is_empty())
            {
                self.spawn_wake(&conversation_id);
            }
            if let Some(retry) = self.control_retries.get_mut(&conversation_id) {
                retry.attempts = retry.attempts.saturating_add(1);
                retry.next_attempt = now
                    + delivery_retry_delay(&conversation_id, retry.attempts, retry.attempts >= 5);
            }
        }
    }

    async fn resume_deliveries_for_peer(&mut self, peer: NodeId) {
        let peer_s = peer.to_string();
        let conversation_ids: Vec<_> = self
            .index
            .conversations
            .iter()
            .filter(|(_, stored)| stored.public.members.iter().any(|member| member == &peer_s))
            .map(|(id, _)| id.clone())
            .filter(|id| {
                self.pending_deliveries
                    .values()
                    .any(|conversation_id| conversation_id == id)
            })
            .collect();
        for conversation_id in conversation_ids {
            self.resume_queued_deliveries(&conversation_id).await;
        }
        let control_ids: Vec<_> = self
            .index
            .conversations
            .iter()
            .filter(|(_, stored)| stored.public.members.iter().any(|member| member == &peer_s))
            .map(|(id, _)| id.clone())
            .filter(|id| self.conversation_has_pending_control(id))
            .collect();
        for conversation_id in control_ids {
            self.schedule_control_retry(&conversation_id, true);
        }
    }

    async fn resume_queued_deliveries(&mut self, conversation_id: &str) {
        let message_ids: Vec<_> = self
            .pending_deliveries
            .iter()
            .filter(|(_, pending_conversation)| *pending_conversation == conversation_id)
            .map(|(message_id, _)| message_id.clone())
            .collect();
        if message_ids.is_empty() {
            return;
        }
        // Already probing or waking — a remote insert just means pull docs;
        // publish_timeline in the caller handles that. Extra resume/wake here
        // was flooding the peer (~20 wakes/sec in logs) and delaying receipts.
        if self.retry_state.contains_key(conversation_id)
            || self
                .wake_inflight
                .get(conversation_id)
                .copied()
                .unwrap_or(0)
                > 0
        {
            return;
        }
        self.wake_failures.remove(conversation_id);
        info!(
            conversation = %log_id(conversation_id),
            messages = message_ids.len(),
            "resuming queued chat deliveries"
        );
        for message_id in &message_ids {
            self.schedule_delivery_retry(conversation_id, message_id, true);
            self.queued.push_back(ChatNotification::Delivery {
                message_id: message_id.clone(),
                state: DeliveryState::Pending,
                detail: Some("peer online; delivering".to_owned()),
            });
        }
        self.spawn_wake(conversation_id);
        self.spawn_doc_sync(conversation_id);
    }

    async fn apply_sync_request(&mut self, remote: NodeId, request: &SyncRequest) -> Result<()> {
        let carries_control_ack = !request.attachment_acks.is_empty()
            || !request.deletion_acks.is_empty()
            || request.accepted_history_epoch.is_some();
        self.apply_control_acks(&request.conversation_id, remote, request);
        let remote_s = remote.to_string();
        let mut accepted_messages = 0u32;
        for message in &request.messages {
            if message.author_id != remote_s {
                warn!(
                    conversation = %log_id(&request.conversation_id),
                    peer = %remote.fmt_short(),
                    message = %log_id(&message.message_id),
                    "ignored chat ALPN message with mismatched author"
                );
                continue;
            }
            if let Err(error) = message.validate() {
                warn!(
                    conversation = %log_id(&request.conversation_id),
                    peer = %remote.fmt_short(),
                    message = %log_id(&message.message_id),
                    "ignored invalid chat ALPN message: {error:#}"
                );
                continue;
            }
            let staged = self
                .staged_inbound
                .entry(request.conversation_id.clone())
                .or_default()
                .insert(message.message_id.clone(), message.clone())
                .is_none();
            if staged {
                accepted_messages += 1;
                info!(
                    conversation = %log_id(&request.conversation_id),
                    peer = %remote.fmt_short(),
                    message = %log_id(&message.message_id),
                    "accepted chat message over ALPN fast path"
                );
            }
        }
        if !request.receipts.is_empty() {
            self.apply_direct_receipts(&request.conversation_id, remote, &request.receipts);
        }
        let accepted_deletions =
            self.apply_direct_deletions(&request.conversation_id, remote, &request.deletions);
        if !accepted_deletions.is_empty() {
            self.spawn_control_ack(
                &request.conversation_id,
                remote,
                vec![],
                accepted_deletions.clone(),
                None,
            );
        }
        // Publish first so staged bodies + receipts hit the UI immediately,
        // then nudge docs for durable multi-device sync.
        if accepted_messages > 0 || !request.receipts.is_empty() || !accepted_deletions.is_empty() {
            if let Err(error) = self.publish_timeline(&request.conversation_id).await {
                warn!(
                    conversation = %log_id(&request.conversation_id),
                    "failed to publish timeline after ALPN wake: {error:#}"
                );
            }
        }
        if let Err(error) = self
            .pull_conversation(&request.conversation_id, request.ticket.as_deref())
            .await
        {
            if accepted_messages == 0
                && request.receipts.is_empty()
                && accepted_deletions.is_empty()
                && !carries_control_ack
            {
                return Err(error);
            }
            warn!(
                conversation = %log_id(&request.conversation_id),
                peer = %remote.fmt_short(),
                "docs pull after chat ALPN wake failed: {error:#}"
            );
        }
        Ok(())
    }

    fn apply_direct_deletions(
        &mut self,
        conversation_id: &str,
        remote: NodeId,
        deletions: &[ReplicatedDeletion],
    ) -> Vec<String> {
        if !self.is_conversation_member(conversation_id, remote) {
            return Vec::new();
        }
        let remote_s = remote.to_string();
        let staged = self
            .staged_deletions
            .entry(conversation_id.to_owned())
            .or_default();
        let mut accepted = Vec::new();
        for deletion in deletions.iter().take(128) {
            if deletion.validate().is_err() {
                continue;
            }
            let inserted = staged
                .insert(deletion.message_id.clone(), remote_s.clone())
                .is_none();
            accepted.push(deletion.message_id.clone());
            if inserted {
                info!(
                    conversation = %log_id(conversation_id),
                    peer = %remote.fmt_short(),
                    message = %log_id(&deletion.message_id),
                    "accepted message deletion over ALPN fast path"
                );
            }
        }
        #[cfg(test)]
        self.direct_deletions_accepted
            .fetch_add(accepted.len() as u64, Ordering::Relaxed);
        accepted
    }

    async fn apply_attachment_push(&mut self, remote: NodeId, push: AttachmentPush) -> Result<()> {
        if push.kind != "attachment-push" || push.version != 1 {
            bail!("unsupported attachment push version {}", push.version);
        }
        if !self.is_conversation_member(&push.conversation_id, remote) {
            bail!("attachment sender is not a conversation member");
        }
        if push.byte_len == 0 || push.byte_len > MAX_ATTACHMENT_PUSH_BYTES {
            bail!("attachment push size is outside the safety limit");
        }
        if !self.retention.includes(push.sent_at, now_millis())
            || (push.attachment_kind == AttachmentKind::Image
                && self
                    .max_image_bytes
                    .is_some_and(|limit| push.byte_len > limit))
            || (push.attachment_kind == AttachmentKind::InlineFile && push.byte_len > 64 * 1024)
        {
            self.spawn_control_ack(
                &push.conversation_id,
                remote,
                vec![push.hash.clone()],
                vec![],
                None,
            );
            return Ok(());
        }
        if push.data.len() as u64 != push.byte_len {
            bail!("attachment push body length does not match metadata");
        }
        let expected = Hash::from_str(&push.hash).context("invalid attachment push hash")?;
        if Hash::new(&push.data) != expected {
            bail!("attachment push hash mismatch");
        }
        let tag = self
            .blobs
            .import_bytes(push.data.into(), BlobFormat::Raw)
            .await?;
        if *tag.hash() != expected {
            bail!("imported attachment push hash mismatch");
        }
        self.blobs
            .set_tag(
                attachment_blob_tag(expected),
                Some(HashAndFormat::raw(expected)),
            )
            .await?;
        self.attachment_downloads.remove(&expected);
        self.attachment_retries.remove(&expected);
        info!(
            conversation = %log_id(&push.conversation_id),
            peer = %remote.fmt_short(),
            image = %expected.fmt_short(),
            bytes = push.byte_len,
            "accepted pushed chat image"
        );
        self.spawn_control_ack(
            &push.conversation_id,
            remote,
            vec![push.hash.clone()],
            vec![],
            None,
        );
        self.publish_timeline(&push.conversation_id).await?;
        Ok(())
    }

    fn apply_direct_receipts(
        &mut self,
        conversation_id: &str,
        remote: NodeId,
        receipts: &[ReplicatedReceipt],
    ) {
        if !self.is_conversation_member(conversation_id, remote) {
            return;
        }
        for receipt in receipts {
            if receipt.validate().is_err() {
                continue;
            }
            let Some(pending_conversation) = self.pending_deliveries.get(&receipt.message_id)
            else {
                continue;
            };
            if pending_conversation != conversation_id {
                continue;
            }
            self.pending_deliveries.remove(&receipt.message_id);
            self.pending_outbound.remove(&receipt.message_id);
            info!(
                conversation = %log_id(conversation_id),
                message = %log_id(&receipt.message_id),
                peer = %remote.fmt_short(),
                "chat message marked delivered after ALPN receipt"
            );
            self.queued.push_back(ChatNotification::Delivery {
                message_id: receipt.message_id.clone(),
                state: DeliveryState::Delivered,
                detail: None,
            });
        }
        self.retry_state.retain(|_, state| {
            state
                .messages
                .retain(|message_id| self.pending_deliveries.contains_key(message_id));
            !state.messages.is_empty()
        });
    }

    pub async fn delete_message(
        &mut self,
        conversation_id: String,
        message_id: String,
        scope: DeleteScope,
    ) {
        let result = match scope {
            DeleteScope::Local => self
                .delete_message_locally(&conversation_id, &message_id)
                .map(|()| None),
            DeleteScope::Everyone => self
                .insert_replicated_deletion(&conversation_id, &message_id)
                .await
                .map(Some),
        };
        match result {
            Ok(deletion) => {
                let pending: Vec<_> = self
                    .pending_file_downloads
                    .values()
                    .filter(|item| {
                        item.conversation_id == conversation_id && item.message_id == message_id
                    })
                    .map(|item| (item.message_id.clone(), item.hash.clone()))
                    .collect();
                for (id, hash) in pending {
                    if let Err(error) = self.cancel_file(id, hash).await {
                        warn!("could not discard download for deleted message: {error:#}");
                    }
                }
                info!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    ?scope,
                    "message tombstone committed"
                );
                if let Err(error) = self.publish_timeline(&conversation_id).await {
                    self.queued.push_back(ChatNotification::Error(format!(
                        "Could not refresh deleted message: {error:#}"
                    )));
                }
                if let Some(deletion) = deletion {
                    self.register_deletion_delivery(&conversation_id, deletion);
                    self.spawn_doc_sync(&conversation_id);
                    self.spawn_wake(&conversation_id);
                }
            }
            Err(error) => {
                warn!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    ?scope,
                    "failed to delete message: {error:#}"
                );
                if let Err(refresh_error) = self.publish_timeline(&conversation_id).await {
                    warn!(
                        conversation = %log_id(&conversation_id),
                        "failed to roll back optimistic message deletion: {refresh_error:#}"
                    );
                }
                self.queued.push_back(ChatNotification::Error(format!(
                    "Could not delete message: {error:#}"
                )));
            }
        }
    }

    pub async fn restore_message(&mut self, conversation_id: String, message_id: String) {
        match self.restore_message_locally(&conversation_id, &message_id) {
            Ok(()) => {
                info!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    "local message tombstone removed"
                );
                if let Err(error) = self.publish_timeline(&conversation_id).await {
                    self.queued.push_back(ChatNotification::Error(format!(
                        "Could not refresh restored message: {error:#}"
                    )));
                }
            }
            Err(error) => {
                warn!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    "failed to restore message: {error:#}"
                );
                if let Err(refresh_error) = self.publish_timeline(&conversation_id).await {
                    warn!(
                        conversation = %log_id(&conversation_id),
                        "failed to roll back optimistic message restore: {refresh_error:#}"
                    );
                }
                self.queued.push_back(ChatNotification::Error(format!(
                    "Could not restore message: {error:#}"
                )));
            }
        }
    }

    pub async fn clear_history(&mut self, conversation_id: String) {
        match self.rotate_conversation_document(&conversation_id).await {
            Ok(epoch) => {
                let pending: Vec<_> = self
                    .pending_file_downloads
                    .values()
                    .filter(|item| item.conversation_id == conversation_id)
                    .map(|item| (item.message_id.clone(), item.hash.clone()))
                    .collect();
                for (id, hash) in pending {
                    if let Err(error) = self.cancel_file(id, hash).await {
                        warn!("could not discard download after history clear: {error:#}");
                    }
                }
                info!(
                    conversation = %log_id(&conversation_id),
                    history_epoch = epoch,
                    "chat history deleted and conversation rotated onto a fresh document"
                );
                // Peers on a stale document need the new ticket before a pull
                // nudge — keep-alive made SyncRequest often win the race and
                // get ignored as non-member.
                self.invite_members_wait(&conversation_id).await;
                self.spawn_wake(&conversation_id);
            }
            Err(error) => {
                warn!(
                    conversation = %log_id(&conversation_id),
                    "failed to clear chat history: {error:#}"
                );
                if let Err(refresh_error) = self.publish_timeline(&conversation_id).await {
                    warn!(
                        conversation = %log_id(&conversation_id),
                        "failed to refresh timeline after history clear error: {refresh_error:#}"
                    );
                }
                self.queued.push_back(ChatNotification::Error(format!(
                    "Could not clear history: {error:#}"
                )));
            }
        }
    }

    async fn rotate_conversation_document(&mut self, conversation_id: &str) -> Result<u64> {
        let current = self
            .index
            .conversations
            .get(conversation_id)
            .cloned()
            .context("unknown conversation")?;
        let old_document_id = current.public.document_id.clone();
        let old_attachment_hashes: BTreeSet<_> = self
            .load_messages(&current)
            .await?
            .into_iter()
            .flat_map(|message| message.attachments)
            .filter_map(|attachment| Hash::from_str(&attachment.hash).ok())
            .collect();
        let next_epoch = current.public.history_epoch.saturating_add(1);
        let fresh = self
            .create_conversation(
                current.public.id.clone(),
                current.public.title.clone(),
                current.public.kind.clone(),
                current.public.members.clone(),
                next_epoch,
            )
            .await?;
        let epoch = fresh.public.history_epoch;
        let new_document_id = fresh.public.document_id.clone();

        self.forget_conversation_local_state(conversation_id);
        self.subscriptions.remove(conversation_id);
        self.index
            .conversations
            .insert(conversation_id.to_owned(), fresh);
        self.persist_index()?;
        self.release_unreferenced_attachment_tags(old_attachment_hashes)
            .await?;
        self.drop_document(&old_document_id).await;
        self.open_and_publish(conversation_id).await?;
        info!(
            conversation = %log_id(conversation_id),
            old_document = %log_id(&old_document_id),
            new_document = %log_id(&new_document_id),
            history_epoch = epoch,
            "rotated chat document after history delete"
        );
        Ok(epoch)
    }

    async fn release_unreferenced_attachment_tags(
        &self,
        mut candidates: BTreeSet<Hash>,
    ) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }
        for stored in self.index.conversations.values() {
            for message in self.load_messages(stored).await? {
                for attachment in message.attachments {
                    if let Ok(hash) = Hash::from_str(&attachment.hash) {
                        candidates.remove(&hash);
                    }
                }
            }
            if candidates.is_empty() {
                break;
            }
        }
        for hash in candidates {
            self.blobs.set_tag(attachment_blob_tag(hash), None).await?;
        }
        Ok(())
    }

    fn merge_staged_inbound(&mut self, conversation_id: &str, messages: &mut Vec<ChatMessage>) {
        let Some(staged) = self.staged_inbound.get_mut(conversation_id) else {
            return;
        };
        let present: BTreeSet<_> = messages
            .iter()
            .map(|message| message.message_id.clone())
            .collect();
        staged.retain(|message_id, _| !present.contains(message_id));
        for message in staged.values() {
            messages.push(message.clone());
        }
        messages.sort();
        if staged.is_empty() {
            self.staged_inbound.remove(conversation_id);
        }
    }

    fn merge_staged_deletions(&mut self, conversation_id: &str, messages: &mut [ChatMessage]) {
        let Some(staged) = self.staged_deletions.get_mut(conversation_id) else {
            return;
        };
        staged.retain(|message_id, author_id| {
            let Some(message) = messages
                .iter_mut()
                .find(|message| message.message_id == *message_id)
            else {
                return true;
            };
            if message.deletion == Some(MessageDeletion::Everyone) {
                return false;
            }
            if message.author_id == *author_id {
                message.deletion = Some(MessageDeletion::Everyone);
            }
            true
        });
        if staged.is_empty() {
            self.staged_deletions.remove(conversation_id);
        }
    }

    fn forget_conversation_local_state(&mut self, conversation_id: &str) {
        let dropped: Vec<_> = self
            .pending_deliveries
            .iter()
            .filter(|(_, pending_conversation)| *pending_conversation == conversation_id)
            .map(|(message_id, _)| message_id.clone())
            .collect();
        for message_id in dropped {
            self.pending_deliveries.remove(&message_id);
            self.pending_outbound.remove(&message_id);
        }
        self.staged_inbound.remove(conversation_id);
        self.staged_deletions.remove(conversation_id);
        let controls_removed = self
            .reliable_control
            .attachments
            .remove(conversation_id)
            .is_some()
            | self
                .reliable_control
                .deletions
                .remove(conversation_id)
                .is_some()
            | self
                .reliable_control
                .invites
                .remove(conversation_id)
                .is_some();
        self.control_retries.remove(conversation_id);
        if controls_removed {
            self.save_reliable_control();
        }
        self.retry_state.remove(conversation_id);
        self.attachment_retries
            .retain(|_, retry| retry.conversation_id != conversation_id);
        if self
            .local_deletions
            .conversations
            .remove(conversation_id)
            .is_some()
        {
            if let Err(error) = save_local_deletions(
                &self.root.join("local-deletions.json"),
                &self.local_deletions,
            ) {
                warn!(
                    conversation = %log_id(conversation_id),
                    "failed to drop local deletions after history clear: {error:#}"
                );
            }
        }
    }

    async fn drop_document(&self, document_id: &str) {
        let Ok(namespace) = NamespaceId::from_str(document_id) else {
            return;
        };
        if let Err(error) = self.docs.drop_doc(namespace).await {
            warn!(
                document = %log_id(document_id),
                "failed to drop chat document storage: {error:#}"
            );
        }
    }

    async fn create_conversation(
        &self,
        id: String,
        title: String,
        kind: ConversationKind,
        members: Vec<String>,
        history_epoch: u64,
    ) -> Result<StoredConversation> {
        let doc = self.docs.create().await?;
        let ticket = doc
            .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
            .await?;
        Ok(StoredConversation {
            public: ChatConversation {
                id,
                title,
                kind,
                members,
                document_id: doc.id().to_string(),
                history_epoch,
            },
            ticket: ticket.to_string(),
        })
    }

    async fn accept_invite(&mut self, remote: NodeId, mut invite: ChatInvite) -> Result<()> {
        info!(peer = %remote.fmt_short(), conversation = %log_id(&invite.conversation.id), "accepting chat invitation");
        if invite.version != 1 {
            bail!("unsupported invitation version {}", invite.version);
        }
        if invite.conversation.title.len() > 512 || invite.conversation.members.len() > 128 {
            bail!("invitation metadata exceeds safety limits");
        }
        if !invite
            .conversation
            .members
            .iter()
            .any(|id| id == &self.our_node_id.to_string())
            || !invite
                .conversation
                .members
                .iter()
                .any(|id| id == &remote.to_string())
        {
            bail!("invitation membership does not match its sender and recipient");
        }
        if matches!(invite.conversation.kind, ConversationKind::Direct { .. }) {
            let expected = direct_conversation_id(self.our_node_id, remote);
            if invite.conversation.id != expected || invite.conversation.members.len() != 2 {
                bail!("direct-message invitation has inconsistent members");
            }
            invite.conversation.kind = ConversationKind::Direct {
                peer_id: remote.to_string(),
            };
        }
        let _: DocTicket =
            DocTicket::from_str(&invite.ticket).context("invalid document ticket")?;

        let id = invite.conversation.id.clone();
        let current = self.index.conversations.get(&id).cloned();
        let (replace, history_reset) = match current.as_ref() {
            None => (true, false),
            Some(current) => {
                let invite_epoch = invite.conversation.history_epoch;
                let current_epoch = current.public.history_epoch;
                if invite_epoch > current_epoch {
                    (true, true)
                } else if invite_epoch < current_epoch
                    || invite.conversation.document_id == current.public.document_id
                {
                    (false, false)
                } else {
                    (
                        invite.conversation.document_id < current.public.document_id,
                        false,
                    )
                }
            }
        };
        if !replace {
            debug!(
                conversation = %log_id(&id),
                peer = %remote.fmt_short(),
                "ignored chat invitation for a non-canonical replica"
            );
            return Ok(());
        }

        // History resets intentionally discard prior messages. Concurrent DM
        // creation still migrates into the canonical replica.
        let migrated = if history_reset {
            Vec::new()
        } else if let Some(current) = current.as_ref() {
            self.load_messages(current).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        let old_document_id = current
            .as_ref()
            .map(|stored| stored.public.document_id.clone());
        if history_reset {
            self.forget_conversation_local_state(&id);
        }
        self.index.conversations.insert(
            id.clone(),
            StoredConversation {
                public: invite.conversation,
                ticket: invite.ticket,
            },
        );
        self.subscriptions.remove(&id);
        self.persist_index()?;
        if let Some(old_document_id) = old_document_id {
            let new_document_id = &self.index.conversations[&id].public.document_id;
            if old_document_id != *new_document_id {
                self.drop_document(&old_document_id).await;
            }
        }
        self.open_and_publish(&id).await?;
        info!(
            conversation = %log_id(&id),
            history_reset,
            "chat invitation imported"
        );
        if invite.inviter_display_name.is_some()
            || invite.inviter_avatar_hash.is_some()
            || invite.inviter_accent_color.is_some()
        {
            self.queued.push_back(ChatNotification::PeerIdentity {
                peer: remote.to_string(),
                display_name: invite.inviter_display_name,
                avatar_hash: invite.inviter_avatar_hash,
                accent_color: invite.inviter_accent_color,
            });
        }
        for message in migrated {
            let migrate_deletion = message.deletion == Some(MessageDeletion::Everyone);
            if let Err(error) = self.insert_message(&id, &message).await {
                warn!(conversation = %log_id(&id), "failed to migrate a message to the canonical chat document: {error:#}");
            } else if migrate_deletion {
                if let Err(error) = self
                    .insert_replicated_deletion(&id, &message.message_id)
                    .await
                {
                    warn!(conversation = %log_id(&id), "failed to migrate a message deletion to the canonical chat document: {error:#}");
                }
            }
        }
        Ok(())
    }

    async fn pull_conversation(&mut self, id: &str, remote_ticket: Option<&str>) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(id)
            .cloned()
            .context("unknown conversation")?;
        nudge_doc_sync(
            &self.docs,
            &stored,
            self.our_node_id,
            &self.endpoint,
            remote_ticket,
        )
        .await?;
        self.publish_timeline(id).await
    }

    async fn open_and_publish(&mut self, id: &str) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(id)
            .cloned()
            .context("unknown conversation")?;
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let mut peers: Vec<_> = ticket
            .nodes
            .iter()
            .filter(|addr| addr.node_id != self.our_node_id)
            .cloned()
            .collect();
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        for node in stored
            .public
            .members
            .iter()
            .filter_map(|value| NodeId::from_str(value).ok())
            .filter(|node| *node != self.our_node_id)
        {
            if !peers.iter().any(|addr| addr.node_id == node) {
                peers.push(NodeAddr::from(node));
            }
        }
        doc.start_sync(peers).await?;
        let document_id = document_id.to_string();
        if self.subscriptions.get(id) != Some(&document_id) {
            info!(conversation = %log_id(id), "subscribed to chat document events");
            let mut events = doc.subscribe().await?;
            self.subscriptions
                .insert(id.to_owned(), document_id.clone());
            let tx = self.doc_event_tx.clone();
            let id = id.to_owned();
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    match event {
                        Ok(LiveEvent::InsertLocal { .. }) => {
                            debug!(conversation = %log_id(&id), source = "local", "chat document changed");
                            if tx
                                .send(DocumentSignal::Changed {
                                    conversation_id: id.clone(),
                                    from_remote: false,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(LiveEvent::InsertRemote { .. }) => {
                            debug!(conversation = %log_id(&id), source = "remote", "chat document changed");
                            if tx
                                .send(DocumentSignal::Changed {
                                    conversation_id: id.clone(),
                                    from_remote: true,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(LiveEvent::ContentReady { .. }) | Ok(LiveEvent::PendingContentReady) => {
                            debug!(conversation = %log_id(&id), source = "content-ready", "chat document changed");
                            // May be local or remote content; do not use this
                            // alone to resume offline queues (InsertRemote /
                            // inbound protocol cover peer-online).
                            if tx
                                .send(DocumentSignal::Changed {
                                    conversation_id: id.clone(),
                                    from_remote: false,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(LiveEvent::SyncFinished(_)) => {
                            debug!(conversation = %log_id(&id), source = "sync-finished", "chat document changed");
                            // Refresh timeline only. Do NOT mark from_remote —
                            // that re-entered resume/wake on every sync finish
                            // and delayed real receipt handling by seconds.
                            if tx
                                .send(DocumentSignal::Changed {
                                    conversation_id: id.clone(),
                                    from_remote: false,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            warn!(conversation = %log_id(&id), "chat subscription ended: {error:#}");
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                let _ = tx
                    .send(DocumentSignal::SubscriptionEnded {
                        conversation_id: id,
                        document_id,
                    })
                    .await;
            });
        }
        self.publish_timeline(id).await
    }

    async fn publish_timeline(&mut self, id: &str) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(id)
            .cloned()
            .context("unknown conversation")?;
        let limit = self
            .timeline_limits
            .get(id)
            .copied()
            .unwrap_or(CHAT_TIMELINE_PAGE);
        let (mut messages, has_more) = self.load_recent_messages(&stored, limit).await?;
        self.merge_staged_inbound(id, &mut messages);
        self.merge_staged_deletions(id, &mut messages);
        let now = now_millis();
        messages.retain(|message| message.visible_under(self.retention, now));
        for message in &messages {
            if message.author_id == self.our_node_id.to_string() {
                for attachment in &message.attachments {
                    if attachment.kind == AttachmentKind::FileOffer {
                        if let Ok(hash) = Hash::from_str(&attachment.hash) {
                            self.offered_file_sizes.insert(hash, attachment.byte_len);
                        }
                    }
                }
            }
        }
        // Older pages may still contain durable file offers after ordinary
        // messages on this page have expired under the local retention policy.
        self.request_missing_attachments(id, &messages).await;
        let mut wrote_inline_receipts = false;
        for message in &mut messages {
            if message.author_id == self.our_node_id.to_string()
                || !self.retention.includes(message.sent_at, now)
            {
                continue;
            }
            for attachment in &message.attachments {
                if attachment.kind == AttachmentKind::FileOffer
                    || message
                        .file_receivers
                        .get(&attachment.hash)
                        .is_some_and(|receivers| receivers.contains(&self.our_node_id.to_string()))
                {
                    continue;
                }
                let Ok(hash) = Hash::from_str(&attachment.hash) else {
                    continue;
                };
                if self
                    .blobs
                    .get(&hash)
                    .await?
                    .is_some_and(|blob| blob.is_complete())
                {
                    self.record_file_receipt(id, &message.message_id, &attachment.hash)
                        .await?;
                    message
                        .file_receivers
                        .entry(attachment.hash.clone())
                        .or_default()
                        .insert(self.our_node_id.to_string());
                    wrote_inline_receipts = true;
                }
            }
        }
        if wrote_inline_receipts {
            self.spawn_doc_sync(id);
        }
        for message in &messages {
            if message.author_id != self.our_node_id.to_string() {
                if let Some(version) = message.client_version.as_deref() {
                    if let Ok(author) = NodeId::from_str(&message.author_id) {
                        note_peer_client_version(author, Some(version), "message");
                    }
                }
            }
        }
        let new_receipts = self
            .acknowledge_received_messages(&stored, &messages)
            .await?;
        // Friend already sees their message on our side; their UI stays on
        // "syncing" until they pull our receipt. Debounce wakes — firing one
        // per timeline publish caused dial storms that blocked real sends.
        // Receipts ride the chat ALPN wake so the sender need not wait on docs.
        if !new_receipts.is_empty() {
            self.spawn_doc_sync(id);
            self.spawn_receipt_wake(id, new_receipts);
        }
        let delivered = self.load_delivered_message_ids(&stored).await?;
        for message in &messages {
            if message.author_id == self.our_node_id.to_string()
                && delivered.contains(&message.message_id)
                && self
                    .pending_deliveries
                    .remove(&message.message_id)
                    .is_some()
            {
                self.pending_outbound.remove(&message.message_id);
                info!(
                    conversation = %log_id(id),
                    message = %log_id(&message.message_id),
                    "chat message marked delivered after remote receipt"
                );
                self.queued.push_back(ChatNotification::Delivery {
                    message_id: message.message_id.clone(),
                    state: DeliveryState::Delivered,
                    detail: None,
                });
            }
        }
        self.retry_state.retain(|_, state| {
            state
                .messages
                .retain(|message_id| self.pending_deliveries.contains_key(message_id));
            !state.messages.is_empty()
        });
        debug!(
            conversation = %log_id(id),
            messages = messages.len(),
            "published chat timeline"
        );
        self.queued.push_back(ChatNotification::Conversation {
            conversation: stored.public,
            messages,
            has_more,
        });
        Ok(())
    }

    fn schedule_attachment_retry(&mut self, conversation_id: &str, hash: Hash) {
        let retry = self
            .attachment_retries
            .entry(hash)
            .or_insert_with(|| AttachmentRetry {
                conversation_id: conversation_id.to_owned(),
                attempts: 0,
                next_attempt: tokio::time::Instant::now(),
            });
        retry.conversation_id = conversation_id.to_owned();
        retry.attempts = retry.attempts.saturating_add(1);
        retry.next_attempt = tokio::time::Instant::now() + attachment_retry_delay(retry.attempts);
        trace!(
            conversation = %log_id(conversation_id),
            image = %hash.fmt_short(),
            attempts = retry.attempts,
            "scheduled missing image retry"
        );
    }

    async fn retry_due_attachment_downloads(&mut self) {
        let now = tokio::time::Instant::now();
        let due: Vec<_> = self
            .attachment_retries
            .iter()
            .filter(|(hash, retry)| {
                retry.next_attempt <= now && !self.attachment_downloads.contains(*hash)
            })
            .map(|(hash, retry)| (*hash, retry.conversation_id.clone()))
            .collect();

        for (hash, conversation_id) in due {
            let Some(stored) = self.index.conversations.get(&conversation_id).cloned() else {
                self.attachment_retries.remove(&hash);
                continue;
            };
            let mut messages = match self.load_messages(&stored).await {
                Ok(messages) => messages,
                Err(error) => {
                    trace!(
                        conversation = %log_id(&conversation_id),
                        image = %hash.fmt_short(),
                        "could not inspect missing image before retry: {error:#}"
                    );
                    self.schedule_attachment_retry(&conversation_id, hash);
                    continue;
                }
            };
            self.merge_staged_inbound(&conversation_id, &mut messages);
            let hash_string = hash.to_string();
            let still_needed = messages.iter().any(|message| {
                message.author_id != self.our_node_id.to_string()
                    && self.retention.includes(message.sent_at, now_millis())
                    && message.attachments.iter().any(|attachment| {
                        attachment.hash == hash_string
                            && attachment.kind != AttachmentKind::FileOffer
                            && attachment.data.is_none()
                            && !(attachment.kind == AttachmentKind::Image
                                && self
                                    .max_image_bytes
                                    .is_some_and(|limit| attachment.byte_len > limit))
                    })
            });
            if !still_needed {
                self.attachment_retries.remove(&hash);
                continue;
            }
            self.request_missing_attachments(&conversation_id, &messages)
                .await;
        }
    }

    fn attachment_providers(
        &self,
        conversation_id: &str,
        preferred: Option<NodeId>,
    ) -> Vec<NodeAddr> {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return Vec::new();
        };
        let mut providers = BTreeMap::<NodeId, NodeAddr>::new();

        // The document ticket may contain the only currently usable relay or
        // direct address. Register it before the blobs downloader dials by
        // NodeId; iroh-blobs' downloader uses the endpoint address book for
        // those dials.
        if let Ok(ticket) = DocTicket::from_str(&stored.ticket) {
            for address in ticket.nodes {
                if address.node_id == self.our_node_id {
                    continue;
                }
                if !address.is_empty() {
                    let _ = self.endpoint.add_node_addr(address.clone());
                }
                providers.insert(address.node_id, address);
            }
        }

        for node in stored
            .public
            .members
            .iter()
            .filter_map(|member| NodeId::from_str(member).ok())
            .filter(|node| *node != self.our_node_id)
        {
            if let Some(info) = self.endpoint.remote_info(node) {
                let address: NodeAddr = info.into();
                if !address.is_empty() {
                    let _ = self.endpoint.add_node_addr(address.clone());
                }
                providers.insert(node, address);
            } else {
                providers
                    .entry(node)
                    .or_insert_with(|| NodeAddr::from(node));
            }
        }
        let mut providers: Vec<_> = providers.into_values().collect();
        // The author is the only peer guaranteed to have the original blob.
        // Trying it first also avoids waiting on unrelated/offline group
        // members before reaching the actual source.
        if let Some(preferred) = preferred {
            providers.sort_by_key(|address| address.node_id != preferred);
        }
        providers
    }

    async fn request_missing_attachments(
        &mut self,
        conversation_id: &str,
        messages: &[ChatMessage],
    ) {
        let now = tokio::time::Instant::now();
        for message in messages {
            if message.author_id == self.our_node_id.to_string() {
                continue;
            }
            let author = NodeId::from_str(&message.author_id).ok();
            let providers = self.attachment_providers(conversation_id, author);
            for attachment in &message.attachments {
                if attachment.kind == AttachmentKind::FileOffer
                    || !self.retention.includes(message.sent_at, now_millis())
                {
                    continue;
                }
                if attachment.kind == AttachmentKind::Image
                    && self
                        .max_image_bytes
                        .is_some_and(|limit| attachment.byte_len > limit)
                {
                    continue;
                }
                let Ok(hash) = Hash::from_str(&attachment.hash) else {
                    continue;
                };
                if self
                    .attachment_retries
                    .get(&hash)
                    .is_some_and(|retry| retry.next_attempt > now)
                {
                    continue;
                }
                if self
                    .blobs
                    .get(&hash)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|blob| blob.is_complete())
                {
                    self.attachment_retries.remove(&hash);
                    continue;
                }
                if attachment.data.is_some() || !self.attachment_downloads.insert(hash) {
                    continue;
                }
                let downloader = self.blob_downloader.clone();
                let blob_store = self.blobs.clone();
                let tx = self.wake_tx.clone();
                let conversation_id = conversation_id.to_owned();
                let providers = providers.clone();
                let byte_len = attachment.byte_len;
                let timeout = attachment_download_timeout(byte_len);
                if providers.is_empty() {
                    self.attachment_downloads.remove(&hash);
                    self.schedule_attachment_retry(&conversation_id, hash);
                    continue;
                }
                tokio::spawn(async move {
                    let hash_and_format = HashAndFormat::raw(hash);
                    // iroh-blobs requires the caller to pin a download while it
                    // is in progress; otherwise GC is allowed to remove its
                    // partial/just-completed data before the UI consumes it.
                    let _download_pin = blob_store.temp_tag(hash_and_format);
                    let request = DownloadRequest::new(hash_and_format, providers);
                    let mut handle = downloader.queue(request).await;
                    let mut succeeded = match tokio::time::timeout(timeout, &mut handle).await {
                        Ok(Ok(_)) => true,
                        Ok(Err(error)) => {
                            debug!(image = %hash.fmt_short(), "image download failed: {error}");
                            false
                        }
                        Err(_) => {
                            // Dropping a DownloadHandle does not cancel its
                            // intent. Explicit cancellation is required or a
                            // stuck intent keeps this hash occupied forever.
                            downloader.cancel(handle).await;
                            debug!(
                                image = %hash.fmt_short(),
                                timeout_ms = timeout.as_millis(),
                                "image download timed out; cancelled for a fresh retry"
                            );
                            false
                        }
                    };
                    if succeeded {
                        if let Err(error) = blob_store
                            .set_tag(attachment_blob_tag(hash), Some(hash_and_format))
                            .await
                        {
                            debug!(
                                image = %hash.fmt_short(),
                                "could not persist downloaded image tag: {error}"
                            );
                            succeeded = false;
                        }
                    }
                    let _ = tx
                        .send(ChatInput::AttachmentDownloadFinished {
                            conversation_id,
                            hash,
                            byte_len,
                            succeeded,
                        })
                        .await;
                });
            }
        }
    }

    async fn acknowledge_received_messages(
        &self,
        stored: &StoredConversation,
        messages: &[ChatMessage],
    ) -> Result<Vec<ReplicatedReceipt>> {
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let mut wrote = Vec::new();
        let mut existing_entries = doc
            .get_many(
                Query::author(self.author)
                    .key_prefix(RECEIPT_PREFIX)
                    .build(),
            )
            .await?;
        let mut existing = BTreeSet::new();
        while let Some(entry) = existing_entries.next().await {
            let entry = entry?;
            if let Some(message_id) = receipt_message_id_from_key(entry.key()) {
                existing.insert(message_id);
            }
        }
        for message in messages
            .iter()
            .filter(|message| message.author_id != self.our_node_id.to_string())
        {
            let receipt = ReplicatedReceipt::new(message.message_id.clone());
            if existing.insert(message.message_id.clone()) {
                doc.set_bytes(
                    self.author,
                    receipt.entry_key(),
                    serde_json::to_vec(&receipt)?,
                )
                .await?;
                debug!(
                    conversation = %log_id(&stored.public.id),
                    message = %log_id(&message.message_id),
                    "acknowledged received chat message"
                );
                wrote.push(receipt);
            }
        }
        Ok(wrote)
    }

    async fn load_delivered_message_ids(
        &self,
        stored: &StoredConversation,
    ) -> Result<BTreeSet<String>> {
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let mut entries = doc
            .get_many(Query::key_prefix(RECEIPT_PREFIX).build())
            .await?;
        let mut delivered = BTreeSet::new();
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            if entry.author() == self.author {
                continue;
            }
            // The key is `receipt/{message_id}`. Trust a remote receipt entry as
            // soon as it exists — waiting for the blob body caused the UI to
            // keep showing delivery retries after the peer already had the
            // message (and had already written the receipt).
            if let Some(message_id) = receipt_message_id_from_key(entry.key()) {
                delivered.insert(message_id);
                continue;
            }
            let len = usize::try_from(entry.content_len()).unwrap_or(usize::MAX);
            if len == 0 || len > 16 * 1024 {
                continue;
            }
            let Some(blob) = self.blobs.get(&entry.content_hash()).await? else {
                continue;
            };
            if !blob.is_complete() {
                continue;
            }
            let mut reader = blob.data_reader();
            let bytes = reader.read_at(0, len).await?;
            let Ok(receipt) = serde_json::from_slice::<ReplicatedReceipt>(&bytes) else {
                continue;
            };
            if receipt.validate().is_ok() {
                delivered.insert(receipt.message_id);
            }
        }
        Ok(delivered)
    }

    async fn load_messages(&self, stored: &StoredConversation) -> Result<Vec<ChatMessage>> {
        self.load_messages_query(stored, None).await
    }

    async fn load_recent_messages(
        &self,
        stored: &StoredConversation,
        limit: usize,
    ) -> Result<(Vec<ChatMessage>, bool)> {
        let query_limit = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
        let mut messages = self.load_messages_query(stored, Some(query_limit)).await?;
        let has_more = messages.len() > limit;
        if has_more {
            let excess = messages.len() - limit;
            messages.drain(..excess);
        }
        Ok((messages, has_more))
    }

    async fn load_messages_query(
        &self,
        stored: &StoredConversation,
        limit: Option<u64>,
    ) -> Result<Vec<ChatMessage>> {
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let mut query =
            Query::key_prefix(MESSAGE_PREFIX).sort_by(SortBy::KeyAuthor, SortDirection::Desc);
        if let Some(limit) = limit {
            query = query.limit(limit);
        }
        let mut entries = doc.get_many(query.build()).await?;
        let mut messages = BTreeMap::<String, (ChatMessage, AuthorId)>::new();
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            let len = usize::try_from(entry.content_len()).unwrap_or(usize::MAX);
            if len == 0 || len > MAX_MESSAGE_BYTES + 16 * 1024 {
                continue;
            }
            let Some(blob) = self.blobs.get(&entry.content_hash()).await? else {
                continue;
            };
            if !blob.is_complete() {
                continue;
            }
            let mut reader = blob.data_reader();
            let bytes = reader.read_at(0, len).await?;
            let Ok(message) = serde_json::from_slice::<ChatMessage>(&bytes) else {
                continue;
            };
            if message.validate().is_ok() {
                messages
                    .entry(message.message_id.clone())
                    .or_insert((message, entry.author()));
            }
        }

        let mut deletion_entries = doc
            .get_many(Query::key_prefix(DELETION_PREFIX).build())
            .await?;
        while let Some(entry) = deletion_entries.next().await {
            let entry = entry?;
            let len = usize::try_from(entry.content_len()).unwrap_or(usize::MAX);
            if len == 0 || len > 16 * 1024 {
                continue;
            }
            let Some(blob) = self.blobs.get(&entry.content_hash()).await? else {
                continue;
            };
            if !blob.is_complete() {
                continue;
            }
            let mut reader = blob.data_reader();
            let bytes = reader.read_at(0, len).await?;
            let Ok(deletion) = serde_json::from_slice::<ReplicatedDeletion>(&bytes) else {
                continue;
            };
            if deletion.validate().is_err() {
                continue;
            }
            if let Some((message, message_author)) = messages.get_mut(&deletion.message_id) {
                if entry.author() == *message_author {
                    message.deletion = Some(MessageDeletion::Everyone);
                }
            }
        }

        let mut file_receipts = doc
            .get_many(Query::key_prefix(FILE_RECEIPT_PREFIX).build())
            .await?;
        while let Some(entry) = file_receipts.next().await {
            let entry = entry?;
            let len = usize::try_from(entry.content_len()).unwrap_or(usize::MAX);
            if len == 0 || len > 4096 {
                continue;
            }
            let Some(blob) = self.blobs.get(&entry.content_hash()).await? else {
                continue;
            };
            if !blob.is_complete() {
                continue;
            }
            let mut reader = blob.data_reader();
            let bytes = reader.read_at(0, len).await?;
            let Ok(receipt) = serde_json::from_slice::<FileReceipt>(&bytes) else {
                continue;
            };
            if receipt.entry_key().as_bytes() != entry.key()
                || !stored.public.members.contains(&receipt.receiver_id)
            {
                continue;
            }
            if let Some((message, _)) = messages.get_mut(&receipt.message_id) {
                if message
                    .attachments
                    .iter()
                    .any(|attachment| attachment.hash == receipt.hash)
                {
                    message
                        .file_receivers
                        .entry(receipt.hash)
                        .or_default()
                        .insert(receipt.receiver_id);
                }
            }
        }

        let mut sharing_entries = doc
            .get_many(Query::key_prefix(FILE_SHARING_PREFIX).build())
            .await?;
        let mut sharing_by_owner_hash = BTreeMap::<(String, String), (i64, bool)>::new();
        while let Some(entry) = sharing_entries.next().await {
            let entry = entry?;
            let len = usize::try_from(entry.content_len()).unwrap_or(usize::MAX);
            if len == 0 || len > 4096 {
                continue;
            }
            let Some(blob) = self.blobs.get(&entry.content_hash()).await? else {
                continue;
            };
            if !blob.is_complete() {
                continue;
            }
            let mut reader = blob.data_reader();
            let bytes = reader.read_at(0, len).await?;
            let Ok(state) = serde_json::from_slice::<FileSharingState>(&bytes) else {
                continue;
            };
            if state.entry_key().as_bytes() != entry.key() {
                continue;
            }
            if let Some((message, author)) = messages.get(&state.message_id) {
                if entry.author() != *author
                    || !message.attachments.iter().any(|attachment| {
                        attachment.kind == AttachmentKind::FileOffer
                            && attachment.hash == state.hash
                    })
                {
                    continue;
                }
                let key = (message.author_id.clone(), state.hash);
                let current = sharing_by_owner_hash
                    .entry(key)
                    .or_insert((state.changed_at, state.serving));
                if state.changed_at > current.0 {
                    *current = (state.changed_at, state.serving);
                }
            }
        }

        for (message, _) in messages.values_mut() {
            for attachment in &message.attachments {
                if attachment.kind != AttachmentKind::FileOffer {
                    continue;
                }
                let stopped_locally = message.author_id == self.our_node_id.to_string()
                    && self.stopped_file_index.hashes.contains(&attachment.hash);
                let stopped_remotely = sharing_by_owner_hash
                    .get(&(message.author_id.clone(), attachment.hash.clone()))
                    .is_some_and(|(_, serving)| !serving);
                if stopped_locally || stopped_remotely {
                    message.stopped_file_offers.insert(attachment.hash.clone());
                }
            }
        }

        if let Some(locally_deleted) = self.local_deletions.conversations.get(&stored.public.id) {
            for message_id in locally_deleted {
                if let Some((message, _)) = messages.get_mut(message_id) {
                    message.deletion = Some(MessageDeletion::Local);
                }
            }
        }

        let mut messages: Vec<_> = messages.into_values().map(|(message, _)| message).collect();
        messages.sort();
        Ok(messages)
    }

    fn delete_message_locally(&mut self, conversation_id: &str, message_id: &str) -> Result<()> {
        if !self.index.conversations.contains_key(conversation_id) {
            bail!("unknown conversation");
        }
        if !is_message_id(message_id) {
            bail!("invalid message id");
        }
        let inserted = self
            .local_deletions
            .conversations
            .entry(conversation_id.to_owned())
            .or_default()
            .insert(message_id.to_owned());
        if let Err(error) = save_local_deletions(
            &self.root.join("local-deletions.json"),
            &self.local_deletions,
        ) {
            if inserted {
                if let Some(message_ids) =
                    self.local_deletions.conversations.get_mut(conversation_id)
                {
                    message_ids.remove(message_id);
                    if message_ids.is_empty() {
                        self.local_deletions.conversations.remove(conversation_id);
                    }
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn restore_message_locally(&mut self, conversation_id: &str, message_id: &str) -> Result<()> {
        if !self.index.conversations.contains_key(conversation_id) {
            bail!("unknown conversation");
        }
        if !is_message_id(message_id) {
            bail!("invalid message id");
        }
        let removed = self
            .local_deletions
            .conversations
            .get_mut(conversation_id)
            .is_some_and(|message_ids| message_ids.remove(message_id));
        if !removed {
            bail!("message is not deleted locally");
        }
        if self
            .local_deletions
            .conversations
            .get(conversation_id)
            .is_some_and(BTreeSet::is_empty)
        {
            self.local_deletions.conversations.remove(conversation_id);
        }
        if let Err(error) = save_local_deletions(
            &self.root.join("local-deletions.json"),
            &self.local_deletions,
        ) {
            self.local_deletions
                .conversations
                .entry(conversation_id.to_owned())
                .or_default()
                .insert(message_id.to_owned());
            return Err(error);
        }
        Ok(())
    }

    async fn insert_replicated_deletion(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<ReplicatedDeletion> {
        if !is_message_id(message_id) {
            bail!("invalid message id");
        }
        let stored = self
            .index
            .conversations
            .get(conversation_id)
            .context("unknown conversation")?;
        let messages = self.load_messages(stored).await?;
        let message = messages
            .iter()
            .find(|message| message.message_id == message_id)
            .context("message is no longer available")?;
        if message.author_id != self.our_node_id.to_string() {
            bail!("only the author can delete a message for everyone");
        }

        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let mut authored_entries = doc
            .get_many(
                Query::author(self.author)
                    .key_exact(message.entry_key())
                    .build(),
            )
            .await?;
        if authored_entries.next().await.transpose()?.is_none() {
            bail!("the local identity did not author this message");
        }
        let deletion = ReplicatedDeletion::new(message_id.to_owned());
        doc.set_bytes(
            self.author,
            deletion.entry_key(),
            serde_json::to_vec(&deletion)?,
        )
        .await?;
        Ok(deletion)
    }

    async fn insert_message(&self, conversation_id: &str, message: &ChatMessage) -> Result<()> {
        message.validate()?;
        if message.author_id != self.our_node_id.to_string() {
            bail!("cannot send a message for a different Wire identity");
        }
        let stored = self
            .index
            .conversations
            .get(conversation_id)
            .context("unknown conversation")?;
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let value = serde_json::to_vec(message)?;
        doc.set_bytes(self.author, message.entry_key(), value)
            .await?;
        Ok(())
    }

    fn send_pending_invites(&self, conversation_id: &str) {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return;
        };
        let Some(pending) = self.reliable_control.invites.get(conversation_id) else {
            return;
        };
        if pending.history_epoch != stored.public.history_epoch {
            return;
        }
        let peers: Vec<_> = self
            .other_members(stored)
            .into_iter()
            .filter(|peer| pending.pending_peers.contains(&peer.to_string()))
            .collect();
        if peers.is_empty() {
            return;
        }
        #[cfg(test)]
        self.invite_attempts
            .fetch_add(peers.len() as u64, Ordering::Relaxed);
        let sessions = self.sessions.clone();
        let docs = self.docs.clone();
        let endpoint = self.endpoint.clone();
        let stored = stored.clone();
        let own_display_name = self.own_display_name.clone();
        let own_avatar_hash = self.own_avatar_hash.clone();
        let own_accent_color = self.own_accent_color.clone();
        tokio::spawn(async move {
            let ticket = refresh_share_ticket(&docs, &stored, &endpoint)
                .await
                .unwrap_or_else(|_| stored.ticket.clone());
            let invite = ChatInvite {
                version: 1,
                conversation: stored.public,
                ticket,
                client_version: Some(crate::APP_VERSION.to_owned()),
                inviter_display_name: own_display_name,
                inviter_avatar_hash: own_avatar_hash,
                inviter_accent_color: own_accent_color,
            };
            for peer in peers {
                if let Err(error) = send_invite(sessions.clone(), peer, &invite).await {
                    trace!(peer = %peer.fmt_short(), "chat peer not currently reachable: {error:#}");
                }
            }
        });
    }

    async fn invite_members_wait(&mut self, conversation_id: &str) {
        self.register_invite_delivery(conversation_id);
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return;
        };
        let ticket = refresh_share_ticket(&self.docs, stored, &self.endpoint)
            .await
            .unwrap_or_else(|_| stored.ticket.clone());
        let invite = ChatInvite {
            version: 1,
            conversation: stored.public.clone(),
            ticket,
            client_version: Some(crate::APP_VERSION.to_owned()),
            inviter_display_name: self.own_display_name.clone(),
            inviter_avatar_hash: self.own_avatar_hash.clone(),
            inviter_accent_color: self.own_accent_color.clone(),
        };
        let mut join_set = tokio::task::JoinSet::new();
        for peer in self.other_members(stored) {
            #[cfg(test)]
            self.invite_attempts.fetch_add(1, Ordering::Relaxed);
            let sessions = self.sessions.clone();
            let invite = invite.clone();
            join_set.spawn(async move { send_invite(sessions, peer, &invite).await.is_ok() });
        }
        while join_set.join_next().await.is_some() {}
    }

    fn other_members(&self, stored: &StoredConversation) -> Vec<NodeId> {
        stored
            .public
            .members
            .iter()
            .filter_map(|member| NodeId::from_str(member).ok())
            .filter(|peer| *peer != self.our_node_id)
            .collect()
    }

    fn persist_index(&self) -> Result<()> {
        save_index(&self.root.join("index.json"), &self.index)
    }
}

async fn send_invite(sessions: ChatSessionPool, peer: NodeId, invite: &ChatInvite) -> Result<()> {
    send_chat_packet(sessions, peer, ChatProtocolMessage::Invite(invite.clone())).await
}

async fn send_sync_request(
    sessions: ChatSessionPool,
    peer: NodeId,
    conversation_id: &str,
    ticket: Option<&str>,
    payload: WakePayload,
) -> Result<()> {
    send_chat_packet(
        sessions,
        peer,
        ChatProtocolMessage::SyncRequest(SyncRequest {
            kind: "sync-request".to_owned(),
            version: 1,
            conversation_id: conversation_id.to_owned(),
            ticket: ticket.map(str::to_owned),
            client_version: Some(crate::APP_VERSION.to_owned()),
            messages: payload.messages,
            receipts: payload.receipts,
            deletions: payload.deletions,
            attachment_acks: payload.attachment_acks,
            deletion_acks: payload.deletion_acks,
            accepted_history_epoch: payload.accepted_history_epoch,
        }),
    )
    .await
}

async fn push_pending_attachments(
    blobs: BlobStore,
    sessions: ChatSessionPool,
    peers: &[NodeId],
    conversation_id: &str,
    attachments: &[PendingAttachmentDelivery],
) {
    for attachment in attachments {
        if attachment.byte_len == 0 || attachment.byte_len > MAX_ATTACHMENT_PUSH_BYTES {
            continue;
        }
        let Ok(hash) = Hash::from_str(&attachment.hash) else {
            continue;
        };
        let Ok(Some(blob)) = blobs.get(&hash).await else {
            continue;
        };
        if !blob.is_complete() {
            continue;
        }
        let Ok(len) = usize::try_from(attachment.byte_len) else {
            continue;
        };
        let mut reader = blob.data_reader();
        let Ok(data) = reader.read_at(0, len).await else {
            continue;
        };
        for peer in peers {
            if !attachment.pending_peers.contains(&peer.to_string()) {
                continue;
            }
            let packet = ChatProtocolMessage::AttachmentPush(AttachmentPush {
                kind: "attachment-push".to_owned(),
                version: 1,
                conversation_id: conversation_id.to_owned(),
                hash: attachment.hash.clone(),
                byte_len: attachment.byte_len,
                attachment_kind: attachment.attachment_kind,
                sent_at: attachment.sent_at,
                data: data.to_vec(),
            });
            if let Err(error) = send_chat_packet(sessions.clone(), *peer, packet).await {
                debug!(
                    conversation = %log_id(conversation_id),
                    peer = %peer.fmt_short(),
                    image = %attachment.hash.get(..10).unwrap_or(&attachment.hash),
                    "could not push chat image over sender connection: {error:#}"
                );
            }
        }
    }
}

async fn wake_peers(
    sessions: ChatSessionPool,
    peers: Vec<NodeId>,
    conversation_id: &str,
    ticket: Option<&str>,
    payload: WakePayload,
) -> bool {
    if peers.is_empty() {
        return false;
    }
    let mut join_set = tokio::task::JoinSet::new();
    for peer in peers {
        let sessions = sessions.clone();
        let conversation_id = conversation_id.to_owned();
        let ticket = ticket.map(str::to_owned);
        let payload = payload.clone();
        join_set.spawn(async move {
            match send_sync_request(sessions, peer, &conversation_id, ticket.as_deref(), payload)
                .await
            {
                Ok(()) => true,
                Err(error) => {
                    trace!(
                        peer = %peer.fmt_short(),
                        conversation = %log_id(&conversation_id),
                        "chat delivery wake-up did not reach peer: {error:#}"
                    );
                    false
                }
            }
        });
    }
    let mut reached = false;
    while let Some(result) = join_set.join_next().await {
        reached |= result.unwrap_or(false);
    }
    reached
}

async fn refresh_share_ticket(
    docs: &MemClient,
    stored: &StoredConversation,
    endpoint: &Endpoint,
) -> Result<String> {
    let ticket = DocTicket::from_str(&stored.ticket)?;
    let document_id = NamespaceId::from_str(&stored.public.document_id)?;
    let doc = match docs.open(document_id).await {
        Ok(Some(doc)) => doc,
        _ => docs.import(ticket).await?,
    };
    // Ensure our own endpoint addresses are current before sharing.
    let _ = endpoint.node_id();
    let fresh = doc
        .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
        .await?;
    Ok(fresh.to_string())
}

async fn nudge_doc_sync(
    docs: &MemClient,
    stored: &StoredConversation,
    our_node_id: NodeId,
    endpoint: &Endpoint,
    remote_ticket: Option<&str>,
) -> Result<()> {
    let local_ticket = DocTicket::from_str(&stored.ticket)?;
    let mut peers: Vec<_> = local_ticket
        .nodes
        .iter()
        .filter(|addr| addr.node_id != our_node_id)
        .cloned()
        .collect();
    // Prefer addresses from the sender's fresh ticket when present.
    if let Some(remote) = remote_ticket {
        if let Ok(ticket) = DocTicket::from_str(remote) {
            let remote_doc = ticket.capability.id().to_string();
            if remote_doc == stored.public.document_id {
                for node in ticket.nodes {
                    if node.node_id == our_node_id {
                        continue;
                    }
                    if let Some(existing) = peers.iter_mut().find(|p| p.node_id == node.node_id) {
                        *existing = node;
                    } else {
                        peers.push(node);
                    }
                }
            }
        }
    }
    // Enrich with whatever magicsock already learned (chat ALPN path, etc.).
    for node in stored
        .public
        .members
        .iter()
        .filter_map(|value| NodeId::from_str(value).ok())
        .filter(|node| *node != our_node_id)
    {
        if let Some(info) = endpoint.remote_info(node) {
            let addr: NodeAddr = info.into();
            if let Some(existing) = peers.iter_mut().find(|p| p.node_id == node) {
                *existing = addr;
            } else {
                peers.push(addr);
            }
        } else if !peers.iter().any(|addr| addr.node_id == node) {
            peers.push(NodeAddr::from(node));
        }
    }
    let document_id = NamespaceId::from_str(&stored.public.document_id)?;
    let doc = match docs.open(document_id).await {
        Ok(Some(doc)) => doc,
        _ => docs.import(local_ticket).await?,
    };
    doc.start_sync(peers).await?;
    Ok(())
}

async fn send_chat_packet(
    sessions: ChatSessionPool,
    peer: NodeId,
    packet: ChatProtocolMessage,
) -> Result<()> {
    let payload = serde_json::to_vec(&packet)?;
    if payload.len() > MAX_INVITE_BYTES {
        bail!("chat protocol message exceeds safety cap");
    }
    let body = match &packet {
        ChatProtocolMessage::AttachmentPush(push) => push.data.as_slice(),
        _ => &[],
    };
    let stream_timeout = if body.is_empty() {
        CHAT_STREAM_TIMEOUT
    } else {
        attachment_download_timeout(body.len() as u64)
    };
    // One send/dial at a time per peer — concurrent wakes were closing each
    // other's fresh connections and forcing multi-second redial storms.
    let gate = sessions.peer_gate(peer).await;
    let _guard = gate.lock().await;

    // Try warm session with a tight timeout, then fresh dial.
    if let Some(connection) = sessions.get(peer).await {
        match tokio::time::timeout(
            if body.is_empty() {
                CHAT_REUSE_TIMEOUT
            } else {
                stream_timeout
            },
            send_chat_packet_on(&connection, &payload, body),
        )
        .await
        {
            Ok(Ok(())) => {
                sessions.touch(peer).await;
                sessions.remember_connection(peer, &connection);
                log_chat_packet_sent(peer, &packet, true);
                return Ok(());
            }
            Ok(Err(error)) => {
                trace!(
                    peer = %peer.fmt_short(),
                    "chat session reuse failed; redialing: {error:#}"
                );
                sessions.forget_if(peer, &connection).await;
            }
            Err(_) => {
                trace!(peer = %peer.fmt_short(), "chat session reuse timed out; redialing");
                sessions.forget_if(peer, &connection).await;
            }
        }
    }
    let connection = sessions.dial(peer).await?;
    match tokio::time::timeout(
        stream_timeout,
        send_chat_packet_on(&connection, &payload, body),
    )
    .await
    {
        Ok(Ok(())) => {
            sessions.touch(peer).await;
            sessions.remember_connection(peer, &connection);
            log_chat_packet_sent(peer, &packet, false);
            Ok(())
        }
        Ok(Err(error)) => {
            sessions.forget_if(peer, &connection).await;
            Err(error).context("chat session send failed")
        }
        Err(_) => {
            sessions.forget_if(peer, &connection).await;
            bail!("chat session send timed out")
        }
    }
}

fn log_chat_packet_sent(peer: NodeId, packet: &ChatProtocolMessage, reused: bool) {
    match packet {
        ChatProtocolMessage::Invite(invite) => {
            info!(
                peer = %peer.fmt_short(),
                conversation = %log_id(&invite.conversation.id),
                reused,
                "chat invitation sent"
            );
        }
        ChatProtocolMessage::SyncRequest(request) => {
            debug!(
                peer = %peer.fmt_short(),
                conversation = %log_id(&request.conversation_id),
                reused,
                messages = request.messages.len(),
                receipts = request.receipts.len(),
                deletions = request.deletions.len(),
                attachment_acks = request.attachment_acks.len(),
                deletion_acks = request.deletion_acks.len(),
                accepted_history_epoch = ?request.accepted_history_epoch,
                "chat delivery wake-up sent"
            );
        }
        ChatProtocolMessage::AttachmentPush(push) => {
            debug!(
                peer = %peer.fmt_short(),
                conversation = %log_id(&push.conversation_id),
                image = %push.hash.get(..10).unwrap_or(&push.hash),
                bytes = push.byte_len,
                reused,
                "chat image pushed over sender connection"
            );
        }
    }
}

async fn send_chat_packet_on(connection: &Connection, payload: &[u8], body: &[u8]) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    send.write_all(payload).await?;
    if !body.is_empty() {
        send.write_all(body).await?;
    }
    send.finish()?;
    let mut ack = [0u8; 2];
    recv.read_exact(&mut ack).await?;
    if &ack != b"ok" {
        bail!("chat peer returned an invalid protocol acknowledgement");
    }
    Ok(())
}

async fn accept_chat_stream(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<ChatProtocolMessage> {
    let mut length = [0u8; 4];
    recv.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_INVITE_BYTES {
        bail!("chat invitation exceeds safety cap");
    }
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes).await?;
    let mut message: ChatProtocolMessage =
        serde_json::from_slice(&bytes).context("invalid Wire chat protocol message")?;
    if let ChatProtocolMessage::AttachmentPush(push) = &mut message {
        if push.byte_len == 0 || push.byte_len > MAX_ATTACHMENT_PUSH_BYTES {
            bail!("chat attachment push exceeds safety cap");
        }
        let len = usize::try_from(push.byte_len).context("attachment push is too large")?;
        push.data.resize(len, 0);
        recv.read_exact(&mut push.data).await?;
    }
    send.write_all(b"ok").await?;
    send.finish()?;
    Ok(message)
}

fn log_id(value: &str) -> &str {
    value.get(..24).unwrap_or(value)
}

fn note_peer_client_version(peer: NodeId, version: Option<&str>, via: &str) {
    use std::sync::Mutex;
    static SEEN: Mutex<BTreeMap<String, Option<String>>> = Mutex::new(BTreeMap::new());
    let key = peer.to_string();
    let next = version.map(str::to_owned);
    let mut guard = match SEEN.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    if guard.get(&key) == Some(&next) {
        return;
    }
    guard.insert(key, next.clone());
    match next.as_deref() {
        Some(version) if version == crate::APP_VERSION => {
            info!(
                peer = %peer.fmt_short(),
                peer_version = %version,
                local_version = %crate::APP_VERSION,
                via,
                "peer client version matches local"
            );
        }
        Some(version) => {
            info!(
                peer = %peer.fmt_short(),
                peer_version = %version,
                local_version = %crate::APP_VERSION,
                via,
                "peer client version differs from local"
            );
        }
        None => {
            info!(
                peer = %peer.fmt_short(),
                local_version = %crate::APP_VERSION,
                via,
                "peer did not advertise client_version (older build or stripped field)"
            );
        }
    }
}

pub fn direct_conversation_id(a: NodeId, b: NodeId) -> String {
    let mut ids = [a.to_string(), b.to_string()];
    ids.sort();
    format!("dm/{}/{}", ids[0], ids[1])
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn next_nonce() -> u64 {
    let counter = NONCE.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    time.rotate_left(17) ^ counter
}

fn delivery_retry_delay(conversation_id: &str, attempts: u8, offline: bool) -> Duration {
    let base_ms = if offline {
        // Peer unreachable: rare background probes only. Aggressive 3s loops
        // made offline DMs show "delivery retry 30+" forever.
        match attempts {
            0..=5 => 5_000,
            6..=10 => 15_000,
            11..=20 => 30_000,
            _ => MAX_RETRY_SECONDS.saturating_mul(1000),
        }
    } else {
        // Fast, flat probes while a reachable peer's receipt is still in flight.
        // Continuous chat must not idle for many seconds after a successful wake.
        match attempts {
            0 | 1 => 400,
            2 => 700,
            3 => 1_000,
            4..=10 => 1_500,
            _ => 3_000,
        }
    };
    let jitter_span = if offline { 1_000 } else { 200 };
    let jitter_ms = conversation_id
        .bytes()
        .fold(u64::from(attempts), |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(u64::from(byte))
        })
        % jitter_span;
    Duration::from_millis(base_ms.saturating_add(jitter_ms))
}

fn attachment_retry_delay(attempts: u8) -> Duration {
    match attempts {
        0 | 1 => Duration::from_secs(2),
        2 => Duration::from_secs(5),
        3 => Duration::from_secs(10),
        4 => Duration::from_secs(20),
        _ => ATTACHMENT_RETRY_MAX,
    }
}

fn attachment_download_timeout(byte_len: u64) -> Duration {
    // Allow roughly 256 KiB/s after a fixed dial/relay allowance. This keeps
    // small-image recovery quick without cancelling legitimate large images
    // merely because the peer is on a slow uplink.
    let transfer_seconds = byte_len.div_ceil(256 * 1024);
    (ATTACHMENT_DOWNLOAD_TIMEOUT_MIN + Duration::from_secs(transfer_seconds))
        .min(ATTACHMENT_DOWNLOAD_TIMEOUT_MAX)
}

fn attachment_blob_tag(hash: Hash) -> Tag {
    Tag::from(format!("wire-chat-attachment-{hash}"))
}

fn download_blob_tag(hash: Hash) -> Tag {
    Tag::from(format!("wire-chat-download-{hash}"))
}

fn verified_range_bytes(ranges: &iroh_blobs::protocol::RangeSpec, total: u64) -> u64 {
    const CHUNK_BYTES: u64 = 1024;
    let chunks = total.div_ceil(CHUNK_BYTES);
    ranges
        .to_chunk_ranges()
        .iter()
        .map(|range| {
            let start = match range.start_bound() {
                Bound::Included(chunk) => chunk.0,
                _ => 0,
            };
            let end = match range.end_bound() {
                Bound::Excluded(chunk) => chunk.0,
                _ => chunks,
            };
            let start = start.min(chunks).saturating_mul(CHUNK_BYTES);
            let end = end.min(chunks).saturating_mul(CHUNK_BYTES);
            end.min(total).saturating_sub(start.min(total))
        })
        .sum::<u64>()
        .min(total)
}

async fn partial_download_bytes(blobs: &BlobStore, hash: Hash, total: u64) -> u64 {
    let Ok(Some(entry)) = blobs.get_mut(&hash).await else {
        return 0;
    };
    if entry.is_complete() {
        return total;
    }
    let Ok(ranges) = iroh_blobs::get::db::valid_ranges::<BlobStore>(&entry).await else {
        return 0;
    };
    verified_range_bytes(&iroh_blobs::protocol::RangeSpec::new(ranges), total)
}

fn sorted_members(nodes: impl IntoIterator<Item = NodeId>) -> Vec<String> {
    let mut members: Vec<_> = nodes.into_iter().map(|node| node.to_string()).collect();
    members.sort();
    members.dedup();
    members
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0xf) as usize] as char);
    }
    result
}

fn is_message_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn receipt_message_id_from_key(key: &[u8]) -> Option<String> {
    let key = std::str::from_utf8(key).ok()?;
    let message_id = key.strip_prefix("receipt/")?;
    is_message_id(message_id).then(|| message_id.to_owned())
}

fn load_index(path: &Path) -> ChatIndex {
    load_local_json(path, "chat index")
}

fn load_local_deletions(path: &Path) -> LocalDeletionIndex {
    load_local_json(path, "local deletion index")
}

fn load_reliable_control(path: &Path) -> ReliableControlIndex {
    load_local_json(path, "reliable-control index")
}

fn load_pending_file_downloads(path: &Path) -> BTreeMap<(String, String), PendingFileDownload> {
    let index: PendingFileDownloadIndex = load_local_json(path, "file download index");
    index
        .downloads
        .into_iter()
        .map(|item| ((item.message_id.clone(), item.hash.clone()), item))
        .collect()
}

fn load_local_json<T: serde::de::DeserializeOwned + Default>(path: &Path, label: &str) -> T {
    match persistence::read_json(path) {
        Ok(Some(value)) => value,
        Ok(None) => T::default(),
        Err(error) => {
            warn!(path = %path.display(), "could not load {label}: {error:#}");
            T::default()
        }
    }
}

fn save_index(path: &Path, index: &ChatIndex) -> Result<()> {
    persistence::write_json(path, index)
}

fn save_local_deletions(path: &Path, index: &LocalDeletionIndex) -> Result<()> {
    persistence::write_json(path, index)
}

fn save_reliable_control(path: &Path, index: &ReliableControlIndex) -> Result<()> {
    persistence::write_json(path, index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::{protocol::Router, RelayMode, SecretKey};

    fn node(seed: u8) -> NodeId {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn direct_ids_are_symmetric() {
        assert_eq!(
            direct_conversation_id(node(1), node(2)),
            direct_conversation_id(node(2), node(1))
        );
    }

    #[test]
    fn messages_have_unique_sortable_immutable_keys() {
        let a = node(1);
        let first = ChatMessage::new(a, "hello".to_owned());
        let second = ChatMessage::new(a, "world".to_owned());
        assert_ne!(first.message_id, second.message_id);
        assert_ne!(first.entry_key(), second.entry_key());
        assert!(first.entry_key().starts_with("message/"));
        assert!(first.validate().is_ok());
    }

    #[test]
    fn deletion_state_is_never_embedded_in_the_message_blob() {
        let mut message = ChatMessage::new(node(1), "sensitive text".to_owned());
        message.deletion = Some(MessageDeletion::Local);
        let encoded = serde_json::to_vec(&message).unwrap();
        let decoded: ChatMessage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.deletion, None);
        assert!(!String::from_utf8(encoded).unwrap().contains("deletion"));
    }

    #[test]
    fn reliable_control_queue_survives_restart() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("reliable-control.json");
        let peer = node(2).to_string();
        let mut index = ReliableControlIndex::default();
        index
            .attachments
            .entry("dm/test".to_owned())
            .or_default()
            .insert(
                "image-hash".to_owned(),
                PendingAttachmentDelivery {
                    message_id: "a".repeat(64),
                    hash: "image-hash".to_owned(),
                    byte_len: 42,
                    attachment_kind: AttachmentKind::Image,
                    sent_at: now_millis(),
                    pending_peers: BTreeSet::from([peer.clone()]),
                },
            );
        index
            .deletions
            .entry("dm/test".to_owned())
            .or_default()
            .insert(
                "b".repeat(64),
                PendingDeletionDelivery {
                    deletion: ReplicatedDeletion::new("b".repeat(64)),
                    pending_peers: BTreeSet::from([peer.clone()]),
                },
            );
        index.invites.insert(
            "dm/test".to_owned(),
            PendingInviteDelivery {
                history_epoch: 3,
                pending_peers: BTreeSet::from([peer]),
            },
        );

        save_reliable_control(&path, &index)?;
        let restored = load_reliable_control(&path);
        assert_eq!(restored.attachments["dm/test"]["image-hash"].byte_len, 42);
        assert_eq!(
            restored.deletions["dm/test"][&"b".repeat(64)]
                .deletion
                .message_id,
            "b".repeat(64)
        );
        assert_eq!(restored.invites["dm/test"].history_epoch, 3);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_invite_retry_survives_service_restart() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let secret = SecretKey::from_bytes(&[19; 32]);
        let peer = node(20);
        let (endpoint, router, mut service) = spawn_test_node(temp.path(), secret.clone()).await?;
        let conversation_id = direct_conversation_id(endpoint.node_id(), peer);
        let stored = service
            .create_conversation(
                conversation_id.clone(),
                "Offline".to_owned(),
                ConversationKind::Direct {
                    peer_id: peer.to_string(),
                },
                sorted_members([endpoint.node_id(), peer]),
                0,
            )
            .await?;
        service
            .index
            .conversations
            .insert(conversation_id.clone(), stored);
        service.persist_index()?;
        service.register_invite_delivery(&conversation_id);
        assert!(service.conversation_has_pending_control(&conversation_id));
        router.shutdown().await?;
        drop(service);

        let (_endpoint, router, restored) = spawn_test_node(temp.path(), secret).await?;
        assert!(restored.conversation_has_pending_control(&conversation_id));
        assert!(restored.control_retries.contains_key(&conversation_id));
        router.shutdown().await?;
        Ok(())
    }

    #[test]
    fn legacy_messages_default_to_no_attachments() {
        let message = ChatMessage::new(node(1), "legacy".to_owned());
        let mut value = serde_json::to_value(message).unwrap();
        value.as_object_mut().unwrap().remove("attachments");
        let decoded: ChatMessage = serde_json::from_value(value).unwrap();
        assert!(decoded.attachments.is_empty());
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn image_only_messages_carry_metadata_not_bytes() {
        let bytes = Arc::new(vec![1, 2, 3, 4]);
        let hash = Hash::new(bytes.as_slice()).to_string();
        let attachment = ChatAttachment {
            kind: AttachmentKind::Image,
            id: hash.clone(),
            name: "pixel.png".to_owned(),
            media_type: "image/png".to_owned(),
            byte_len: bytes.len() as u64,
            width: 1,
            height: 1,
            hash,
            data: Some(bytes),
        };
        let message = ChatMessage::new_with_attachments(node(1), String::new(), vec![attachment]);
        assert!(message.validate().is_ok());
        let encoded = serde_json::to_vec(&message).unwrap();
        assert!(!encoded.windows(9).any(|window| window == b"1,2,3,4"));
        let decoded: ChatMessage = serde_json::from_slice(&encoded).unwrap();
        assert!(decoded.attachments[0].data.is_none());
    }

    #[test]
    fn file_offer_persists_metadata_and_receiver_records_without_contents() {
        let contents = b"private zip payload";
        let hash = Hash::new(contents).to_string();
        let attachment = ChatAttachment {
            kind: AttachmentKind::FileOffer,
            id: hash.clone(),
            name: "archive.zip".to_owned(),
            media_type: "application/octet-stream".to_owned(),
            byte_len: contents.len() as u64,
            width: 0,
            height: 0,
            hash: hash.clone(),
            data: None,
        };
        let mut message =
            ChatMessage::new_with_attachments(node(1), String::new(), vec![attachment]);
        message
            .file_receivers
            .insert(hash.clone(), BTreeSet::from([node(2).to_string()]));
        message.validate().unwrap();
        let encoded = serde_json::to_string(&message).unwrap();
        assert!(!encoded.contains("private zip payload"));
        assert!(!encoded.contains("file_receivers"));
        let decoded: ChatMessage = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.attachments[0].kind, AttachmentKind::FileOffer);
        assert!(decoded.file_receivers.is_empty());
        let now = now_millis();
        message.sent_at = 1;
        assert!(message.visible_under(RetentionPolicy::Days(7), now));
        let mut plain = ChatMessage::new(node(1), "ordinary text".to_owned());
        plain.sent_at = 1;
        assert!(!plain.visible_under(RetentionPolicy::Days(7), now));
        let receipt = FileReceipt {
            message_id: message.message_id,
            hash,
            receiver_id: node(2).to_string(),
            received_at: 123,
        };
        assert!(receipt
            .entry_key()
            .starts_with(std::str::from_utf8(FILE_RECEIPT_PREFIX).unwrap()));
    }

    #[test]
    fn retention_is_local_time_filtering() {
        let now = 100 * 24 * 60 * 60 * 1000;
        assert!(RetentionPolicy::Unlimited.includes(0, now));
        assert!(RetentionPolicy::Days(7).includes(now - 6 * 24 * 60 * 60 * 1000, now));
        assert!(!RetentionPolicy::Days(7).includes(now - 8 * 24 * 60 * 60 * 1000, now));
    }

    #[test]
    fn long_messages_have_no_ui_sized_limit_but_keep_a_safety_cap() {
        let message = ChatMessage::new(node(1), "x".repeat(200_000));
        assert!(message.validate().is_ok());
        let too_large = ChatMessage::new(node(1), "x".repeat(MAX_MESSAGE_BYTES + 1));
        assert!(too_large.validate().is_err());
    }

    async fn spawn_test_node(
        root: &Path,
        secret: SecretKey,
    ) -> Result<(Endpoint, Router, ChatService)> {
        let endpoint = Endpoint::builder()
            .secret_key(secret)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![
                iroh_blobs::ALPN.to_vec(),
                iroh_docs::ALPN.to_vec(),
                iroh_gossip::ALPN.to_vec(),
                CHAT_ALPN.to_vec(),
            ])
            .bind()
            .await?;
        let protocols = ChatService::build(endpoint.clone(), root).await?;
        let router = Router::builder(endpoint.clone())
            .accept(iroh_blobs::ALPN, protocols.provider.clone())
            .accept(iroh_docs::ALPN, protocols.docs.clone())
            .accept(iroh_gossip::ALPN, protocols.gossip.clone())
            .accept(CHAT_ALPN, protocols.invites.clone())
            .spawn()
            .await?;
        Ok((endpoint, router, protocols.service))
    }

    async fn spawn_test_node_without_blob_provider(
        root: &Path,
        secret: SecretKey,
    ) -> Result<(Endpoint, Router, ChatService)> {
        let endpoint = Endpoint::builder()
            .secret_key(secret)
            .relay_mode(RelayMode::Disabled)
            .alpns(vec![
                iroh_blobs::ALPN.to_vec(),
                iroh_docs::ALPN.to_vec(),
                iroh_gossip::ALPN.to_vec(),
                CHAT_ALPN.to_vec(),
            ])
            .bind()
            .await?;
        let protocols = ChatService::build(endpoint.clone(), root).await?;
        // Deliberately omit the blob protocol. This models the real asymmetric
        // path where sender -> receiver chat works but receiver -> sender blob
        // dialing cannot be established.
        let router = Router::builder(endpoint.clone())
            .accept(iroh_docs::ALPN, protocols.docs.clone())
            .accept(iroh_gossip::ALPN, protocols.gossip.clone())
            .accept(CHAT_ALPN, protocols.invites.clone())
            .spawn()
            .await?;
        Ok((endpoint, router, protocols.service))
    }

    async fn wait_for_body(service: &mut ChatService, expected: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let ChatNotification::Conversation { messages, .. } =
                    service.next_notification().await
                {
                    if messages.iter().any(|message| message.body == expected) {
                        return;
                    }
                }
            }
        })
        .await
        .context("timed out waiting for replicated chat message")?;
        Ok(())
    }

    async fn wait_for_attachment(
        service: &mut ChatService,
        conversation_id: &str,
        message_id: &str,
        expected_hash: &str,
        expected: &[u8],
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut requested = false;
            loop {
                match service.next_notification().await {
                    ChatNotification::Conversation { messages, .. } if !requested => {
                        if let Some(attachment) = messages
                            .iter()
                            .find(|message| message.message_id == message_id)
                            .and_then(|message| {
                                message
                                    .attachments
                                    .iter()
                                    .find(|attachment| attachment.hash == expected_hash)
                            })
                        {
                            service
                                .load_attachment_data(
                                    conversation_id,
                                    &attachment.hash,
                                    attachment.byte_len,
                                )
                                .await?;
                            requested = true;
                        }
                    }
                    ChatNotification::AttachmentData { hash, data }
                        if hash == expected_hash && data.as_slice() == expected =>
                    {
                        return Result::<_>::Ok(());
                    }
                    _ => {}
                }
            }
        })
        .await
        .context("timed out waiting for replicated chat image")??;
        Ok(())
    }

    async fn wait_for_delivery(service: &mut ChatService, message_id: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let ChatNotification::Delivery {
                    message_id: observed,
                    state,
                    ..
                } = service.next_notification().await
                {
                    if observed == message_id && state == DeliveryState::Delivered {
                        return;
                    }
                }
            }
        })
        .await
        .context("timed out waiting for remote delivery receipt")?;
        Ok(())
    }

    async fn wait_for_attachment_ack(
        service: &mut ChatService,
        conversation_id: &str,
        hash: &str,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let pending = service
                    .reliable_control
                    .attachments
                    .get(conversation_id)
                    .is_some_and(|entries| entries.contains_key(hash));
                if !pending {
                    return;
                }
                let input = service.wait_input().await;
                let _ = service.process_input(input).await;
            }
        })
        .await
        .context("timed out waiting for image acknowledgement")?;
        Ok(())
    }

    async fn wait_for_deletion_ack(
        service: &mut ChatService,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let pending = service
                    .reliable_control
                    .deletions
                    .get(conversation_id)
                    .is_some_and(|entries| entries.contains_key(message_id));
                if !pending {
                    return;
                }
                let input = service.wait_input().await;
                let _ = service.process_input(input).await;
            }
        })
        .await
        .context("timed out waiting for deletion acknowledgement")?;
        Ok(())
    }

    async fn wait_for_invite_ack(service: &mut ChatService, conversation_id: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let pending = service
                    .reliable_control
                    .invites
                    .get(conversation_id)
                    .is_some_and(|invite| !invite.pending_peers.is_empty());
                if !pending {
                    return;
                }
                let input = service.wait_input().await;
                let _ = service.process_input(input).await;
            }
        })
        .await
        .context("timed out waiting for conversation acknowledgement")?;
        Ok(())
    }

    async fn wait_for_deletion(
        service: &mut ChatService,
        message_id: &str,
        expected: MessageDeletion,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let ChatNotification::Conversation { messages, .. } =
                    service.next_notification().await
                {
                    if messages.iter().any(|message| {
                        message.message_id == message_id && message.deletion == Some(expected)
                    }) {
                        return;
                    }
                }
            }
        })
        .await
        .context("timed out waiting for replicated message deletion")?;
        Ok(())
    }

    async fn wait_for_direct_deletion(service: &mut ChatService, message_id: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let notification = service.next_notification().await;
                let arrived_over_chat =
                    service.direct_deletions_accepted.load(Ordering::Relaxed) > 0;
                let visible = matches!(
                    notification,
                    ChatNotification::Conversation { ref messages, .. }
                        if messages.iter().any(|message| {
                            message.message_id == message_id
                                && message.deletion == Some(MessageDeletion::Everyone)
                        })
                );
                if arrived_over_chat && visible {
                    return;
                }
            }
        })
        .await
        .context("timed out waiting for direct message deletion")?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn delete_for_everyone_is_pushed_over_sender_connection() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("wire_app_lib=debug,iroh_docs=info")
            .with_test_writer()
            .try_init();
        let temp = tempfile::tempdir()?;
        let left_secret = SecretKey::from_bytes(&[31; 32]);
        let right_secret = SecretKey::from_bytes(&[32; 32]);
        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&temp.path().join("left"), left_secret).await?;
        let (right_endpoint, right_router, mut right) =
            spawn_test_node(&temp.path().join("right"), right_secret).await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;

        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        let outbound = ChatMessage::new(left_endpoint.node_id(), "delete me".to_owned());
        let message_id = outbound.message_id.clone();
        left.send_message(conversation_id.clone(), outbound).await;
        wait_for_body(&mut right, "delete me").await?;

        left.delete_message(
            conversation_id.clone(),
            message_id.clone(),
            DeleteScope::Everyone,
        )
        .await;
        wait_for_direct_deletion(&mut right, &message_id).await?;
        wait_for_deletion_ack(&mut left, &conversation_id, &message_id).await?;

        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn profile_snapshots_survive_message_replication() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let left_secret = SecretKey::from_bytes(&[51; 32]);
        let right_secret = SecretKey::from_bytes(&[52; 32]);
        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&temp.path().join("left"), left_secret).await?;
        let (right_endpoint, right_router, mut right) =
            spawn_test_node(&temp.path().join("right"), right_secret).await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;

        left.set_own_profile(
            Some("Noah".to_owned()),
            Some("abc123".to_owned()),
            Some("#E67E22".to_owned()),
        );
        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        let outbound = ChatMessage::new_with_attachments(
            left_endpoint.node_id(),
            "snapshot check".to_owned(),
            Vec::new(),
        )
        .with_author_profile(
            Some("Noah".to_owned()),
            Some("abc123".to_owned()),
            Some("#E67E22".to_owned()),
        );
        left.send_message(conversation_id.clone(), outbound.clone())
            .await;
        // The receiver must see the full identity snapshot, not just the
        // accent color: name + avatar hash drive the peer cache.
        let seen = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let ChatNotification::Conversation { messages, .. } =
                    right.next_notification().await
                {
                    if let Some(message) = messages
                        .iter()
                        .find(|message| message.message_id == outbound.message_id)
                    {
                        return message.clone();
                    }
                }
            }
        })
        .await
        .context("timed out waiting for replicated chat message")?;
        assert_eq!(seen.author_display_name.as_deref(), Some("Noah"));
        assert_eq!(seen.author_avatar_hash.as_deref(), Some("abc123"));
        assert_eq!(seen.author_accent_color.as_deref(), Some("#E67E22"));

        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_nodes_exchange_and_reload_messages_without_calls() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("wire_app_lib=debug,iroh_docs=info")
            .with_test_writer()
            .try_init();
        let temp = tempfile::tempdir()?;
        let left_root = temp.path().join("left");
        let right_root = temp.path().join("right");
        let left_secret = SecretKey::from_bytes(&[41; 32]);
        let right_secret = SecretKey::from_bytes(&[73; 32]);

        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&left_root, left_secret.clone()).await?;
        let (right_endpoint, right_router, mut right) =
            spawn_test_node(&right_root, right_secret.clone()).await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;

        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        let outbound = ChatMessage::new(left_endpoint.node_id(), "offline from calls".to_owned());
        let outbound_id = outbound.message_id.clone();
        left.send_message(conversation_id.clone(), outbound).await;
        wait_for_body(&mut right, "offline from calls").await?;
        wait_for_delivery(&mut left, &outbound_id).await?;
        assert_eq!(
            right.invite_attempts.load(Ordering::Relaxed),
            0,
            "accepting an invite must not immediately echo another invite"
        );

        left.delete_message(
            conversation_id.clone(),
            outbound_id.clone(),
            DeleteScope::Everyone,
        )
        .await;
        wait_for_deletion(&mut right, &outbound_id, MessageDeletion::Everyone).await?;

        let document_id = right
            .index
            .conversations
            .get(&conversation_id)
            .context("right replica did not import the conversation")?
            .public
            .document_id
            .clone();
        let _ = right
            .process_input(ChatInput::Document(DocumentSignal::SubscriptionEnded {
                conversation_id: conversation_id.clone(),
                document_id: document_id.clone(),
            }))
            .await;
        assert_eq!(
            right.subscriptions.get(&conversation_id),
            Some(&document_id),
            "an ended live subscription must be reattached without restarting"
        );

        let later = ChatMessage::new(left_endpoint.node_id(), "later live message".to_owned());
        let later_id = later.message_id.clone();
        left.send_message(conversation_id.clone(), later).await;
        wait_for_body(&mut right, "later live message").await?;

        right
            .delete_message(
                conversation_id.clone(),
                later_id.clone(),
                DeleteScope::Local,
            )
            .await;
        let right_stored = right.index.conversations[&conversation_id].clone();
        let right_messages = right.load_messages(&right_stored).await?;
        assert_eq!(
            right_messages
                .iter()
                .find(|message| message.message_id == later_id)
                .and_then(|message| message.deletion),
            Some(MessageDeletion::Local)
        );
        let left_stored = left.index.conversations[&conversation_id].clone();
        let left_messages = left.load_messages(&left_stored).await?;
        assert_eq!(
            left_messages
                .iter()
                .find(|message| message.message_id == later_id)
                .and_then(|message| message.deletion),
            None,
            "a local deletion must never replicate"
        );

        let reply = ChatMessage::new(right_endpoint.node_id(), "live reply".to_owned());
        right.send_message(conversation_id.clone(), reply).await;
        wait_for_body(&mut left, "live reply").await?;

        left_router.shutdown().await?;
        right_router.shutdown().await?;
        drop(left);
        drop(right);

        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&left_root, left_secret).await?;
        let (right_endpoint, right_router, mut right) =
            spawn_test_node(&right_root, right_secret).await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;

        wait_for_deletion(&mut left, &outbound_id, MessageDeletion::Everyone).await?;
        wait_for_deletion(&mut right, &outbound_id, MessageDeletion::Everyone).await?;
        wait_for_deletion(&mut right, &later_id, MessageDeletion::Local).await?;

        right
            .restore_message(conversation_id.clone(), later_id.clone())
            .await;
        let right_stored = right.index.conversations[&conversation_id].clone();
        let restored_messages = right.load_messages(&right_stored).await?;
        let restored = restored_messages
            .iter()
            .find(|message| message.message_id == later_id)
            .context("restored message disappeared")?;
        assert_eq!(restored.deletion, None);
        assert_eq!(restored.body, "later live message");
        assert!(
            !load_local_deletions(&right.root.join("local-deletions.json"))
                .conversations
                .get(&conversation_id)
                .is_some_and(|message_ids| message_ids.contains(&later_id)),
            "restoring a message must remove its persisted local tombstone"
        );
        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn image_attachment_downloads_after_message_metadata_arrives() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("wire_app_lib=debug,iroh_blobs=debug")
            .with_test_writer()
            .try_init();
        let temp = tempfile::tempdir()?;
        let left_root = temp.path().join("left");
        let right_root = temp.path().join("right");
        let left_secret = SecretKey::from_bytes(&[101; 32]);
        let right_secret = SecretKey::from_bytes(&[102; 32]);
        let image = b"wire-image-transfer".to_vec();
        let image_hash = Hash::new(&image).to_string();

        let (left_endpoint, left_router, mut left) =
            spawn_test_node_without_blob_provider(&left_root, left_secret).await?;
        let (right_endpoint, right_router, mut right) =
            spawn_test_node(&right_root, right_secret).await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;

        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        let attachment = ChatAttachment {
            kind: AttachmentKind::Image,
            id: image_hash.clone(),
            name: "transfer.png".to_owned(),
            media_type: "image/png".to_owned(),
            byte_len: image.len() as u64,
            width: 1,
            height: 1,
            hash: image_hash.clone(),
            data: Some(Arc::new(image.clone())),
        };
        let message = ChatMessage::new_with_attachments(
            left_endpoint.node_id(),
            String::new(),
            vec![attachment],
        );
        let message_id = message.message_id.clone();
        left.send_message(conversation_id.clone(), message).await;

        wait_for_attachment(
            &mut right,
            &conversation_id,
            &message_id,
            &image_hash,
            &image,
        )
        .await?;
        wait_for_attachment_ack(&mut left, &conversation_id, &image_hash).await?;

        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn small_text_file_syncs_automatically() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&temp.path().join("left"), SecretKey::from_bytes(&[113; 32])).await?;
        let (right_endpoint, right_router, mut right) = spawn_test_node(
            &temp.path().join("right"),
            SecretKey::from_bytes(&[114; 32]),
        )
        .await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;
        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        let bytes = b"small UTF-8 text\n".to_vec();
        let hash = Hash::new(&bytes).to_string();
        let attachment = ChatAttachment {
            kind: AttachmentKind::InlineFile,
            id: hash.clone(),
            name: "notes.txt".to_owned(),
            media_type: "text/plain".to_owned(),
            byte_len: bytes.len() as u64,
            width: 0,
            height: 0,
            hash: hash.clone(),
            data: Some(Arc::new(bytes.clone())),
        };
        let message = ChatMessage::new_with_attachments(
            left_endpoint.node_id(),
            String::new(),
            vec![attachment],
        );
        let message_id = message.message_id.clone();
        assert!(left.send_message(conversation_id.clone(), message).await);
        wait_for_attachment(&mut right, &conversation_id, &message_id, &hash, &bytes).await?;
        wait_for_attachment_ack(&mut left, &conversation_id, &hash).await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let messages = right
                    .load_messages(&right.index.conversations[&conversation_id])
                    .await?;
                if messages
                    .iter()
                    .find(|message| message.message_id == message_id)
                    .and_then(|message| message.file_receivers.get(&hash))
                    .is_some_and(|receivers| {
                        receivers.contains(&right_endpoint.node_id().to_string())
                    })
                {
                    break Ok::<_, anyhow::Error>(());
                }
                let input = right.wait_input().await;
                let _ = right.process_input(input).await;
            }
        })
        .await??;
        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file_offer_waits_for_request_and_records_completed_receive() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("archive.zip");
        let contents = b"a peer-to-peer file, never a chat body";
        std::fs::write(&source, contents)?;
        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&temp.path().join("left"), SecretKey::from_bytes(&[111; 32])).await?;
        let (right_endpoint, right_router, mut right) = spawn_test_node(
            &temp.path().join("right"),
            SecretKey::from_bytes(&[112; 32]),
        )
        .await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;
        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        left.offer_files(
            conversation_id.clone(),
            "archive for you".to_owned(),
            Vec::new(),
            vec![source],
        );
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let stored = &left.index.conversations[&conversation_id];
                if left
                    .load_messages(stored)
                    .await?
                    .iter()
                    .any(|message| message.body == "archive for you")
                {
                    break Ok::<_, anyhow::Error>(());
                }
                let input = left.wait_input().await;
                let _ = left.process_input(input).await;
            }
        })
        .await??;
        wait_for_body(&mut right, "archive for you").await?;
        let stored = &right.index.conversations[&conversation_id];
        let message = right
            .load_messages(stored)
            .await?
            .into_iter()
            .find(|message| message.body == "archive for you")
            .or_else(|| {
                right
                    .staged_inbound
                    .get(&conversation_id)
                    .and_then(|messages| {
                        messages
                            .values()
                            .find(|message| message.body == "archive for you")
                            .cloned()
                    })
            })
            .context("missing offer")?;
        let offer = &message.attachments[0];
        assert_eq!(offer.kind, AttachmentKind::FileOffer);
        assert!(
            right
                .blobs
                .get(&Hash::from_str(&offer.hash)?)
                .await?
                .is_none(),
            "offer metadata must not automatically download content"
        );
        left.set_file_serving(offer.hash.clone(), false).await?;
        assert!(is_stopped(
            &left.stopped_hashes,
            &Hash::from_str(&offer.hash)?
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let messages = right
                    .load_messages(&right.index.conversations[&conversation_id])
                    .await?;
                if messages
                    .iter()
                    .find(|item| item.message_id == message.message_id)
                    .is_some_and(|item| item.stopped_file_offers.contains(&offer.hash))
                {
                    break Ok::<_, anyhow::Error>(());
                }
                let input = right.wait_input().await;
                let _ = right.process_input(input).await;
            }
        })
        .await??;
        assert!(right
            .request_file(
                conversation_id.clone(),
                message.message_id.clone(),
                offer.hash.clone(),
                temp.path().join("blocked.zip")
            )
            .await
            .is_err());
        let repeated = ChatMessage::new_with_attachments(
            left_endpoint.node_id(),
            "same file again".to_owned(),
            vec![offer.clone()],
        );
        let repeated_id = repeated.message_id.clone();
        assert!(left.send_message(conversation_id.clone(), repeated).await);
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let messages = right
                    .load_messages(&right.index.conversations[&conversation_id])
                    .await?;
                if messages
                    .iter()
                    .find(|item| item.message_id == repeated_id)
                    .is_some_and(|item| item.stopped_file_offers.contains(&offer.hash))
                {
                    break Ok::<_, anyhow::Error>(());
                }
                let input = right.wait_input().await;
                let _ = right.process_input(input).await;
            }
        })
        .await??;
        left.set_file_serving(offer.hash.clone(), true).await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let messages = right
                    .load_messages(&right.index.conversations[&conversation_id])
                    .await?;
                if messages
                    .iter()
                    .find(|item| item.message_id == message.message_id)
                    .is_some_and(|item| !item.stopped_file_offers.contains(&offer.hash))
                {
                    break Ok::<_, anyhow::Error>(());
                }
                let input = right.wait_input().await;
                let _ = right.process_input(input).await;
            }
        })
        .await??;
        let destination = temp.path().join("received.zip");
        std::fs::write(&destination, b"replace this older download")?;
        let pending = PendingFileDownload {
            conversation_id: conversation_id.clone(),
            message_id: message.message_id.clone(),
            hash: offer.hash.clone(),
            path: destination.clone(),
            byte_len: offer.byte_len,
        };
        right
            .pending_file_downloads
            .insert((pending.message_id.clone(), pending.hash.clone()), pending);
        right.save_pending_file_downloads()?;
        right.pending_file_downloads =
            load_pending_file_downloads(&temp.path().join("right/chat/file-downloads.json"));
        right.restore_file_downloads().await;
        assert!(right.queued.iter().any(|notification| matches!(
            notification,
            ChatNotification::FileTransferUpdate {
                message_id,
                path,
                phase: FileTransferPhase::Paused(_),
                ..
            } if message_id == &message.message_id && path == &destination
        )));
        right
            .request_file(
                conversation_id.clone(),
                message.message_id.clone(),
                offer.hash.clone(),
                destination.clone(),
            )
            .await?;
        let pending: PendingFileDownloadIndex =
            persistence::read_json(&temp.path().join("right/chat/file-downloads.json"))?
                .context("download request was not persisted")?;
        assert_eq!(pending.downloads.len(), 1);
        assert_eq!(pending.downloads[0].path, destination);
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(ChatNotification::FileTransfer { result, .. }) =
                    right.pop_notification()
                {
                    result.map_err(anyhow::Error::msg)?;
                    break Ok::<_, anyhow::Error>(());
                }
                let input = right.wait_input().await;
                if let Some(ChatNotification::FileTransfer { result, .. }) =
                    right.process_input(input).await
                {
                    result.map_err(anyhow::Error::msg)?;
                    break Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        assert_eq!(std::fs::read(destination)?, contents);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = left.provider_event_rx.recv().await?;
                left.handle_blob_provider_event(event);
                if left.queued.iter().any(|notification| {
                    matches!(notification,
                    ChatNotification::FileServing { hash, phase: FileServingPhase::Sent, .. }
                    if hash == &offer.hash)
                }) {
                    break Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        let pending: PendingFileDownloadIndex =
            persistence::read_json(&temp.path().join("right/chat/file-downloads.json"))?
                .context("download index disappeared")?;
        assert!(pending.downloads.is_empty());
        let hash = Hash::from_str(&offer.hash)?;
        let cancelled = PendingFileDownload {
            conversation_id: conversation_id.clone(),
            message_id: message.message_id.clone(),
            hash: offer.hash.clone(),
            path: temp.path().join("cancelled.zip"),
            byte_len: offer.byte_len,
        };
        right.pending_file_downloads.insert(
            (cancelled.message_id.clone(), cancelled.hash.clone()),
            cancelled,
        );
        right.save_pending_file_downloads()?;
        right
            .blobs
            .set_tag(download_blob_tag(hash), Some(HashAndFormat::raw(hash)))
            .await?;
        right
            .cancel_file(message.message_id.clone(), offer.hash.clone())
            .await?;
        assert!(right.pending_file_downloads.is_empty());
        assert!(right
            .blobs
            .tags()
            .await?
            .all(|tag| { tag.is_ok_and(|(name, _)| name != download_blob_tag(hash)) }));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let received = right
                    .load_messages(&right.index.conversations[&conversation_id])
                    .await?;
                if received
                    .iter()
                    .find(|item| item.message_id == message.message_id)
                    .and_then(|item| item.file_receivers.get(&offer.hash))
                    .is_some_and(|members| members.contains(&right_endpoint.node_id().to_string()))
                {
                    break Ok::<_, anyhow::Error>(());
                }
                let input = right.wait_input().await;
                let _ = right.process_input(input).await;
            }
        })
        .await??;
        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    #[test]
    fn delivery_retries_are_bounded_and_desynchronised() {
        let first = delivery_retry_delay("dm/example-a", 1, false);
        let second = delivery_retry_delay("dm/example-b", 1, false);
        assert!(first >= Duration::from_millis(400));
        assert!(first < Duration::from_millis(700));
        assert!(delivery_retry_delay("dm/example", 99, false) <= Duration::from_secs(4));
        assert_ne!(
            first, second,
            "conversation-specific jitter avoids retry herds"
        );
        let offline = delivery_retry_delay("dm/offline", 30, true);
        assert!(offline >= Duration::from_secs(60));
        assert!(offline <= Duration::from_secs(61));
        assert!(
            delivery_retry_delay("dm/offline", 3, true) >= Duration::from_secs(5),
            "offline probes must not use the online sub-second schedule"
        );
    }

    #[test]
    fn attachment_download_timeouts_scale_but_stay_bounded() {
        assert_eq!(
            attachment_download_timeout(1),
            ATTACHMENT_DOWNLOAD_TIMEOUT_MIN + Duration::from_secs(1)
        );
        assert_eq!(
            attachment_download_timeout(512 * 1024),
            ATTACHMENT_DOWNLOAD_TIMEOUT_MIN + Duration::from_secs(2)
        );
        assert_eq!(
            attachment_download_timeout(u64::MAX),
            ATTACHMENT_DOWNLOAD_TIMEOUT_MAX
        );
    }

    #[test]
    fn sync_request_keeps_legacy_shape_and_carries_fast_path() {
        let legacy = r#"{"kind":"sync-request","version":1,"conversation_id":"dm/a/b"}"#;
        let parsed: SyncRequest = serde_json::from_str(legacy).unwrap();
        assert!(parsed.messages.is_empty());
        assert!(parsed.receipts.is_empty());
        assert!(parsed.deletions.is_empty());
        assert!(parsed.attachment_acks.is_empty());
        assert!(parsed.deletion_acks.is_empty());
        assert!(parsed.accepted_history_epoch.is_none());
        assert!(parsed.ticket.is_none());

        let author = SecretKey::from_bytes(&[7; 32]).public();
        let message = ChatMessage::new(author, "fast".to_owned());
        let receipt = ReplicatedReceipt::new(message.message_id.clone());
        let deletion = ReplicatedDeletion::new(message.message_id.clone());
        let full = SyncRequest {
            kind: "sync-request".to_owned(),
            version: 1,
            conversation_id: "dm/a/b".to_owned(),
            ticket: Some("ticket".to_owned()),
            client_version: Some("0.4.15".to_owned()),
            messages: vec![message.clone()],
            receipts: vec![receipt],
            deletions: vec![deletion],
            attachment_acks: vec!["hash".to_owned()],
            deletion_acks: vec![message.message_id.clone()],
            accepted_history_epoch: Some(2),
        };
        let encoded = serde_json::to_value(&full).unwrap();
        assert_eq!(encoded["messages"][0]["body"], "fast");
        assert_eq!(encoded["receipts"][0]["message_id"], message.message_id);
        assert_eq!(encoded["deletions"][0]["message_id"], message.message_id);
        assert_eq!(encoded["attachment_acks"][0], "hash");
        assert_eq!(encoded["accepted_history_epoch"], 2);
        let roundtrip: SyncRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(roundtrip.messages.len(), 1);
        assert_eq!(roundtrip.receipts.len(), 1);
        assert_eq!(roundtrip.deletions.len(), 1);
        assert_eq!(roundtrip.attachment_acks.len(), 1);
        assert_eq!(roundtrip.deletion_acks.len(), 1);
        assert_eq!(roundtrip.accepted_history_epoch, Some(2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn history_clear_deletes_document_and_rotates_replicas() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("wire_app_lib=debug,iroh_docs=info")
            .with_test_writer()
            .try_init();
        let temp = tempfile::tempdir()?;
        let left_root = temp.path().join("left");
        let right_root = temp.path().join("right");
        let left_secret = SecretKey::from_bytes(&[11; 32]);
        let right_secret = SecretKey::from_bytes(&[22; 32]);

        let (left_endpoint, left_router, mut left) =
            spawn_test_node(&left_root, left_secret.clone()).await?;
        let (right_endpoint, right_router, mut right) =
            spawn_test_node(&right_root, right_secret.clone()).await?;
        left_endpoint.add_node_addr(right_endpoint.node_addr().await?)?;
        right_endpoint.add_node_addr(left_endpoint.node_addr().await?)?;

        let conversation_id = left
            .ensure_direct(right_endpoint.node_id(), "Right".to_owned())
            .await?;
        let old_document_id = left.index.conversations[&conversation_id]
            .public
            .document_id
            .clone();
        left.send_message(
            conversation_id.clone(),
            ChatMessage::new(left_endpoint.node_id(), "wipe me".to_owned()),
        )
        .await;
        wait_for_body(&mut right, "wipe me").await?;

        left.clear_history(conversation_id.clone()).await;
        wait_for_history_epoch(&mut left, &conversation_id, 1).await?;
        wait_for_history_epoch(&mut right, &conversation_id, 1).await?;
        wait_for_invite_ack(&mut left, &conversation_id).await?;

        let left_stored = left.index.conversations[&conversation_id].clone();
        let right_stored = right.index.conversations[&conversation_id].clone();
        assert_ne!(left_stored.public.document_id, old_document_id);
        assert_eq!(
            left_stored.public.document_id,
            right_stored.public.document_id
        );
        assert_eq!(left_stored.public.history_epoch, 1);
        assert_eq!(right_stored.public.history_epoch, 1);
        assert!(
            left.load_messages(&left_stored).await?.is_empty(),
            "rotated document must start empty"
        );
        assert!(
            right.load_messages(&right_stored).await?.is_empty(),
            "peer must drop old history after rotation invite"
        );
        let old_namespace = NamespaceId::from_str(&old_document_id)?;
        assert!(
            !doc_is_present(&left.docs, old_namespace).await?,
            "initiator must drop old document storage"
        );
        assert!(
            !doc_is_present(&right.docs, old_namespace).await?,
            "peer must drop old document storage"
        );

        left.send_message(
            conversation_id.clone(),
            ChatMessage::new(left_endpoint.node_id(), "after clear".to_owned()),
        )
        .await;
        wait_for_body(&mut right, "after clear").await?;
        wait_for_body(&mut left, "after clear").await?;

        let left_messages = left
            .load_messages(&left.index.conversations[&conversation_id].clone())
            .await?;
        assert_eq!(left_messages.len(), 1);
        assert_eq!(left_messages[0].body, "after clear");

        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }

    async fn wait_for_history_epoch(
        service: &mut ChatService,
        conversation_id: &str,
        epoch: u64,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let ChatNotification::Conversation {
                    conversation,
                    messages,
                    ..
                } = service.next_notification().await
                {
                    if conversation.id == conversation_id
                        && conversation.history_epoch >= epoch
                        && messages.is_empty()
                    {
                        return;
                    }
                }
            }
        })
        .await
        .context("timed out waiting for chat history rotation")?;
        Ok(())
    }

    async fn doc_is_present(docs: &MemClient, namespace: NamespaceId) -> Result<bool> {
        let mut listed = docs.list().await?;
        while let Some(item) = listed.next().await {
            let (id, _) = item?;
            if id == namespace {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
