# simple-bar

A minimal, pixel-exact stand-in for the waybar setup in `~/.config/waybar`,
for the [Mango](https://github.com/DreamMaoMao/mangowc) Wayland compositor.
It draws the same capsules, fonts, colors and spacing with cairo and pango on
a wlr layer surface, without GTK and without a thread or child process per
module.

| | waybar | simple-bar |
|---|---|---|
| Resident memory | ~69 MB (17 MB private) | ~21 MB (2.8 MB private) |
| Threads | 18 | 1 (+ pango's idle fontconfig worker) |
| CPU while idle | — | ~0.1% (6 scheduler ticks per minute) |
| Clock tick | — | 9 µs of drawing |

## Modules

Left: tray, temperature, memory, window title. Center: workspaces.
Right: layout, speaker/mic mute, night light, battery, kernel version, clock.
Every text and click action matches the waybar config it replaces.

## How it stays small

* **One thread, one event loop.** Mango IPC, the PulseAudio socket, battery
  uevents, the tray's D-Bus connection and the clock are file descriptors in
  a single [calloop](https://docs.rs/calloop) loop. Nothing polls.
* **Native protocols instead of helpers.** Mute state comes from a tiny
  PulseAudio native-protocol client (`src/pulse.rs`), not from `pactl` or
  `wpctl` processes. Battery changes arrive as netlink uevents
  (`src/uevent.rs`). Mango snapshots are parsed by a small borrowing JSON
  reader (`src/json.rs`).
* **Damage-tracked painting.** Each frame is laid out into capsules
  ("pills") and diffed against the previous one. A capsule whose outline
  is unchanged gets only its interior repainted (a flat fill plus text,
  no rounded-path rasterization); anything that moved is repainted as a
  span. All of it happens in place, in the shm buffer already on screen.
* **Exact clock ticks.** One `CLOCK_REALTIME` timerfd fires on every whole
  second with no re-arming, and the text is formatted from a precise clock
  reading (libc's `time()` reads a coarse clock that lags the boundary).
* **Waybar's battery math.** The percentage is waybar's rounded
  `charge_now / charge_full` (or `energy_*`) ratio, not the kernel's
  truncated `capacity` file, and the bolt follows waybar's
  `Charging`/`Plugged`/`Full` rules (`src/battery.rs`). Waybar only
  re-reads every 60 s, so between udev events it can lag simple-bar's
  5 s refresh by a percent.
* **Same rendering pipeline as GTK3.** The bar renders at the next integer
  scale and lets the compositor downscale, exactly like waybar does under
  fractional scaling, so text metrics and hinting match pixel for pixel.

## Building

Requires Rust 1.88+, and the cairo, pango and libdbus development files.

```sh
cargo build --release
./target/release/simple-bar
```

The font from the CSS, `JetBrainsMono Nerd Font`, must be installed.

## Development

```sh
cargo fmt --check                     # formatting (rustfmt.toml)
cargo clippy --all-targets            # lints (configured in Cargo.toml)
cargo test                            # unit tests
cargo test -- --ignored               # tests that need a live pulse server
cargo bench                           # criterion benchmarks (benches/bar.rs)
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

Lints are strict: `clippy::all` is denied and `clippy::pedantic` warns, with
only the pixel-math cast lints allowed (see `Cargo.toml`). CI treats warnings
as errors.

### Profiling

`perf` needs `kernel.perf_event_paranoid` ≤ 2. Without it, valgrind works on
the real bar: run it twice under `valgrind --tool=callgrind` (e.g. 12 s and
42 s) and subtract the per-function costs to isolate steady state from
startup. `valgrind --tool=dhat` shows heap churn and what stays live.

## Layout

| Path | Purpose |
|---|---|
| `src/app.rs` | Wayland surfaces, input, and the event loop |
| `src/paint.rs` | Layout, damage tracking, and cairo/pango painting |
| `src/model.rs` | Bar state and sysfs/procfs readings |
| `src/battery.rs` | Waybar's battery percentage and status rules |
| `src/launch.rs` | Click helpers (`wpctl`, night light), reaped |
| `src/mango.rs` | Mango IPC stream and dispatch |
| `src/pulse.rs` | PulseAudio native-protocol client |
| `src/tray.rs` | StatusNotifierItem host/watcher |
| `src/uevent.rs` | Netlink power-supply events |
| `src/sock.rs` | Non-blocking socket reads |
| `src/error.rs` | Startup and runtime errors |
| `src/clock.rs` | Whole-second timerfd and clock text |
| `src/json.rs` | Minimal JSON reader |
| `benches/bar.rs` | Parsing, layout and painting benchmarks |
