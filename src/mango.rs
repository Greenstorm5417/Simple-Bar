//! Mango compositor IPC: the `watch all-monitors` stream and dispatches.

use std::borrow::Cow;
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::json::{self, Json};
use crate::sock;

/// A workspace (Mango "tag") on one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// 1-based tag number.
    pub index: u8,
    /// Currently shown.
    pub active: bool,
    /// A client on it wants attention.
    pub urgent: bool,
}

/// What the bar shows for one output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mon {
    /// Output name, such as `eDP-1`.
    pub name: String,
    /// The output with keyboard focus.
    pub active: bool,
    /// Focused window title, with control characters removed.
    pub title: String,
    /// Layout label for the layout module: Mango's symbols map to fixed
    /// labels, dwl's symbol is shown as sent.
    pub layout_text: Cow<'static, str>,
    /// Workspaces.
    pub tags: Vec<Tag>,
}

/// The `watch all-monitors` stream, read from the main event loop.
#[derive(Debug)]
pub struct Mango {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl AsFd for Mango {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

impl Mango {
    /// Connect to Mango's socket and start watching every monitor.
    ///
    /// # Errors
    /// Fails when no Mango socket is found or it cannot be used.
    pub fn connect() -> io::Result<Self> {
        let path = socket_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Mango IPC socket"))?;
        let mut stream = UnixStream::connect(path)?;
        stream.write_all(b"watch all-monitors\n")?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            buf: Vec::with_capacity(8192),
        })
    }

    /// A second handle on the socket, for registering with the event loop.
    #[must_use]
    pub fn try_clone_stream(&self) -> Option<UnixStream> {
        self.stream.try_clone().ok()
    }

    /// Drain the socket and parse only the newest complete snapshot; older
    /// ones from the same read are already stale.
    ///
    /// # Errors
    /// Returns an error when Mango closed the connection or the read failed.
    pub fn on_readable(&mut self) -> io::Result<Option<Vec<Mon>>> {
        sock::read_available(&mut self.stream, &mut self.buf)?;
        let Some(end) = self.buf.iter().rposition(|&b| b == b'\n') else {
            return Ok(None);
        };
        let start = self.buf[..end]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        let mons = std::str::from_utf8(&self.buf[start..end])
            .ok()
            .and_then(parse_monitors);
        self.buf.drain(..=end);
        Ok(mons)
    }
}

/// Send `dispatch <args>` on a fresh connection, as `mmsg` would.
///
/// # Errors
/// Fails when no Mango socket is found or the write fails.
pub fn dispatch(args: &str) -> io::Result<()> {
    let path = socket_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Mango IPC socket"))?;
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(Duration::from_millis(200)))?;
    writeln!(stream, "dispatch {args}")
}

/// Parse one `watch all-monitors` line.
#[must_use]
pub fn parse_monitors(line: &str) -> Option<Vec<Mon>> {
    let msg = json::parse(line)?;
    let list = msg.get("monitors")?.as_array()?;
    Some(list.iter().filter_map(mon_from_value).collect())
}

/// The waybar `mango/layout` module's `format-<symbol>` labels.
#[must_use]
pub fn layout_text(symbol: &str) -> &'static str {
    match symbol {
        "F" => "\u{f0db} Fair",
        "S" => "\u{f07e} Scroll",
        "DW" => "\u{f074} Dwindle",
        "T" => "\u{f0ce} Tile",
        "M" => "\u{f2d0} Monocle",
        "G" => "\u{f00a} Grid",
        "K" => "\u{f24d} Deck",
        "CT" => "\u{f0db} CenterTile",
        "RT" => "\u{f0db} RightTile",
        "VS" => "\u{f07d} VScroll",
        "VT" => "\u{f0ce} VTile",
        "VG" => "\u{f00a} VGrid",
        "VK" => "\u{f24d} VDeck",
        "VF" => "\u{f0db} VFair",
        _ => "",
    }
}

fn socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MANGO_INSTANCE_SIGNATURE")
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "sock")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("mango-"))
        })
        .last()
}

fn mon_from_value(value: &Json<'_>) -> Option<Mon> {
    let flag = |v: &Json<'_>, key| v.get(key).and_then(Json::as_bool).unwrap_or(false);
    let tags = value
        .get("tags")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|tag| {
            Some(Tag {
                index: u8::try_from(tag.get("index")?.as_u64()?).ok()?,
                active: flag(tag, "is_active"),
                urgent: flag(tag, "is_urgent"),
            })
        })
        .collect();
    let title = value
        .get("active_client")
        .and_then(|client| client.get("title"))
        .and_then(Json::as_str)
        .map(|title| title.chars().filter(|ch| !ch.is_control()).collect())
        .unwrap_or_default();
    Some(Mon {
        name: value.get("name")?.as_str()?.to_owned(),
        active: flag(value, "active"),
        title,
        layout_text: Cow::Borrowed(layout_text(
            value
                .get("layout_symbol")
                .and_then(Json::as_str)
                .unwrap_or(""),
        )),
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_symbol_f_is_fair() {
        assert_eq!(layout_text("F"), "\u{f0db} Fair");
        assert_eq!(layout_text("nope"), "");
    }

    #[test]
    fn snapshot_keeps_title_and_active_tag() {
        let mons = parse_monitors(
            r#"{"monitors":[{"name":"eDP-1","active":true,"layout_symbol":"F","tags":[{"index":1,"is_active":false,"is_urgent":false},{"index":2,"is_active":true,"is_urgent":true}],"active_client":{"title":"Cursor\u0007 Agents","appid":"cursor"}}]}"#,
        )
        .expect("parse");
        assert_eq!(mons[0].title, "Cursor Agents");
        assert!(mons[0].tags[1].active && mons[0].tags[1].urgent);
        assert_eq!(mons[0].layout_text, "\u{f0db} Fair");
    }

    #[test]
    fn newest_line_wins() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let mut mango = Mango {
            stream: ours,
            buf: Vec::new(),
        };
        let line = |name| format!(r#"{{"monitors":[{{"name":"{name}","tags":[]}}]}}"#);
        write!(theirs, "{}\n{}\n{{\"partial", line("old"), line("new")).unwrap();
        let mons = mango.on_readable().unwrap().unwrap();
        assert_eq!(mons[0].name, "new");
        assert_eq!(mango.buf, b"{\"partial");
    }
}
