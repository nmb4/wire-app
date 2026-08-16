use egui::{ViewportBuilder, WindowLevel};

/// Build a native viewport which supplements the app without activating it.
///
/// Notifications and persistent overlays need to stay visible independently
/// of the root window, while only an explicit action should focus the app.
pub(crate) fn independent_builder(title: impl Into<String>) -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title(title)
        .with_decorations(false)
        .with_resizable(false)
        .with_transparent(true)
        .with_active(false)
        .with_taskbar(false)
        .with_window_level(WindowLevel::AlwaysOnTop)
}
