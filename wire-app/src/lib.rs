mod activation;
pub mod app;
mod autostart;
mod chat;
mod client_status;
mod dev_pair;
#[cfg(windows)]
mod global_hotkeys;
mod hidden_event_loop;
pub mod host;
mod notifications;
mod overlay_window;
mod persistence;
mod profile;
mod resource_monitor;
mod runtime;
mod sounds;
mod system_audio;
mod title_bar;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod tray;
pub mod window_frame;
/// The application version embedded at compile time from this package's Cargo manifest.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short git commit hash embedded at compile time (see build.rs, "unknown"
/// when git is unavailable). Shown in Settings so two machines can confirm
/// they run the same build.
pub const GIT_HASH: &str = env!("WIRE_GIT_HASH");

#[cfg(any(windows, target_os = "macos"))]
mod scap_capture;
mod screen_capture;
pub mod theme;
#[cfg(windows)]
mod update;
mod video_decode;
#[cfg(windows)]
mod win_capture;
#[cfg(windows)]
mod win_gdi_capture;
#[cfg(windows)]
pub mod win_mf_codec;
#[cfg(windows)]
mod win_mf_d3d;
#[cfg(windows)]
mod win_video_presenter;
#[cfg(windows)]
mod yuv_convert;
