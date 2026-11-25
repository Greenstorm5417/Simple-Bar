//! The battery module, ported from waybar's Linux battery code (v0.16) so the
//! percentage and the charging bolt agree with it in every state.
//!
//! Waybar does not show the kernel's `capacity` file, which truncates. It
//! computes `round(100 * charge_now / charge_full)` (or the `energy_*`
//! equivalent) per battery and averages, and it turns `Not charging` or
//! `Discharging` into `Plugged` when the adapter says it is online.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Where the kernel lists power supplies.
pub const POWER_SUPPLY: &str = "/sys/class/power_supply";

/// Battery charge state, as waybar names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Charging. Lowest priority when several batteries disagree.
    Charging,
    /// Running on battery.
    Discharging,
    /// On the adapter but held (for example at a charge threshold).
    NotCharging,
    /// Fully charged.
    Full,
    /// The kernel could not tell.
    Unknown,
    /// On the adapter while not charging; only ever derived, never read.
    Plugged,
}

impl Status {
    fn parse(text: &str) -> Self {
        match text.trim() {
            "Charging" => Self::Charging,
            "Discharging" => Self::Discharging,
            "Not charging" => Self::NotCharging,
            "Full" => Self::Full,
            _ => Self::Unknown,
        }
    }

    /// Waybar's config gives `format-charging` and `format-plugged` the bolt;
    /// every other status uses the plain `format` with a level icon.
    #[must_use]
    pub const fn shows_bolt(self) -> bool {
        matches!(self, Self::Charging | Self::Plugged)
    }
}

/// One battery reading, after waybar's adjustments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Charge in percent, 0–100.
    pub capacity: u8,
    /// Charge state.
    pub status: Status,
}

/// The kernel files of one battery, kept open for cheap re-reads.
#[derive(Debug)]
struct Cell {
    status: Option<File>,
    charge_now: Option<File>,
    charge_full: Option<File>,
    energy_now: Option<File>,
    energy_full: Option<File>,
    capacity: Option<File>,
}

impl Cell {
    fn open(dir: &Path) -> Self {
        let open = |name| File::open(dir.join(name)).ok();
        Self {
            status: open("status"),
            charge_now: open("charge_now"),
            charge_full: open("charge_full"),
            energy_now: open("energy_now"),
            energy_full: open("energy_full"),
            capacity: open("capacity"),
        }
    }

    /// Waybar's per-battery capacity: charge ratio, else energy ratio, else
    /// the kernel's `capacity`.
    fn capacity(&self) -> Option<u32> {
        let ratio = |now: &Option<File>, full: &Option<File>| {
            let (now, full) = (read_u32(now.as_ref()?)?, read_u32(full.as_ref()?)?);
            (full != 0).then(|| (100.0 * f64::from(now) / f64::from(full)).round() as u32)
        };
        ratio(&self.charge_now, &self.charge_full)
            .or_else(|| ratio(&self.energy_now, &self.energy_full))
            .or_else(|| read_u32(self.capacity.as_ref()?))
    }
}

/// The adapter waybar consults for `Plugged`.
#[derive(Debug)]
struct Adapter {
    online: Option<File>,
    status: Option<File>,
}

impl Adapter {
    fn open(dir: &Path) -> Self {
        Self {
            online: File::open(dir.join("online")).ok(),
            status: File::open(dir.join("status")).ok(),
        }
    }

    fn online(&self) -> bool {
        self.online
            .as_ref()
            .and_then(read_u32)
            .is_some_and(|v| v != 0)
    }

    /// Online and not itself discharging, waybar's test for `Plugged`.
    fn plugged(&self) -> bool {
        let mut buf = [0u8; 32];
        let discharging = self
            .status
            .as_ref()
            .and_then(|file| read_str(file, &mut buf))
            .is_some_and(|status| status.trim() == "Discharging");
        self.online() && !discharging
    }
}

/// All system batteries plus the adapter, discovered like waybar does.
#[derive(Debug, Default)]
pub struct Battery {
    cells: Vec<Cell>,
    adapter: Option<Adapter>,
}

impl Battery {
    /// Find the batteries and the adapter under [`POWER_SUPPLY`].
    ///
    /// # Errors
    /// Returns the error from listing the directory.
    pub fn discover() -> io::Result<Self> {
        Self::discover_in(Path::new(POWER_SUPPLY))
    }

    fn discover_in(root: &Path) -> io::Result<Self> {
        let mut battery = Self::default();
        // Directory order matters: waybar keeps the *last* supply that has
        // an `online` or `status` file as its adapter.
        for entry in fs::read_dir(root)? {
            let dir: PathBuf = entry?.path();
            if !dir.is_dir() {
                continue;
            }
            if is_system_battery(&dir) {
                battery.cells.push(Cell::open(&dir));
            }
            if dir.join("online").exists() || dir.join("status").exists() {
                battery.adapter = Some(Adapter::open(&dir));
            }
        }
        Ok(battery)
    }

    /// Read every battery. `None` when there is no battery to show.
    #[must_use]
    pub fn read(&self) -> Option<Reading> {
        if self.cells.is_empty() {
            return None;
        }
        let mut buf = [0u8; 32];
        let status = self
            .cells
            .iter()
            .map(|cell| {
                cell.status
                    .as_ref()
                    .and_then(|file| read_str(file, &mut buf))
                    .map_or(Status::Unknown, Status::parse)
            })
            .min()
            .unwrap_or(Status::Unknown);
        let total: u32 = self.cells.iter().filter_map(Cell::capacity).sum();
        let average = f64::from(total) / self.cells.len() as f64;
        Some(self.adjust(average.min(100.0).round() as u8, status))
    }

    /// Waybar's status rules on top of the raw kernel state.
    fn adjust(&self, capacity: u8, mut status: Status) -> Reading {
        let plugged = self.adapter.as_ref().is_some_and(Adapter::plugged);
        if matches!(status, Status::Discharging | Status::NotCharging) && plugged {
            status = Status::Plugged;
        }
        if capacity == 100 && status == Status::Charging {
            status = Status::Full;
        }
        if status == Status::Unknown && self.adapter.is_some() {
            status = if capacity == 100 {
                Status::Full
            } else if plugged {
                Status::Plugged
            } else {
                Status::Discharging
            };
        }
        Reading { capacity, status }
    }
}

/// A `type` of `Battery` with the files waybar requires, and not a
/// peripheral (`scope` of `Device`, such as a stylus).
fn is_system_battery(dir: &Path) -> bool {
    let read = |name| fs::read_to_string(dir.join(name)).unwrap_or_default();
    (dir.join("capacity").exists() || dir.join("charge_now").exists())
        && dir.join("uevent").exists()
        && dir.join("status").exists()
        && read("type").trim() == "Battery"
        && !read("scope").trim().eq_ignore_ascii_case("device")
}

fn read_str<'a>(file: &File, buf: &'a mut [u8]) -> Option<&'a str> {
    let n = file.read_at(buf, 0).ok()?;
    std::str::from_utf8(&buf[..n]).ok()
}

fn read_u32(file: &File) -> Option<u32> {
    let mut buf = [0u8; 24];
    read_str(file, &mut buf)?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `/sys/class/power_supply` in a temporary directory.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("simple-bar-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn supply(&self, name: &str, files: &[(&str, &str)]) -> &Self {
            let dir = self.0.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("uevent"), "").unwrap();
            for (file, value) in files {
                fs::write(dir.join(file), format!("{value}\n")).unwrap();
            }
            self
        }

        fn read(&self) -> Option<Reading> {
            Battery::discover_in(&self.0).unwrap().read()
        }

        /// A battery with an explicit adapter, independent of directory order.
        fn with_adapter(&self, cell: &str, adapter: &str) -> Battery {
            Battery {
                cells: vec![Cell::open(&self.0.join(cell))],
                adapter: Some(Adapter::open(&self.0.join(adapter))),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const BAT: &[(&str, &str)] = &[
        ("type", "Battery"),
        ("energy_now", "14710000"),
        ("energy_full", "57410000"),
        ("capacity", "25"),
    ];

    fn bat(status: &str) -> Vec<(&str, &str)> {
        let mut files = BAT.to_vec();
        files.push(("status", status));
        files
    }

    #[test]
    fn rounds_the_energy_ratio_instead_of_the_truncated_capacity() {
        let fx = Fixture::new("ratio");
        fx.supply("BAT1", &bat("Discharging"));
        let reading = fx.read().unwrap();
        assert_eq!(reading.capacity, 26, "14.71 / 57.41 Wh = 25.6%");
        assert_eq!(reading.status, Status::Discharging);
    }

    #[test]
    fn not_charging_on_the_adapter_is_plugged() {
        let fx = Fixture::new("plugged");
        fx.supply("BAT1", &bat("Not charging"))
            .supply("ACAD", &[("type", "Mains"), ("online", "1")]);
        let battery = fx.with_adapter("BAT1", "ACAD");
        assert_eq!(battery.read().unwrap().status, Status::Plugged);
        assert!(Status::Plugged.shows_bolt());
    }

    #[test]
    fn full_and_not_charging_off_adapter_show_the_level_icon() {
        let fx = Fixture::new("full");
        fx.supply("BAT1", &bat("Full"))
            .supply("ACAD", &[("type", "Mains"), ("online", "1")])
            .supply("OFF", &[("type", "Mains"), ("online", "0")]);
        let status = fx.with_adapter("BAT1", "ACAD").read().unwrap().status;
        assert_eq!(status, Status::Full);
        assert!(!status.shows_bolt());

        fx.supply("BAT2", &bat("Not charging"));
        let status = fx.with_adapter("BAT2", "OFF").read().unwrap().status;
        assert_eq!(status, Status::NotCharging);
        assert!(!status.shows_bolt());
    }

    #[test]
    fn device_scoped_batteries_are_ignored() {
        let fx = Fixture::new("scope");
        fx.supply(
            "hid-stylus",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("capacity", "0"),
                ("status", "Unknown"),
            ],
        );
        assert_eq!(fx.read(), None);
    }

    #[test]
    fn charging_at_100_is_full() {
        let fx = Fixture::new("charging-full");
        fx.supply(
            "BAT1",
            &[
                ("type", "Battery"),
                ("capacity", "100"),
                ("status", "Charging"),
            ],
        )
        .supply("ACAD", &[("type", "Mains"), ("online", "1")]);
        let reading = fx.with_adapter("BAT1", "ACAD").read().unwrap();
        assert_eq!(
            reading,
            Reading {
                capacity: 100,
                status: Status::Full
            }
        );
    }

    #[test]
    fn status_priority_matches_waybar() {
        // Unknown > Full > Not charging > Discharging > Charging; the
        // smallest wins across batteries.
        assert!(Status::Charging < Status::Discharging);
        assert!(Status::Discharging < Status::NotCharging);
        assert!(Status::NotCharging < Status::Full);
        assert!(Status::Full < Status::Unknown);
    }
}
