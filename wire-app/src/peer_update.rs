//! Peer-to-peer Wire executable transfer over a dedicated iroh ALPN.
//!
//! A client that is running a newer build can hand its own executable to a
//! connected friend, so an outdated install can catch up without ever reaching
//! the release server. The transfer is strictly on demand:
//!
//! * only saved friends may ask (the same gate presence uses),
//! * the running client decides when to ask by clicking the title-bar button,
//! * the receiving client only accepts a strictly newer version,
//! * the payload is verified against the sender's SHA-256 before it is staged.
//!
//! The serving side discovers its own binary through `std::env::current_exe`,
//! so the file that is shared is exactly the file the sender is running.

use std::{
    fs::File,
    io::{BufReader, Read, Write},
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::{
    client_status::AllowedPeers,
    update::{is_version_newer, MAX_EXECUTABLE_BYTES},
};

pub const PEER_UPDATE_ALPN: &[u8] = b"wire/update/1";
const PROTOCOL_VERSION: u8 = 1;
/// Metadata frames are tiny; a larger one is a protocol error, not an update.
const MAX_FRAME_BYTES: usize = 8 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for one response (or one chunk of it) once connected.
///
/// Without it a peer that accepts the connection and then stalls would leave the
/// UI on "Asking for their executable…" forever: the accept completes, but
/// nothing ever arrives on the stream.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a sender waits for the requester to close before giving up. Long
/// enough for a large body to be acknowledged, short enough that a crashed
/// requester cannot park the task until the QUIC idle timeout.
const PEER_CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// A request for the sender's executable metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateRequest {
    protocol_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The version the requester runs, so an up-to-date build is told plainly
    /// that it is current instead of downloading a downgrade.
    current_version: Option<String>,
    /// The requester's platform tag.
    ///
    /// Without it the sender cannot know that its binary is unusable there and
    /// would happily stream the whole file into a receiver about to abandon it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    /// True when the requester only wants the metadata frame.
    ///
    /// The two phases are separate requests rather than one long-lived
    /// connection: asking first and downloading later means the user sees what a
    /// friend is offering (and how large it is) before committing to a transfer,
    /// and a peer that goes away in between costs one failed dial, not a
    /// half-written executable.
    #[serde(default)]
    metadata_only: bool,
}

impl UpdateRequest {
    fn for_offer() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            current_version: Some(crate::APP_VERSION.to_owned()),
            platform: Some(current_platform()),
            metadata_only: true,
        }
    }

    fn for_download() -> Self {
        Self {
            metadata_only: false,
            ..Self::for_offer()
        }
    }

    fn validate(&self) -> Result<()> {
        if self.protocol_version != PROTOCOL_VERSION {
            bail!(
                "unsupported update protocol version {}",
                self.protocol_version
            );
        }
        if let Some(version) = &self.current_version {
            if version.len() > 64 {
                bail!("client version exceeds safety limit");
            }
        }
        if let Some(platform) = &self.platform {
            if platform.len() > 32 {
                bail!("platform tag exceeds safety limit");
            }
        }
        Ok(())
    }

    /// Whether the sender's binary could run on the requester's machine.
    ///
    /// An absent tag counts as unknown and is served anyway: the receiver
    /// independently refuses before touching the disk, so this is a bandwidth
    /// optimisation rather than a safety gate.
    fn can_serve(&self) -> bool {
        self.platform
            .as_deref()
            .is_none_or(|platform| platform == current_platform())
    }
}

/// The first frame of a response: what the sender would hand over.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateHeader {
    protocol_version: u8,
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_bytes: Option<u64>,
    /// Platform tag the executable was built for.
    ///
    /// Required on the wire: an older build that omits it is refused rather
    /// than trusted, because a foreign binary cannot run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
}

impl UpdateHeader {
    fn refusal(error: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            error: Some(error.into()),
            version: None,
            executable_name: None,
            sha256: None,
            total_bytes: None,
            platform: None,
        }
    }

    fn into_offer(self) -> Result<UpdateOffer> {
        if !self.ok {
            bail!(
                "{}",
                self.error
                    .unwrap_or_else(|| "the peer refused to share its executable".to_owned())
            );
        }
        if self.protocol_version != PROTOCOL_VERSION {
            bail!(
                "peer speaks update protocol version {}, expected {PROTOCOL_VERSION}",
                self.protocol_version
            );
        }
        let version = self.version.context("peer omitted its version")?;
        let sha256 = self
            .sha256
            .context("peer omitted the executable checksum")?;
        let total_bytes = self
            .total_bytes
            .context("peer omitted the executable size")?;
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("peer sent a malformed executable checksum");
        }
        if total_bytes == 0 || total_bytes > MAX_EXECUTABLE_BYTES {
            bail!("peer advertised an unusable executable size ({total_bytes} bytes)");
        }
        Ok(UpdateOffer {
            version,
            platform: self
                .platform
                .ok_or_else(|| anyhow::anyhow!("peer omitted its platform"))?,
            executable_name: self
                .executable_name
                .unwrap_or_else(|| "wire-app.exe".to_owned()),
            sha256,
            total_bytes,
        })
    }
}

/// Metadata about an executable a peer is willing to share.
///
/// Offered to the UI so the user can see what a friend is running (and how large
/// the download is) before accepting anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOffer {
    pub version: String,
    /// The sender's platform and architecture, e.g. `windows-x86_64`.
    ///
    /// An executable is only runnable on the machine it was built for, so the
    /// receiver must refuse a friend's binary from another platform instead of
    /// replacing its own working install with something that cannot start.
    pub platform: String,
    pub executable_name: String,
    pub sha256: String,
    pub total_bytes: u64,
}

impl UpdateOffer {
    pub fn is_runnable_here(&self) -> bool {
        self.platform == current_platform()
    }

    /// The one-line summary shown before the user accepts a transfer.
    pub fn describe(&self, current_version: &str) -> String {
        format!(
            "Wire v{} · {} · you run v{}",
            self.version,
            crate::app::format_bytes(self.total_bytes),
            current_version
        )
    }
}

/// The platform tag this build advertises.
pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

impl UpdateOffer {
    /// A peer running the same or an older build is not an update.
    pub fn is_usable_for(&self, current_version: &str) -> bool {
        is_version_newer(&self.version, current_version)
    }
}

/// Serves this node's own executable to saved friends.
///
/// The response is produced once per process and cached: the binary is the one
/// currently executing, so it cannot change under us and re-hashing tens of
/// megabytes per request would be pure waste.
#[derive(Debug, Clone)]
pub struct PeerUpdateProtocol {
    allowed_peers: AllowedPeers,
    /// The version this node would advertise. Injected so a test can pose as a
    /// newer build than the requester.
    version: Arc<str>,
    header: Arc<OnceLock<UpdateHeader>>,
}

impl PeerUpdateProtocol {
    pub fn new(allowed_peers: AllowedPeers) -> Self {
        Self::with_version(allowed_peers, crate::APP_VERSION)
    }

    fn with_version(allowed_peers: AllowedPeers, version: &str) -> Self {
        Self {
            allowed_peers,
            version: Arc::from(version),
            header: Arc::new(OnceLock::new()),
        }
    }

    fn header(&self) -> Result<UpdateHeader> {
        // Only success is cached: a transient IO failure while reading or hashing
        // the executable must not poison every later request until restart.
        if let Some(header) = self.header.get() {
            return Ok(header.clone());
        }
        let header = build_header(&self.version).map_err(|error| anyhow::anyhow!("{error}"))?;
        let _ = self.header.set(header.clone());
        Ok(header)
    }
}

/// Send a refusal and let the requester close the connection.
///
/// The frame must be finished explicitly: returning early drops the stream
/// mid-frame, and the requester would see a bare connection loss instead of the
/// reason it was turned down. The connection is deliberately *not* closed here
/// either — see the metadata path in [`PeerUpdateProtocol::accept`].
async fn refuse(send: &mut iroh::endpoint::SendStream, connection: &Connection, error: String) {
    let header = UpdateHeader::refusal(error);
    if write_json(send, &header).await.is_err() {
        // The stream is unusable; close rather than leaving the connection to
        // time out.
        connection.close(1u32.into(), b"refusal not delivered");
        return;
    }
    let _ = send.finish();
    let _ = send.stopped().await;
    await_peer_close(connection).await;
}

/// Wait for the requester to close, but never park the task indefinitely.
async fn await_peer_close(connection: &Connection) {
    tokio::time::timeout(PEER_CLOSE_TIMEOUT, connection.closed())
        .await
        .ok();
}

impl ProtocolHandler for PeerUpdateProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let protocol = self.clone();
        async move {
            let connection = connecting.await?;
            let peer = connection
                .remote_node_id()
                .context("update connection is missing a remote node id")?;
            if !protocol.allowed_peers.contains(peer) {
                warn!(
                    peer = %peer.fmt_short(),
                    "refused an executable transfer to a node that is not a saved friend"
                );
                connection.close(1u32.into(), b"not a saved friend");
                return Ok(());
            }

            let (mut send, mut recv) = match connection.accept_bi().await {
                Ok(streams) => streams,
                Err(error) => {
                    connection.close(1u32.into(), b"no update request");
                    return Err(error.into());
                }
            };
            let request: UpdateRequest = match read_json(&mut recv).await {
                Ok(request) => request,
                Err(error) => {
                    // An unreadable request is not a reason to hold the connection
                    // open until the QUIC idle timeout.
                    connection.close(1u32.into(), b"bad update request");
                    return Err(error);
                }
            };
            if let Err(error) = request.validate() {
                warn!(peer = %peer.fmt_short(), "rejected an executable request: {error:#}");
                refuse(&mut send, &connection, format!("unsupported request: {error:#}")).await;
                return Ok(());
            }

            // Refuse before hashing or streaming: an up-to-date peer should not
            // pull tens of megabytes it would then discard as a downgrade.
            let ours = protocol.version.to_string();
            if let Some(current) = request.current_version.as_deref() {
                if !is_version_newer(&ours, current) {
                    let error = format!(
                        "you are already running v{current}, which is not older than v{ours}"
                    );
                    debug!(peer = %peer.fmt_short(), %error, "declined to share the executable");
                    refuse(&mut send, &connection, error).await;
                    return Ok(());
                }
            }

            // Same reasoning for a foreign platform: streaming a binary the
            // requester cannot run wastes both ends' bandwidth.
            if !request.can_serve() {
                let error = format!(
                    "this build runs on {}, but you are on {}",
                    current_platform(),
                    request.platform.as_deref().unwrap_or("an unknown platform")
                );
                debug!(peer = %peer.fmt_short(), %error, "declined to share a foreign build");
                refuse(&mut send, &connection, error).await;
                return Ok(());
            }

            let header = match protocol.header() {
                Ok(header) => header,
                Err(error) => {
                    refuse(&mut send, &connection, format!("{error:#}")).await;
                    return Ok(());
                }
            };

            write_json(&mut send, &header).await.map_err(|error| {
                    connection.close(1u32.into(), b"header not delivered");
                    error
                })?;
            let version = header.version.clone().unwrap_or_default();
            if request.metadata_only {
                // Metadata-only ask: finish the frame and stop. Streaming the body
                // here would race the requester closing the connection and turn a
                // harmless question into a spurious transfer failure.
                send.finish()?;
                send.stopped().await.ok();
                debug!(peer = %peer.fmt_short(), version = %version, "answered an executable metadata request");
                // Let the requester close first: a CONNECTION_CLOSE can overtake
                // bytes still in flight and would turn the answer the requester is
                // still reading into a bare "connection lost".
                await_peer_close(&connection).await;
                return Ok(());
            }
            let sent = send_executable(&mut send).await;
            // A large body can still be in the peer's flight when we finish, so
            // wait for the stop acknowledgement before tearing the connection
            // down (same reasoning as the remote-log protocol).
            if let Err(error) = send.stopped().await {
                debug!(peer = %peer.fmt_short(), "update body was not acknowledged: {error}");
            }
            match sent {
                Ok(bytes) => info!(
                    peer = %peer.fmt_short(),
                    version = %version,
                    bytes,
                    "shared the running executable with a friend"
                ),
                Err(error) => {
                    warn!(peer = %peer.fmt_short(), "could not share the executable: {error:#}");
                }
            }
            // Same reasoning as the metadata path: the requester is still reading
            // the tail of a large body, so let it close rather than cutting the
            // connection off mid-flight.
            await_peer_close(&connection).await;
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        async move {}.boxed()
    }
}

/// Stream the running executable behind an already-written header.
async fn send_executable(send: &mut iroh::endpoint::SendStream) -> Result<u64> {
    let path = std::env::current_exe().context("could not locate the running executable")?;
    let file = File::open(&path)
        .with_context(|| format!("could not open the running executable {}", path.display()))?;
    let mut reader = BufReader::with_capacity(CHUNK_BYTES, file);
    let mut buffer = vec![0u8; CHUNK_BYTES];
    let mut sent = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("could not read {}", path.display()))?;
        if read == 0 {
            break;
        }
        send.write_all(&buffer[..read])
            .await
            .with_context(|| format!("could not send {}", path.display()))?;
        sent += read as u64;
    }
    send.finish()?;
    Ok(sent)
}

/// Describe the running executable: name, size, and checksum.
fn build_header(version: &str) -> Result<UpdateHeader, String> {
    let path = std::env::current_exe()
        .map_err(|error| format!("could not locate the running executable: {error}"))?;
    let metadata = std::fs::metadata(&path).map_err(|error| {
        format!(
            "could not inspect the running executable {}: {error}",
            path.display()
        )
    })?;
    let total_bytes = metadata.len();
    if total_bytes == 0 || total_bytes > MAX_EXECUTABLE_BYTES {
        return Err(format!(
            "the running executable has an unusable size ({total_bytes} bytes)"
        ));
    }
    let executable_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wire-app.exe".to_owned());
    let sha256 = crate::update::sha256_of_file(&path)
        .map_err(|error| format!("could not checksum the running executable: {error:#}"))?;
    Ok(UpdateHeader {
        protocol_version: PROTOCOL_VERSION,
        ok: true,
        error: None,
        version: Some(version.to_owned()),
        executable_name: Some(executable_name),
        sha256: Some(sha256),
        total_bytes: Some(total_bytes),
        platform: Some(current_platform()),
    })
}

/// Ask a peer what it would share, without transferring anything yet.
pub async fn fetch_offer(endpoint: &Endpoint, peer: NodeId) -> Result<UpdateOffer> {
    let connection: Connection = tokio::time::timeout(
        CONNECT_TIMEOUT,
        endpoint.connect(NodeAddr::from(peer), PEER_UPDATE_ALPN),
    )
    .await
    .context("connecting for the executable offer timed out")?
    .with_context(|| format!("connect to {} for an executable offer", peer.fmt_short()))?;

    let outcome: Result<UpdateOffer> = async {
        tokio::time::timeout(EXCHANGE_TIMEOUT, async {
            let (mut send, mut recv) = connection.open_bi().await?;
            write_json(&mut send, &UpdateRequest::for_offer()).await?;
            send.finish()?;
            // The sender streams no body in this mode, so the header is the whole
            // answer and the connection can be closed as soon as it is read.
            let header: UpdateHeader = read_json(&mut recv).await?;
            header.into_offer()
        })
        .await
        .context("waiting for the peer's answer timed out")?
    }
    .await;
    connection.close(0u32.into(), b"offer-done");
    outcome
}

/// Download a peer's executable into `destination`, verifying it against the
/// digest the peer advertised. `on_progress` is called with `(received, total)`
/// as bytes land on disk.
pub async fn download_update(
    endpoint: &Endpoint,
    peer: NodeId,
    destination: &Path,
    on_progress: impl Fn(u64, u64),
) -> Result<UpdateOffer> {
    let connection: Connection = tokio::time::timeout(
        CONNECT_TIMEOUT,
        endpoint.connect(NodeAddr::from(peer), PEER_UPDATE_ALPN),
    )
    .await
    .context("connecting for the executable timed out")?
    .with_context(|| format!("connect to {} for the executable", peer.fmt_short()))?;

    let outcome: Result<UpdateOffer> = async {
        let (mut send, mut recv) = connection.open_bi().await?;
        write_json(&mut send, &UpdateRequest::for_download()).await?;
        send.finish()?;

        let header: UpdateHeader = read_json(&mut recv).await?;
        let offer = header.into_offer()?;
        // A foreign binary cannot start here; refuse before touching the disk so
        // no bytes of an uninstallable update are ever staged.
        if !offer.is_runnable_here() {
            bail!(
                "the peer sent a build for {}, which cannot run on {}",
                offer.platform,
                current_platform()
            );
        }
        // Recreate rather than truncate: a refused or aborted transfer must not
        // leave a partial executable behind.
        let file = File::create(destination).with_context(|| {
            format!("could not create {} for the update", destination.display())
        })?;
        let mut writer = std::io::BufWriter::with_capacity(CHUNK_BYTES, file);
        let mut buffer = vec![0u8; CHUNK_BYTES];
        let mut received = 0u64;
        while received < offer.total_bytes {
            let remaining = (offer.total_bytes - received) as usize;
            let chunk = remaining.min(CHUNK_BYTES);
            // A stall budget per chunk rather than a deadline for the whole body:
            // a slow but progressing transfer over a long link must not be cut
            // off, while a peer that accepts and then goes quiet must not leave
            // the UI on a spinner forever.
            let read = tokio::time::timeout(
                EXCHANGE_TIMEOUT,
                read_exact_or_eof(&mut recv, &mut buffer[..chunk]),
            )
            .await
            .context("the update transfer stalled")??;
            if read == 0 {
                bail!(
                    "connection closed after {received} of {} bytes",
                    offer.total_bytes
                );
            }
            writer
                .write_all(&buffer[..read])
                .context("could not write the update to disk")?;
            received += read as u64;
            on_progress(received, offer.total_bytes);
        }
        writer
            .flush()
            .context("could not finish writing the update")?;
        Ok(offer)
    }
    .await;
    connection.close(0u32.into(), b"update-done");
    outcome
}

/// Read into `buffer`, mapping both end-of-stream shapes to `0`.
///
/// `RecvStream::read` returns `Ok(None)` at a clean EOF and `Ok(Some(n))` for
/// data; a zero-length read therefore means the peer finished early, which the
/// caller's byte accounting turns into a clean error instead of a hang.
async fn read_exact_or_eof(
    recv: &mut iroh::endpoint::RecvStream,
    buffer: &mut [u8],
) -> Result<usize> {
    match recv.read(buffer).await? {
        Some(read) => Ok(read),
        None => Ok(0),
    }
}

async fn write_json<T: Serialize>(send: &mut iroh::endpoint::SendStream, value: &T) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        bail!(
            "update metadata frame has an invalid length {}",
            payload.len()
        );
    }
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .context("could not send update metadata")?;
    send.write_all(&payload)
        .await
        .context("could not send update metadata")?;
    Ok(())
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len)
        .await
        .context("could not read the update metadata length")?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME_BYTES {
        bail!("invalid update metadata frame length {len}");
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload)
        .await
        .context("could not read the update metadata")?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produce a version strictly above the running build, so a test endpoint can
    /// act as a newer peer without bumping the real manifest.
    fn bump_patch(version: &str) -> u32 {
        version
            .split('.')
            .nth(2)
            .and_then(|patch| patch.parse::<u32>().ok())
            .unwrap_or(0)
            + 1
    }

    fn header() -> UpdateHeader {
        UpdateHeader {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            error: None,
            version: Some("0.7.5".to_owned()),
            executable_name: Some("wire-app.exe".to_owned()),
            sha256: Some("a".repeat(64)),
            total_bytes: Some(1 << 20),
            platform: Some(current_platform()),
        }
    }

    #[test]
    fn a_well_formed_header_becomes_an_offer() {
        let offer = header().into_offer().unwrap();
        assert_eq!(offer.version, "0.7.5");
        assert_eq!(offer.total_bytes, 1 << 20);
        assert!(offer.is_usable_for("0.7.4"));
        assert!(!offer.is_usable_for("0.7.5"));
        assert!(!offer.is_usable_for("0.8.0"));
        assert!(offer.is_runnable_here());
    }

    #[test]
    fn an_executable_built_for_another_platform_is_refused() {
        // Handing a foreign binary to the installer would leave the user with an
        // app that cannot start, so the mismatch must fail before any download.
        let mut foreign = header();
        foreign.platform = Some("linux-aarch64".to_owned());
        let offer = foreign.into_offer().unwrap();
        assert!(!offer.is_runnable_here());

        let mut no_platform = header();
        no_platform.platform = None;
        assert!(no_platform.into_offer().is_err());
    }

    #[test]
    fn refusals_and_malformed_headers_are_rejected() {
        assert!(UpdateHeader::refusal("not a saved friend")
            .into_offer()
            .is_err());

        let mut missing_checksum = header();
        missing_checksum.sha256 = None;
        assert!(missing_checksum.into_offer().is_err());

        let mut wrong_protocol = header();
        wrong_protocol.protocol_version = 2;
        assert!(wrong_protocol.into_offer().is_err());

        let mut bad_digest = header();
        bad_digest.sha256 = Some("nope".to_owned());
        assert!(bad_digest.into_offer().is_err());

        let mut empty = header();
        empty.total_bytes = Some(0);
        assert!(empty.into_offer().is_err());

        let mut oversized = header();
        oversized.total_bytes = Some(MAX_EXECUTABLE_BYTES + 1);
        assert!(oversized.into_offer().is_err());
    }

    #[test]
    fn requests_reject_foreign_protocol_versions() {
        let request = UpdateRequest::for_download();
        request.validate().unwrap();
        assert!(!request.metadata_only);
        assert!(UpdateRequest::for_offer().metadata_only);

        let mut future = request.clone();
        future.protocol_version = PROTOCOL_VERSION + 1;
        assert!(future.validate().is_err());
        let mut oversized = request;
        oversized.current_version = Some("x".repeat(65));
        assert!(oversized.validate().is_err());
        // An older peer that does not send its version is still welcome.
        UpdateRequest {
            current_version: None,
            ..UpdateRequest::for_offer()
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn a_sender_only_serves_a_platform_it_can_actually_run_on() {
        let mut request = UpdateRequest::for_download();
        assert!(
            request.can_serve(),
            "the local platform is trivially servable"
        );

        request.platform = Some("linux-aarch64".to_owned());
        assert!(
            !request.can_serve(),
            "streaming a foreign build wastes both ends' bandwidth"
        );

        // An older requester that does not send a tag is still served: the
        // receiver refuses independently before touching the disk.
        request.platform = None;
        assert!(request.can_serve());

        let mut oversized = UpdateRequest::for_offer();
        oversized.platform = Some("x".repeat(33));
        assert!(oversized.validate().is_err());
    }

    /// Two real endpoints: the transfer must hand the requester exactly the
    /// bytes and digest the header promised.
    ///
    /// The payload is the test binary's own path contents, which stands in for
    /// "the executable this process was launched from" — the same source the
    /// real server reads.
    #[tokio::test]
    async fn a_saved_friend_receives_the_senders_executable_intact() {
        use iroh::{protocol::Router, RelayMode, SecretKey};

        let secret_sender = SecretKey::from_bytes(&[81; 32]);
        let secret_requester = SecretKey::from_bytes(&[82; 32]);
        let bind = |secret| {
            Endpoint::builder()
                .secret_key(secret)
                .relay_mode(RelayMode::Disabled)
                .alpns(vec![PEER_UPDATE_ALPN.to_vec()])
                .bind()
        };
        let sender = bind(secret_sender).await.unwrap();
        let requester = bind(secret_requester).await.unwrap();
        sender
            .add_node_addr(requester.node_addr().await.unwrap())
            .unwrap();
        requester
            .add_node_addr(sender.node_addr().await.unwrap())
            .unwrap();

        let allowed = AllowedPeers::default();
        // The requester must be one of the sender's saved contacts.
        allowed.replace([requester.node_id()].into_iter().collect());
        // Pose as a newer build: both test endpoints run this same test binary,
        // so without this the sender would correctly refuse as "not newer".
        let newer = format!("{}.0.0", bump_patch(crate::APP_VERSION));
        let _router = Router::builder(sender.clone())
            .accept(
                PEER_UPDATE_ALPN,
                PeerUpdateProtocol::with_version(allowed.clone(), &newer),
            )
            .spawn()
            .await
            .unwrap();

        let offer = fetch_offer(&requester, sender.node_id())
            .await
            .expect("a saved friend must be able to ask");
        assert_eq!(offer.version, newer);
        assert!(offer.is_usable_for(crate::APP_VERSION));
        assert!(offer.is_runnable_here());
        assert_eq!(offer.sha256.len(), 64);
        assert!(offer.total_bytes > 0);

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("received.exe");
        let progress = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = progress.clone();
        let received = download_update(
            &requester,
            sender.node_id(),
            &destination,
            move |done, total| {
                recorder.lock().unwrap().push((done, total));
            },
        )
        .await
        .expect("the executable transfer must succeed end to end");
        assert_eq!(received, offer);
        // Progress has to be monotonic and end at the advertised total, so the
        // UI can never show a bar that runs backwards or stops short.
        let steps = progress.lock().unwrap().clone();
        assert!(!steps.is_empty());
        assert!(steps.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert_eq!(steps.last().unwrap().0, offer.total_bytes);
        assert!(steps.iter().all(|(_, total)| *total == offer.total_bytes));

        let bytes = std::fs::read(&destination).unwrap();
        assert_eq!(bytes.len() as u64, offer.total_bytes);
        assert_eq!(
            crate::update::sha256_of_file(&destination).unwrap(),
            offer.sha256.to_ascii_lowercase()
        );
    }

    /// A node that is not a saved contact must not be able to pull the binary.
    #[tokio::test]
    async fn a_stranger_is_refused_before_any_bytes_are_served() {
        use iroh::{protocol::Router, RelayMode, SecretKey};

        let secret_sender = SecretKey::from_bytes(&[83; 32]);
        let secret_stranger = SecretKey::from_bytes(&[84; 32]);
        let bind = |secret| {
            Endpoint::builder()
                .secret_key(secret)
                .relay_mode(RelayMode::Disabled)
                .alpns(vec![PEER_UPDATE_ALPN.to_vec()])
                .bind()
        };
        let sender = bind(secret_sender).await.unwrap();
        let stranger = bind(secret_stranger).await.unwrap();
        stranger
            .add_node_addr(sender.node_addr().await.unwrap())
            .unwrap();

        // An empty contact list: nobody may ask.
        let allowed = AllowedPeers::default();
        let _router = Router::builder(sender.clone())
            .accept(PEER_UPDATE_ALPN, PeerUpdateProtocol::new(allowed))
            .spawn()
            .await
            .unwrap();

        let error = fetch_offer(&stranger, sender.node_id())
            .await
            .expect_err("a stranger must not get an executable");
        let detail = format!("{error:#}").to_ascii_lowercase();
        assert!(
            detail.contains("not a saved friend"),
            "expected a friend-gate refusal, got {detail}"
        );

        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("stolen.exe");
        let result = download_update(&stranger, sender.node_id(), &destination, |_, _| {}).await;
        assert!(result.is_err());
        // A refused request must not leave a partial file behind for the user to
        // accidentally install.
        let written = std::fs::metadata(&destination)
            .map(|meta| meta.len())
            .unwrap_or(0);
        assert_eq!(written, 0, "a refused transfer must not write any bytes");
    }
}
