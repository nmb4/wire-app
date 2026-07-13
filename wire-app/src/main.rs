#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use eframe::NativeOptions;
use tracing::info;
use tracing_subscriber::EnvFilter;
use wire_app::app::App;
use wire_app::window_frame;

const LOG_DIR_NAME: &str = "wire";
const LEGACY_LOG_DIR_NAME: &str = "callme";
const LOG_FILE_PREFIX: &str = "wire-app";
const DEV_PAIR_SESSION_ENV: &str = "WIRE_DEV_PAIR_SESSION";

struct LaunchConfig {
    dev_pair_session: Option<String>,
    dev_child: bool,
    dev_auto_share: bool,
}

impl LaunchConfig {
    fn from_args() -> Self {
        let mut args = std::env::args().skip(1).peekable();
        let mut dev_pair_session = None;
        let mut dev_child = false;
        let mut dev_auto_share = false;
        while let Some(argument) = args.next() {
            if argument == "--dev-child" {
                dev_child = true;
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
            dev_child,
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

fn init_logging(dev_pair: Option<(&str, bool)>) {
    let mut filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wire=info,wire_app=info"));
    if dev_pair.is_some() {
        // Dev-pair logs are test artifacts. Preserve Wire's pipeline diagnostics
        // even when the surrounding shell uses a restrictive RUST_LOG value.
        filter = filter
            .add_directive("wire=info".parse().expect("valid Wire log directive"))
            .add_directive(
                "wire_app=info"
                    .parse()
                    .expect("valid wire-app log directive"),
            );
    }

    let log_path = log_dir().map(|root| match dev_pair {
        Some((session, child)) => root.join(format!(
            "{LOG_FILE_PREFIX}-dev-{session}-{}-{}.log",
            if child { "peer" } else { "host" },
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
                tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_writer(file)
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

fn spawn_dev_peer(session: &str) -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command.args(["--dev-pair", session, "--dev-child"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()?;
    Ok(())
}

fn main() -> Result<(), eframe::Error> {
    let launch = LaunchConfig::from_args();
    if let Some(session) = launch.dev_pair_session.as_deref() {
        std::env::set_var(DEV_PAIR_SESSION_ENV, session);
        std::env::set_var(
            "IROH_SECRET",
            wire::net::generate_ephemeral_secret_key().to_string(),
        );
    }
    if launch.dev_auto_share && !launch.dev_child {
        std::env::set_var("WIRE_DEV_AUTO_SHARE", "1");
    } else {
        std::env::remove_var("WIRE_DEV_AUTO_SHARE");
    }
    init_logging(
        launch
            .dev_pair_session
            .as_deref()
            .map(|session| (session, launch.dev_child)),
    );
    if let Some(session) = launch.dev_pair_session.as_deref() {
        info!(
            "starting isolated dev-pair instance '{}' (role={})",
            session,
            if launch.dev_child { "peer" } else { "host" }
        );
        if !launch.dev_child {
            match spawn_dev_peer(session) {
                Ok(()) => info!("spawned second dev-pair window"),
                Err(error) => tracing::warn!("failed to spawn second dev-pair window: {error}"),
            }
        }
    }
    wire::net::prepare_config_dir();
    let frame_style = App::initial_window_frame_style();
    let rounded = window_frame::style_wants_rounded(frame_style);
    let mut options = NativeOptions::default();
    options.viewport = options
        .viewport
        .with_title("Wire")
        .with_decorations(false)
        .with_transparent(rounded)
        .with_resizable(true)
        .with_min_inner_size([460., 500.])
        .with_inner_size([1100., 720.]);
    App::run(options)
}
