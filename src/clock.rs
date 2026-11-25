//! Wall-clock tick and the `%I:%M:%S %p` text.
//!
//! The tick is one `CLOCK_REALTIME` timerfd armed for absolute whole
//! seconds with a one-second interval. The kernel fires it on each
//! boundary with no re-arming, and `TFD_TIMER_CANCEL_ON_SET` reports clock
//! jumps (NTP, manual changes) so the schedule can be realigned.
//!
//! The text is formatted from a precise `clock_gettime(CLOCK_REALTIME)`
//! reading. libc's `time()` reads the coarse clock, which lags the real
//! one by up to a jiffy, so right after a boundary it still returns the
//! previous second; formatting from it showed every second one second late.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// A timerfd that becomes readable on every whole wall-clock second.
#[derive(Debug)]
pub struct SecondTimer {
    fd: OwnedFd,
}

impl AsFd for SecondTimer {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl SecondTimer {
    /// Create the timer and arm it for the next whole second.
    ///
    /// # Errors
    /// Returns the OS error if the timerfd cannot be created or armed.
    pub fn new() -> io::Result<Self> {
        // SAFETY: timerfd_create has no memory-safety preconditions; the
        // returned descriptor is immediately owned by `OwnedFd`.
        let raw = unsafe {
            libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh, valid descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let timer = Self { fd };
        timer.arm()?;
        Ok(timer)
    }

    fn arm(&self) -> io::Result<()> {
        let now = now();
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: now.tv_sec + 1,
                tv_nsec: 0,
            },
        };
        // SAFETY: `spec` is a valid itimerspec and the old-value pointer may be null.
        let rc = unsafe {
            libc::timerfd_settime(
                self.fd.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME | libc::TFD_TIMER_CANCEL_ON_SET,
                &raw const spec,
                std::ptr::null_mut(),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Consume the pending expirations. Realigns the schedule after the
    /// wall clock was set. Returns true when at least one tick happened.
    #[must_use]
    pub fn acknowledge(&self) -> bool {
        let mut count = 0u64;
        // SAFETY: reads exactly 8 bytes into a live u64.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&raw mut count).cast(),
                size_of::<u64>(),
            )
        };
        if n < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECANCELED) {
            // The clock jumped; arm again from the new time and redraw now.
            let _ = self.arm();
            return true;
        }
        n == 8 && count > 0
    }
}

fn now() -> libc::timespec {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer; CLOCK_REALTIME always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &raw mut ts) };
    ts
}

/// Write the current local time as waybar's `{:%I:%M:%S %p}` into `out`.
pub fn write(out: &mut String) {
    format_at(now().tv_sec, out);
}

/// Format a Unix timestamp in local time as `%I:%M:%S %p`.
pub fn format_at(t: libc::time_t, out: &mut String) {
    out.clear();
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `localtime_r` either fails (null) or fully initializes `tm`.
    let tm = unsafe {
        if libc::localtime_r(&raw const t, tm.as_mut_ptr()).is_null() {
            out.push_str("--:--:--");
            return;
        }
        tm.assume_init()
    };
    let hour = match tm.tm_hour % 12 {
        0 => 12,
        h => h,
    };
    let meridiem = if tm.tm_hour < 12 { "AM" } else { "PM" };
    // Formatting into a String cannot fail.
    let _ = std::fmt::Write::write_fmt(
        out,
        format_args!("{hour:02}:{:02}:{:02} {meridiem}", tm.tm_min, tm.tm_sec),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_twelve_hour_clock() {
        let mut out = String::new();
        format_at(now().tv_sec, &mut out);
        assert_eq!(out.len(), 11, "{out}");
        assert!(out.ends_with(" AM") || out.ends_with(" PM"));
        let hour: u32 = out[..2].parse().unwrap();
        assert!((1..=12).contains(&hour));
    }

    #[test]
    fn timer_arms() {
        let timer = SecondTimer::new().unwrap();
        // Nothing expired yet unless we are right on a boundary.
        let _ = timer.acknowledge();
    }
}
