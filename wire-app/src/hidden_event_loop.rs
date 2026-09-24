//! Controls native redraw delivery while the presenter is hidden.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use eframe::{AppCreator, NativeOptions, UserEvent};
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, DeviceId, StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::WindowId,
};

struct HiddenRedrawApp<'a> {
    inner: eframe::EframeWinitApplication<'a>,
    hidden: Arc<AtomicBool>,
}

impl ApplicationHandler<UserEvent> for HiddenRedrawApp<'_> {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if !self.hidden.load(Ordering::Acquire) {
            self.inner.new_events(event_loop, cause);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.resumed(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        if self.hidden.load(Ordering::Acquire)
            && matches!(event, UserEvent::RequestRepaint { .. })
        {
            return;
        }
        self.inner.user_event(event_loop, event);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if self.hidden.load(Ordering::Acquire) && matches!(event, WindowEvent::RedrawRequested) {
            return;
        }
        self.inner.window_event(event_loop, window_id, event);
    }

    fn device_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        device_id: DeviceId,
        event: DeviceEvent,
    ) {
        self.inner.device_event(event_loop, device_id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.about_to_wait(event_loop);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.exiting(event_loop);
    }
}

/// Run eframe on an event loop that can stop delivering hidden-window redraws.
///
/// eframe's normal `run_native` path keeps the wgpu surface in its redraw
/// pipeline even after the viewport is hidden. On debug builds that can consume
/// a full core while the service itself is idle. The service and tray are owned
/// outside this loop, so suppressing only hidden redraws preserves calls and
/// activation while allowing the native event loop to sleep.
pub(crate) fn run(
    app_name: &str,
    options: NativeOptions,
    app_creator: AppCreator<'_>,
    hidden: Arc<AtomicBool>,
) -> eframe::Result {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(eframe::Error::WinitEventLoop)?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let inner = eframe::create_native(app_name, options, app_creator, &event_loop);
    let mut app = HiddenRedrawApp { inner, hidden };
    event_loop
        .run_app(&mut app)
        .map_err(eframe::Error::WinitEventLoop)
}
