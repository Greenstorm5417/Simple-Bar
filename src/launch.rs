//! Fire-and-forget helper processes for click actions, reaped so they never
//! linger as zombies.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const WPCTL: &str = "/usr/bin/wpctl";
const NIGHTLIGHT: &str = ".local/bin/nightlight";

/// Starts click helpers and collects them once they exit.
#[derive(Debug, Default)]
pub(crate) struct Launcher {
    children: Vec<Child>,
}

impl Launcher {
    /// Toggle the default sink's mute with `wpctl` (used when pulse is down).
    pub(crate) fn toggle_sink_mute(&mut self) {
        self.spawn(WPCTL, ["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]);
    }

    /// Toggle the default source's mute with `wpctl` (used when pulse is down).
    pub(crate) fn toggle_source_mute(&mut self) {
        self.spawn(WPCTL, ["set-mute", "@DEFAULT_AUDIO_SOURCE@", "toggle"]);
    }

    /// Change the default sink's volume by 5%, like waybar's scroll actions.
    pub(crate) fn step_volume(&mut self, up: bool) {
        let step = if up { "5%+" } else { "5%-" };
        self.spawn(WPCTL, ["set-volume", "@DEFAULT_AUDIO_SINK@", step]);
    }

    /// Run `~/.local/bin/nightlight toggle`.
    pub(crate) fn toggle_nightlight(&mut self) {
        if let Some(home) = std::env::var_os("HOME") {
            self.spawn(PathBuf::from(home).join(NIGHTLIGHT), ["toggle"]);
        }
    }

    /// Forget helpers that have exited, reaping them.
    pub(crate) fn reap(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    fn spawn<const N: usize>(&mut self, program: impl AsRef<OsStr>, args: [&str; N]) {
        let spawned = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        // A missing helper is not worth more than a skipped click.
        if let Ok(child) = spawned {
            self.children.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaps_exited_children() {
        let mut launcher = Launcher::default();
        launcher.spawn("/bin/true", []);
        assert_eq!(launcher.children.len(), 1);
        for _ in 0..100 {
            launcher.reap();
            if launcher.children.is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("child was never reaped");
    }
}
