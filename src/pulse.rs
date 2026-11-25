//! Tiny `PulseAudio` native-protocol client (served by pipewire-pulse).
//!
//! It subscribes to sink, source and server events and asks for the default
//! sink's and source's mute flags whenever one fires. There is no `pactl`
//! process and no `wpctl` fork per event; a mute flip is one round trip on
//! a socket that is already open.

use std::ffi::OsString;
use std::io::{self, ErrorKind, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use crate::sock;

const CMD_ERROR: u32 = 0;
const CMD_REPLY: u32 = 2;
const CMD_AUTH: u32 = 8;
const CMD_SET_CLIENT_NAME: u32 = 9;
const CMD_GET_SINK_INFO: u32 = 21;
const CMD_GET_SOURCE_INFO: u32 = 23;
const CMD_SUBSCRIBE: u32 = 35;
const CMD_SET_SINK_MUTE: u32 = 39;
const CMD_SET_SOURCE_MUTE: u32 = 40;
const CMD_SUBSCRIBE_EVENT: u32 = 66;

const PROTOCOL_VERSION: u32 = 32;
const INVALID: u32 = u32::MAX;
const MASK_SINK: u32 = 0x1;
const MASK_SOURCE: u32 = 0x2;
const MASK_SERVER: u32 = 0x80;
const FACILITY_MASK: u32 = 0xf;
const FACILITY_SINK: u32 = 0;
const FACILITY_SOURCE: u32 = 1;
const FACILITY_SERVER: u32 = 7;
const HEADER: usize = 20;
const COOKIE_LEN: usize = 256;
const DEFAULT_SINK: &str = "@DEFAULT_SINK@";
const DEFAULT_SOURCE: &str = "@DEFAULT_SOURCE@";

/// Which default device a request is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    /// The default output.
    Sink,
    /// The default input.
    Source,
}

/// A connection to the pulse server that tracks both mute flags.
#[derive(Debug)]
pub struct Pulse {
    stream: UnixStream,
    rbuf: Vec<u8>,
    next_tag: u32,
    sink_tag: Option<u32>,
    source_tag: Option<u32>,
    requery: bool,
    sink_muted: Option<bool>,
    source_muted: Option<bool>,
}

impl AsFd for Pulse {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

impl Pulse {
    /// Connect to the pulse socket (see [`socket_path`]), authenticate, subscribe,
    /// and ask for the initial mute flags. The answers arrive through
    /// [`Pulse::on_readable`].
    ///
    /// # Errors
    /// Fails when `XDG_RUNTIME_DIR` is unset or the socket cannot be used.
    pub fn connect() -> io::Result<Self> {
        let path = socket_path(|name| std::env::var_os(name)).ok_or_else(|| {
            io::Error::new(ErrorKind::NotFound, "no PulseAudio socket configured")
        })?;
        let stream = UnixStream::connect(path)?;
        let mut pulse = Self {
            stream,
            rbuf: Vec::with_capacity(512),
            next_tag: 0,
            sink_tag: None,
            source_tag: None,
            requery: false,
            sink_muted: None,
            source_muted: None,
        };

        let mut auth = pulse.request(CMD_AUTH);
        auth.u32(PROTOCOL_VERSION).arbitrary(&cookie());
        pulse.send(auth)?;

        let mut name = pulse.request(CMD_SET_CLIENT_NAME);
        name.proplist(&[("application.name", "simple-bar")]);
        pulse.send(name)?;

        let mut subscribe = pulse.request(CMD_SUBSCRIBE);
        subscribe.u32(MASK_SINK | MASK_SOURCE | MASK_SERVER);
        pulse.send(subscribe)?;

        pulse.query()?;
        pulse.stream.set_nonblocking(true)?;
        Ok(pulse)
    }

    /// A second handle on the socket, for registering with the event loop.
    #[must_use]
    pub fn try_clone_stream(&self) -> Option<UnixStream> {
        self.stream.try_clone().ok()
    }

    /// Mute flag of the default device, once the server has answered.
    #[must_use]
    pub const fn muted(&self, device: Device) -> Option<bool> {
        match device {
            Device::Sink => self.sink_muted,
            Device::Source => self.source_muted,
        }
    }

    /// Handle everything the server sent. Returns whether a mute flag changed.
    ///
    /// # Errors
    /// Returns an error when the connection is closed or broken; the caller
    /// drops this client and reconnects later.
    pub fn on_readable(&mut self) -> io::Result<bool> {
        sock::read_available(&mut self.stream, &mut self.rbuf)?;
        let buf = std::mem::take(&mut self.rbuf);
        let mut changed = false;
        let mut off = 0;
        while let Some(header) = buf.get(off..off + HEADER) {
            let len = be32(header) as usize;
            let channel = be32(&header[4..]);
            let Some(payload) = buf.get(off + HEADER..off + HEADER + len) else {
                break;
            };
            off += HEADER + len;
            if channel == INVALID {
                changed |= self.on_packet(payload)?;
            }
        }
        self.rbuf = buf;
        self.rbuf.drain(..off);
        Ok(changed)
    }

    /// Set the mute flag of a default device. Returns false when the request
    /// could not be sent.
    pub fn set_mute(&mut self, device: Device, mute: bool) -> bool {
        let (cmd, name) = match device {
            Device::Sink => (CMD_SET_SINK_MUTE, DEFAULT_SINK),
            Device::Source => (CMD_SET_SOURCE_MUTE, DEFAULT_SOURCE),
        };
        let mut req = self.request(cmd);
        req.u32(INVALID).string(name).bool(mute);
        self.send(req).is_ok()
    }

    fn on_packet(&mut self, packet: &[u8]) -> io::Result<bool> {
        let mut r = Reader::new(packet);
        let (Some(cmd), Some(tag)) = (r.u32(), r.u32()) else {
            return Err(ErrorKind::InvalidData.into());
        };
        match cmd {
            CMD_SUBSCRIBE_EVENT => {
                let facility = r.u32().unwrap_or(0) & FACILITY_MASK;
                if matches!(facility, FACILITY_SINK | FACILITY_SOURCE | FACILITY_SERVER) {
                    self.query()?;
                }
                Ok(false)
            }
            CMD_REPLY | CMD_ERROR => {
                let device = if self.sink_tag == Some(tag) {
                    self.sink_tag = None;
                    Device::Sink
                } else if self.source_tag == Some(tag) {
                    self.source_tag = None;
                    Device::Source
                } else {
                    return Ok(false);
                };
                // An error (no such device) keeps the last known flag.
                let muted = if cmd == CMD_REPLY {
                    info_muted(&mut r)
                } else {
                    None
                };
                let slot = match device {
                    Device::Sink => &mut self.sink_muted,
                    Device::Source => &mut self.source_muted,
                };
                let changed = muted.is_some() && *slot != muted;
                if muted.is_some() {
                    *slot = muted;
                }
                if self.requery && self.sink_tag.is_none() && self.source_tag.is_none() {
                    self.requery = false;
                    self.query()?;
                }
                Ok(changed)
            }
            _ => Ok(false),
        }
    }

    /// Ask for both mute flags. A burst of events while a query is in
    /// flight collapses into a single follow-up query.
    fn query(&mut self) -> io::Result<()> {
        if self.sink_tag.is_some() || self.source_tag.is_some() {
            self.requery = true;
            return Ok(());
        }
        let sink = self.next_tag;
        let mut req = self.request(CMD_GET_SINK_INFO);
        req.u32(INVALID).string(DEFAULT_SINK);
        self.send(req)?;
        self.sink_tag = Some(sink);

        let source = self.next_tag;
        let mut req = self.request(CMD_GET_SOURCE_INFO);
        req.u32(INVALID).string(DEFAULT_SOURCE);
        self.send(req)?;
        self.source_tag = Some(source);
        Ok(())
    }

    fn request(&mut self, cmd: u32) -> Packet {
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1) & 0x7fff_ffff;
        let mut packet = Packet::new();
        packet.u32(cmd).u32(tag);
        packet
    }

    fn send(&mut self, packet: Packet) -> io::Result<()> {
        let frame = packet.finish();
        // Requests are tiny and the socket buffer always has room; a
        // transient WouldBlock just retries.
        let mut sent = 0;
        while sent < frame.len() {
            match self.stream.write(&frame[sent..]) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(n) => sent += n,
                Err(err)
                    if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

/// Where libpulse would connect: the first local server in `PULSE_SERVER`,
/// else `native` in `PULSE_RUNTIME_PATH`, else `$XDG_RUNTIME_DIR/pulse/native`.
/// A session with its own runtime directory (such as dwl's) shares the
/// login session's server through these variables.
pub fn socket_path(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(servers) = var("PULSE_SERVER")
        && let Some(path) = servers.to_str().and_then(local_server)
    {
        return Some(path);
    }
    if let Some(dir) = var("PULSE_RUNTIME_PATH") {
        return Some(PathBuf::from(dir).join("native"));
    }
    var("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("pulse/native"))
}

/// The first `unix:` (or bare absolute path) entry of a `PULSE_SERVER` list.
/// Entries may carry a `{machine-id}` prefix.
fn local_server(servers: &str) -> Option<PathBuf> {
    servers.split_whitespace().find_map(|server| {
        let server = match server.strip_prefix('{') {
            Some(rest) => rest.split_once('}')?.1,
            None => server,
        };
        let path = server.strip_prefix("unix:").unwrap_or(server);
        path.starts_with('/').then(|| PathBuf::from(path))
    })
}

/// Read the mute flag from a `GET_SINK_INFO`/`GET_SOURCE_INFO` reply body
/// (everything after the command and tag).
#[must_use]
pub fn reply_muted(body: &[u8]) -> Option<bool> {
    info_muted(&mut Reader::new(body))
}

/// Sink and source info both start: index, name, description, sample spec,
/// channel map, owner module, volume, mute.
fn info_muted(r: &mut Reader<'_>) -> Option<bool> {
    for _ in 0..7 {
        r.skip()?;
    }
    r.bool()
}

/// The auth cookie. pipewire-pulse ignores it; a real pulseaudio needs it.
fn cookie() -> Vec<u8> {
    std::env::var_os("PULSE_COOKIE")
        .map(PathBuf::from)
        .or_else(|| Some(Path::new(&std::env::var_os("HOME")?).join(".config/pulse/cookie")))
        .and_then(|path| std::fs::read(path).ok())
        .filter(|cookie| cookie.len() == COOKIE_LEN)
        .unwrap_or_else(|| vec![0; COOKIE_LEN])
}

/// A framed control packet: the 20-byte descriptor, then a tagstruct.
struct Packet(Vec<u8>);

impl Packet {
    fn new() -> Self {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&[0; HEADER]);
        Self(buf)
    }

    fn u32(&mut self, value: u32) -> &mut Self {
        self.0.push(b'L');
        self.0.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn string(&mut self, value: &str) -> &mut Self {
        self.0.push(b't');
        self.0.extend_from_slice(value.as_bytes());
        self.0.push(0);
        self
    }

    fn bool(&mut self, value: bool) -> &mut Self {
        self.0.push(if value { b'1' } else { b'0' });
        self
    }

    fn arbitrary(&mut self, data: &[u8]) -> &mut Self {
        self.0.push(b'x');
        self.0.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.0.extend_from_slice(data);
        self
    }

    fn proplist(&mut self, entries: &[(&str, &str)]) -> &mut Self {
        self.0.push(b'P');
        for (key, value) in entries {
            let len = value.len() as u32 + 1;
            self.string(key).u32(len);
            self.0.push(b'x');
            self.0.extend_from_slice(&len.to_be_bytes());
            self.0.extend_from_slice(value.as_bytes());
            self.0.push(0);
        }
        self.0.push(b'N');
        self
    }

    /// Fill in the descriptor: length, control channel, no offset, no flags.
    fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - HEADER) as u32;
        self.0[..4].copy_from_slice(&len.to_be_bytes());
        self.0[4..8].copy_from_slice(&INVALID.to_be_bytes());
        self.0
    }
}

const fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Sequential reader over a tagstruct.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn byte(&mut self) -> Option<u8> {
        let value = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(value)
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let bytes = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }

    fn raw32(&mut self) -> Option<u32> {
        self.take(4).map(be32)
    }

    fn u32(&mut self) -> Option<u32> {
        if self.byte()? != b'L' {
            return None;
        }
        self.raw32()
    }

    fn bool(&mut self) -> Option<bool> {
        match self.byte()? {
            b'1' => Some(true),
            b'0' => Some(false),
            _ => None,
        }
    }

    fn skip(&mut self) -> Option<()> {
        let tag = self.byte()?;
        self.skip_value(tag)
    }

    /// Skip one value whose type tag was already read.
    fn skip_value(&mut self, tag: u8) -> Option<()> {
        let len = match tag {
            b'1' | b'0' | b'N' => 0,
            b'B' => 1,
            b'L' | b'V' => 4,
            b'a' => 6,
            b'R' | b'r' | b'U' | b'T' => 8,
            b'x' => self.raw32()? as usize,
            b'm' => usize::from(self.byte()?),
            b'v' => usize::from(self.byte()?) * 4,
            b't' => self.buf.get(self.pos..)?.iter().position(|&c| c == 0)? + 1,
            b'P' => {
                // Key string, length, arbitrary value; until a null string.
                loop {
                    match self.byte()? {
                        b'N' => return Some(()),
                        b't' => {
                            self.skip_value(b't')?;
                            self.skip()?;
                            self.skip()?;
                        }
                        _ => return None,
                    }
                }
            }
            b'f' => {
                self.skip()?;
                return self.skip();
            }
            _ => return None,
        };
        self.take(len).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink-info reply body as pipewire-pulse sends it, cut after `mute`.
    fn sink_info(muted: bool) -> Vec<u8> {
        let mut p = Packet(Vec::new());
        p.u32(5).string("alsa_output").string("Speakers");
        p.0.extend_from_slice(&[b'a', 3, 2, 0, 0, 0xbb, 0x80]);
        p.0.extend_from_slice(&[b'm', 2, 1, 2]);
        p.u32(INVALID);
        p.0.extend_from_slice(&[b'v', 2, 0, 1, 0, 0, 0, 1, 0, 0]);
        p.bool(muted);
        p.0
    }

    /// A fake environment for [`socket_path`].
    fn env<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn finds_the_socket_like_libpulse() {
        let base = [("XDG_RUNTIME_DIR", "/run/user/1000/dwl")];
        assert_eq!(
            socket_path(env(&base)),
            Some("/run/user/1000/dwl/pulse/native".into())
        );
        let dwl = [
            ("XDG_RUNTIME_DIR", "/run/user/1000/dwl"),
            ("PULSE_RUNTIME_PATH", "/run/user/1000/pulse"),
            ("PULSE_SERVER", "unix:/run/user/1000/pulse/native"),
        ];
        assert_eq!(
            socket_path(env(&dwl)),
            Some("/run/user/1000/pulse/native".into())
        );
        let runtime_only = [("PULSE_RUNTIME_PATH", "/run/user/1000/pulse")];
        assert_eq!(
            socket_path(env(&runtime_only)),
            Some("/run/user/1000/pulse/native".into())
        );
        let remote_first = [("PULSE_SERVER", "tcp:host:4713 {abc}unix:/tmp/p/native")];
        assert_eq!(
            socket_path(env(&remote_first)),
            Some("/tmp/p/native".into())
        );
    }

    #[test]
    fn reads_mute_after_seven_fields() {
        assert_eq!(reply_muted(&sink_info(true)), Some(true));
        assert_eq!(reply_muted(&sink_info(false)), Some(false));
    }

    #[test]
    fn truncated_reply_is_rejected() {
        let body = sink_info(true);
        assert_eq!(reply_muted(&body[..body.len() - 1]), None);
    }

    #[test]
    fn skips_proplists() {
        let mut p = Packet(Vec::new());
        p.proplist(&[("a", "b"), ("key", "value")]).bool(true);
        let mut r = Reader::new(&p.0);
        r.skip().unwrap();
        assert_eq!(r.bool(), Some(true));
    }

    #[test]
    fn frames_carry_length_and_control_channel() {
        let mut p = Packet::new();
        p.u32(7);
        let frame = p.finish();
        assert_eq!(be32(&frame), 5);
        assert_eq!(be32(&frame[4..]), INVALID);
    }

    #[test]
    #[ignore = "needs a running pulse server"]
    fn live_pulse() {
        let mut pulse = Pulse::connect().expect("connect to pulse");
        for _ in 0..50 {
            let _ = pulse.on_readable();
            if pulse.muted(Device::Sink).is_some() && pulse.muted(Device::Source).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (sink, source) = (pulse.muted(Device::Sink), pulse.muted(Device::Source));
        eprintln!("sink {sink:?} source {source:?}");
        assert!(sink.is_some() && source.is_some());
    }
}
