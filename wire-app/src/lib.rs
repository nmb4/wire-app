pub mod app;
mod dev_pair;
mod resource_monitor;
mod sounds;
mod title_bar;
pub mod window_frame;
/// The application version embedded at compile time from this package's Cargo manifest.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(windows)]
mod scap_capture;
mod screen_capture;
pub mod theme;
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
mod yuv_convert;
