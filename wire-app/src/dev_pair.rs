use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::NodeId;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tracing::info;

const SESSION_ENV: &str = "WIRE_DEV_PAIR_SESSION";

#[derive(Serialize, Deserialize)]
struct RendezvousRecord {
    pid: u32,
    node_id: String,
}

pub struct DevPairState {
    session: String,
    rendezvous_path: PathBuf,
    owns_rendezvous: bool,
}

impl DevPairState {
    pub fn from_env() -> Option<Self> {
        let session = std::env::var(SESSION_ENV).ok()?;
        let session = sanitize_session(&session);
        let rendezvous_path = std::env::temp_dir()
            .join("wire")
            .join("dev-pairs")
            .join(format!("{session}.json"));
        Some(Self {
            session,
            rendezvous_path,
            owns_rendezvous: false,
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Registers this instance. The first process becomes the listener; the
    /// second receives the first process's node id and initiates exactly one call.
    pub fn register(&mut self, own_node_id: NodeId) -> Result<Option<NodeId>> {
        if let Some(parent) = self.rendezvous_path.parent() {
            std::fs::create_dir_all(parent).context("creating dev-pair rendezvous directory")?;
        }

        for _ in 0..4 {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.rendezvous_path)
            {
                Ok(mut file) => {
                    let record = RendezvousRecord {
                        pid: std::process::id(),
                        node_id: own_node_id.to_string(),
                    };
                    serde_json::to_writer(&mut file, &record)
                        .context("writing dev-pair rendezvous")?;
                    file.flush().context("flushing dev-pair rendezvous")?;
                    self.owns_rendezvous = true;
                    info!(
                        "dev pair '{}' waiting as rendezvous owner ({})",
                        self.session,
                        own_node_id.fmt_short()
                    );
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(record) = read_record_with_retry(&self.rendezvous_path)? {
                        if record.pid == std::process::id() {
                            return Ok(None);
                        }
                        if process_is_alive(record.pid) {
                            let peer = NodeId::from_str(&record.node_id)
                                .context("parsing dev-pair peer node id")?;
                            if peer != own_node_id {
                                info!(
                                    "dev pair '{}' found peer {} in process {}",
                                    self.session,
                                    peer.fmt_short(),
                                    record.pid
                                );
                                return Ok(Some(peer));
                            }
                        }
                    }

                    // A crashed process can leave a stale record. Remove it and
                    // retry the create-new election without touching live records.
                    let _ = std::fs::remove_file(&self.rendezvous_path);
                }
                Err(error) => return Err(error).context("opening dev-pair rendezvous"),
            }
        }

        anyhow::bail!("could not elect a dev-pair rendezvous owner")
    }
}

impl Drop for DevPairState {
    fn drop(&mut self) {
        if !self.owns_rendezvous {
            return;
        }
        let Ok(contents) = std::fs::read(&self.rendezvous_path) else {
            return;
        };
        let Ok(record) = serde_json::from_slice::<RendezvousRecord>(&contents) else {
            return;
        };
        if record.pid == std::process::id() {
            let _ = std::fs::remove_file(&self.rendezvous_path);
        }
    }
}

fn read_record_with_retry(path: &std::path::Path) -> Result<Option<RendezvousRecord>> {
    for _ in 0..20 {
        match std::fs::read(path) {
            Ok(contents) => match serde_json::from_slice(&contents) {
                Ok(record) => return Ok(Some(record)),
                Err(_) => thread::sleep(Duration::from_millis(25)),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading dev-pair rendezvous"),
        }
    }
    Ok(None)
}

fn process_is_alive(pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]));
    system.process(pid).is_some()
}

pub fn sanitize_session(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(48)
        .collect();
    if sanitized.is_empty() {
        "local".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_session;

    #[test]
    fn sanitizes_session_for_file_names() {
        assert_eq!(sanitize_session("local benchmark #1"), "localbenchmark1");
        assert_eq!(sanitize_session("../"), "local");
        assert_eq!(sanitize_session("a_b-c"), "a_b-c");
    }
}
