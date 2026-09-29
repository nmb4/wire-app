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
    pub(crate) fn start(
        path: PathBuf,
        ctx: Context,
        hwnd: Option<isize>,
        hidden: Arc<AtomicBool>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let hwnd = Arc::new(Mutex::new(hwnd));
        let thread_stop = stop.clone();
        let thread_hwnd = hwnd.clone();
        let thread_hidden = hidden.clone();
        let join = thread::Builder::new()
            .name("wire-activation-watch".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    if path.exists() {
                        thread_hidden.store(false, Ordering::Release);
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
        UI::WindowsAndMessaging::{ShowWindowAsync, SW_SHOW},
    };

    // The GUI refreshes this handle every frame. Release its mutex before any
    // native window operation so activation cannot prevent a frame from running.
    let hwnd = hwnd.lock().ok().and_then(|current| *current);
    let Some(hwnd) = hwnd else {
        return;
    };
    unsafe {
        let hwnd = HWND(hwnd as *mut std::ffi::c_void);
        // ShowWindow can wait for the owning GUI thread. The watcher must never
        // wait for that thread, including while the GUI is joining us on exit.
        // App::show_window applies focus when it consumes activation.request.
        let _ = ShowWindowAsync(hwnd, SW_SHOW);
    }
}

#[cfg(not(windows))]
fn show_native_window(_hwnd: &Mutex<Option<isize>>) {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use windows::{
        core::w,
        Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, IsWindowVisible, PeekMessageW, MSG,
            PM_REMOVE, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        },
    };

    #[test]
    fn activation_does_not_wait_for_the_window_owner_to_pump_messages() {
        // Use a real, off-screen window, owned by this thread. A synchronous
        // cross-thread ShowWindow would wait while we wait for the watcher.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("Wire activation regression test"),
                WS_POPUP,
                -32000,
                -32000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .expect("create activation test window");
        let hwnd = Arc::new(Mutex::new(Some(window.0 as isize)));
        let watcher_hwnd = hwnd.clone();
        let (finished_tx, finished_rx) = mpsc::channel();
        let watcher = thread::spawn(move || {
            for _ in 0..8 {
                show_native_window(&watcher_hwnd);
            }
            finished_tx.send(()).unwrap();
        });

        let completed = finished_rx.recv_timeout(Duration::from_secs(2)).is_ok();
        let handle_available = hwnd.try_lock().is_ok();
        // Deliver the queued show requests only after testing that activation
        // returned without help from the GUI, then clean up on the owner thread.
        let visible = unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, Some(window), 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&message);
            }
            let visible = IsWindowVisible(window).as_bool();
            DestroyWindow(window).expect("destroy activation test window");
            visible
        };
        assert!(completed, "activation waited for the GUI thread");
        watcher.join().unwrap();
        assert!(handle_available, "activation retained the HWND mutex");
        assert!(visible, "activation did not reveal the window");
    }
}
