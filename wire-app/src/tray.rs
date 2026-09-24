//! System-tray integration for the process-owned desktop shell.

use std::sync::{
    mpsc::{self, Receiver},
    Arc, Mutex,
};

use anyhow::{Context as _, Result};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

const OPEN_MENU_ID: &str = "wire.tray.open";
const QUIT_MENU_ID: &str = "wire.tray.quit";
const TRAY_GUID: u128 = 0xB9EA_A764_0E31_5A2F_9C74_6AC5_BCA1_822E;

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
}

impl TrayController {
    pub(crate) fn new(ctx: &egui::Context) -> Result<Self> {
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
            .with_guid(TRAY_GUID)
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .with_tooltip("Wire")
            .build()
            .context("create system tray icon")?;

        let (signal_tx, signal_rx) = mpsc::channel();
        let wake_window = Arc::new(Mutex::new(None));
        let icon_signal_tx = signal_tx.clone();
        let icon_wake_window = wake_window.clone();
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
                show_native_window(&icon_wake_window);
            } else {
                wake_native_event_loop(&icon_wake_window);
            }
            repaint_ctx.request_repaint();
        }));
        let repaint_ctx = ctx.clone();
        let menu_signal_tx = signal_tx;
        let menu_wake_window = wake_window.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            tracing::debug!(id = event.id.as_ref(), "received tray menu event");
            let _ = menu_signal_tx.send(TraySignal::Menu(event));
            // Menu events can arrive while the root window is hidden. Reveal it
            // before waking egui so Open and Quit are both delivered reliably.
            show_native_window(&menu_wake_window);
            repaint_ctx.request_repaint();
        }));

        Ok(Self {
            _icon: tray,
            signal_rx,
            wake_window,
        })
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
    Icon::from_rgba(image.into_raw(), width, height).context("convert tray icon asset")
}
