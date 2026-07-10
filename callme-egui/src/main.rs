#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

use std::fs::{self, OpenOptions};
use std::path::PathBuf;

use callme_egui::app::App;
use eframe::NativeOptions;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,callme=info,callme_egui=info"));

    let log_path = std::env::var("LOCALAPPDATA").ok().map(|root| {
        PathBuf::from(root)
            .join("callme")
            .join(format!("callme-{}.log", std::process::id()))
    });

    if let Some(path) = log_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
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
    let mut options = NativeOptions::default();
    options.viewport = options
        .viewport
        .with_title("Callme")
        .with_resizable(true)
        .with_min_inner_size([460., 500.])
        .with_inner_size([1100., 720.]);
    App::run(options)
}
