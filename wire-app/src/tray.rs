//! System-tray integration for the process-owned desktop shell.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc, Mutex,
};

use anyhow::{Context as _, Result};
use image::imageops::FilterType;
use sha2::{Digest, Sha256};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const OPEN_MENU_ID: &str = "wire.tray.open";
const QUIT_MENU_ID: &str = "wire.tray.quit";
/// Identity for the tray icon when the executable path is unavailable.
const FALLBACK_TRAY_GUID: u128 = 0xB9EA_A764_0E31_5A2F_9C74_6AC5_BCA1_822E;
/// The notification area never draws more than 32x32, so the much larger app
/// icon would only be downsampled by the shell.
const TRAY_ICON_EDGE: u32 = 32;

pub(crate) enum TrayAction {
    Show,
    Quit,
}

enum TraySignal {
    Icon(TrayIconEvent),
    Menu(MenuEvent),
}

pub(crate) struct TrayController {
    // tray-icon owns OS resources through this handle. The menu is retained by it.
    _icon: TrayIcon,
    signal_rx: Receiver<TraySignal>,
    wake_window: Arc<Mutex<Option<isize>>>,
    registered: bool,
}

impl TrayController {
    pub(crate) fn new(ctx: &egui::Context, hidden: Arc<AtomicBool>) -> Result<Self> {
        let icon = load_icon()?;
        let menu = Menu::new();
        let open = MenuItem::with_id(OPEN_MENU_ID, "Open Wire", true, None);
        let separator = PredefinedMenuItem::separator();
        let quit = MenuItem::with_id(QUIT_MENU_ID, "Quit Wire", true, None);
        menu.append(&open).context("add tray Open item")?;
        menu.append(&separator).context("add tray separator")?;
        menu.append(&quit).context("add tray Quit item")?;

        // This is built from eframe's app-creation callback, after its native
        // event loop is active. That is required for macOS and safest on Windows.
        let tray = TrayIconBuilder::new()
            .with_id("wire")
            .with_guid(tray_guid())
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .with_tooltip("Wire")
            .build()
            .context("create system tray icon")?;
        let registered = tray.rect().is_some();

        let (signal_tx, signal_rx) = mpsc::channel();
        let wake_window = Arc::new(Mutex::new(None));
        let icon_signal_tx = signal_tx.clone();
        let icon_wake_window = wake_window.clone();
        let icon_hidden = hidden.clone();
        let repaint_ctx = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(
                &event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } | TrayIconEvent::DoubleClick { .. }
            ) {
                tracing::debug!("received tray activation event");
            }
            let activate = matches!(
                &event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } | TrayIconEvent::DoubleClick { .. }
            );
            let _ = icon_signal_tx.send(TraySignal::Icon(event));
            if activate {
                icon_hidden.store(false, Ordering::Release);
                show_native_window(&icon_wake_window);
            } else {
                wake_native_event_loop(&icon_wake_window);
            }
            repaint_ctx.request_repaint();
        }));
        let repaint_ctx = ctx.clone();
        let menu_signal_tx = signal_tx;
        let menu_wake_window = wake_window.clone();
        let menu_hidden = hidden.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            tracing::debug!(id = event.id.as_ref(), "received tray menu event");
            let _ = menu_signal_tx.send(TraySignal::Menu(event));
            // Menu events can arrive while the root window is hidden. Reveal it
            // before waking egui so Open and Quit are both delivered reliably.
            menu_hidden.store(false, Ordering::Release);
            show_native_window(&menu_wake_window);
            repaint_ctx.request_repaint();
        }));

        Ok(Self {
            _icon: tray,
            signal_rx,
            wake_window,
            registered,
        })
    }

    /// Whether the notification area actually holds the icon.
    ///
    /// Windows reports `Shell_NotifyIcon(NIM_ADD)` failures only to the caller,
    /// and tray-icon discards that result so it can retry after an explorer
    /// restart. A rejected registration is therefore indistinguishable from a
    /// successful one, which is how a running process ends up with no tray icon
    /// and no clue why. Ask the shell for the icon's rectangle instead: the
    /// notification area answers that only for icons it is really showing.
    pub(crate) fn is_registered(&self) -> bool {
        self.registered
    }

    #[cfg(windows)]
    pub(crate) fn set_wake_window(&self, hwnd: isize) {
        if let Ok(mut wake_window) = self.wake_window.lock() {
            *wake_window = Some(hwnd);
        }
    }

    pub(crate) fn try_recv(&self) -> Option<TrayAction> {
        loop {
            let signal = match self.signal_rx.try_recv() {
                Ok(signal) => signal,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => return None,
            };
            match signal {
                TraySignal::Icon(
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        ..
                    }
                    | TrayIconEvent::DoubleClick { .. },
                ) => return Some(TrayAction::Show),
                TraySignal::Menu(event) if event.id == OPEN_MENU_ID => {
                    return Some(TrayAction::Show);
                }
                TraySignal::Menu(event) if event.id == QUIT_MENU_ID => {
                    return Some(TrayAction::Quit);
                }
                TraySignal::Icon(_) | TraySignal::Menu(_) => {}
            }
        }
    }
}

#[cfg(windows)]
fn root_window(wake_window: &Mutex<Option<isize>>) -> Option<windows::Win32::Foundation::HWND> {
    let wake_window = wake_window.lock().ok()?;
    let hwnd = (*wake_window)? as *mut std::ffi::c_void;
    Some(windows::Win32::Foundation::HWND(hwnd))
}

#[cfg(windows)]
fn show_native_window(wake_window: &Mutex<Option<isize>>) {
    use windows::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_SHOW};

    let Some(hwnd) = root_window(wake_window) else {
        return;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

#[cfg(windows)]
fn wake_native_event_loop(wake_window: &Mutex<Option<isize>>) {
    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{PostMessageW, WM_NULL},
    };

    let Some(hwnd) = root_window(wake_window) else {
        return;
    };
    unsafe {
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    }
}

#[cfg(not(windows))]
fn show_native_window(_wake_window: &Mutex<Option<isize>>) {}

#[cfg(not(windows))]
fn wake_native_event_loop(_wake_window: &Mutex<Option<isize>>) {}

fn load_icon() -> Result<Icon> {
    let image = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .context("load tray icon asset")?
        .into_rgba8();
    let (width, height) = image.dimensions();
    if width > TRAY_ICON_EDGE || height > TRAY_ICON_EDGE {
        let image =
            image::imageops::resize(&image, TRAY_ICON_EDGE, TRAY_ICON_EDGE, FilterType::Lanczos3);
        let (width, height) = image.dimensions();
        return Icon::from_rgba(image.into_raw(), width, height).context("convert tray icon asset");
    }
    Icon::from_rgba(image.into_raw(), width, height).context("convert tray icon asset")
}

/// Derive the tray icon's identity from the executable's location.
///
/// Windows binds a notification icon GUID to the full path of the first binary
/// that registers it, and rejects `NIM_ADD` for that GUID from every other path.
/// A GUID that is compiled into the source therefore works right up to the
/// moment Wire is rebuilt into another profile directory, installed, renamed, or
/// moved, after which the shell keeps no icon for the process and rejects the
/// registration for the rest of that user's session. Deriving the GUID from the
/// path gives every location its own identity, which is the same rule the shell
/// applies to tray icons that pass no GUID at all.
fn tray_guid() -> u128 {
    let Ok(executable) = std::env::current_exe() else {
        tracing::warn!(
            "could not locate the Wire executable; the tray icon uses a fixed identity that \
             Windows may reject once another build registered it"
        );
        return FALLBACK_TRAY_GUID;
    };
    let path = std::fs::canonicalize(&executable).unwrap_or(executable);
    let mut identity = path.to_string_lossy().into_owned();
    // Windows paths are case-insensitive, so two spellings of one location must
    // not produce two identities.
    #[cfg(windows)]
    identity.make_ascii_lowercase();
    guid_for(&identity)
}

/// Turn an opaque identity string into a well-formed version 4 GUID.
fn guid_for(identity: &str) -> u128 {
    let mut bytes: [u8; 16] = Sha256::digest(identity.as_bytes())[..16]
        .try_into()
        .expect("a SHA-256 digest is longer than 16 bytes");
    // Stamp the RFC 4122 version and variant bits so the hash reads as a random
    // GUID instead of an arbitrary 128-bit number.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    u128::from_le_bytes(bytes)
}

#[cfg(test)]
mod tray_guid_tests {
    use super::*;

    #[test]
    fn one_executable_path_always_yields_one_identity() {
        // The whole point of deriving the GUID is that the same binary asks the
        // shell for the same identity on every launch.
        assert_eq!(
            guid_for("C:\\Program Files\\Wire\\wire.exe"),
            guid_for("C:\\Program Files\\Wire\\wire.exe")
        );
    }

    #[test]
    fn each_executable_path_yields_its_own_identity() {
        // A moved or rebuilt binary has to get a fresh identity, otherwise the
        // shell keeps rejecting the registration and Wire shows no tray icon.
        let installed = guid_for("C:\\Program Files\\Wire\\wire.exe");
        let development = guid_for("C:\\dev\\wire-app\\target\\debug\\wire-app.exe");
        let release = guid_for("C:\\dev\\wire-app\\target\\release\\wire-app.exe");
        assert_ne!(installed, development);
        assert_ne!(development, release);
    }

    #[test]
    fn identities_are_well_formed_version_4_guids() {
        let guid = guid_for("C:\\Program Files\\Wire\\wire.exe");
        let bytes = guid.to_le_bytes();
        assert_eq!(bytes[6] & 0xf0, 0x40, "version 4");
        assert_eq!(bytes[8] & 0xc0, 0x80, "variant 1");
    }
}
