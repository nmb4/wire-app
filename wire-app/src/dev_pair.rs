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
const PEER_INDEX_ENV: &str = "WIRE_DEV_PAIR_INDEX";
const RENDEZVOUS_ATTEMPTS: usize = 100;
const RENDEZVOUS_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
struct RendezvousRecord {
    pid: u32,
    node_id: String,
}

pub struct DevPairState {
    session: String,
    peer_index: usize,
    rendezvous_dir: PathBuf,
    own_record_path: PathBuf,
    owns_record: bool,
}

impl DevPairState {
    pub fn from_env() -> Option<Self> {
        let session = sanitize_session(&std::env::var(SESSION_ENV).ok()?);
        let peer_index = std::env::var(PEER_INDEX_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let rendezvous_dir = std::env::temp_dir()
            .join("wire")
            .join("dev-pairs")
            .join(&session);
        Some(Self::new(session, peer_index, rendezvous_dir))
    }

    fn new(session: String, peer_index: usize, rendezvous_dir: PathBuf) -> Self {
        let own_record_path = rendezvous_dir.join(format!("peer-{peer_index}.json"));
        Self {
            session,
            peer_index,
            rendezvous_dir,
            own_record_path,
            owns_record: false,
        }
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Registers this instance and returns every earlier participant. Connecting
    /// each new participant to all lower indexes produces one full-mesh call
    /// without duplicate connections between any pair.
    pub fn register(&mut self, own_node_id: NodeId) -> Result<Vec<NodeId>> {
        std::fs::create_dir_all(&self.rendezvous_dir)
            .context("creating dev-call rendezvous directory")?;
        self.write_own_record(own_node_id)?;

        let mut peers = Vec::with_capacity(self.peer_index);
        for index in 0..self.peer_index {
            let path = self.rendezvous_dir.join(format!("peer-{index}.json"));
            let record = wait_for_live_record(&path)
                .with_context(|| format!("waiting for dev-call participant {index}"))?;
            let peer =
                NodeId::from_str(&record.node_id).context("parsing dev-call peer node id")?;
            if peer != own_node_id {
                peers.push(peer);
            }
        }

        info!(
            "dev call '{}' participant {} registered as {} with {} earlier peer(s)",
            self.session,
            self.peer_index,
            own_node_id.fmt_short(),
            peers.len()
        );
        Ok(peers)
    }

    fn write_own_record(&mut self, own_node_id: NodeId) -> Result<()> {
        for _ in 0..4 {
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&self.own_record_path)
            {
                Ok(mut file) => {
                    let record = RendezvousRecord {
                        pid: std::process::id(),
                        node_id: own_node_id.to_string(),
                    };
                    serde_json::to_writer(&mut file, &record)
                        .context("writing dev-call rendezvous")?;
                    file.flush().context("flushing dev-call rendezvous")?;
                    self.owns_record = true;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(record) = read_record_with_retry(&self.own_record_path)? {
                        if process_is_alive(record.pid) {
                            anyhow::bail!(
                                "dev-call participant index {} is already used by process {}",
                                self.peer_index,
                                record.pid
                            );
                        }
                    }
                    let _ = std::fs::remove_file(&self.own_record_path);
                }
                Err(error) => return Err(error).context("opening dev-call rendezvous"),
            }
        }
        anyhow::bail!(
            "could not register dev-call participant {}",
            self.peer_index
        )
    }
}

impl Drop for DevPairState {
    fn drop(&mut self) {
        if !self.owns_record {
            return;
        }
        let Ok(contents) = std::fs::read(&self.own_record_path) else {
            return;
        };
        let Ok(record) = serde_json::from_slice::<RendezvousRecord>(&contents) else {
            return;
        };
        if record.pid == std::process::id() {
            let _ = std::fs::remove_file(&self.own_record_path);
            let _ = std::fs::remove_dir(&self.rendezvous_dir);
        }
    }
}

fn wait_for_live_record(path: &std::path::Path) -> Result<RendezvousRecord> {
    for _ in 0..RENDEZVOUS_ATTEMPTS {
        if let Some(record) = read_record_with_retry(path)? {
            if process_is_alive(record.pid) {
                return Ok(record);
            }
        }
        thread::sleep(RENDEZVOUS_RETRY_DELAY);
    }
    anyhow::bail!("timed out waiting for {}", path.display())
}

fn read_record_with_retry(path: &std::path::Path) -> Result<Option<RendezvousRecord>> {
    for _ in 0..20 {
        match std::fs::read(path) {
            Ok(contents) => match serde_json::from_slice(&contents) {
                Ok(record) => return Ok(Some(record)),
                Err(_) => thread::sleep(Duration::from_millis(25)),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("reading dev-call rendezvous"),
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
    use super::{sanitize_session, DevPairState};

    #[test]
    fn sanitizes_session_for_file_names() {
        assert_eq!(sanitize_session("local benchmark #1"), "localbenchmark1");
        assert_eq!(sanitize_session("../"), "local");
        assert_eq!(sanitize_session("a_b-c"), "a_b-c");
    }

    #[test]
    fn three_participants_form_a_full_mesh() {
        let suffix = wire::net::generate_ephemeral_secret_key()
            .public()
            .fmt_short();
        let root = std::env::temp_dir().join(format!("wire-dev-call-test-{suffix}"));
        let node_ids: Vec<_> = (0..3)
            .map(|_| wire::net::generate_ephemeral_secret_key().public())
            .collect();
        let mut states: Vec<_> = (0..3)
            .map(|index| DevPairState::new("test".to_owned(), index, root.clone()))
            .collect();

        assert!(states[0].register(node_ids[0]).unwrap().is_empty());
        assert_eq!(states[1].register(node_ids[1]).unwrap(), vec![node_ids[0]]);
        assert_eq!(
            states[2].register(node_ids[2]).unwrap(),
            vec![node_ids[0], node_ids[1]]
        );
    }
}
