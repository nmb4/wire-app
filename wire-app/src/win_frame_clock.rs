//! Non-spinning media pacing adapted from IsoCast's Windows clock.rs (MIT).
//! See third-party/isocast/README.md for source revision and license.

use std::time::Instant;

use anyhow::{Context, Result};
use windows::core::{Owned, PCWSTR};
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimerEx, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

pub(crate) struct FrameClock(Owned<HANDLE>);

impl FrameClock {
    pub(crate) fn new() -> Result<Self> {
        let handle = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .context("creating high-resolution capture timer")?;
        Ok(Self(unsafe { Owned::new(handle) }))
    }

    pub(crate) fn sleep_until(&self, deadline: Instant) -> Result<()> {
        loop {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_nanos();
            if remaining == 0 {
                return Ok(());
            }
            // Windows absolute due-times use UTC FILETIME, not Instant/QPC.
            // Round the relative interval UP to 100 ns and re-arm early wakes.
            let due = -(remaining.div_ceil(100).min(i64::MAX as u128) as i64);
            unsafe { SetWaitableTimerEx(*self.0, &due, 0, None, None, None, 0) }?;
            let status = unsafe { WaitForSingleObject(*self.0, INFINITE) };
            if status == WAIT_FAILED {
                return Err(windows::core::Error::from_win32())
                    .context("waiting for capture timer");
            }
            anyhow::ensure!(
                status == WAIT_OBJECT_0,
                "unexpected capture timer status {}",
                status.0
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timer_waits_until_deadline_and_accepts_overdue_frames() {
        let timer = FrameClock::new().unwrap();
        let deadline = Instant::now() + Duration::from_millis(10);
        timer.sleep_until(deadline).unwrap();
        assert!(Instant::now() >= deadline);
        timer.sleep_until(deadline).unwrap();
    }
}
