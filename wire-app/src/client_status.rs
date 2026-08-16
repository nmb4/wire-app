//! Private client-to-client presence and version exchange.
//!
//! This protocol is intentionally infrastructure-only. It lets saved contacts
//! announce lifecycle changes and compare client versions without coupling
//! presence to chat messages or exposing protocol controls in the UI.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use async_channel::{Receiver, Sender};
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::{debug, warn};

pub const CLIENT_STATUS_ALPN: &[u8] = b"wire/client-status/2";
const MAX_PACKET_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const SHUTDOWN_BROADCAST_TIMEOUT: Duration = Duration::from_secs(3);
pub const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const OFFLINE_AFTER_CONSECUTIVE_FAILURES: u8 = 3;
const MISSED_GROUP_CALL_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default)]
struct ProbeHealth {
    generation: u64,
    consecutive_failures: u8,
    offline_emitted: bool,
}

impl ProbeHealth {
    fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    fn succeeded(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.consecutive_failures = 0;
        self.offline_emitted = false;
        true
    }

    fn failed(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures < OFFLINE_AFTER_CONSECUTIVE_FAILURES || self.offline_emitted {
            return false;
        }
        self.offline_emitted = true;
        true
    }

    fn observed_inbound(&mut self) {
        // Invalidate any outbound probe still racing this authoritative packet.
        self.generation = self.generation.wrapping_add(1).max(1);
        self.consecutive_failures = 0;
        self.offline_emitted = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Online,
    Offline,
}

#[derive(Clone, Debug)]
pub struct StatusUpdate {
    pub peer: NodeId,
    pub availability: Availability,
    #[cfg_attr(not(windows), allow(dead_code))]
    pub client_version: Option<String>,
    pub active_group_calls: Vec<GroupCallAnnouncement>,
}

/// Ephemeral group-call room state replicated during the existing presence
/// heartbeat. This lets a client that was offline discover and join a call as
/// soon as it comes back without requiring a server or a durable chat message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCallAnnouncement {
    pub call_id: String,
    pub conversation_id: String,
    pub title: String,
    pub initiator: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub participants: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StatusPacket {
    protocol_version: u8,
    availability: Availability,
    client_version: String,
    active_group_calls: Vec<GroupCallAnnouncement>,
}

impl StatusPacket {
    fn new(availability: Availability, active_group_calls: Vec<GroupCallAnnouncement>) -> Self {
        Self {
            protocol_version: 2,
            availability,
            client_version: crate::APP_VERSION.to_owned(),
            active_group_calls,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.protocol_version != 2 {
            bail!(
                "unsupported client status protocol version {}",
                self.protocol_version
            );
        }
        if self.client_version.len() > 64 {
            bail!("client version exceeds safety limit");
        }
        if self.active_group_calls.len() > 32
            || self.active_group_calls.iter().any(|call| {
                call.call_id.len() > 160
                    || call.conversation_id.len() > 512
                    || call.title.len() > 256
                    || call.initiator.len() > 128
                    || call.participants.len() > 64
                    || call.participants.iter().any(|peer| peer.len() > 128)
            })
        {
            bail!("group call advertisement exceeds safety limit");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ClientStatusProtocol {
    endpoint: Endpoint,
    allowed_peers: Arc<RwLock<BTreeSet<NodeId>>>,
    update_tx: Sender<StatusUpdate>,
    update_rx: Receiver<StatusUpdate>,
    active_group_calls: Arc<RwLock<Vec<GroupCallAnnouncement>>>,
    probe_health: Arc<Mutex<BTreeMap<NodeId, ProbeHealth>>>,
}

impl ClientStatusProtocol {
    pub fn new(endpoint: Endpoint) -> Self {
        let (update_tx, update_rx) = async_channel::bounded(64);
        Self {
            endpoint,
            allowed_peers: Arc::new(RwLock::new(BTreeSet::new())),
            update_tx,
            update_rx,
            active_group_calls: Arc::new(RwLock::new(Vec::new())),
            probe_health: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn set_active_group_calls(&self, calls: Vec<GroupCallAnnouncement>) {
        *self
            .active_group_calls
            .write()
            .expect("client status call lock poisoned") = calls;
    }

    fn active_group_calls(&self) -> Vec<GroupCallAnnouncement> {
        let now = now_millis();
        self.active_group_calls
            .read()
            .expect("client status call lock poisoned")
            .iter()
            .filter(|call| group_call_is_advertisable(call, now))
            .cloned()
            .collect()
    }

    pub fn replace_peers(&self, peers: BTreeSet<NodeId>) {
        *self
            .allowed_peers
            .write()
            .expect("client status peer lock poisoned") = peers;
    }

    pub async fn next_update(&self) -> Result<StatusUpdate> {
        Ok(self.update_rx.recv().await?)
    }

    pub fn announce_online(&self, peers: impl IntoIterator<Item = NodeId>) {
        for peer in peers {
            let endpoint = self.endpoint.clone();
            let update_tx = self.update_tx.clone();
            let active_group_calls = self.active_group_calls();
            let probe_health = self.probe_health.clone();
            let generation = probe_health
                .lock()
                .expect("client status probe lock poisoned")
                .entry(peer)
                .or_default()
                .begin();
            tokio::spawn(async move {
                match exchange(&endpoint, peer, Availability::Online, active_group_calls).await {
                    Ok(packet) => {
                        let current = probe_health
                            .lock()
                            .expect("client status probe lock poisoned")
                            .entry(peer)
                            .or_default()
                            .succeeded(generation);
                        if !current {
                            debug!(peer = %peer.fmt_short(), generation, "ignored stale client status response");
                            return;
                        }
                        debug!(
                            peer = %peer.fmt_short(),
                            calls = packet.active_group_calls.len(),
                            "client status probe succeeded"
                        );
                        let _ = update_tx
                            .send(StatusUpdate {
                                peer,
                                availability: packet.availability,
                                client_version: Some(packet.client_version),
                                active_group_calls: packet.active_group_calls,
                            })
                            .await;
                    }
                    Err(error) => {
                        debug!(peer = %peer.fmt_short(), "client status probe failed: {error:#}");
                        let confirmed_offline = probe_health
                            .lock()
                            .expect("client status probe lock poisoned")
                            .entry(peer)
                            .or_default()
                            .failed(generation);
                        if confirmed_offline {
                            debug!(
                                peer = %peer.fmt_short(),
                                failures = OFFLINE_AFTER_CONSECUTIVE_FAILURES,
                                "peer considered offline after consecutive probe failures"
                            );
                            let _ = update_tx
                                .send(StatusUpdate {
                                    peer,
                                    availability: Availability::Offline,
                                    client_version: None,
                                    active_group_calls: Vec::new(),
                                })
                                .await;
                        }
                    }
                }
            });
        }
    }

    pub fn refresh_allowed_peers(&self) {
        let peers = self
            .allowed_peers
            .read()
            .expect("client status peer lock poisoned")
            .clone();
        self.announce_online(peers);
    }

    pub async fn broadcast_offline(&self) {
        let peers = self
            .allowed_peers
            .read()
            .expect("client status peer lock poisoned")
            .clone();
        if peers.is_empty() {
            return;
        }

        let endpoint = self.endpoint.clone();
        let broadcast = async move {
            let mut tasks = tokio::task::JoinSet::new();
            for peer in peers {
                let endpoint = endpoint.clone();
                tasks.spawn(async move {
                    if let Err(error) =
                        exchange(&endpoint, peer, Availability::Offline, Vec::new()).await
                    {
                        debug!(
                            peer = %peer.fmt_short(),
                            "offline status announcement failed: {error:#}"
                        );
                    }
                });
            }
            while tasks.join_next().await.is_some() {}
        };
        if tokio::time::timeout(SHUTDOWN_BROADCAST_TIMEOUT, broadcast)
            .await
            .is_err()
        {
            debug!("timed out while broadcasting offline status");
        }
    }

    fn peer_is_allowed(&self, peer: NodeId) -> bool {
        self.allowed_peers
            .read()
            .expect("client status peer lock poisoned")
            .contains(&peer)
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn group_call_is_advertisable(call: &GroupCallAnnouncement, now_ms: i64) -> bool {
    call.ended_at_ms.is_none_or(|ended_at| {
        ended_at > now_ms
            || now_ms.saturating_sub(ended_at) < MISSED_GROUP_CALL_TTL.as_millis() as i64
    })
}

impl ProtocolHandler for ClientStatusProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let protocol = self.clone();
        async move {
            let connection = connecting.await?;
            let peer = connection.remote_node_id()?;
            if !protocol.peer_is_allowed(peer) {
                warn!(
                    peer = %peer.fmt_short(),
                    "ignored client status from a node that is not a saved friend"
                );
                connection.close(1u32.into(), b"not a saved friend");
                return Ok(());
            }

            let (mut send, mut recv) = connection.accept_bi().await?;
            let packet: StatusPacket = read_packet(&mut recv).await?;
            packet.validate()?;
            protocol
                .probe_health
                .lock()
                .expect("client status probe lock poisoned")
                .entry(peer)
                .or_default()
                .observed_inbound();
            protocol
                .update_tx
                .send(StatusUpdate {
                    peer,
                    availability: packet.availability,
                    client_version: Some(packet.client_version),
                    active_group_calls: packet.active_group_calls,
                })
                .await?;

            write_packet(
                &mut send,
                &StatusPacket::new(Availability::Online, protocol.active_group_calls()),
            )
            .await?;
            send.finish()?;
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        async move {}.boxed()
    }
}

async fn exchange(
    endpoint: &Endpoint,
    peer: NodeId,
    availability: Availability,
    active_group_calls: Vec<GroupCallAnnouncement>,
) -> Result<StatusPacket> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let connection: Connection = endpoint
            .connect(NodeAddr::from(peer), CLIENT_STATUS_ALPN)
            .await
            .with_context(|| format!("connect to {} for client status", peer.fmt_short()))?;
        let (mut send, mut recv) = connection.open_bi().await?;
        write_packet(
            &mut send,
            &StatusPacket::new(availability, active_group_calls),
        )
        .await?;
        send.finish()?;
        let response: StatusPacket = read_packet(&mut recv).await?;
        response.validate()?;
        Result::<_>::Ok(response)
    })
    .await
    .context("client status exchange timed out")?
}

async fn write_packet<T: Serialize>(
    send: &mut iroh::endpoint::SendStream,
    packet: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(packet)?;
    if bytes.is_empty() || bytes.len() > MAX_PACKET_BYTES {
        bail!("invalid client status packet length {}", bytes.len());
    }
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

async fn read_packet<T: DeserializeOwned>(recv: &mut iroh::endpoint::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_PACKET_BYTES {
        bail!("invalid client status packet length {len}");
    }
    let mut bytes = vec![0; len];
    recv.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_packet_rejects_unknown_protocol_versions() {
        let mut packet = StatusPacket::new(Availability::Online, Vec::new());
        packet.protocol_version = 1;
        assert!(packet.validate().is_err());
    }

    #[test]
    fn status_packet_bounds_group_call_metadata() {
        let mut packet = StatusPacket::new(
            Availability::Online,
            vec![GroupCallAnnouncement {
                call_id: "call".to_owned(),
                conversation_id: "group".to_owned(),
                title: "Group".to_owned(),
                initiator: "peer".to_owned(),
                started_at_ms: 1,
                ended_at_ms: None,
                participants: vec!["x".repeat(129)],
            }],
        );
        assert!(packet.validate().is_err());
        packet.active_group_calls[0].participants = vec!["peer".to_owned()];
        packet.validate().unwrap();
    }

    #[test]
    fn transient_and_stale_probe_failures_do_not_withdraw_presence() {
        let mut health = ProbeHealth::default();
        let first = health.begin();
        assert!(!health.failed(first));

        let second = health.begin();
        assert!(!health.failed(first), "stale completion must be ignored");
        assert!(!health.failed(second));

        let third = health.begin();
        assert!(health.failed(third));
        assert!(!health.failed(third), "offline is emitted only once");

        let recovered = health.begin();
        assert!(health.succeeded(recovered));
        let after_recovery = health.begin();
        assert!(!health.failed(after_recovery));
    }

    #[test]
    fn inbound_heartbeat_invalidates_a_racing_failed_probe() {
        let mut health = ProbeHealth::default();
        let generation = health.begin();
        health.observed_inbound();
        assert!(!health.failed(generation));
        assert_eq!(health.consecutive_failures, 0);
    }

    #[test]
    fn missed_group_call_advertisements_expire() {
        let now = 2 * MISSED_GROUP_CALL_TTL.as_millis() as i64;
        let mut call = GroupCallAnnouncement {
            call_id: "call".to_owned(),
            conversation_id: "group".to_owned(),
            title: "Group".to_owned(),
            initiator: "peer".to_owned(),
            started_at_ms: 1,
            ended_at_ms: None,
            participants: vec!["peer".to_owned()],
        };
        assert!(group_call_is_advertisable(&call, now));
        call.ended_at_ms = Some(now - 1_000);
        assert!(group_call_is_advertisable(&call, now));
        call.ended_at_ms = Some(now - MISSED_GROUP_CALL_TTL.as_millis() as i64);
        assert!(!group_call_is_advertisable(&call, now));
    }
}
