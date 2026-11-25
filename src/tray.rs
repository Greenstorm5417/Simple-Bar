//! `StatusNotifierItem` host. Owns the watcher name when nobody else does,
//! otherwise mirrors the watcher that is already running (waybar's, during
//! side-by-side testing).
//!
//! Messages are pulled off the bus with `Channel::pop_message` inside
//! `Tray::dispatch`, so all state lives in the `Tray` itself: no filter
//! callbacks, no shared state, no locks.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use dbus::arg::Variant;
use dbus::blocking::Proxy;
use dbus::blocking::stdintf::org_freedesktop_dbus::{Properties, RequestNameReply};
use dbus::channel::{BusType, Channel};
use dbus::message::MessageType;
use dbus::{Error, Message};

const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MAX_ITEMS: usize = 12;
const DO_NOT_QUEUE: u32 = 0x4;
const CALL_TIMEOUT: Duration = Duration::from_millis(400);

const MATCH_RULES: [&str; 3] = [
    "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
    "type='signal',interface='org.kde.StatusNotifierWatcher'",
    "type='signal',interface='org.kde.StatusNotifierItem',member='NewIcon'",
];

const INTROSPECTION: &str = concat!(
    "<node>",
    "<interface name='org.kde.StatusNotifierWatcher'>",
    "<method name='RegisterStatusNotifierItem'><arg type='s' direction='in'/></method>",
    "<method name='RegisterStatusNotifierHost'><arg type='s' direction='in'/></method>",
    "<property name='RegisteredStatusNotifierItems' type='as' access='read'/>",
    "<property name='IsStatusNotifierHostRegistered' type='b' access='read'/>",
    "<property name='ProtocolVersion' type='i' access='read'/>",
    "<signal name='StatusNotifierItemRegistered'><arg type='s'/></signal>",
    "</interface>",
    "</node>"
);

/// One tray icon, ready to blit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayIcon {
    service: String,
    path: String,
    /// Pixel width.
    pub width: i32,
    /// Pixel height.
    pub height: i32,
    /// Premultiplied little-endian BGRA, the layout cairo and `wl_shm` share.
    pub bgra: Vec<u8>,
}

impl TrayIcon {
    fn is(&self, service: &str, path: &str) -> bool {
        self.service == service && self.path == path
    }
}

/// The tray's D-Bus connection and its icons.
pub(crate) struct Tray {
    bus: Channel,
    fd: OwnedFd,
    own: bool,
    items: Vec<TrayIcon>,
    generation: u64,
}

impl AsFd for Tray {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Tray {
    /// Connect to the session bus and take or mirror the watcher.
    pub(crate) fn open() -> Result<Self, Error> {
        let mut bus = Channel::get_private(BusType::Session)?;
        bus.set_watch_enabled(true);
        // SAFETY: libdbus owns the watch fd for the channel's lifetime; the
        // borrow only lives long enough to duplicate it.
        let watch = unsafe { BorrowedFd::borrow_raw(bus.watch().fd) };
        let fd = watch
            .try_clone_to_owned()
            .map_err(|err| Error::new_failed(&err.to_string()))?;
        let mut tray = Self {
            bus,
            fd,
            own: false,
            items: Vec::new(),
            generation: 0,
        };
        let daemon = tray.proxy(BUS, BUS_PATH);
        for rule in MATCH_RULES {
            daemon.method_call::<(), _, _, _>(BUS, "AddMatch", (rule,))?;
        }
        tray.try_own();
        if !tray.own {
            tray.mirror_remote();
        }
        Ok(tray)
    }

    /// The current icons, oldest first.
    pub(crate) fn icons(&self) -> &[TrayIcon] {
        &self.items
    }

    /// Bumped every time [`Tray::icons`] changes.
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Handle everything the bus delivered. Returns whether the icons changed.
    pub(crate) fn dispatch(&mut self) -> bool {
        let mut changed = false;
        // Blocking calls made while handling a message can queue further
        // messages inside libdbus without the fd turning readable again, so
        // keep popping until the queue stays empty.
        loop {
            if self.bus.read_write(Some(Duration::ZERO)).is_err() {
                break;
            }
            let Some(msg) = self.bus.pop_message() else {
                break;
            };
            changed |= self.on_message(&msg);
        }
        self.bus.flush();
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        changed
    }

    /// Ask the item under an icon to activate, as a left click does.
    pub(crate) fn activate(&self, index: usize) {
        let Some(icon) = self.items.get(index) else {
            return;
        };
        let proxy = self.proxy(&icon.service, &icon.path);
        // An item that does not answer just ignores the click.
        let _ = proxy.method_call::<(), _, _, _>(ITEM_IFACE, "Activate", (0i32, 0i32));
    }

    fn proxy<'a>(&'a self, dest: &'a str, path: &'a str) -> Proxy<'a, &'a Channel> {
        Proxy::new(dest, path, CALL_TIMEOUT, &self.bus)
    }

    fn try_own(&mut self) {
        let reply = self.proxy(BUS, BUS_PATH).method_call::<(u32,), _, _, _>(
            BUS,
            "RequestName",
            (WATCHER, DO_NOT_QUEUE),
        );
        let owner = [
            RequestNameReply::PrimaryOwner,
            RequestNameReply::AlreadyOwner,
        ];
        if let Ok((code,)) = reply
            && owner.iter().any(|&reply| reply as u32 == code)
        {
            self.own = true;
            self.items.clear();
        }
    }

    fn on_message(&mut self, msg: &Message) -> bool {
        match msg.msg_type() {
            MessageType::MethodCall => self.on_method(msg),
            MessageType::Signal => self.on_signal(msg),
            _ => false,
        }
    }

    fn on_signal(&mut self, msg: &Message) -> bool {
        let (iface, member) = (msg.interface(), msg.member());
        match (iface.as_deref(), member.as_deref()) {
            (Some(BUS), Some("NameOwnerChanged")) => {
                let Ok((name, _old, new)) = msg.read3::<&str, &str, &str>() else {
                    return false;
                };
                if name == WATCHER && !self.own {
                    if new.is_empty() {
                        self.try_own();
                    }
                    return if self.own { true } else { self.mirror_remote() };
                }
                if new.is_empty() {
                    let before = self.items.len();
                    self.items.retain(|icon| icon.service != name);
                    return self.items.len() != before;
                }
                false
            }
            (Some(WATCHER), _) if !self.own => self.mirror_remote(),
            (Some(ITEM_IFACE), Some("NewIcon")) => {
                let Some(sender) = msg.sender() else {
                    return false;
                };
                self.reload(&sender)
            }
            _ => false,
        }
    }

    fn on_method(&mut self, msg: &Message) -> bool {
        let (iface, member) = (msg.interface(), msg.member());
        if msg.path().as_deref() != Some(WATCHER_PATH) {
            if let Some(reply) = dbus::channel::default_reply(msg) {
                let _ = self.bus.send(reply);
            }
            return false;
        }
        let mut changed = false;
        let reply = match (iface.as_deref(), member.as_deref()) {
            (Some(WATCHER), Some("RegisterStatusNotifierItem")) => match msg.read1::<&str>() {
                Ok(arg) => {
                    changed = self.register(arg, msg.sender().as_deref().unwrap_or(""));
                    Some(msg.method_return())
                }
                Err(_) => None,
            },
            (Some(WATCHER), Some("RegisterStatusNotifierHost")) => Some(msg.method_return()),
            (Some("org.freedesktop.DBus.Properties"), Some("Get")) => self.property(msg),
            (Some("org.freedesktop.DBus.Introspectable"), Some("Introspect")) => {
                Some(msg.method_return().append1(INTROSPECTION))
            }
            _ => None,
        };
        // Anything unhandled gets the standard "unknown method" error. Sends
        // are best effort: a caller that vanished needs no reply.
        if let Some(reply) = reply.or_else(|| dbus::channel::default_reply(msg)) {
            let _ = self.bus.send(reply);
        }
        changed
    }

    /// Take an item that registered with us. Returns whether it was added.
    fn register(&mut self, arg: &str, sender: &str) -> bool {
        if let Ok(signal) =
            Message::new_signal(WATCHER_PATH, WATCHER, "StatusNotifierItemRegistered")
        {
            let _ = self.bus.send(signal.append1(arg));
        }
        let (service, path) = split_item(arg, sender);
        if self.items.len() >= MAX_ITEMS || self.items.iter().any(|icon| icon.is(&service, &path)) {
            return false;
        }
        match load_icon(self, &service, &path) {
            Some(icon) => {
                self.items.push(icon);
                true
            }
            None => false,
        }
    }

    fn property(&self, msg: &Message) -> Option<Message> {
        let (iface, prop) = msg.read2::<&str, &str>().ok()?;
        if iface != WATCHER {
            return None;
        }
        let reply = msg.method_return();
        Some(match prop {
            "RegisteredStatusNotifierItems" => {
                let ids: Vec<String> = self
                    .items
                    .iter()
                    .map(|icon| format!("{}{}", icon.service, icon.path))
                    .collect();
                reply.append1(Variant(ids))
            }
            "IsStatusNotifierHostRegistered" => reply.append1(Variant(true)),
            "ProtocolVersion" => reply.append1(Variant(0i32)),
            _ => return None,
        })
    }

    /// Reload the pixmaps of every icon `service` provides.
    fn reload(&mut self, service: &str) -> bool {
        let paths: Vec<String> = self
            .items
            .iter()
            .filter(|icon| icon.service == service)
            .map(|icon| icon.path.clone())
            .collect();
        let mut changed = false;
        for path in paths {
            let Some(icon) = load_icon(self, service, &path) else {
                continue;
            };
            if let Some(slot) = self.items.iter_mut().find(|have| have.is(service, &path))
                && *slot != icon
            {
                *slot = icon;
                changed = true;
            }
        }
        changed
    }

    /// Copy the running watcher's items, in the order waybar shows them
    /// (oldest process first). Returns whether the icons changed.
    fn mirror_remote(&mut self) -> bool {
        let ids: Vec<String> = self
            .proxy(WATCHER, WATCHER_PATH)
            .get(WATCHER, "RegisteredStatusNotifierItems")
            .unwrap_or_default();
        let mut wanted: Vec<(u64, String, String)> = ids
            .iter()
            .map(|id| split_item(id, ""))
            .filter(|(service, _)| !service.is_empty())
            .map(|(service, path)| {
                (
                    start_time(self, &service).unwrap_or(u64::MAX),
                    service,
                    path,
                )
            })
            .collect();
        wanted.sort_unstable();
        wanted.truncate(MAX_ITEMS);
        let unchanged = wanted.len() == self.items.len()
            && wanted
                .iter()
                .zip(&self.items)
                .all(|((_, service, path), icon)| icon.is(service, path));
        if unchanged {
            return false;
        }
        let mut current = std::mem::take(&mut self.items);
        self.items = wanted
            .into_iter()
            .filter_map(|(_, service, path)| {
                match current.iter().position(|icon| icon.is(&service, &path)) {
                    Some(pos) => Some(current.swap_remove(pos)),
                    None => load_icon(self, &service, &path),
                }
            })
            .collect();
        true
    }
}

/// Split a registration argument into bus name and object path. Items may
/// register with just a path (then the sender is the service) or just a
/// bus name (then the path is the standard one).
fn split_item(arg: &str, sender: &str) -> (String, String) {
    if arg.starts_with('/') {
        return (sender.to_owned(), arg.to_owned());
    }
    match arg.split_once('/') {
        Some((service, path)) => (service.to_owned(), format!("/{path}")),
        None => (arg.to_owned(), ITEM_PATH.to_owned()),
    }
}

/// When a bus peer's process started, in clock ticks since boot.
fn start_time(tray: &Tray, service: &str) -> Option<u64> {
    let (pid,): (u32,) = tray
        .proxy(BUS, BUS_PATH)
        .method_call(BUS, "GetConnectionUnixProcessID", (service,))
        .ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 22 (starttime) is the 20th field after the parenthesized comm.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

fn load_icon(tray: &Tray, service: &str, path: &str) -> Option<TrayIcon> {
    let pixmaps: Vec<(i32, i32, Vec<u8>)> = tray
        .proxy(service, path)
        .get(ITEM_IFACE, "IconPixmap")
        .ok()?;
    let (width, height, argb) = pick_pixmap(&pixmaps)?;
    Some(TrayIcon {
        service: service.to_owned(),
        path: path.to_owned(),
        width,
        height,
        bgra: premultiply(argb),
    })
}

/// The smallest pixmap at least 16 px on its longer side, else the largest.
fn pick_pixmap(pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<(i32, i32, &[u8])> {
    pixmaps
        .iter()
        .filter(|(w, h, data)| *w > 0 && *h > 0 && data.len() == (*w as usize) * (*h as usize) * 4)
        .min_by_key(|(w, h, _)| {
            let side = (*w).max(*h);
            (side < 16, if side < 16 { -side } else { side })
        })
        .map(|(w, h, data)| (*w, *h, data.as_slice()))
}

/// Network-order ARGB (the SNI pixmap format) to premultiplied BGRA.
fn premultiply(argb: &[u8]) -> Vec<u8> {
    let scale = |channel: u8, alpha: u8| ((u16::from(channel) * u16::from(alpha)) / 255) as u8;
    let (pixels, _) = argb.as_chunks::<4>();
    pixels
        .iter()
        .flat_map(|&[a, r, g, b]| [scale(b, a), scale(g, a), scale(r, a), a])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_registration_arguments() {
        assert_eq!(
            split_item("/org/ayatana/NotificationItem/x", ":1.5"),
            (":1.5".into(), "/org/ayatana/NotificationItem/x".into())
        );
        assert_eq!(
            split_item("org.kde.StatusNotifierItem-1-1", ":1.5"),
            ("org.kde.StatusNotifierItem-1-1".into(), ITEM_PATH.into())
        );
        assert_eq!(
            split_item(":1.9/StatusNotifierItem", ""),
            (":1.9".into(), ITEM_PATH.into())
        );
    }

    #[test]
    fn premultiplies_into_bgra() {
        assert_eq!(premultiply(&[128, 255, 0, 64]), vec![32, 0, 128, 128]);
        assert_eq!(premultiply(&[0, 255, 255, 255]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn picks_the_smallest_pixmap_of_at_least_16px() {
        let px = |side: i32| (side, side, vec![0; (side * side * 4) as usize]);
        let pixmaps = [px(8), px(64), px(22), px(12)];
        assert_eq!(pick_pixmap(&pixmaps).map(|p| p.0), Some(22));
        let small = [px(8), px(12)];
        assert_eq!(pick_pixmap(&small).map(|p| p.0), Some(12));
    }
}
