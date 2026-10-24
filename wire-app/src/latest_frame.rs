//! A single pending raw frame. Replacing it bounds latency under overload;
//! this must never be used for encoded interdependent video packets.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

struct State<T> {
    frame: Option<T>,
    sender_alive: bool,
    receiver_alive: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

pub(crate) struct Sender<T>(Arc<Shared<T>>);
pub(crate) struct Receiver<T>(Arc<Shared<T>>);

pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            frame: None,
            sender_alive: true,
            receiver_alive: true,
        }),
        ready: Condvar::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl<T> Sender<T> {
    /// Returns whether an older pending frame was replaced. Never waits for
    /// consumption or runs a frame's destructor while holding the mutex.
    pub(crate) fn send(&self, frame: T) -> Result<bool, T> {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.receiver_alive {
            return Err(frame);
        }
        let previous = state.frame.replace(frame);
        drop(state);
        self.0.ready.notify_one();
        Ok(previous.is_some())
    }
}

impl<T> Receiver<T> {
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let (mut state, _) = self
            .0
            .ready
            .wait_timeout_while(state, timeout, |state| {
                state.frame.is_none() && state.sender_alive
            })
            .unwrap_or_else(|e| e.into_inner());
        if let Some(frame) = state.frame.take() {
            Ok(frame)
        } else if !state.sender_alive {
            Err(RecvTimeoutError::Disconnected)
        } else {
            Err(RecvTimeoutError::Timeout)
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sender_alive = false;
        self.0.ready.notify_one();
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.receiver_alive = false;
        let pending = state.frame.take();
        drop(state);
        drop(pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_consumer_gets_current_picture_and_shutdown_releases_frames() {
        let (tx, rx) = channel();
        let frame = Arc::new(());
        for sequence in 0..1000 {
            tx.send((sequence, frame.clone())).unwrap();
            assert_eq!(Arc::strong_count(&frame), 2);
        }
        let (sequence, held) = rx.recv_timeout(Duration::ZERO).unwrap();
        assert_eq!(sequence, 999);
        tx.send((1000, frame.clone())).unwrap();
        drop(rx);
        assert_eq!(Arc::strong_count(&frame), 2);
        drop(held);
        assert_eq!(Arc::strong_count(&frame), 1);
        assert!(tx.send((1001, frame.clone())).is_err());
        assert_eq!(Arc::strong_count(&frame), 1);
    }

    #[test]
    fn producer_shutdown_wakes_receiver_and_drains_last_frame() {
        let (tx, rx) = channel();
        tx.send(42).unwrap();
        drop(tx);
        assert_eq!(rx.recv_timeout(Duration::ZERO), Ok(42));
        assert_eq!(
            rx.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Disconnected)
        );

        let (tx, rx) = channel::<()>();
        let waiting = std::thread::spawn(move || rx.recv_timeout(Duration::from_secs(10)));
        drop(tx);
        assert_eq!(waiting.join().unwrap(), Err(RecvTimeoutError::Disconnected));
    }
}
