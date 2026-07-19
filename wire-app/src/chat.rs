use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use futures_lite::StreamExt;
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use iroh_blobs::{
    net_protocol::Blobs,
    store::{fs::Store as BlobStore, Map},
};
use iroh_docs::{
    engine::LiveEvent,
    protocol::Docs,
    rpc::{
        client::docs::{MemClient, ShareMode},
        AddrInfoOptions,
    },
    store::Query,
    AuthorId, DocTicket, NamespaceId,
};
use iroh_gossip::net::Gossip;
use iroh_io::AsyncSliceReader;
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, trace, warn};

pub const CHAT_ALPN: &[u8] = b"wire/chat-invite/1";
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_INVITE_BYTES: usize = 256 * 1024;
const MESSAGE_PREFIX: &[u8] = b"message/";
const DELETION_PREFIX: &[u8] = b"deletion/";
const RECEIPT_PREFIX: &[u8] = b"receipt/";
const RETRY_TICK: Duration = Duration::from_secs(1);
const MAX_RETRY_SECONDS: u64 = 60;
/// Keep chat-plane QUIC sessions warm for bursty back-and-forth.
/// See `docs/chat-keepalive-sessions.md`.
const CHAT_SESSION_IDLE: Duration = Duration::from_secs(60);
const CHAT_SESSION_POOL_CAP: usize = 8;
/// Half-open pooled sessions must fail fast — logs showed a hard ~3s floor on
/// every send while waiting out a full stream timeout on a dead reuse.
const CHAT_REUSE_TIMEOUT: Duration = Duration::from_millis(400);
const CHAT_STREAM_TIMEOUT: Duration = Duration::from_secs(3);
const CHAT_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// First delivery re-probe after send (receipt / docs pull may still be in flight).
const CHAT_FIRST_RETRY: Duration = Duration::from_millis(400);
static NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionPolicy {
    Unlimited,
    Days(u32),
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::Unlimited
    }
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
    #[serde(skip)]
    pub deletion: Option<MessageDeletion>,
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
    pub fn new(author_id: NodeId, body: String) -> Self {
        let sent_at = now_millis();
        let nonce = next_nonce();
        let mut hasher = Sha256::new();
        hasher.update(author_id.as_bytes());
        hasher.update(sent_at.to_be_bytes());
        hasher.update(nonce.to_be_bytes());
        hasher.update(body.as_bytes());
        let message_id = hex_bytes(&hasher.finalize());
        Self {
            version: 1,
            message_id,
            author_id: author_id.to_string(),
            sent_at,
            body,
            nonce,
            deletion: None,
        }
    }

    pub fn entry_key(&self) -> String {
        format!(
            "message/{:020}/{}/{:016x}",
            self.sent_at, self.author_id, self.nonce
        )
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported message version {}", self.version);
        }
        if self.body.trim().is_empty() {
            bail!("message is empty");
        }
        if self.body.len() > MAX_MESSAGE_BYTES {
            bail!("message is larger than the 1 MiB safety cap");
        }
        if !is_message_id(&self.message_id) || self.author_id.is_empty() {
            bail!("message identity is incomplete");
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
    },
    Delivery {
        message_id: String,
        state: DeliveryState,
        detail: Option<String>,
    },
    Error(String),
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
struct ChatInvite {
    version: u8,
    conversation: ChatConversation,
    ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ChatProtocolMessage {
    Invite(ChatInvite),
    SyncRequest(SyncRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SyncRequest {
    kind: String,
    version: u8,
    conversation_id: String,
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
            sessions.insert(remote, connection.clone()).await;
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
            sessions.forget(remote).await;
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

    async fn insert(&self, peer: NodeId, connection: Connection) {
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
            return;
        }
        guard.sessions.insert(
            peer,
            HotSession {
                connection,
                last_used: tokio::time::Instant::now(),
            },
        );
        Self::evict_locked(&mut guard.sessions);
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
    async fn forget(&self, peer: NodeId) {
        self.inner.lock().await.sessions.remove(&peer);
    }

    /// Drop a pooled handle after a failed stream. Only close if this was our
    /// pooled connection — never close a live peer session on a guess.
    async fn invalidate(&self, peer: NodeId) {
        self.inner.lock().await.sessions.remove(&peer);
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
    pub blobs: Blobs<BlobStore>,
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
    our_node_id: NodeId,
    invite_rx: async_channel::Receiver<IncomingInvite>,
    doc_event_tx: async_channel::Sender<DocumentSignal>,
    doc_event_rx: async_channel::Receiver<DocumentSignal>,
    wake_tx: async_channel::Sender<ChatInput>,
    wake_rx: async_channel::Receiver<ChatInput>,
    subscriptions: BTreeMap<String, String>,
    queued: VecDeque<ChatNotification>,
    retry_tick: tokio::time::Interval,
    retry_state: BTreeMap<String, ConversationRetry>,
    pending_deliveries: BTreeMap<String, String>,
    /// In-flight background wakes per conversation — prevents a losing race
    /// from parking deliveries as Queued right after a successful wake.
    wake_inflight: BTreeMap<String, u32>,
    #[cfg(test)]
    invite_attempts: AtomicU64,
}

#[derive(Debug)]
struct ConversationRetry {
    attempts: u8,
    next_attempt: tokio::time::Instant,
    messages: BTreeSet<String>,
}

pub(crate) enum ChatInput {
    Invite(IncomingInvite),
    Document(DocumentSignal),
    /// Background wake finished (send path does not block on dial/reuse).
    WakeFinished {
        conversation_id: String,
        reached: bool,
    },
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
        let blobs = Blobs::builder(blob_store.clone()).build(&endpoint);
        let gossip = Gossip::builder().spawn(endpoint.clone()).await?;
        let docs = Docs::persistent(docs_path).spawn(&blobs, &gossip).await?;
        debug!("persistent Iroh Docs engine opened");
        let sessions = ChatSessionPool::new(endpoint.clone());
        let (invites, invite_rx) = ChatInviteProtocol::new(sessions.clone());
        let client = docs.client().clone();
        let author = client.authors().default().await?;
        let (doc_event_tx, doc_event_rx) = async_channel::bounded(256);
        let (wake_tx, wake_rx) = async_channel::bounded(64);
        let index = load_index(&root.join("index.json"));
        let local_deletions = load_local_deletions(&root.join("local-deletions.json"));
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
            our_node_id: endpoint.node_id(),
            invite_rx,
            doc_event_tx,
            doc_event_rx,
            wake_tx,
            wake_rx,
            subscriptions: BTreeMap::new(),
            queued: VecDeque::new(),
            retry_tick: tokio::time::interval_at(
                tokio::time::Instant::now() + RETRY_TICK,
                RETRY_TICK,
            ),
            retry_state: BTreeMap::new(),
            pending_deliveries: BTreeMap::new(),
            wake_inflight: BTreeMap::new(),
            #[cfg(test)]
            invite_attempts: AtomicU64::new(0),
        };
        service.initialize().await;
        info!(
            node = %service.our_node_id.fmt_short(),
            conversations = conversation_count,
            storage = %service.root.display(),
            "text chat service ready"
        );
        Ok(ChatProtocols {
            blobs,
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
        self.retry_invites();
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
                pending_by_conversation
                    .entry(conversation_id.clone())
                    .or_default()
                    .push(message.message_id);
            }
        }
        for (conversation_id, message_ids) in pending_by_conversation {
            let reached = self.wake_members(&conversation_id).await;
            if reached {
                for message_id in message_ids {
                    self.schedule_delivery_retry(&conversation_id, &message_id, true);
                    self.queued.push_back(ChatNotification::Delivery {
                        message_id,
                        state: DeliveryState::Pending,
                        detail: Some("delivering".to_owned()),
                    });
                }
            } else {
                for message_id in message_ids {
                    self.queued.push_back(ChatNotification::Delivery {
                        message_id,
                        state: DeliveryState::Queued,
                        detail: Some("waiting for peer".to_owned()),
                    });
                }
            }
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
            incoming = self.invite_rx.recv() => ChatInput::Invite(
                incoming.expect("chat invitation protocol channel closed")
            ),
            changed = self.doc_event_rx.recv() => ChatInput::Document(
                changed.expect("chat document event channel closed")
            ),
            wake = self.wake_rx.recv() => wake.expect("chat wake channel closed"),
            _ = self.retry_tick.tick() => ChatInput::Retry,
        }
    }

    pub(crate) async fn process_input(&mut self, input: ChatInput) -> Option<ChatNotification> {
        match input {
            ChatInput::Invite(incoming) => match incoming.message {
                ChatProtocolMessage::Invite(invite) => {
                    let remote = incoming.remote;
                    if let Err(error) = self.accept_invite(remote, invite).await {
                        return Some(ChatNotification::Error(format!(
                            "Chat invitation failed: {error:#}"
                        )));
                    }
                    self.resume_deliveries_for_peer(remote).await;
                }
                ChatProtocolMessage::SyncRequest(request) => {
                    if request.kind != "sync-request" || request.version != 1 {
                        return Some(ChatNotification::Error(format!(
                            "Unsupported chat sync request version {}",
                            request.version
                        )));
                    }
                    if self.is_conversation_member(&request.conversation_id, incoming.remote) {
                        // Accept-side pull. Prefer a cheap start_sync + timeline
                        // refresh over full re-subscribe on every wake.
                        if let Err(error) = self.pull_conversation(&request.conversation_id).await {
                            warn!(
                                conversation = %log_id(&request.conversation_id),
                                peer = %incoming.remote.fmt_short(),
                                "failed to immediately sync requested chat: {error:#}"
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
            },
            ChatInput::Document(signal) => match signal {
                DocumentSignal::Changed {
                    conversation_id,
                    from_remote,
                } => {
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
            ChatInput::Retry => {
                self.retry_due_deliveries().await;
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

    pub async fn send_message(&mut self, conversation_id: String, message: ChatMessage) {
        let message_id = message.message_id.clone();
        let body_bytes = message.body.len();
        match self.insert_message(&conversation_id, &message).await {
            Ok(()) => {
                info!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    bytes = body_bytes,
                    "message committed to local chat replica"
                );
                self.pending_deliveries
                    .insert(message_id.clone(), conversation_id.clone());
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
            }
        }
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
        if immediate {
            state.next_attempt = now;
        }
    }

    fn spawn_wake(&mut self, conversation_id: &str) {
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
        let wake_tx = self.wake_tx.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            let reached = wake_peers(sessions, peers, &conversation_id).await;
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
        tokio::spawn(async move {
            if let Err(error) = nudge_doc_sync(&docs, &stored, our_node_id).await {
                trace!(
                    conversation = %log_id(&stored.public.id),
                    "doc sync nudge failed: {error:#}"
                );
            }
        });
    }

    async fn apply_wake_finished(&mut self, conversation_id: &str, reached: bool) {
        if let Some(count) = self.wake_inflight.get_mut(conversation_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.wake_inflight.remove(conversation_id);
            }
        }
        let inflight = self.wake_inflight.get(conversation_id).copied().unwrap_or(0);
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
            // A success means the peer is up — never leave messages Queued.
            for message_id in &pending {
                self.schedule_delivery_retry(conversation_id, message_id, false);
                self.queued.push_back(ChatNotification::Delivery {
                    message_id: message_id.clone(),
                    state: DeliveryState::Pending,
                    detail: Some("delivering".to_owned()),
                });
            }
            // Peer acked SyncRequest; pull any receipts they may have just written.
            self.spawn_doc_sync(conversation_id);
            if let Err(error) = self.publish_timeline(conversation_id).await {
                warn!(
                    conversation = %log_id(conversation_id),
                    "failed to refresh timeline after wake: {error:#}"
                );
            }
            return;
        }
        // Only park when every in-flight wake lost — a concurrent success must win.
        if inflight > 0 || self.retry_state.contains_key(conversation_id) {
            return;
        }
        info!(
            conversation = %log_id(conversation_id),
            messages = pending.len(),
            "no chat member reachable; parking deliveries as queued"
        );
        for message_id in pending {
            self.queued.push_back(ChatNotification::Delivery {
                message_id,
                state: DeliveryState::Queued,
                detail: Some("waiting for peer".to_owned()),
            });
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
            let reached = self.wake_members(&conversation_id).await;
            if !reached {
                self.retry_state.remove(&conversation_id);
                for message_id in message_ids {
                    if self.pending_deliveries.contains_key(&message_id) {
                        self.queued.push_back(ChatNotification::Delivery {
                            message_id,
                            state: DeliveryState::Queued,
                            detail: Some("waiting for peer".to_owned()),
                        });
                    }
                }
                continue;
            }
            if let Err(error) = self.publish_timeline(&conversation_id).await {
                warn!(
                    conversation = %log_id(&conversation_id),
                    "chat delivery refresh failed: {error:#}"
                );
            }
            let Some(state) = self.retry_state.get_mut(&conversation_id) else {
                // A receipt can arrive while the immediate sync above is
                // publishing its timeline, which removes this retry state.
                continue;
            };
            if state.messages.is_empty() {
                continue;
            }
            let attempts = {
                state.attempts = state.attempts.saturating_add(1);
                state.next_attempt = now + delivery_retry_delay(&conversation_id, state.attempts);
                state.attempts
            };
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

    async fn resume_deliveries_for_peer(&mut self, peer: NodeId) {
        let peer_s = peer.to_string();
        let conversation_ids: Vec<_> = self
            .index
            .conversations
            .iter()
            .filter(|(_, stored)| {
                stored
                    .public
                    .members
                    .iter()
                    .any(|member| member == &peer_s)
            })
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
        // Already actively retrying this conversation — leave the timer alone.
        if self.retry_state.contains_key(conversation_id) {
            return;
        }
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
        let reached = self.wake_members(conversation_id).await;
        if !reached {
            self.retry_state.remove(conversation_id);
            for message_id in message_ids {
                self.queued.push_back(ChatNotification::Delivery {
                    message_id,
                    state: DeliveryState::Queued,
                    detail: Some("waiting for peer".to_owned()),
                });
            }
            return;
        }
        if let Err(error) = self.publish_timeline(conversation_id).await {
            warn!(
                conversation = %log_id(conversation_id),
                "failed to refresh timeline while resuming queued deliveries: {error:#}"
            );
        }
    }

    /// Ask members to pull the current doc. Returns true if at least one other
    /// member accepted the wake. Does **not** re-send invites — those are
    /// reserved for create / clear / initialize (see `invite_members`).
    async fn wake_members(&self, conversation_id: &str) -> bool {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return false;
        };
        wake_peers(
            self.sessions.clone(),
            self.other_members(stored),
            conversation_id,
        )
        .await
    }

    pub async fn delete_message(
        &mut self,
        conversation_id: String,
        message_id: String,
        scope: DeleteScope,
    ) {
        let result = match scope {
            DeleteScope::Local => self.delete_message_locally(&conversation_id, &message_id),
            DeleteScope::Everyone => {
                self.insert_replicated_deletion(&conversation_id, &message_id)
                    .await
            }
        };
        match result {
            Ok(()) => {
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
                info!(
                    conversation = %log_id(&conversation_id),
                    history_epoch = epoch,
                    "chat history deleted and conversation rotated onto a fresh document"
                );
                // Peers on a stale document need the new ticket before a pull
                // nudge — keep-alive made SyncRequest often win the race and
                // get ignored as non-member.
                self.invite_members_wait(&conversation_id).await;
                let _ = self.wake_members(&conversation_id).await;
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

    fn forget_conversation_local_state(&mut self, conversation_id: &str) {
        self.pending_deliveries
            .retain(|_, pending_conversation| pending_conversation != conversation_id);
        self.retry_state.remove(conversation_id);
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
                } else if invite_epoch < current_epoch {
                    (false, false)
                } else if invite.conversation.document_id == current.public.document_id {
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
        let old_document_id = current.as_ref().map(|stored| stored.public.document_id.clone());
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

    async fn pull_conversation(&mut self, id: &str) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(id)
            .cloned()
            .context("unknown conversation")?;
        nudge_doc_sync(&self.docs, &stored, self.our_node_id).await?;
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
                            // A finished sync with a peer is a strong signal
                            // they are online — resume any parked sends.
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
        let messages = self.load_messages(&stored).await?;
        let wrote_receipts = self
            .acknowledge_received_messages(&stored, &messages)
            .await?;
        // Friend already sees their message on our side; their UI stays on
        // "syncing" until they pull our receipt. Nudge them immediately.
        if wrote_receipts {
            self.spawn_doc_sync(id);
            self.spawn_wake(id);
        }
        let delivered = self.load_delivered_message_ids(&stored).await?;
        let visible: BTreeSet<_> = messages
            .iter()
            .map(|message| message.message_id.clone())
            .collect();
        let dropped_pending: Vec<_> = self
            .pending_deliveries
            .iter()
            .filter(|(message_id, conversation_id)| {
                *conversation_id == id && !visible.contains(*message_id)
            })
            .map(|(message_id, _)| message_id.clone())
            .collect();
        for message_id in dropped_pending {
            self.pending_deliveries.remove(&message_id);
        }
        for message in &messages {
            if message.author_id == self.our_node_id.to_string()
                && delivered.contains(&message.message_id)
                && self
                    .pending_deliveries
                    .remove(&message.message_id)
                    .is_some()
            {
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
        });
        Ok(())
    }

    async fn acknowledge_received_messages(
        &self,
        stored: &StoredConversation,
        messages: &[ChatMessage],
    ) -> Result<bool> {
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let mut wrote_any = false;
        for message in messages
            .iter()
            .filter(|message| message.author_id != self.our_node_id.to_string())
        {
            let receipt = ReplicatedReceipt::new(message.message_id.clone());
            let mut existing = doc
                .get_many(
                    Query::author(self.author)
                        .key_exact(receipt.entry_key())
                        .build(),
                )
                .await?;
            if existing.next().await.transpose()?.is_none() {
                doc.set_bytes(
                    self.author,
                    receipt.entry_key(),
                    serde_json::to_vec(&receipt)?,
                )
                .await?;
                wrote_any = true;
                debug!(
                    conversation = %log_id(&stored.public.id),
                    message = %log_id(&message.message_id),
                    "acknowledged received chat message"
                );
            }
        }
        Ok(wrote_any)
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
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let mut entries = doc
            .get_many(Query::key_prefix(MESSAGE_PREFIX).build())
            .await?;
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
    ) -> Result<()> {
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
        Ok(())
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

    fn retry_invites(&self) {
        let ids: Vec<_> = self.index.conversations.keys().cloned().collect();
        for id in ids {
            self.invite_members(&id);
        }
    }

    fn invite_members(&self, conversation_id: &str) {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return;
        };
        let invite = ChatInvite {
            version: 1,
            conversation: stored.public.clone(),
            ticket: stored.ticket.clone(),
        };
        for peer in self.other_members(stored) {
            #[cfg(test)]
            self.invite_attempts.fetch_add(1, Ordering::Relaxed);
            let sessions = self.sessions.clone();
            let invite = invite.clone();
            tokio::spawn(async move {
                if let Err(error) = send_invite(sessions, peer, &invite).await {
                    trace!(peer = %peer.fmt_short(), "chat peer not currently reachable: {error:#}");
                }
            });
        }
    }

    async fn invite_members_wait(&self, conversation_id: &str) {
        let Some(stored) = self.index.conversations.get(conversation_id) else {
            return;
        };
        let invite = ChatInvite {
            version: 1,
            conversation: stored.public.clone(),
            ticket: stored.ticket.clone(),
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
    send_chat_packet(
        sessions,
        peer,
        ChatProtocolMessage::Invite(invite.clone()),
    )
    .await
}

async fn send_sync_request(
    sessions: ChatSessionPool,
    peer: NodeId,
    conversation_id: &str,
) -> Result<()> {
    send_chat_packet(
        sessions,
        peer,
        ChatProtocolMessage::SyncRequest(SyncRequest {
            kind: "sync-request".to_owned(),
            version: 1,
            conversation_id: conversation_id.to_owned(),
        }),
    )
    .await
}

async fn wake_peers(
    sessions: ChatSessionPool,
    peers: Vec<NodeId>,
    conversation_id: &str,
) -> bool {
    if peers.is_empty() {
        return false;
    }
    let mut join_set = tokio::task::JoinSet::new();
    for peer in peers {
        let sessions = sessions.clone();
        let conversation_id = conversation_id.to_owned();
        join_set.spawn(async move {
            match send_sync_request(sessions, peer, &conversation_id).await {
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

async fn nudge_doc_sync(
    docs: &MemClient,
    stored: &StoredConversation,
    our_node_id: NodeId,
) -> Result<()> {
    let ticket = DocTicket::from_str(&stored.ticket)?;
    let mut peers: Vec<_> = ticket
        .nodes
        .iter()
        .filter(|addr| addr.node_id != our_node_id)
        .cloned()
        .collect();
    let document_id = NamespaceId::from_str(&stored.public.document_id)?;
    let doc = match docs.open(document_id).await {
        Ok(Some(doc)) => doc,
        _ => docs.import(ticket).await?,
    };
    for node in stored
        .public
        .members
        .iter()
        .filter_map(|value| NodeId::from_str(value).ok())
        .filter(|node| *node != our_node_id)
    {
        if !peers.iter().any(|addr| addr.node_id == node) {
            peers.push(NodeAddr::from(node));
        }
    }
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
    // One send/dial at a time per peer — concurrent wakes were closing each
    // other's fresh connections and forcing multi-second redial storms.
    let gate = sessions.peer_gate(peer).await;
    let _guard = gate.lock().await;

    // Try warm session with a tight timeout, then fresh dial.
    if let Some(connection) = sessions.get(peer).await {
        match tokio::time::timeout(CHAT_REUSE_TIMEOUT, send_chat_packet_on(&connection, &payload))
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
                sessions.forget(peer).await;
            }
            Err(_) => {
                trace!(peer = %peer.fmt_short(), "chat session reuse timed out; redialing");
                sessions.forget(peer).await;
            }
        }
    }
    let connection = sessions.dial(peer).await?;
    match tokio::time::timeout(CHAT_STREAM_TIMEOUT, send_chat_packet_on(&connection, &payload))
        .await
    {
        Ok(Ok(())) => {
            sessions.touch(peer).await;
            sessions.remember_connection(peer, &connection);
            log_chat_packet_sent(peer, &packet, false);
            Ok(())
        }
        Ok(Err(error)) => {
            sessions.forget(peer).await;
            Err(error).context("chat session send failed")
        }
        Err(_) => {
            sessions.forget(peer).await;
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
                "chat delivery wake-up sent"
            );
        }
    }
}

async fn send_chat_packet_on(connection: &Connection, payload: &[u8]) -> Result<()> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    send.write_all(payload).await?;
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
    let message: ChatProtocolMessage =
        serde_json::from_slice(&bytes).context("invalid Wire chat protocol message")?;
    send.write_all(b"ok").await?;
    send.finish()?;
    Ok(message)
}

fn log_id(value: &str) -> &str {
    value.get(..24).unwrap_or(value)
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

fn delivery_retry_delay(conversation_id: &str, attempts: u8) -> Duration {
    // Fast, flat probes while waiting on receipt. The old 2/4/8/16s exponential
    // schedule left continuous chats idle for many seconds after a wake already
    // succeeded at the chat-ALPN layer (docs pull / receipt still in flight).
    let base_ms = match attempts {
        0 | 1 => 400,
        2 => 700,
        3 => 1_000,
        4..=10 => 1_500,
        _ => 3_000.min(MAX_RETRY_SECONDS.saturating_mul(1000)),
    };
    let jitter_ms = conversation_id
        .bytes()
        .fold(u64::from(attempts), |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(u64::from(byte))
        })
        % 200;
    Duration::from_millis(base_ms + jitter_ms)
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
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn load_local_deletions(path: &Path) -> LocalDeletionIndex {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_index(path: &Path, index: &ChatIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(index)?)?;
    Ok(())
}

fn save_local_deletions(path: &Path, index: &LocalDeletionIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(index)?)?;
    Ok(())
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
            .accept(iroh_blobs::ALPN, protocols.blobs.clone())
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_nodes_exchange_and_reload_messages_without_calls() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("wire_app=debug,iroh_docs=info")
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

    #[test]
    fn delivery_retries_are_bounded_and_desynchronised() {
        let first = delivery_retry_delay("dm/example-a", 1);
        let second = delivery_retry_delay("dm/example-b", 1);
        assert!(first >= Duration::from_millis(400));
        assert!(first < Duration::from_millis(700));
        assert!(delivery_retry_delay("dm/example", 99) <= Duration::from_secs(4));
        assert_ne!(
            first, second,
            "conversation-specific jitter avoids retry herds"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn history_clear_deletes_document_and_rotates_replicas() -> Result<()> {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("wire_app=debug,iroh_docs=info")
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
