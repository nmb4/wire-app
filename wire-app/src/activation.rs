//! Wakes a hidden presenter when another launch requests activation.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use egui::Context;

pub(crate) struct ActivationWatcher {
    stop: Arc<AtomicBool>,
    hwnd: Arc<Mutex<Option<isize>>>,
    join: Option<JoinHandle<()>>,
}

impl ActivationWatcher {
    pub(crate) fn start(path: PathBuf, ctx: Context, hwnd: Option<isize>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let hwnd = Arc::new(Mutex::new(hwnd));
        let thread_stop = stop.clone();
        let thread_hwnd = hwnd.clone();
        let join = thread::Builder::new()
            .name("wire-activation-watch".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    if path.exists() {
                        show_native_window(&thread_hwnd);
                        ctx.request_repaint();
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            })
            .ok();
        Self { stop, hwnd, join }
    }

    #[cfg(windows)]
    pub(crate) fn set_hwnd(&self, hwnd: Option<isize>) {
        if let Ok(mut current) = self.hwnd.lock() {
            *current = hwnd;
        }
    }
}

impl Drop for ActivationWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(windows)]
fn show_native_window(hwnd: &Mutex<Option<isize>>) {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow, SW_SHOW},
    };

    let Ok(hwnd) = hwnd.lock() else {
        return;
    };
    let Some(hwnd) = *hwnd else {
        return;
    };
    unsafe {
        let hwnd = HWND(hwnd as *mut std::ffi::c_void);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

#[cfg(not(windows))]
fn show_native_window(_hwnd: &Mutex<Option<isize>>) {}
