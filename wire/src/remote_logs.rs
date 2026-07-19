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
    time::SystemTime,
};

use anyhow::{bail, Context, Result};
use iroh::{endpoint::Connection, protocol::ProtocolHandler, Endpoint, NodeAddr, NodeId};
use n0_future::{boxed::BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

pub const LOGS_ALPN: &[u8] = b"wire/logs/1";
const LOG_DIR_NAME: &str = "wire";
const LOG_FILE_PREFIX: &str = "wire-app";
const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const HARD_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

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
}

impl LogsRequest {
    pub fn fetch_latest(max_bytes: u64) -> Self {
        Self {
            version: 1,
            kind: "fetch-latest".to_owned(),
            max_bytes: Some(max_bytes),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported logs protocol version {}", self.version);
        }
        if self.kind != "fetch-latest" {
            bail!("unsupported logs request kind {}", self.kind);
        }
        Ok(())
    }
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
    let max_bytes = max_bytes.clamp(1, HARD_MAX_BYTES);
    let connection: Connection = endpoint
        .connect(NodeAddr::from(peer), LOGS_ALPN)
        .await
        .with_context(|| format!("connect to {} for logs", peer.fmt_short()))?;
    let (mut send, mut recv) = connection.open_bi().await?;
    let request = LogsRequest::fetch_latest(max_bytes);
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
    let mut bytes = vec![0u8; body_len as usize];
    if body_len > 0 {
        recv.read_exact(&mut bytes).await?;
    }
    if meta.sent_bytes != body_len {
        warn!(
            meta_sent = meta.sent_bytes,
            body_len,
            "log meta sent_bytes mismatch; using body length"
        );
    }
    Ok(FetchedLogs { meta, bytes })
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

    let Some(path) = resolve_latest_log_path() else {
        write_error_meta(send, our_node_id, "no local wire-app log file found".to_owned()).await?;
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
        node_id: our_node_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        pid: std::process::id(),
        total_file_bytes,
        sent_bytes: bytes.len() as u64,
        truncated,
    };
    write_json(send, &meta).await?;
    send.write_all(&(bytes.len() as u64).to_be_bytes()).await?;
    if !bytes.is_empty() {
        send.write_all(&bytes).await?;
    }
    send.finish()?;
    info!(
        path = %path.display(),
        sent_bytes = meta.sent_bytes,
        truncated,
        "served remote log fetch"
    );
    Ok(())
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
        node_id: our_node_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        pid: std::process::id(),
        total_file_bytes: 0,
        sent_bytes: 0,
        truncated: false,
    };
    write_json(send, &meta).await?;
    send.write_all(&0u64.to_be_bytes()).await?;
    send.finish()?;
    Ok(())
}

async fn write_json<T: Serialize>(
    send: &mut iroh::endpoint::SendStream,
    value: &T,
) -> Result<()> {
    let payload = serde_json::to_vec(value)?;
    if payload.len() > MAX_REQUEST_BYTES {
        bail!("logs protocol JSON exceeds safety cap");
    }
    send.write_all(&(payload.len() as u32).to_be_bytes()).await?;
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
    let mut file = fs::File::open(path)
        .with_context(|| format!("open log file {}", path.display()))?;
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
}
