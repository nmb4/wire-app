//! Private client-to-client presence and version exchange.
//!
//! This protocol is intentionally infrastructure-only. It lets saved contacts
//! announce lifecycle changes and compare client versions without coupling
//! presence to chat messages or exposing protocol controls in the UI.

use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_channel::{Receiver, Sender};
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::{debug, warn};

pub const CLIENT_STATUS_ALPN: &[u8] = b"wire/client-status/1";
const MAX_PACKET_BYTES: usize = 4 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const SHUTDOWN_BROADCAST_TIMEOUT: Duration = Duration::from_secs(3);

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
    pub client_version: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StatusPacket {
    protocol_version: u8,
    availability: Availability,
    client_version: String,
}

impl StatusPacket {
    fn new(availability: Availability) -> Self {
        Self {
            protocol_version: 1,
            availability,
            client_version: crate::APP_VERSION.to_owned(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.protocol_version != 1 {
            bail!(
                "unsupported client status protocol version {}",
                self.protocol_version
            );
        }
        if self.client_version.len() > 64 {
            bail!("client version exceeds safety limit");
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
}

impl ClientStatusProtocol {
    pub fn new(endpoint: Endpoint) -> Self {
        let (update_tx, update_rx) = async_channel::bounded(64);
        Self {
            endpoint,
            allowed_peers: Arc::new(RwLock::new(BTreeSet::new())),
            update_tx,
            update_rx,
        }
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
            tokio::spawn(async move {
                let update = match exchange(&endpoint, peer, Availability::Online).await {
                    Ok(packet) => StatusUpdate {
                        peer,
                        availability: packet.availability,
                        client_version: Some(packet.client_version),
                    },
                    Err(error) => {
                        debug!(peer = %peer.fmt_short(), "client status probe failed: {error:#}");
                        StatusUpdate {
                            peer,
                            availability: Availability::Offline,
                            client_version: None,
                        }
                    }
                };
                let _ = update_tx.send(update).await;
            });
        }
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
                    if let Err(error) = exchange(&endpoint, peer, Availability::Offline).await {
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
                .update_tx
                .send(StatusUpdate {
                    peer,
                    availability: packet.availability,
                    client_version: Some(packet.client_version),
                })
                .await?;

            write_packet(&mut send, &StatusPacket::new(Availability::Online)).await?;
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
) -> Result<StatusPacket> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let connection: Connection = endpoint
            .connect(NodeAddr::from(peer), CLIENT_STATUS_ALPN)
            .await
            .with_context(|| format!("connect to {} for client status", peer.fmt_short()))?;
        let (mut send, mut recv) = connection.open_bi().await?;
        write_packet(&mut send, &StatusPacket::new(availability)).await?;
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
        let mut packet = StatusPacket::new(Availability::Online);
        packet.protocol_version = 2;
        assert!(packet.validate().is_err());
    }
}
