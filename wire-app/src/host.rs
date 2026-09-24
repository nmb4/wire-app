//! Process-lifetime ownership for the Wire service and its presenter clients.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

use fs2::FileExt;

use crate::runtime;

/// Failure to become the one per-user Wire application host.
#[derive(Debug)]
pub enum StartError {
    AlreadyRunning,
    Other(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => formatter.write_str("another Wire instance is already running"),
            Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StartError {}

/// Owns the background service independently from any egui application object.
pub struct ApplicationHost {
    _instance_lock: File,
    worker: runtime::WorkerHandle,
}

/// A presenter-side connection to the process-owned service.
pub struct ServiceClient {
    inner: runtime::ServiceClient,
    activation_path: PathBuf,
}

impl ApplicationHost {
    pub fn start() -> Result<(Self, ServiceClient), StartError> {
        let config_dir = wire::net::config_dir()
            .ok_or_else(|| StartError::Other("Wire config directory is unavailable".to_owned()))?;
        fs::create_dir_all(&config_dir)
            .map_err(|error| StartError::Other(format!("create Wire config directory: {error}")))?;
        let lock_path = config_dir.join("application.lock");
        let instance_lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| StartError::Other(format!("open Wire instance lock: {error}")))?;
        if let Err(error) = instance_lock.try_lock_exclusive() {
            let already_running = error.kind() == std::io::ErrorKind::WouldBlock
                || matches!(error.raw_os_error(), Some(32 | 33));
            return Err(if already_running {
                StartError::AlreadyRunning
            } else {
                StartError::Other(format!("lock Wire instance: {error}"))
            });
        }

        let activation_path = config_dir.join("activation.request");
        // A request left by a crashed process must not make a new instance
        // immediately steal focus.
        let _ = fs::remove_file(&activation_path);
        let (worker, client) = runtime::spawn();
        Ok((
            Self {
                _instance_lock: instance_lock,
                worker,
            },
            ServiceClient {
                inner: client,
                activation_path,
            },
        ))
    }

    /// Asks an already-running host to reveal its presenter window.
    pub fn request_activation() -> Result<(), String> {
        let config_dir = wire::net::config_dir()
            .ok_or_else(|| "Wire config directory is unavailable".to_owned())?;
        fs::create_dir_all(&config_dir)
            .map_err(|error| format!("create Wire config directory: {error}"))?;
        fs::write(
            config_dir.join("activation.request"),
            std::process::id().to_string(),
        )
        .map_err(|error| format!("request Wire activation: {error}"))
    }

    /// Gracefully stops networking, calls, capture, and background work.
    pub fn shutdown(self) {
        self.worker.shutdown();
    }
}

impl ServiceClient {
    pub(crate) fn command_sender(&self) -> &async_channel::Sender<runtime::Command> {
        self.inner.command_sender()
    }

    pub(crate) fn try_recv(&self) -> Result<runtime::Event, async_channel::TryRecvError> {
        self.inner.try_recv()
    }

    pub(crate) fn set_update_callback(&self, callback: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.inner.set_update_callback(callback);
    }

    pub(crate) fn set_presenter_active(&self, active: bool) {
        self.inner.set_presenter_active(active);
    }

    pub(crate) fn discard_buffered_media_events(&self) -> usize {
        self.inner.discard_buffered_media_events()
    }

    pub(crate) fn activation_path(&self) -> PathBuf {
        self.activation_path.clone()
    }

    pub(crate) fn take_activation_request(&self) -> bool {
        match fs::remove_file(&self.activation_path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                tracing::warn!("could not remove Wire activation request: {error}");
                false
            }
        }
    }
}
