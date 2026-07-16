use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
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
        if self.message_id.is_empty() || self.author_id.is_empty() {
            bail!("message identity is incomplete");
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
    Pending,
    Synced,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatInvite {
    version: u8,
    conversation: ChatConversation,
    ticket: String,
}

#[derive(Debug)]
pub(crate) struct IncomingInvite {
    remote: NodeId,
    invite: ChatInvite,
}

#[derive(Debug, Clone)]
pub struct ChatInviteProtocol {
    tx: async_channel::Sender<IncomingInvite>,
}

impl ChatInviteProtocol {
    fn new() -> (Self, async_channel::Receiver<IncomingInvite>) {
        let (tx, rx) = async_channel::bounded(64);
        (Self { tx }, rx)
    }
}

impl ProtocolHandler for ChatInviteProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let tx = self.tx.clone();
        async move {
            let connection = connecting.await?;
            let remote = connection.remote_node_id()?;
            info!(peer = %remote.fmt_short(), "received chat protocol connection");
            let (mut send, mut recv) = connection.accept_bi().await?;
            let mut length = [0u8; 4];
            recv.read_exact(&mut length).await?;
            let length = u32::from_be_bytes(length) as usize;
            if length > MAX_INVITE_BYTES {
                bail!("chat invitation exceeds safety cap");
            }
            let mut bytes = vec![0; length];
            recv.read_exact(&mut bytes).await?;
            let invite: ChatInvite =
                serde_json::from_slice(&bytes).context("invalid Wire chat invitation")?;
            tx.send(IncomingInvite { remote, invite }).await?;
            send.write_all(b"ok").await?;
            send.finish()?;
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        async move {}.boxed()
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
    docs: MemClient,
    blobs: BlobStore,
    author: AuthorId,
    root: PathBuf,
    index: ChatIndex,
    our_node_id: NodeId,
    invite_rx: async_channel::Receiver<IncomingInvite>,
    doc_event_tx: async_channel::Sender<String>,
    doc_event_rx: async_channel::Receiver<String>,
    subscriptions: BTreeSet<String>,
    queued: VecDeque<ChatNotification>,
    retry: tokio::time::Interval,
    #[cfg(test)]
    invite_attempts: AtomicU64,
}

pub(crate) enum ChatInput {
    Invite(IncomingInvite),
    DocumentChanged(String),
    Retry,
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
        let (invites, invite_rx) = ChatInviteProtocol::new();
        let client = docs.client().clone();
        let author = client.authors().default().await?;
        let (doc_event_tx, doc_event_rx) = async_channel::bounded(256);
        let index = load_index(&root.join("index.json"));
        let conversation_count = index.conversations.len();
        let mut service = Self {
            endpoint: endpoint.clone(),
            docs: client,
            blobs: blob_store,
            author,
            root,
            index,
            our_node_id: endpoint.node_id(),
            invite_rx,
            doc_event_tx,
            doc_event_rx,
            subscriptions: BTreeSet::new(),
            queued: VecDeque::new(),
            retry: tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_secs(30),
                Duration::from_secs(30),
            ),
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
        self.retry_invites();
    }

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
            changed = self.doc_event_rx.recv() => ChatInput::DocumentChanged(
                changed.expect("chat document event channel closed")
            ),
            _ = self.retry.tick() => ChatInput::Retry,
        }
    }

    pub(crate) async fn process_input(&mut self, input: ChatInput) -> Option<ChatNotification> {
        match input {
            ChatInput::Invite(incoming) => {
                if let Err(error) = self.accept_invite(incoming).await {
                    return Some(ChatNotification::Error(format!(
                        "Chat invitation failed: {error:#}"
                    )));
                }
            }
            ChatInput::DocumentChanged(id) => {
                if let Err(error) = self.publish_timeline(&id).await {
                    return Some(ChatNotification::Error(format!(
                        "Could not refresh chat: {error:#}"
                    )));
                }
            }
            ChatInput::Retry => self.retry_invites(),
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
            self.retry_invites();
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
        self.retry_invites();
        Ok(id)
    }

    pub async fn send_message(&mut self, conversation_id: String, message: ChatMessage) {
        let message_id = message.message_id.clone();
        let body_bytes = message.body.len();
        let result = self
            .insert_message(&conversation_id, &message)
            .await
            .map(|_| DeliveryState::Synced);
        match result {
            Ok(state) => {
                info!(
                    conversation = %log_id(&conversation_id),
                    message = %log_id(&message_id),
                    bytes = body_bytes,
                    "message committed to local chat replica"
                );
                self.queued.push_back(ChatNotification::Delivery {
                    message_id,
                    state,
                    detail: None,
                });
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

    async fn create_conversation(
        &self,
        id: String,
        title: String,
        kind: ConversationKind,
        members: Vec<String>,
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
            },
            ticket: ticket.to_string(),
        })
    }

    async fn accept_invite(&mut self, incoming: IncomingInvite) -> Result<()> {
        info!(peer = %incoming.remote.fmt_short(), conversation = %log_id(&incoming.invite.conversation.id), "accepting chat invitation");
        let mut invite = incoming.invite;
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
                .any(|id| id == &incoming.remote.to_string())
        {
            bail!("invitation membership does not match its sender and recipient");
        }
        if matches!(invite.conversation.kind, ConversationKind::Direct { .. }) {
            let expected = direct_conversation_id(self.our_node_id, incoming.remote);
            if invite.conversation.id != expected || invite.conversation.members.len() != 2 {
                bail!("direct-message invitation has inconsistent members");
            }
            invite.conversation.kind = ConversationKind::Direct {
                peer_id: incoming.remote.to_string(),
            };
        }
        let _: DocTicket =
            DocTicket::from_str(&invite.ticket).context("invalid document ticket")?;

        let id = invite.conversation.id.clone();
        let replace = self
            .index
            .conversations
            .get(&id)
            .map(|current| invite.conversation.document_id < current.public.document_id)
            .unwrap_or(true);
        if !replace {
            debug!(
                conversation = %log_id(&id),
                peer = %incoming.remote.fmt_short(),
                "ignored chat invitation for a non-canonical replica"
            );
            return Ok(());
        }

        let migrated = if let Some(current) = self.index.conversations.get(&id) {
            self.load_messages(current).await.unwrap_or_default()
        } else {
            Vec::new()
        };
        self.index.conversations.insert(
            id.clone(),
            StoredConversation {
                public: invite.conversation,
                ticket: invite.ticket,
            },
        );
        self.subscriptions.remove(&id);
        self.persist_index()?;
        self.open_and_publish(&id).await?;
        info!(conversation = %log_id(&id), "chat invitation imported");
        for message in migrated {
            if let Err(error) = self.insert_message(&id, &message).await {
                warn!(conversation = %log_id(&id), "failed to migrate a message to the canonical chat document: {error:#}");
            }
        }
        Ok(())
    }

    async fn open_and_publish(&mut self, id: &str) -> Result<()> {
        let stored = self
            .index
            .conversations
            .get(id)
            .cloned()
            .context("unknown conversation")?;
        let ticket = DocTicket::from_str(&stored.ticket)?;
        let document_id = NamespaceId::from_str(&stored.public.document_id)?;
        let doc = match self.docs.open(document_id).await {
            Ok(Some(doc)) => doc,
            _ => self.docs.import(ticket).await?,
        };
        let peers = stored
            .public
            .members
            .iter()
            .filter_map(|value| NodeId::from_str(value).ok())
            .filter(|node| *node != self.our_node_id)
            .map(NodeAddr::from)
            .collect();
        doc.start_sync(peers).await?;
        if self.subscriptions.insert(id.to_owned()) {
            info!(conversation = %log_id(id), "subscribed to chat document events");
            let mut events = doc.subscribe().await?;
            let tx = self.doc_event_tx.clone();
            let id = id.to_owned();
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    match event {
                        Ok(LiveEvent::InsertLocal { .. }) => {
                            debug!(conversation = %log_id(&id), source = "local", "chat document changed");
                            if tx.send(id.clone()).await.is_err() {
                                break;
                            }
                        }
                        Ok(LiveEvent::InsertRemote { .. }) => {
                            debug!(conversation = %log_id(&id), source = "remote", "chat document changed");
                            if tx.send(id.clone()).await.is_err() {
                                break;
                            }
                        }
                        Ok(LiveEvent::ContentReady { .. }) | Ok(LiveEvent::PendingContentReady) => {
                            debug!(conversation = %log_id(&id), source = "content-ready", "chat document changed");
                            if tx.send(id.clone()).await.is_err() {
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
        let mut messages = BTreeMap::<String, ChatMessage>::new();
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
                    .or_insert(message);
            }
        }
        let mut messages: Vec<_> = messages.into_values().collect();
        messages.sort();
        Ok(messages)
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
        for stored in self.index.conversations.values() {
            let invite = ChatInvite {
                version: 1,
                conversation: stored.public.clone(),
                ticket: stored.ticket.clone(),
            };
            for member in &stored.public.members {
                let Ok(peer) = NodeId::from_str(member) else {
                    continue;
                };
                if peer == self.our_node_id {
                    continue;
                }
                #[cfg(test)]
                self.invite_attempts.fetch_add(1, Ordering::Relaxed);
                let endpoint = self.endpoint.clone();
                let invite = invite.clone();
                tokio::spawn(async move {
                    if let Err(error) = send_invite(endpoint, peer, &invite).await {
                        trace!(peer = %peer.fmt_short(), "chat peer not currently reachable: {error:#}");
                    }
                });
            }
        }
    }

    fn persist_index(&self) -> Result<()> {
        save_index(&self.root.join("index.json"), &self.index)
    }
}

async fn send_invite(endpoint: Endpoint, peer: NodeId, invite: &ChatInvite) -> Result<()> {
    let payload = serde_json::to_vec(invite)?;
    if payload.len() > MAX_INVITE_BYTES {
        bail!("chat invitation exceeds safety cap");
    }
    trace!(peer = %peer.fmt_short(), conversation = %log_id(&invite.conversation.id), "sending chat invitation");
    let connection: Connection = endpoint.connect(NodeAddr::from(peer), CHAT_ALPN).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    send.write_all(&payload).await?;
    send.finish()?;
    let mut ack = [0u8; 2];
    recv.read_exact(&mut ack).await?;
    if &ack != b"ok" {
        bail!("chat peer returned an invalid invitation acknowledgement");
    }
    info!(peer = %peer.fmt_short(), conversation = %log_id(&invite.conversation.id), "chat invitation sent");
    Ok(())
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

fn load_index(path: &Path) -> ChatIndex {
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
        left.send_message(conversation_id, outbound).await;
        wait_for_body(&mut right, "offline from calls").await?;
        assert_eq!(
            right.invite_attempts.load(Ordering::Relaxed),
            0,
            "accepting an invite must not immediately echo another invite"
        );

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

        wait_for_body(&mut left, "offline from calls").await?;
        wait_for_body(&mut right, "offline from calls").await?;
        left_router.shutdown().await?;
        right_router.shutdown().await?;
        Ok(())
    }
}
