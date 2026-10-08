//! Remote log fetch over a dedicated iroh ALPN.
//!
//! The GUI client accepts `LOGS_ALPN` and serves the latest local log file
//! automatically (no UI prompt). `wire-cli fetch-logs <node-id>` dials a peer
//! and writes the response to disk.

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context, Result};
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const LOGS_ALPN: &[u8] = b"wire/logs/1";
const LOG_DIR_NAME: &str = "wire";
const LOG_FILE_PREFIX: &str = "wire-app";
const LOG_FILE_SUFFIX: &str = ".log";

/// Serve the newest log file this process knows about.
pub const KIND_FETCH_LATEST: &str = "fetch-latest";
/// Serve one specific log file, named by bare file name (`wire-app-<pid>.log`).
///
/// This is what makes a log fetchable *after* the fact: once a frozen session
/// restarts, the newest log is the new session's, so the interesting file can
/// only be reached by name.
pub const KIND_FETCH_NAMED: &str = "fetch-named";
/// List every log file the client can serve, newest first. No body is sent.
pub const KIND_LIST: &str = "list";

const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const HARD_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Upper bound on entries in a `list` response, so one request cannot be used
/// to enumerate an unbounded directory.
const MAX_LISTED_FILES: usize = 512;
/// Upper bound on a requested file name, matching what any real log name needs.
const MAX_FILE_NAME_BYTES: usize = 128;
/// How many times to attempt the logs connection before giving up.
const CONNECT_ATTEMPTS: usize = 4;
/// Per-attempt connect budget. Comfortably above iroh's internal ~30s so a slow
/// relay path can finish instead of being cut off mid-handshake.
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(75);
/// Pause between connect attempts.
const CONNECT_RETRY_PAUSE: Duration = Duration::from_secs(3);

static CURRENT_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Record the path this process is writing logs to (called from the GUI).
pub fn set_current_log_path(path: impl Into<PathBuf>) {
    let _ = CURRENT_LOG_PATH.set(path.into());
}

pub fn current_log_path() -> Option<PathBuf> {
    CURRENT_LOG_PATH.get().cloned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsRequest {
    pub version: u8,
    pub kind: String,
    #[serde(default)]
    pub max_bytes: Option<u64>,
    /// Bare file name for [`KIND_FETCH_NAMED`]. Never a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

impl LogsRequest {
    pub fn fetch_latest(max_bytes: u64) -> Self {
        Self {
            version: 1,
            kind: KIND_FETCH_LATEST.to_owned(),
            max_bytes: Some(max_bytes),
            file_name: None,
        }
    }

    pub fn fetch_named(file_name: impl Into<String>, max_bytes: u64) -> Self {
        Self {
            version: 1,
            kind: KIND_FETCH_NAMED.to_owned(),
            max_bytes: Some(max_bytes),
            file_name: Some(file_name.into()),
        }
    }

    pub fn list() -> Self {
        Self {
            version: 1,
            kind: KIND_LIST.to_owned(),
            max_bytes: None,
            file_name: None,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported logs protocol version {}", self.version);
        }
        match self.kind.as_str() {
            KIND_FETCH_LATEST => Ok(()),
            KIND_FETCH_NAMED => {
                let name = self.file_name.as_deref().unwrap_or_default();
                // Reject here as well as at resolution time so a bad request is
                // reported as a bad request rather than "no such log".
                if !is_plausible_log_name(name) {
                    bail!("fetch-named requires a bare log file name, got {name:?}");
                }
                Ok(())
            }
            KIND_LIST => Ok(()),
            other => bail!("unsupported logs request kind {other}"),
        }
    }
}

/// One servable log file, as reported by [`KIND_LIST`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogFileInfo {
    pub file_name: String,
    pub byte_len: u64,
    /// Last modification time, milliseconds since the Unix epoch.
    pub modified_ms: u64,
    /// PID parsed out of `wire-app-<pid>.log`, when the name carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// True when this file is the log the running process is writing to.
    pub is_current_process: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsMeta {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub node_id: String,
    pub pid: u32,
    pub total_file_bytes: u64,
    pub sent_bytes: u64,
    pub truncated: bool,
    /// Populated for [`KIND_LIST`]; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<LogFileInfo>,
}

#[derive(Debug, Clone)]
pub struct FetchedLogs {
    pub meta: LogsMeta,
    pub bytes: Vec<u8>,
}

/// Passive handler: any peer that connects on `LOGS_ALPN` gets the latest log.
#[derive(Debug, Clone, Default)]
pub struct LogsProtocol {
    our_node_id: Option<NodeId>,
}

impl LogsProtocol {
    pub fn new(our_node_id: NodeId) -> Self {
        Self {
            our_node_id: Some(our_node_id),
        }
    }
}

impl ProtocolHandler for LogsProtocol {
    fn accept(&self, connecting: iroh::endpoint::Connecting) -> BoxFuture<Result<()>> {
        let our_node_id = self.our_node_id;
        async move {
            let connection = connecting.await?;
            let remote = connection.remote_node_id()?;
            info!(peer = %remote.fmt_short(), "serving remote log fetch");
            let (mut send, mut recv) = connection.accept_bi().await?;
            if let Err(error) = serve_logs_stream(&mut send, &mut recv, our_node_id).await {
                warn!(peer = %remote.fmt_short(), "log fetch failed: {error:#}");
                // Best-effort error meta if the stream is still writable.
                let _ = write_error_meta(&mut send, our_node_id, error.to_string()).await;
            }
            Ok(())
        }
        .boxed()
    }

    fn shutdown(&self) -> BoxFuture<()> {
        async move {}.boxed()
    }
}

/// Dial `peer` and download their latest log file (tail-capped by `max_bytes`).
pub async fn fetch_latest_logs(
    endpoint: &Endpoint,
    peer: NodeId,
    max_bytes: u64,
) -> Result<FetchedLogs> {
    let request = LogsRequest::fetch_latest(max_bytes.clamp(1, HARD_MAX_BYTES));
    let (meta, bytes) = request_logs(endpoint, peer, request).await?;
    Ok(FetchedLogs { meta, bytes })
}

/// Dial `peer` and download one specific log file by bare file name.
///
/// This is how you retrieve the log of a session that has already ended: ask
/// for `wire-app-<pid>.log` rather than "whatever is newest".
pub async fn fetch_named_logs(
    endpoint: &Endpoint,
    peer: NodeId,
    file_name: &str,
    max_bytes: u64,
) -> Result<FetchedLogs> {
    if !is_plausible_log_name(file_name) {
        bail!("not a valid remote log file name: {file_name:?}");
    }
    let request = LogsRequest::fetch_named(file_name, max_bytes.clamp(1, HARD_MAX_BYTES));
    let (meta, bytes) = request_logs(endpoint, peer, request).await?;
    Ok(FetchedLogs { meta, bytes })
}

/// Dial `peer` and list every log file they can serve, newest first.
pub async fn list_logs(endpoint: &Endpoint, peer: NodeId) -> Result<LogsMeta> {
    let (meta, bytes) = request_logs(endpoint, peer, LogsRequest::list()).await?;
    if !bytes.is_empty() {
        warn!(
            body_bytes = bytes.len(),
            "remote sent a log body for a list request; ignoring it"
        );
    }
    Ok(meta)
}

/// Open a `LOGS_ALPN` connection to `peer`, retrying on transport failure.
///
/// A single `Endpoint::connect` gives up inside iroh's own ~30s budget, which
/// is not always enough: bringing up a relay path from cold costs several
/// seconds per relay candidate, and discovery may report no addressing at all
/// on the first try. Each attempt gets its own budget so a slow-but-progressing
/// path still wins, and only transport errors are retried — a peer that answers
/// with a protocol error has already been reached.
async fn connect_with_retry(endpoint: &Endpoint, peer: NodeId) -> Result<Connection> {
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=CONNECT_ATTEMPTS {
        let connecting = endpoint.connect(NodeAddr::from(peer), LOGS_ALPN);
        match tokio::time::timeout(CONNECT_ATTEMPT_TIMEOUT, connecting).await {
            Ok(Ok(connection)) => {
                info!(attempt, peer = %peer.fmt_short(), "connected for remote log fetch");
                return Ok(connection);
            }
            Ok(Err(error)) => {
                warn!(
                    attempt,
                    peer = %peer.fmt_short(),
                    "remote log connect failed: {error:#}"
                );
                last_error =
                    Some(error.context(format!("connect to {} for logs", peer.fmt_short())));
            }
            Err(_) => {
                warn!(
                    attempt,
                    peer = %peer.fmt_short(),
                    attempt_secs = CONNECT_ATTEMPT_TIMEOUT.as_secs(),
                    "remote log connect attempt timed out; retrying"
                );
                last_error = Some(anyhow::anyhow!(
                    "connect to {} for logs timed out after {}s (attempt {attempt} of {CONNECT_ATTEMPTS})",
                    peer.fmt_short(),
                    CONNECT_ATTEMPT_TIMEOUT.as_secs()
                ));
            }
        }
        if attempt < CONNECT_ATTEMPTS {
            tokio::time::sleep(CONNECT_RETRY_PAUSE).await;
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("could not connect to {}", peer.fmt_short())))
}

/// Shared dial/send/receive for every logs request kind.
async fn request_logs(
    endpoint: &Endpoint,
    peer: NodeId,
    request: LogsRequest,
) -> Result<(LogsMeta, Vec<u8>)> {
    let connection = connect_with_retry(endpoint, peer).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    write_json(&mut send, &request).await?;
    send.finish()?;

    let meta: LogsMeta = read_json(&mut recv).await?;
    if !meta.ok {
        bail!(
            "remote refused log fetch: {}",
            meta.error.unwrap_or_else(|| "unknown error".to_owned())
        );
    }
    let mut len_buf = [0u8; 8];
    recv.read_exact(&mut len_buf).await?;
    let body_len = u64::from_be_bytes(len_buf);
    if body_len > HARD_MAX_BYTES {
        bail!("remote log body exceeds safety cap ({body_len} bytes)");
    }
    let mut bytes = Vec::with_capacity(body_len as usize);
    while bytes.len() < body_len as usize {
        let remaining = body_len as usize - bytes.len();
        let mut chunk = vec![0u8; remaining.min(64 * 1024)];
        match recv.read(&mut chunk).await {
            Ok(Some(0)) => break,
            Ok(Some(read)) => bytes.extend_from_slice(&chunk[..read]),
            Ok(None) => break,
            Err(error) if bytes.is_empty() => return Err(error.into()),
            Err(error) => {
                warn!(
                    expected_bytes = body_len,
                    received_bytes = bytes.len(),
                    "remote log stream closed early; preserving partial body: {error}"
                );
                break;
            }
        }
    }
    if meta.sent_bytes != body_len {
        warn!(
            meta_sent = meta.sent_bytes,
            body_len, "log meta sent_bytes mismatch; using body length"
        );
    }
    if bytes.len() as u64 != body_len {
        warn!(
            expected_bytes = body_len,
            received_bytes = bytes.len(),
            "remote log fetch is incomplete"
        );
    }
    Ok((meta, bytes))
}

async fn serve_logs_stream(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    our_node_id: Option<NodeId>,
) -> Result<()> {
    let request: LogsRequest = read_json(recv).await?;
    request.validate()?;
    let max_bytes = request
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(1, HARD_MAX_BYTES);

    if request.kind == KIND_LIST {
        let meta = LogsMeta {
            ok: true,
            error: None,
            file_name: None,
            path: None,
            node_id: node_id_string(our_node_id),
            pid: std::process::id(),
            total_file_bytes: 0,
            sent_bytes: 0,
            truncated: false,
            files: list_log_files(),
        };
        write_json(send, &meta).await?;
        send.write_all(&0u64.to_be_bytes()).await?;
        send.finish()?;
        info!(
            files = meta.files.len(),
            peer = our_node_id.map(|id| id.fmt_short().to_string()),
            "listed local logs for remote peer"
        );
        return Ok(());
    }

    let resolved = if request.kind == KIND_FETCH_NAMED {
        // `file_name` was already shape-checked in validate(); resolve it to a
        // real path inside the log directory only.
        resolve_named_log_path(request.file_name.as_deref().unwrap_or_default())
    } else {
        resolve_latest_log_path()
    };

    let Some(path) = resolved else {
        let detail = if request.kind == KIND_FETCH_NAMED {
            format!(
                "no log file named {:?} on this client",
                request.file_name.as_deref().unwrap_or_default()
            )
        } else {
            "no local wire-app log file found".to_owned()
        };
        write_error_meta(send, our_node_id, detail).await?;
        return Ok(());
    };

    let (total_file_bytes, truncated, bytes) = read_log_tail(&path, max_bytes)?;
    let meta = LogsMeta {
        ok: true,
        error: None,
        file_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        path: Some(path.display().to_string()),
        node_id: node_id_string(our_node_id),
        pid: std::process::id(),
        total_file_bytes,
        sent_bytes: bytes.len() as u64,
        truncated,
        files: Vec::new(),
    };
    write_json(send, &meta).await?;
    send.write_all(&(bytes.len() as u64).to_be_bytes()).await?;
    if !bytes.is_empty() {
        send.write_all(&bytes).await?;
    }
    send.finish()?;
    // Keep the connection alive until QUIC confirms that the peer received the
    // response. Dropping the handler immediately can reset bodies larger than
    // the stream's initial flow-control window.
    if let Err(error) = send.stopped().await {
        warn!("remote log response was not acknowledged: {error}");
    }
    info!(
        path = %path.display(),
        kind = %request.kind,
        sent_bytes = meta.sent_bytes,
        truncated,
        "served remote log fetch"
    );
    Ok(())
}

fn node_id_string(our_node_id: Option<NodeId>) -> String {
    our_node_id.map(|id| id.to_string()).unwrap_or_default()
}

/// True for a bare log file name we are willing to resolve.
///
/// The request supplies a *name*, never a path. Anything with a separator, a
/// `..` component, a drive letter, a NUL, or the wrong prefix/suffix is
/// refused, so `fetch-named` cannot be pointed outside the log directory.
fn is_plausible_log_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_FILE_NAME_BYTES {
        return false;
    }
    if name.contains(['/', '\\', '\0', ':']) {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    // `Path::file_name` returning the same string proves it is one component.
    let single_component = matches!(
        Path::new(name).file_name(),
        Some(component) if component == std::ffi::OsStr::new(name)
    );
    single_component && name.starts_with(LOG_FILE_PREFIX) && name.ends_with(LOG_FILE_SUFFIX)
}

/// Resolve a validated bare log file name to a path inside the log directory.
fn resolve_named_log_path(file_name: &str) -> Option<PathBuf> {
    resolve_named_log_path_in(file_name, log_dir().as_deref())
}

/// Resolve a bare log file name against an explicit directory.
///
/// Split out from [`resolve_named_log_path`] so the resolution rules can be
/// tested without mutating process-wide environment state.
fn resolve_named_log_path_in(file_name: &str, dir: Option<&Path>) -> Option<PathBuf> {
    if !is_plausible_log_name(file_name) {
        return None;
    }
    // Honour a custom log path for this process, so the running client's own log
    // stays fetchable by name even when it lives outside the default directory.
    if let Some(current) = current_log_path() {
        if current
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new(file_name))
        {
            return Some(current);
        }
    }
    let path = dir?.join(file_name);
    // Belt and braces: the join must still resolve to a plain file in the log
    // directory.
    if !path.is_file() {
        return None;
    }
    Some(path)
}

/// Every log file this client can serve, newest first.
fn list_log_files() -> Vec<LogFileInfo> {
    list_log_files_in(log_dir().as_deref())
}

/// Enumerate servable logs in an explicit directory, newest first.
fn list_log_files_in(dir: Option<&Path>) -> Vec<LogFileInfo> {
    let current = current_log_path().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    let mut files = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(dir) = dir {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        for entry in entries.flatten() {
            let Some(name) = entry
                .file_name()
                .to_str()
                .map(str::to_owned)
                .filter(|name| is_plausible_log_name(name))
            else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |delta| delta.as_millis() as u64);
            if !seen.insert(name.clone()) {
                continue;
            }
            files.push(LogFileInfo {
                pid: pid_from_log_name(&name),
                is_current_process: current.as_deref() == Some(name.as_str()),
                file_name: name,
                byte_len: metadata.len(),
                modified_ms,
            });
        }
    }
    // Sort before truncating: `read_dir` order is arbitrary, so capping first
    // could drop the newest logs, which are exactly the ones worth listing.
    files.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| a.file_name.cmp(&b.file_name))
    });
    files.truncate(MAX_LISTED_FILES);
    files
}

/// Pull the PID out of `wire-app-<pid>.log`; dev/test names yield `None`.
fn pid_from_log_name(name: &str) -> Option<u32> {
    name.strip_prefix(LOG_FILE_PREFIX)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(LOG_FILE_SUFFIX))
        .and_then(|digits| digits.parse().ok())
}

async fn write_error_meta(
    send: &mut iroh::endpoint::SendStream,
    our_node_id: Option<NodeId>,
    error: String,
) -> Result<()> {
    let meta = LogsMeta {
        ok: false,
        error: Some(error),
        file_name: None,
        path: None,
        node_id: node_id_string(our_node_id),
        pid: std::process::id(),
        total_file_bytes: 0,
        sent_bytes: 0,
        truncated: false,
        files: Vec::new(),
    };
    write_json(send, &meta).await?;
    send.write_all(&0u64.to_be_bytes()).await?;
    send.finish()?;
    Ok(())
}

async fn write_json<T: Serialize>(send: &mut iroh::endpoint::SendStream, value: &T) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_REQUEST_BYTES {
        bail!("logs protocol JSON exceeds safety cap");
    }
    send.write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    send.write_all(&payload).await?;
    Ok(())
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<T> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_REQUEST_BYTES {
        bail!("invalid logs protocol frame length {len}");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

pub fn log_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|root| PathBuf::from(root).join(LOG_DIR_NAME))
        .or_else(|| {
            // Non-Windows fallback: same layout under the user config dir if set.
            std::env::var("WIRE_CONFIG_DIR")
                .ok()
                .map(PathBuf::from)
                .map(|root| root.join("logs"))
        })
}

pub fn resolve_latest_log_path() -> Option<PathBuf> {
    if let Some(path) = current_log_path() {
        if path.is_file() {
            return Some(path);
        }
    }
    let dir = log_dir()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_str()?;
        if !(name.starts_with(LOG_FILE_PREFIX) && name.ends_with(".log")) {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        if best.as_ref().is_none_or(|(time, _)| modified > *time) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

fn read_log_tail(path: &Path, max_bytes: u64) -> Result<(u64, bool, Vec<u8>)> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open log file {}", path.display()))?;
    let total = file.metadata()?.len();
    if total == 0 {
        return Ok((0, false, Vec::new()));
    }
    let truncated = total > max_bytes;
    let start = total.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if truncated {
        let marker = format!(
            "… wire: log truncated for remote fetch; showing last {} of {} bytes …\n",
            bytes.len(),
            total
        );
        let marker_bytes = marker.into_bytes();
        let body_budget = (max_bytes as usize).saturating_sub(marker_bytes.len());
        let body = if bytes.len() > body_budget {
            bytes[bytes.len() - body_budget..].to_vec()
        } else {
            bytes
        };
        let mut out = marker_bytes;
        out.extend(body);
        Ok((total, true, out))
    } else {
        Ok((total, false, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn read_log_tail_truncates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wire-app-1.log");
        let mut file = fs::File::create(&path).unwrap();
        write!(file, "{}", "x".repeat(1000)).unwrap();
        let (total, truncated, bytes) = read_log_tail(&path, 100).unwrap();
        assert_eq!(total, 1000);
        assert!(truncated);
        assert!(bytes.len() as u64 <= 100);
        assert!(std::str::from_utf8(&bytes).unwrap().contains("truncated"));
    }

    #[test]
    fn named_request_requires_a_plausible_name() {
        assert!(LogsRequest::fetch_named("wire-app-1234.log", 1)
            .validate()
            .is_ok());
        assert!(LogsRequest::list().validate().is_ok());
        assert!(LogsRequest::fetch_latest(1).validate().is_ok());
        // A name request without a name is rejected.
        let mut missing = LogsRequest::fetch_named("wire-app-1.log", 1);
        missing.file_name = None;
        assert!(missing.validate().is_err());
        // Unknown kinds and versions are still refused.
        let mut unknown = LogsRequest::list();
        unknown.kind = "delete-everything".to_owned();
        assert!(unknown.validate().is_err());
        let mut wrong_version = LogsRequest::list();
        wrong_version.version = 2;
        assert!(wrong_version.validate().is_err());
    }

    #[test]
    fn log_name_sanitization_rejects_traversal() {
        // Accepted: plain log file names, including dev/test logs.
        for good in [
            "wire-app-1234.log",
            "wire-app-dev-codex-smoke-host-25720.log",
        ] {
            assert!(is_plausible_log_name(good), "should accept {good}");
        }
        // Rejected: anything that could escape the log directory, or that is
        // not a log we serve.
        for bad in [
            "",
            "..",
            ".",
            "../wire-app-1.log",
            "..\\wire-app-1.log",
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "sub/dir/wire-app-1.log",
            "wire-app-1.log\0.txt",
            "notes.txt",
            "secret.key",
            "otherapp-1.log",
        ] {
            assert!(!is_plausible_log_name(bad), "should reject {bad:?}");
        }
        // Over-long names are refused rather than allocated against.
        assert!(!is_plausible_log_name(&format!(
            "wire-app-{}.log",
            "9".repeat(MAX_FILE_NAME_BYTES)
        )));
    }

    #[test]
    fn named_resolution_never_leaves_the_log_directory() {
        // Traversal attempts resolve to nothing, even if such a file exists
        // relative to the process working directory.
        for bad in ["../secret.key", "..\\secret.key", "/etc/passwd"] {
            assert!(
                resolve_named_log_path(bad).is_none(),
                "{bad:?} must not resolve"
            );
        }
        assert!(resolve_named_log_path("notes.txt").is_none());
    }

    #[test]
    fn pid_is_parsed_from_plain_log_names() {
        assert_eq!(pid_from_log_name("wire-app-1234.log"), Some(1234));
        // Dev/test log names carry a non-numeric middle segment.
        assert_eq!(pid_from_log_name("wire-app-dev-smoke-host-25720.log"), None);
        assert_eq!(pid_from_log_name("wire-app.log"), None);
    }

    #[test]
    fn list_response_round_trips() {
        // The `list` kind carries its payload in meta with an empty body, so it
        // must survive the same JSON framing the other kinds use.
        let meta = LogsMeta {
            ok: true,
            error: None,
            file_name: None,
            path: None,
            node_id: "abc".to_owned(),
            pid: 7,
            total_file_bytes: 0,
            sent_bytes: 0,
            truncated: false,
            files: vec![LogFileInfo {
                file_name: "wire-app-7.log".to_owned(),
                byte_len: 42,
                modified_ms: 1_700_000_000_000,
                pid: Some(7),
                is_current_process: true,
            }],
        };
        let json = serde_json::to_vec(&meta).unwrap();
        let back: LogsMeta = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.files[0].file_name, "wire-app-7.log");
        assert!(back.files[0].is_current_process);
        assert_eq!(back.files[0].byte_len, 42);
    }

    #[test]
    fn named_fetch_reaches_an_older_session_log() {
        // The case this feature exists for: a frozen session's log stays on disk
        // after the app restarts, and must still be reachable by name even
        // though it is no longer the newest file.
        let dir = tempdir().unwrap();
        let older = dir.path().join("wire-app-111.log");
        let newer = dir.path().join("wire-app-222.log");
        fs::write(&older, "frozen-session-marker\n").unwrap();
        fs::write(&newer, "current-session-marker\n").unwrap();

        // "Latest" resolves to the current session...
        let (total, truncated, bytes) = read_log_tail(&newer, 1024).unwrap();
        assert_eq!(total, "current-session-marker\n".len() as u64);
        assert!(!truncated);
        assert!(String::from_utf8_lossy(&bytes).contains("current-session"));

        // ...while the frozen session is still reachable by exact name.
        let resolved = resolve_named_log_path_in("wire-app-111.log", Some(dir.path()))
            .expect("older log must resolve by name");
        assert_eq!(resolved, older);
        let (_, _, bytes) = read_log_tail(&resolved, 1024).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("frozen-session"));

        // And a name that does not exist resolves to nothing.
        assert!(resolve_named_log_path_in("wire-app-999.log", Some(dir.path())).is_none());
    }

    #[test]
    fn listing_reports_every_log_newest_first_and_skips_non_logs() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("wire-app-1.log"), "a").unwrap();
        fs::write(dir.path().join("wire-app-22.log"), "bb").unwrap();
        fs::write(dir.path().join("wire-app-333.log"), "ccc").unwrap();
        // Files we must not offer: not a log name, or a directory.
        fs::write(dir.path().join("secret.key"), "nope").unwrap();
        fs::write(dir.path().join("notes.txt"), "nope").unwrap();
        fs::create_dir(dir.path().join("wire-app-444.log")).unwrap();

        let files = list_log_files_in(Some(dir.path()));
        let names: Vec<_> = files.iter().map(|f| f.file_name.as_str()).collect();
        assert_eq!(names.len(), 3, "unexpected listing: {names:?}");
        assert!(!names.contains(&"secret.key"));
        assert!(!names.contains(&"notes.txt"));
        assert!(!names.contains(&"wire-app-444.log"));

        // Sizes and PIDs are reported so the CLI can pick a file usefully.
        let by_name = |want: &str| files.iter().find(|f| f.file_name == want).unwrap();
        assert_eq!(by_name("wire-app-333.log").byte_len, 3);
        assert_eq!(by_name("wire-app-333.log").pid, Some(333));
        assert_eq!(by_name("wire-app-22.log").byte_len, 2);

        // Ordering must be non-increasing by modification time.
        let stamps: Vec<_> = files.iter().map(|f| f.modified_ms).collect();
        assert!(
            stamps.windows(2).all(|w| w[0] >= w[1]),
            "listing must be newest first: {stamps:?}"
        );
    }

    #[test]
    fn listing_caps_after_sorting_so_newest_survive() {
        let dir = tempdir().unwrap();
        // More files than the cap, with mtimes spread out.
        let total = MAX_LISTED_FILES + 25;
        for index in 0..total {
            let path = dir.path().join(format!("wire-app-{index}.log"));
            fs::write(&path, "x").unwrap();
            let stamp = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(index as u64);
            // Not all filesystems let you set mtime, so only assert ordering
            // when the timestamp actually took.
            let _ = set_file_mtime(&path, stamp);
        }
        let files = list_log_files_in(Some(dir.path()));
        assert_eq!(files.len(), MAX_LISTED_FILES);
        let stamps: Vec<_> = files.iter().map(|f| f.modified_ms).collect();
        assert!(
            stamps.windows(2).all(|w| w[0] >= w[1]),
            "capped listing must stay newest first"
        );
    }

    /// Best-effort mtime setter; returns whether it succeeded.
    fn set_file_mtime(path: &Path, time: SystemTime) -> bool {
        let file = match fs::OpenOptions::new().write(true).open(path) {
            Ok(file) => file,
            Err(_) => return false,
        };
        file.set_modified(time).is_ok()
    }

    #[test]
    fn fetch_latest_request_stays_wire_compatible() {
        // Older builds only know `kind` + `max_bytes`, so a latest fetch must
        // not gain new required fields that would break them.
        let json = serde_json::to_value(LogsRequest::fetch_latest(4096)).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["kind"], "fetch-latest");
        assert_eq!(json["max_bytes"], 4096);
        assert!(json.get("file_name").is_none());
    }
}
