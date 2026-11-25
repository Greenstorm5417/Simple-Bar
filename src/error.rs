//! The errors that stop the bar from starting or running.

use std::fmt;

/// Why the bar could not start or had to stop.
#[derive(Debug)]
pub enum Error {
    /// No Wayland compositor to connect to.
    Connect(wayland_client::ConnectError),
    /// The initial registry roundtrip failed.
    Registry(wayland_client::globals::GlobalError),
    /// A required Wayland global is missing.
    Bind(&'static str, wayland_client::globals::BindError),
    /// Cairo could not set up text measurement.
    Cairo(cairo::Error),
    /// An OS call failed (timerfd, sockets).
    Io(std::io::Error),
    /// The event loop failed.
    EventLoop(calloop::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(_) => f.write_str("cannot connect to the Wayland compositor"),
            Self::Registry(_) => f.write_str("cannot read the Wayland registry"),
            Self::Bind(global, _) => write!(f, "the compositor does not offer {global}"),
            Self::Cairo(_) => f.write_str("cairo setup failed"),
            Self::Io(_) => f.write_str("system call failed"),
            Self::EventLoop(_) => f.write_str("event loop failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(err) => Some(err),
            Self::Registry(err) => Some(err),
            Self::Bind(_, err) => Some(err),
            Self::Cairo(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::EventLoop(err) => Some(err),
        }
    }
}

impl From<wayland_client::ConnectError> for Error {
    fn from(err: wayland_client::ConnectError) -> Self {
        Self::Connect(err)
    }
}

impl From<wayland_client::globals::GlobalError> for Error {
    fn from(err: wayland_client::globals::GlobalError) -> Self {
        Self::Registry(err)
    }
}

impl From<cairo::Error> for Error {
    fn from(err: cairo::Error) -> Self {
        Self::Cairo(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<calloop::Error> for Error {
    fn from(err: calloop::Error) -> Self {
        Self::EventLoop(err)
    }
}

impl<T> From<calloop::InsertError<T>> for Error {
    fn from(err: calloop::InsertError<T>) -> Self {
        Self::EventLoop(err.error)
    }
}

/// A specialized `Result` for starting and running the bar.
pub type Result<T, E = Error> = std::result::Result<T, E>;
