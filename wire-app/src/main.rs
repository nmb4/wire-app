#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use eframe::NativeOptions;
use tracing::info;
use tracing_subscriber::EnvFilter;
use wire_app::app::App;
use wire_app::window_frame;

const LOG_DIR_NAME: &str = "wire";
const LEGACY_LOG_DIR_NAME: &str = "callme";
const LOG_FILE_PREFIX: &str = "wire-app";
const DEV_PAIR_SESSION_ENV: &str = "WIRE_DEV_PAIR_SESSION";
const DEV_PAIR_INDEX_ENV: &str = "WIRE_DEV_PAIR_INDEX";
const DEV_CALL_PARTICIPANTS: usize = 3;
const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;
const RUNAWAY_LOG_BYTES: u64 = 64 * 1024 * 1024;
const STALE_LOG_AGE: Duration = Duration::from_secs(10 * 60);
const LOG_CAP_MARKER: &[u8] =
    b"\nwire: log size cap reached; further output from this process is suppressed\n";

struct CappedLogFile {
    file: fs::File,
    written: u64,
    capped: bool,
}

impl CappedLogFile {
    fn new(file: fs::File) -> Self {
        let written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Self {
            file,
            written,
            capped: false,
        }
    }

    fn mark_capped(&mut self) -> io::Result<()> {
        if !self.capped {
            self.file.write_all(LOG_CAP_MARKER)?;
            self.file.flush()?;
            self.capped = true;
        }
        Ok(())
    }
}

impl Write for CappedLogFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.capped {
            return Ok(buffer.len());
        }
        if self.written >= MAX_LOG_BYTES {
            self.mark_capped()?;
            return Ok(buffer.len());
        }

        let remaining = usize::try_from(MAX_LOG_BYTES - self.written).unwrap_or(usize::MAX);
        let accepted = remaining.min(buffer.len());
        self.file.write_all(&buffer[..accepted])?;
        self.written += accepted as u64;
        if accepted < buffer.len() {
            self.mark_capped()?;
        }
        // The cap deliberately behaves like a sink after accepting the prefix so
        // tracing does not repeatedly report an I/O failure.
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn dev_pair_config_dir(session: &str, peer_index: usize) -> PathBuf {
    std::env::temp_dir()
        .join("wire")
        .join("dev-pairs")
        .join(session)
        .join(format!("config-peer-{peer_index}"))
}

struct LaunchConfig {
    dev_pair_session: Option<String>,
    dev_peer_index: usize,
    dev_auto_share: bool,
}

impl LaunchConfig {
    fn from_args() -> Self {
        let mut args = std::env::args().skip(1).peekable();
        let mut dev_pair_session = None;
        let mut dev_peer_index = 0;
        let mut dev_auto_share = false;
        while let Some(argument) = args.next() {
            if argument == "--dev-child" {
                // Keep old hand-written invocations working as participant 1.
                dev_peer_index = 1;
            } else if let Some(value) = argument.strip_prefix("--dev-peer-index=") {
                dev_peer_index = value.parse().unwrap_or(0);
            } else if argument == "--dev-auto-share" {
                dev_auto_share = true;
            } else if let Some(value) = argument.strip_prefix("--dev-pair=") {
                dev_pair_session = Some(sanitize_session(value));
            } else if argument == "--dev-pair" {
                let value = args
                    .next_if(|next| !next.starts_with('-'))
                    .unwrap_or_else(|| format!("local-{}", std::process::id()));
                dev_pair_session = Some(sanitize_session(&value));
            }
        }
        Self {
            dev_pair_session,
            dev_peer_index,
            dev_auto_share,
        }
    }
}

fn sanitize_session(value: &str) -> String {
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

fn log_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|root| PathBuf::from(root).join(LOG_DIR_NAME))
}

fn legacy_log_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .map(|root| PathBuf::from(root).join(LEGACY_LOG_DIR_NAME))
}

fn cleanup_legacy_log_dir() {
    let Some(legacy) = legacy_log_dir() else {
        return;
    };
    if !legacy.exists() {
        return;
    }
    let Ok(mut entries) = fs::read_dir(&legacy) else {
        return;
    };
    if entries.next().is_some() {
        return;
    }
    let _ = fs::remove_dir(&legacy);
}

fn cleanup_runaway_logs(root: &std::path::Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_wire_log = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(LOG_FILE_PREFIX) && name.ends_with(".log"));
        if !is_wire_log {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_LOG_AGE);
        if metadata.len() > RUNAWAY_LOG_BYTES && stale {
            let _ = fs::remove_file(path);
        }
    }
}

fn init_logging(dev_pair: Option<(&str, usize)>) {
    let mut filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,wire=info,wire_app=info,wire_app::chat=debug"));
    // Wire's own lifecycle and chat diagnostics must remain available even when
    // the surrounding shell sets a restrictive global filter such as `warn`.
    filter = filter
        .add_directive(
            "wire_app=info"
                .parse()
                .expect("valid wire-app log directive"),
        )
        .add_directive(
            "wire_app::chat=debug"
                .parse()
                .expect("valid Wire chat log directive"),
        );
    if dev_pair.is_some() {
        // Dev-pair logs are test artifacts. Preserve Wire's pipeline diagnostics
        // even when the surrounding shell uses a restrictive RUST_LOG value.
        filter = filter.add_directive("wire=info".parse().expect("valid Wire log directive"));
    }

    let log_root = log_dir();
    if let Some(root) = &log_root {
        let _ = fs::create_dir_all(root);
        cleanup_runaway_logs(root);
    }
    let log_path = log_root.map(|root| match dev_pair {
        Some((session, peer_index)) => root.join(format!(
            "{LOG_FILE_PREFIX}-dev-{session}-{}-{}.log",
            if peer_index == 0 {
                "host".to_owned()
            } else {
                format!("peer-{peer_index}")
            },
            std::process::id()
        )),
        None => root.join(format!("{LOG_FILE_PREFIX}-{}.log", std::process::id())),
    });

    if let Some(path) = log_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        cleanup_legacy_log_dir();
        let mut open_options = OpenOptions::new();
        open_options.create(true).write(true);
        if dev_pair.is_some() {
            open_options.truncate(true);
        } else {
            open_options.append(true);
        }
        match open_options.open(&path) {
            Ok(file) => {
                wire::remote_logs::set_current_log_path(path.clone());
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(std::sync::Mutex::new(CappedLogFile::new(file)))
                    .with_ansi(false)
                    .init();
                info!("logging to {}", path.display());
                return;
            }
            Err(err) => {
                eprintln!("failed to open log file {}: {err}", path.display());
            }
        }
    }

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn spawn_dev_peer(session: &str, peer_index: usize) -> std::io::Result<std::process::Child> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command.args([
        "--dev-pair",
        session,
        &format!("--dev-peer-index={peer_index}"),
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()
}

fn main() -> Result<(), eframe::Error> {
    let launch = LaunchConfig::from_args();
    let mut dev_children = Vec::new();
    if let Some(session) = launch.dev_pair_session.as_deref() {
        std::env::set_var(DEV_PAIR_SESSION_ENV, session);
        std::env::set_var(DEV_PAIR_INDEX_ENV, launch.dev_peer_index.to_string());
        // Every fixture instance owns a separate persistent Docs database. Sharing
        // the normal config directory makes the spawned processes contend for the
        // same redb lock and prevents their workers from starting.
        std::env::set_var(
            "WIRE_CONFIG_DIR",
            dev_pair_config_dir(session, launch.dev_peer_index),
        );
        std::env::set_var(
            "IROH_SECRET",
            wire::net::generate_ephemeral_secret_key().to_string(),
        );
    }
    if launch.dev_auto_share && launch.dev_peer_index == 0 {
        std::env::set_var("WIRE_DEV_AUTO_SHARE", "1");
    } else {
        std::env::remove_var("WIRE_DEV_AUTO_SHARE");
    }
    init_logging(
        launch
            .dev_pair_session
            .as_deref()
            .map(|session| (session, launch.dev_peer_index)),
    );
    info!(version = wire_app::APP_VERSION, "starting Wire");
    if let Some(session) = launch.dev_pair_session.as_deref() {
        info!(
            "starting isolated dev-call instance '{}' (participant={})",
            session, launch.dev_peer_index
        );
        if launch.dev_peer_index == 0 {
            for peer_index in 1..DEV_CALL_PARTICIPANTS {
                match spawn_dev_peer(session, peer_index) {
                    Ok(child) => {
                        info!(
                            "spawned dev-call participant {peer_index} (pid={})",
                            child.id()
                        );
                        dev_children.push(child);
                    }
                    Err(error) => {
                        tracing::warn!("failed to spawn dev-call participant {peer_index}: {error}")
                    }
                }
            }
        }
    }
    wire::net::prepare_config_dir();
    let frame_style = App::initial_window_frame_style();
    let rounded = window_frame::style_wants_rounded(frame_style);
    let mut options = NativeOptions::default();
    options.renderer = eframe::Renderer::Wgpu;
    options.viewport = options
        .viewport
        .with_title("Wire")
        .with_decorations(false)
        .with_transparent(rounded)
        .with_resizable(true)
        .with_min_inner_size([460., 500.])
        .with_inner_size([1100., 720.]);
    if launch.dev_pair_session.is_some() {
        let offset = launch.dev_peer_index as f32 * 44.0;
        options.viewport = options
            .viewport
            .with_position([56.0 + offset, 48.0 + offset]);
    }
    let result = App::run(options);
    for mut child in dev_children {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
    result
}
