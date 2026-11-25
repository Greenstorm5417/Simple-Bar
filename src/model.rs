//! Bar state and the system readings behind it. Every text matches the
//! corresponding waybar module in `~/.config/waybar/config.jsonc`.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::Command;

use calloop::channel::Sender;

use crate::battery::{Battery, Reading};
use crate::json::{self, Json};
use crate::mango::Mon;
use crate::pulse::Device;

/// Speaker muted.
pub const ICON_MUTE: &str = "\u{f0581}";
/// Microphone live.
pub const ICON_MIC: &str = "\u{f036c}";
/// Microphone muted.
pub const ICON_MIC_MUTE: &str = "\u{f036d}";
/// Night light.
pub const ICON_NIGHT: &str = "\u{f0594}";
/// Thermometer.
pub const ICON_TEMP: &str = "\u{f2c9}";
/// Memory chip.
pub const ICON_MEM: &str = "\u{f035b}";
/// Tux.
pub const ICON_KERNEL: &str = "\u{f17c}";
/// Battery charging or plugged in.
pub const ICON_CHARGE: &str = "\u{f0084}";

/// Waybar's `format-icons` for the battery, empty to full.
const BATTERY_ICONS: [&str; 11] = [
    "\u{f0083}",
    "\u{f007a}",
    "\u{f007b}",
    "\u{f007c}",
    "\u{f007d}",
    "\u{f007e}",
    "\u{f007f}",
    "\u{f0080}",
    "\u{f0081}",
    "\u{f0082}",
    "\u{f0079}",
];

const NIGHTLIGHT_PID: &str = "green-nightlight.pid";
const KERNEL_SCRIPT: &str = ".local/bin/kernel-version-status";
const TEMP_CONFIGURED: &str = "/sys/class/hwmon/hwmon2/temp1_input";
const TEMP_FALLBACK: &str = "/sys/class/thermal/thermal_zone0/temp";

/// Kernel update state from the kernel status script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelClass {
    /// Running the newest installed kernel.
    Current,
    /// A newer kernel is installed.
    Update,
    /// The script could not tell.
    Unknown,
}

/// The kernel module's text and state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kernel {
    /// Module text.
    pub text: String,
    /// Module state.
    pub class: KernelClass,
}

/// Sensor files stay open; each refresh is a single `pread` at offset 0.
#[derive(Debug)]
struct Sensors {
    meminfo: Option<File>,
    temp: Option<File>,
    battery: Battery,
}

/// Everything the bars display.
#[derive(Debug)]
pub struct Model {
    /// Per-output state from Mango.
    pub mons: Vec<Mon>,
    /// Temperature text.
    pub temp: String,
    /// Memory text.
    pub mem: String,
    /// Battery text; empty when there is no battery.
    pub battery: String,
    /// Default sink muted.
    pub sink_muted: bool,
    /// Default source muted.
    pub source_muted: bool,
    /// Night light running.
    pub night: bool,
    /// Kernel module.
    pub kernel: Kernel,
    sensors: Sensors,
}

impl Model {
    /// Open the sensor files and take the first readings.
    #[must_use]
    pub fn open() -> Self {
        let temp_path = if std::fs::exists(TEMP_CONFIGURED).unwrap_or(false) {
            TEMP_CONFIGURED
        } else {
            TEMP_FALLBACK
        };
        let mut model = Self {
            mons: Vec::new(),
            temp: String::new(),
            mem: String::new(),
            battery: String::new(),
            sink_muted: false,
            source_muted: false,
            night: false,
            kernel: local_kernel(),
            sensors: Sensors {
                meminfo: File::open("/proc/meminfo").ok(),
                temp: File::open(temp_path).ok(),
                battery: Battery::discover().unwrap_or_default(),
            },
        };
        model.refresh_night();
        model.refresh_mem();
        model.refresh_temp();
        model.refresh_battery();
        model
    }

    /// The mute flag shown for a device.
    pub fn muted_mut(&mut self, device: Device) -> &mut bool {
        match device {
            Device::Sink => &mut self.sink_muted,
            Device::Source => &mut self.source_muted,
        }
    }

    /// Re-check the night light. Returns whether it changed.
    pub fn refresh_night(&mut self) -> bool {
        replace(&mut self.night, nightlight_on())
    }

    /// Re-read memory use. Returns whether the text changed.
    pub fn refresh_mem(&mut self) -> bool {
        let mut buf = [0u8; 4096];
        read0(self.sensors.meminfo.as_ref(), &mut buf)
            .and_then(memory_text)
            .is_some_and(|next| replace(&mut self.mem, next))
    }

    /// Re-read the temperature. Returns whether the text changed.
    pub fn refresh_temp(&mut self) -> bool {
        let mut buf = [0u8; 32];
        read0(self.sensors.temp.as_ref(), &mut buf)
            .and_then(temperature_text)
            .is_some_and(|next| replace(&mut self.temp, next))
    }

    /// Re-read the battery. Returns whether the text changed.
    pub fn refresh_battery(&mut self) -> bool {
        let text = self
            .sensors
            .battery
            .read()
            .map(battery_text)
            .unwrap_or_default();
        replace(&mut self.battery, text)
    }

    /// Look for batteries and adapters again, after a power supply was
    /// added or removed. Returns whether the text changed.
    pub fn rediscover_battery(&mut self) -> bool {
        if let Ok(battery) = Battery::discover() {
            self.sensors.battery = battery;
        }
        self.refresh_battery()
    }

    /// Show the clicked tag right away; Mango's next event confirms it.
    pub fn focus_tag(&mut self, output: &str, index: u8) {
        let pos = self
            .mons
            .iter()
            .position(|mon| mon.name == output)
            .or_else(|| self.mons.iter().position(|mon| mon.active))
            .unwrap_or(0);
        let Some(mon) = self.mons.get_mut(pos) else {
            return;
        };
        for tag in &mut mon.tags {
            tag.active = tag.index == index;
            if tag.active {
                tag.urgent = false;
            }
        }
    }

    /// The monitor state for an output, falling back to the focused one.
    #[must_use]
    pub fn mon_for(&self, output: &str) -> Option<&Mon> {
        self.mons
            .iter()
            .find(|mon| mon.name == output)
            .or_else(|| self.mons.iter().find(|mon| mon.active))
            .or_else(|| self.mons.first())
    }

    /// Store a kernel status. Returns whether it changed.
    pub fn set_kernel(&mut self, kernel: Kernel) -> bool {
        replace(&mut self.kernel, kernel)
    }
}

/// Store `next` in `slot`; returns whether that changed anything.
fn replace<T: PartialEq>(slot: &mut T, next: T) -> bool {
    let changed = *slot != next;
    if changed {
        *slot = next;
    }
    changed
}

fn read0<'a>(file: Option<&File>, buf: &'a mut [u8]) -> Option<&'a str> {
    let n = file?.read_at(buf, 0).ok()?;
    std::str::from_utf8(&buf[..n]).ok()
}

/// Run the kernel status script on a short-lived thread that exits after
/// sending its one result. If no thread can be started, the module keeps
/// showing the running kernel.
pub fn spawn_kernel(tx: Sender<Kernel>) {
    let _thread = std::thread::Builder::new()
        .name("kernel-status".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            if let Some(kernel) = kernel_from_script() {
                // The receiver only goes away while the bar is exiting.
                let _ = tx.send(kernel);
            }
        });
}

/// Waybar's default unit is GiB. `{used:0.1f}` prints that value after the
/// module's pre-round to two decimals. `used = MemTotal - MemAvailable`.
#[must_use]
pub fn format_used_gb(total_kib: u64, avail_kib: u64, zfs_kib: u64) -> String {
    let free = avail_kib.saturating_add(zfs_kib);
    let used = total_kib.saturating_sub(free) as f32;
    let gib = used / (1024.0 * 1024.0);
    let rounded = (gib * 100.0).round() / 100.0;
    format!("{rounded:.1}")
}

/// Index into waybar's `format-icons` for a charge percentage.
#[must_use]
pub fn battery_index(percent: u8) -> usize {
    let size = BATTERY_ICONS.len();
    let divisor = (100 / size).max(1);
    (usize::from(percent) / divisor).min(size - 1)
}

/// The battery module text: `format-charging`/`format-plugged` show the
/// bolt, every other status the level icon.
#[must_use]
pub fn battery_text(reading: Reading) -> String {
    let Reading { capacity, status } = reading;
    let icon = if status.shows_bolt() {
        ICON_CHARGE
    } else {
        BATTERY_ICONS[battery_index(capacity)]
    };
    format!("{icon} {capacity}%")
}

/// The memory module text from `/proc/meminfo` contents.
#[must_use]
pub fn memory_text(meminfo: &str) -> Option<String> {
    let mut total = 0u64;
    let mut avail = 0u64;
    let mut zfs = 0u64;
    for line in meminfo.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let slot = match key {
            "MemTotal:" => &mut total,
            "MemAvailable:" => &mut avail,
            "zfs_size:" => &mut zfs,
            _ => continue,
        };
        *slot = value.parse().unwrap_or(0);
    }
    (total != 0).then(|| format!("{ICON_MEM} {}GB", format_used_gb(total, avail, zfs)))
}

fn temperature_text(raw: &str) -> Option<String> {
    let milli: f64 = raw.trim().parse().ok()?;
    let celsius = (milli / 1000.0).round() as i32;
    Some(format!("{ICON_TEMP} {celsius}\u{b0}C"))
}

fn nightlight_on() -> bool {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    let mut buf = [0u8; 32];
    let pid = File::open(PathBuf::from(dir).join(NIGHTLIGHT_PID))
        .ok()
        .and_then(|file| read0(Some(&file), &mut buf)?.trim().parse::<i32>().ok());
    // SAFETY: `kill(pid, 0)` only checks that the process exists. It does not
    // signal it. A recycled pid is the same race the nightlight script has.
    pid.is_some_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
}

fn local_kernel() -> Kernel {
    let running = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let running = running.trim();
    let version = running.split('_').next().unwrap_or(running);
    Kernel {
        text: format!("{ICON_KERNEL} {version}"),
        class: KernelClass::Current,
    }
}

fn kernel_from_script() -> Option<Kernel> {
    let script = PathBuf::from(std::env::var_os("HOME")?).join(KERNEL_SCRIPT);
    let out = Command::new(script).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_kernel(&out.stdout)
}

fn parse_kernel(bytes: &[u8]) -> Option<Kernel> {
    let value = json::parse(std::str::from_utf8(bytes).ok()?.trim())?;
    let text = value.get("text")?.as_str()?.to_owned();
    let class = match value.get("class").and_then(Json::as_str) {
        Some("update-available") => KernelClass::Update,
        Some("unknown") => KernelClass::Unknown,
        _ => KernelClass::Current,
    };
    Some(Kernel { text, class })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::Status;

    #[test]
    fn used_gib_matches_waybar_one_decimal() {
        assert_eq!(format_used_gb(15_645_336, 12_595_448, 0), "2.9");
    }

    #[test]
    fn memory_text_from_meminfo() {
        let meminfo = "MemTotal:       15645336 kB\nMemFree: 1 kB\nMemAvailable:   12595448 kB\n";
        assert_eq!(memory_text(meminfo).unwrap(), format!("{ICON_MEM} 2.9GB"));
        assert_eq!(memory_text("junk"), None);
    }

    #[test]
    fn battery_icons_follow_waybar_divisor() {
        assert_eq!(battery_index(0), 0);
        assert_eq!(battery_index(8), 0);
        assert_eq!(battery_index(9), 1);
        assert_eq!(battery_index(86), 9);
        assert_eq!(battery_index(90), 10);
        assert_eq!(battery_index(100), 10);
    }

    #[test]
    fn battery_text_by_status() {
        let text = |capacity, status| battery_text(Reading { capacity, status });
        assert_eq!(text(40, Status::Charging), format!("{ICON_CHARGE} 40%"));
        assert_eq!(text(80, Status::Plugged), format!("{ICON_CHARGE} 80%"));
        assert_eq!(
            text(40, Status::Discharging),
            format!("{} 40%", BATTERY_ICONS[4])
        );
        assert_eq!(
            text(100, Status::Full),
            format!("{} 100%", BATTERY_ICONS[10])
        );
        assert_eq!(
            text(80, Status::NotCharging),
            format!("{} 80%", BATTERY_ICONS[8])
        );
    }

    #[test]
    fn temperature_rounds_millidegrees() {
        assert_eq!(
            temperature_text("38500\n").unwrap(),
            format!("{ICON_TEMP} 39\u{b0}C")
        );
    }

    #[test]
    fn kernel_class_from_script_json() {
        let kernel =
            parse_kernel(br#"{"text":"linux 7.3.6","tooltip":"x","class":"update-available"}"#)
                .expect("json");
        assert_eq!(kernel.class, KernelClass::Update);
        assert_eq!(kernel.text, "linux 7.3.6");
    }
}
