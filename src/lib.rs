//! simple-bar: a pixel-exact stand-in for the waybar configured in
//! `~/.config/waybar`, drawn with cairo and pango on a wlr layer surface.
//!
//! Everything runs on one thread in one [`calloop`] loop. Mango IPC, the
//! `PulseAudio` socket, power-supply uevents, the tray's D-Bus connection and
//! the clock are file descriptors in that loop, so the process sleeps until
//! one of them has something to say and then repaints only what changed.

mod app;
pub mod battery;
pub mod clock;
pub mod dwl;
mod error;
pub mod json;
mod launch;
pub mod mango;
pub mod model;
pub mod paint;
pub mod pulse;
mod sock;
pub mod tray;
mod uevent;

pub use app::run;
pub use error::{Error, Result};
