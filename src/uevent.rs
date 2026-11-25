//! Kernel uevents for power supplies. The adapter and battery announce
//! plug/unplug and charge-state changes here, so the bar needs no fast
//! polling to notice them.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// The netlink group the kernel broadcasts uevents on.
const KERNEL_GROUP: u32 = 1;

/// What a batch of uevents said about power supplies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PowerEvents {
    /// A supply changed state (plugged in, unplugged, charge status).
    pub(crate) changed: bool,
    /// A supply appeared or disappeared.
    pub(crate) hotplug: bool,
}

/// A netlink socket subscribed to kernel uevents.
#[derive(Debug)]
pub(crate) struct Uevents {
    fd: OwnedFd,
}

impl AsFd for Uevents {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Uevents {
    pub(crate) fn open() -> io::Result<Self> {
        // SAFETY: plain socket(2); the result is checked before use.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `sockaddr_nl` is plain data; all-zero is a valid value.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = KERNEL_GROUP;
        // SAFETY: `addr` is a valid `sockaddr_nl` and the length matches it.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                (&raw const addr).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    /// Drain pending messages and report what they said about power supplies.
    pub(crate) fn drain(&self) -> PowerEvents {
        let mut events = PowerEvents::default();
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: recv(2) into a live stack buffer of the given length.
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            match usize::try_from(n) {
                Ok(0) => return events,
                Ok(len) => events.merge(parse(&buf[..len])),
                Err(_) => match io::Error::last_os_error().raw_os_error() {
                    // The socket overflowed; assume we missed something.
                    Some(libc::ENOBUFS) => events.merge(PowerEvents {
                        changed: true,
                        hotplug: true,
                    }),
                    Some(libc::EINTR) => {}
                    _ => return events,
                },
            }
        }
    }
}

impl PowerEvents {
    const fn merge(&mut self, other: Self) {
        self.changed |= other.changed;
        self.hotplug |= other.hotplug;
    }
}

/// Read one uevent: `action@devpath` followed by NUL-separated `KEY=value`.
fn parse(msg: &[u8]) -> PowerEvents {
    let mut fields = msg.split(|&b| b == 0);
    let header = fields.next().unwrap_or_default();
    if !fields.any(|field| field == b"SUBSYSTEM=power_supply") {
        return PowerEvents::default();
    }
    let hotplug = header.starts_with(b"add@") || header.starts_with(b"remove@");
    PowerEvents {
        changed: true,
        hotplug,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_power_supply_events() {
        let change =
            b"change@/devices/x/power_supply/ACAD\0ACTION=change\0SUBSYSTEM=power_supply\0";
        assert_eq!(
            parse(change),
            PowerEvents {
                changed: true,
                hotplug: false
            }
        );
        let add = b"add@/devices/y/power_supply/BAT1\0ACTION=add\0SUBSYSTEM=power_supply\0";
        assert!(parse(add).hotplug);
        let usb = b"add@/devices/usb1\0ACTION=add\0SUBSYSTEM=usb\0";
        assert_eq!(parse(usb), PowerEvents::default());
    }
}
