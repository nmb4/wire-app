use std::{sync::mpsc, thread, time::Duration};

use egui::Context;
use tracing::{info, warn};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_RCONTROL, VK_RSHIFT,
};

const POLL_INTERVAL: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    ToggleMute,
    ToggleDeafen,
}

#[derive(Default)]
struct KeyEdges {
    right_shift_down: bool,
    right_control_down: bool,
}

impl KeyEdges {
    fn update(&mut self, right_shift_down: bool, right_control_down: bool) -> [Option<Action>; 2] {
        let mute = (right_shift_down && !self.right_shift_down).then_some(Action::ToggleMute);
        let deafen =
            (right_control_down && !self.right_control_down).then_some(Action::ToggleDeafen);
        self.right_shift_down = right_shift_down;
        self.right_control_down = right_control_down;
        [mute, deafen]
    }
}

pub(crate) struct GlobalHotkeys {
    event_rx: mpsc::Receiver<Action>,
}

impl GlobalHotkeys {
    pub(crate) fn start(repaint_context: Context) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        if let Err(error) = thread::Builder::new()
            .name("wire-global-hotkeys".to_owned())
            .spawn(move || run_key_monitor(event_tx, repaint_context))
        {
            warn!("failed to start global hotkey monitor: {error}");
        }
        Self { event_rx }
    }

    pub(crate) fn try_recv(&self) -> Option<Action> {
        self.event_rx.try_recv().ok()
    }
}

fn key_is_down(key: VIRTUAL_KEY) -> bool {
    // GetAsyncKeyState's sign bit is set while this specific physical-side key is down.
    (unsafe { GetAsyncKeyState(key.0 as i32) }) < 0
}

fn run_key_monitor(event_tx: mpsc::Sender<Action>, repaint_context: Context) {
    let mut edges = KeyEdges {
        // Do not toggle if Wire is launched while either shortcut is already held.
        right_shift_down: key_is_down(VK_RSHIFT),
        right_control_down: key_is_down(VK_RCONTROL),
    };
    info!("global hotkeys active (Right Shift: mute, Right Control: deafen)");

    loop {
        thread::sleep(POLL_INTERVAL);
        let actions = edges.update(key_is_down(VK_RSHIFT), key_is_down(VK_RCONTROL));
        for action in actions.into_iter().flatten() {
            info!(?action, "global hotkey pressed");
            if event_tx.send(action).is_err() {
                return;
            }
            repaint_context.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_once_on_each_press_not_while_held() {
        let mut edges = KeyEdges::default();
        assert_eq!(edges.update(true, false), [Some(Action::ToggleMute), None]);
        assert_eq!(edges.update(true, false), [None, None]);
        assert_eq!(edges.update(false, false), [None, None]);
        assert_eq!(edges.update(true, false), [Some(Action::ToggleMute), None]);
    }

    #[test]
    fn tracks_right_shift_and_right_control_independently() {
        let mut edges = KeyEdges::default();
        assert_eq!(
            edges.update(false, true),
            [None, Some(Action::ToggleDeafen)]
        );
        assert_eq!(edges.update(true, true), [Some(Action::ToggleMute), None]);
        assert_eq!(edges.update(true, true), [None, None]);
    }
}
