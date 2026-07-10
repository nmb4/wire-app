#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use wire_app::app::App;
use eframe::NativeOptions;
use tracing::info;
use tracing_subscriber::EnvFilter;

const LOG_DIR_NAME: &str = "wire";
const LEGACY_LOG_DIR_NAME: &str = "callme";
const LOG_FILE_PREFIX: &str = "wire-app";

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

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,wire=info,wire_app=info"));

    let log_path = log_dir().map(|root| {
        root.join(format!("{LOG_FILE_PREFIX}-{}.log", std::process::id()))
    });

    if let Some(path) = log_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        cleanup_legacy_log_dir();
        match OpenOptions::new().create(true).append(true).open(&path) {
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

fn main() -> Result<(), eframe::Error> {
    init_logging();
    wire::net::prepare_config_dir();
    let mut options = NativeOptions::default();
    options.viewport = options
        .viewport
        .with_title("Wire")
        .with_resizable(true)
        .with_min_inner_size([460., 500.])
        .with_inner_size([1100., 720.]);
    App::run(options)
}