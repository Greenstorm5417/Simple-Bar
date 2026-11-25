//! The Wayland side: layer surfaces, input, and the event loop that ties
//! every source together.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use calloop::channel::Event as ChannelEvent;
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

use crate::clock::{self, SecondTimer};
use crate::dwl::{self, OutputState as DwlState, ZdwlIpcManagerV2, ZdwlIpcOutputV2};
use crate::error::{Error, Result};
use crate::launch::Launcher;
use crate::mango::{self, Mango};
use crate::model::{self, Kernel, Model};
use crate::paint::{self, Canvas, EmptyTitle, Frame, Hit, HitRect, Painter, Pill};
use crate::pulse::{Device, Pulse};
use crate::tray::{Tray, TrayIcon};
use crate::uevent::{PowerEvents, Uevents};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// Memory refresh, waybar's `memory` interval; night light and battery
/// ride along (waybar polls the battery only every 60 s, so it can lag).
const SENSORS_EVERY: u32 = 5;
/// Waybar's `temperature` default interval.
const TEMP_EVERY: u32 = 10;
/// Retry lost IPC connections this often.
const RECONNECT_EVERY: u32 = 2;
const KERNEL_EVERY: Duration = Duration::from_secs(900);
/// The adapter's uevent can arrive before the battery updates its status.
const BATTERY_SETTLE: Duration = Duration::from_millis(1500);
/// How long a new surface waits for its preferred fractional scale.
const SCALE_GRACE: Duration = Duration::from_millis(300);

type Handle = LoopHandle<'static, App>;

/// Identifies a bar across output hotplug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BarId(u64);

/// Buffer size and scale that the stored pills were painted at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    width: i32,
    height: i32,
    scale: u32,
}

/// What the compositor told us about a surface's scale.
#[derive(Clone, Copy, Debug)]
struct Scale {
    /// The output's integer scale.
    integer: u32,
    /// The fractional-scale protocol's preferred scale, in 120ths.
    preferred: Option<u32>,
}

impl Scale {
    /// GTK3 (waybar) has no fractional scaling: it lays out and draws at the
    /// next integer scale and the compositor downsamples. Rendering the same
    /// way keeps text metrics, hinting and pixel snapping identical.
    fn render(self) -> u32 {
        self.preferred
            .map_or(self.integer, |p| p.div_ceil(120))
            .max(1)
    }
}

/// How the bar learns about workspaces, the title and the layout.
enum Ipc {
    /// Mango's `watch all-monitors` socket; `connected` is false while it
    /// is being retried.
    Mango { connected: bool },
    /// dwl's `dwl-ipc-unstable-v2` Wayland protocol.
    Dwl(ZdwlIpcManagerV2),
}

/// A bar's dwl IPC object and the batch of events it is collecting.
struct DwlOutput {
    proxy: ZdwlIpcOutputV2,
    state: DwlState,
}

/// Where the pointer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hover {
    bar: BarId,
    target: Option<Hit>,
}

struct Bar {
    id: BarId,
    output: wl_output::WlOutput,
    name: String,
    output_width: i32,
    layer: LayerSurface,
    viewport: Option<WpViewport>,
    /// Held so the compositor keeps sending preferred-scale events.
    _fractional: Option<WpFractionalScaleV1>,
    /// Logical size from the last configure; zero until configured.
    size: (u32, u32),
    configured: bool,
    scale: Scale,
    created: Instant,
    pool: SlotPool,
    buffer: Option<Buffer>,
    painted: Option<Geometry>,
    pills: Vec<Pill>,
    hits: Vec<HitRect>,
    dwl: Option<DwlOutput>,
}

impl Bar {
    fn hit_at(&self, x: f64, y: f64) -> Option<Hit> {
        self.hits
            .iter()
            .find(|hit| hit.rect.contains(x, y))
            .map(|hit| hit.hit)
    }

    /// Paint `pills` and commit. Repaints only changed pills, in place,
    /// when the buffer on screen has been released and the geometry holds.
    fn present(&mut self, painter: &mut Painter, pills: Vec<Pill>, tray: &[TrayIcon]) {
        let (width, height) = self.size;
        let scale = self.scale.render();
        let geometry = Geometry {
            width: (width * scale) as i32,
            height: (height * scale) as i32,
            scale,
        };
        let stride = geometry.width * 4;
        let mut repaint = if self.painted == Some(geometry) {
            let changed = paint::damage(&self.pills, &pills);
            if changed.is_empty() {
                return;
            }
            Some(changed)
        } else {
            None
        };
        let reusable = repaint.is_some()
            && self
                .buffer
                .as_ref()
                .is_some_and(|buffer| buffer.canvas(&mut self.pool).is_some());
        if !reusable {
            repaint = None;
            let Ok((buffer, _)) = self.pool.create_buffer(
                geometry.width,
                geometry.height,
                stride,
                wl_shm::Format::Argb8888,
            ) else {
                return;
            };
            self.buffer = Some(buffer);
            if let Some(viewport) = &self.viewport {
                viewport.set_destination(width as i32, height as i32);
            }
            // The viewport, not the buffer scale, maps buffer to surface.
            let _ = self.layer.set_buffer_scale(1);
        }
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        let Some(pixels) = buffer.canvas(&mut self.pool) else {
            return;
        };
        let canvas = Canvas {
            pixels,
            width: geometry.width,
            height: geometry.height,
            stride,
        };
        painter.set_scale(f64::from(scale));
        let Ok(damage) = painter.paint(canvas, f64::from(height), &pills, repaint.as_ref(), tray)
        else {
            return;
        };
        let surface = self.layer.wl_surface();
        if buffer.attach_to(surface).is_err() {
            return;
        }
        for rect in damage {
            surface.damage_buffer(rect.x, rect.y, rect.width, rect.height);
        }
        self.layer.commit();
        self.pills = pills;
        self.painted = Some(geometry);
    }
}

struct App {
    handle: Handle,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    fractional_mgr: Option<WpFractionalScaleManagerV1>,
    viewporter: Option<WpViewporter>,
    cursor_mgr: Option<WpCursorShapeManagerV1>,
    cursor_dev: Option<WpCursorShapeDeviceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    bars: Vec<Bar>,
    next_id: u64,
    bar_height: u32,
    painter: Painter,
    model: Model,
    pulse: Option<Pulse>,
    ipc: Ipc,
    tray: Option<Tray>,
    launcher: Launcher,
    clock: String,
    hover: Option<Hover>,
    dirty: bool,
    exit: bool,
    tick: u32,
}

/// Connect to the compositor and run the bar until every output is gone.
///
/// # Errors
/// Fails when the compositor is unreachable, lacks a required global
/// (compositor, layer shell, shm), or the event loop breaks.
pub fn run() -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;
    let mut app = App::new(&globals, &qh, event_loop.handle())?;
    WaylandSource::new(conn, queue).insert(event_loop.handle())?;
    app.start_sources()?;

    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
        if std::mem::take(&mut app.dirty) {
            app.draw();
        }
    }
    Ok(())
}

impl App {
    fn new(globals: &GlobalList, qh: &QueueHandle<Self>, handle: Handle) -> Result<Self> {
        let bind = |name| move |err| Error::Bind(name, err);
        let compositor = CompositorState::bind(globals, qh).map_err(bind("wl_compositor"))?;
        let layer_shell = LayerShell::bind(globals, qh).map_err(bind("zwlr_layer_shell_v1"))?;
        let shm = Shm::bind(globals, qh).map_err(bind("wl_shm"))?;
        let fractional_mgr = globals.bind(qh, 1..=1, GlobalUd).ok();
        let viewporter = globals.bind(qh, 1..=1, GlobalUd).ok();
        let cursor_mgr = globals.bind(qh, 1..=1, GlobalUd).ok();
        // dwl advertises its IPC global; anything else is taken to be Mango.
        let ipc = globals
            .bind::<ZdwlIpcManagerV2, _, _>(qh, 1..=2, GlobalUd)
            .map_or(Ipc::Mango { connected: false }, Ipc::Dwl);
        let mut painter = Painter::new()?;
        let mut clock = String::new();
        clock::write(&mut clock);
        Ok(Self {
            handle,
            registry_state: RegistryState::new(globals),
            seat_state: SeatState::new(globals, qh),
            output_state: OutputState::new(globals, qh),
            shm,
            compositor,
            layer_shell,
            fractional_mgr,
            viewporter,
            cursor_mgr,
            cursor_dev: None,
            pointer: None,
            bars: Vec::new(),
            next_id: 0,
            bar_height: painter.bar_height(),
            painter,
            model: Model::open(),
            pulse: None,
            ipc,
            tray: Tray::open().ok(),
            launcher: Launcher::default(),
            clock,
            hover: None,
            dirty: true,
            exit: false,
            tick: 0,
        })
    }

    /// Register every event source. IPC sources that are down now are
    /// retried from the clock tick.
    fn start_sources(&mut self) -> Result<()> {
        self.connect_pulse();
        if matches!(self.ipc, Ipc::Mango { .. }) {
            self.connect_mango();
        }
        self.watch_tray();
        self.watch_power();

        let timer = SecondTimer::new()?;
        self.handle.insert_source(
            Generic::new(timer, Interest::READ, Mode::Level),
            |_, timer, app| {
                if timer.acknowledge() {
                    app.on_second();
                }
                Ok(PostAction::Continue)
            },
        )?;

        let (kernel_tx, kernel_rx) = calloop::channel::channel::<Kernel>();
        self.handle.insert_source(kernel_rx, |event, (), app| {
            if let ChannelEvent::Msg(kernel) = event {
                app.dirty |= app.model.set_kernel(kernel);
            }
        })?;
        model::spawn_kernel(kernel_tx.clone());
        self.handle
            .insert_source(Timer::from_duration(KERNEL_EVERY), move |_, (), _| {
                model::spawn_kernel(kernel_tx.clone());
                TimeoutAction::ToDuration(KERNEL_EVERY)
            })?;
        Ok(())
    }

    fn connect_pulse(&mut self) {
        let Ok(pulse) = Pulse::connect() else {
            return;
        };
        let Some(stream) = pulse.try_clone_stream() else {
            return;
        };
        let source = Generic::new(stream, Interest::READ, Mode::Level);
        let inserted = self.handle.insert_source(source, |_, _, app| {
            let Some(pulse) = app.pulse.as_mut() else {
                return Ok(PostAction::Remove);
            };
            let Ok(changed) = pulse.on_readable() else {
                app.pulse = None;
                return Ok(PostAction::Remove);
            };
            if changed {
                for device in [Device::Sink, Device::Source] {
                    *app.model.muted_mut(device) = pulse.muted(device).unwrap_or(false);
                }
                app.dirty = true;
            }
            Ok(PostAction::Continue)
        });
        if inserted.is_ok() {
            self.pulse = Some(pulse);
        }
    }

    fn connect_mango(&mut self) {
        let Ok(mut mango) = Mango::connect() else {
            return;
        };
        let Some(stream) = mango.try_clone_stream() else {
            return;
        };
        let source = Generic::new(stream, Interest::READ, Mode::Level);
        let inserted =
            self.handle
                .insert_source(source, move |_, _, app| match mango.on_readable() {
                    Ok(Some(mons)) => {
                        if mons != app.model.mons {
                            app.model.mons = mons;
                            app.dirty = true;
                        }
                        Ok(PostAction::Continue)
                    }
                    Ok(None) => Ok(PostAction::Continue),
                    Err(_) => {
                        app.ipc = Ipc::Mango { connected: false };
                        Ok(PostAction::Remove)
                    }
                });
        self.ipc = Ipc::Mango {
            connected: inserted.is_ok(),
        };
    }

    fn watch_tray(&mut self) {
        let Some(fd) = self
            .tray
            .as_ref()
            .and_then(|tray| tray.as_fd().try_clone_to_owned().ok())
        else {
            return;
        };
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        let inserted = self.handle.insert_source(source, |_, _, app| {
            if app.tray.as_mut().is_some_and(Tray::dispatch) {
                app.dirty = true;
            }
            Ok(PostAction::Continue)
        });
        if inserted.is_err() {
            self.tray = None;
        }
    }

    fn watch_power(&self) {
        let Ok(uevents) = Uevents::open() else {
            return;
        };
        let source = Generic::new(uevents, Interest::READ, Mode::Level);
        // Without uevents the 5 s poll still catches every change.
        let _ = self.handle.insert_source(source, |_, uevents, app| {
            let events = uevents.drain();
            if events.changed {
                app.on_power(events);
            }
            Ok(PostAction::Continue)
        });
    }

    fn on_power(&mut self, events: PowerEvents) {
        self.dirty |= if events.hotplug {
            self.model.rediscover_battery()
        } else {
            self.model.refresh_battery()
        };
        let settle = Timer::from_duration(BATTERY_SETTLE);
        let _ = self.handle.insert_source(settle, |_, (), app| {
            app.dirty |= app.model.refresh_battery();
            TimeoutAction::Drop
        });
    }

    fn on_second(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        clock::write(&mut self.clock);
        self.dirty = true;
        if self.tick.is_multiple_of(SENSORS_EVERY) {
            self.model.refresh_mem();
            self.model.refresh_night();
            self.model.refresh_battery();
        }
        if self.tick.is_multiple_of(TEMP_EVERY) {
            self.model.refresh_temp();
        }
        if self.tick.is_multiple_of(RECONNECT_EVERY) {
            if self.pulse.is_none() {
                self.connect_pulse();
            }
            if matches!(self.ipc, Ipc::Mango { connected: false }) {
                self.connect_mango();
            }
        }
        self.launcher.reap();
    }

    fn draw(&mut self) {
        let Self {
            bars,
            model,
            painter,
            tray,
            clock,
            hover,
            fractional_mgr,
            dirty,
            ipc,
            ..
        } = self;
        let empty_title = match ipc {
            Ipc::Dwl(_) => EmptyTitle::Keep,
            Ipc::Mango { .. } => EmptyTitle::Hide,
        };
        let tray_icons = tray.as_ref().map_or(&[][..], Tray::icons);
        for bar in bars.iter_mut().filter(|bar| bar.configured) {
            if fractional_mgr.is_some()
                && bar.scale.preferred.is_none()
                && bar.created.elapsed() < SCALE_GRACE
            {
                // Draw once the compositor has told us the real scale.
                *dirty = true;
                continue;
            }
            let mon = model.mon_for(&bar.name);
            let frame = Frame {
                tags: mon.map_or(&[], |mon| &mon.tags),
                title: mon.map_or("", |mon| &mon.title),
                empty_title,
                layout: mon.map_or("", |mon| &mon.layout_text),
                temp: &model.temp,
                mem: &model.mem,
                battery: &model.battery,
                sink_muted: model.sink_muted,
                source_muted: model.source_muted,
                night_on: model.night,
                kernel: &model.kernel.text,
                kernel_class: model.kernel.class,
                clock,
                tray: tray_icons,
                tray_gen: tray.as_ref().map_or(0, Tray::generation),
            };
            let hovered = hover
                .filter(|hover| hover.bar == bar.id)
                .and_then(|hover| hover.target);
            let (width, height) = bar.size;
            painter.set_scale(f64::from(bar.scale.render()));
            let (pills, hits) = painter.layout(
                f64::from(width),
                f64::from(height),
                &frame,
                hovered.as_ref(),
            );
            bar.hits = hits;
            bar.present(painter, pills, tray_icons);
        }
    }

    fn bar_mut(&mut self, output: &wl_output::WlOutput) -> Option<&mut Bar> {
        self.bars.iter_mut().find(|bar| &bar.output == output)
    }

    fn sync_output(&mut self, output: &wl_output::WlOutput) {
        let Some(info) = self.output_state.info(output) else {
            return;
        };
        let Some(bar) = self.bar_mut(output) else {
            return;
        };
        if let Some(name) = info.name {
            bar.name = name;
        }
        if let Some((width, _)) = info.logical_size {
            bar.output_width = width;
        }
        bar.scale.integer = info.scale_factor.max(1).unsigned_abs();
        self.dirty = true;
    }

    fn toggle_mute(&mut self, device: Device) {
        let muted = self.model.muted_mut(device);
        *muted = !*muted;
        let mute = *muted;
        let sent = self
            .pulse
            .as_mut()
            .is_some_and(|pulse| pulse.set_mute(device, mute));
        if !sent {
            match device {
                Device::Sink => self.launcher.toggle_sink_mute(),
                Device::Source => self.launcher.toggle_source_mute(),
            }
        }
        self.dirty = true;
    }

    fn activate(&mut self, bar: BarId, hit: Hit) {
        match hit {
            Hit::Tag(index) => self.view_tag(bar, index),
            Hit::Layout => {
                // Waybar's dwl layout module has no click action.
                if matches!(self.ipc, Ipc::Mango { .. }) {
                    // A dead socket gets picked up by the reconnect tick.
                    let _ = mango::dispatch("switch_layout");
                }
            }
            Hit::Speaker => self.toggle_mute(Device::Sink),
            Hit::Mic => self.toggle_mute(Device::Source),
            Hit::Night => {
                self.model.night = !self.model.night;
                self.launcher.toggle_nightlight();
                self.dirty = true;
            }
            Hit::Tray(index) => {
                if let Some(tray) = &self.tray {
                    tray.activate(index);
                }
            }
        }
    }

    /// Left click on a tag: show only that tag.
    fn view_tag(&mut self, bar: BarId, index: u8) {
        let Some(bar) = self.bars.iter().find(|b| b.id == bar) else {
            return;
        };
        match (&self.ipc, &bar.dwl) {
            (Ipc::Dwl(_), Some(dwl)) => {
                let (tags, toggle_tagset) = dwl::view_tag(index);
                dwl.proxy.set_tags(tags, toggle_tagset);
            }
            (Ipc::Dwl(_), None) => return,
            (Ipc::Mango { .. }, _) => {
                // A dead socket gets picked up by the reconnect tick.
                let _ = mango::dispatch(&format!("view,{index}"));
            }
        }
        let output = bar.name.clone();
        self.model.focus_tag(&output, index);
        self.dirty = true;
    }

    /// Right click on a tag: add it to or remove it from the view (dwl).
    fn toggle_tag(&self, bar: BarId, index: u8) {
        if let Some(dwl) = self
            .bars
            .iter()
            .find(|b| b.id == bar)
            .and_then(|b| b.dwl.as_ref())
        {
            let (tags, toggle_tagset) = dwl::toggle_tag(index, dwl.state.active_mask());
            dwl.proxy.set_tags(tags, toggle_tagset);
        }
    }

    fn set_hover(&mut self, hover: Option<Hover>) {
        if hover != self.hover {
            self.hover = hover;
            self.dirty = true;
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if let Some(bar) = self
            .bars
            .iter_mut()
            .find(|bar| bar.layer.wl_surface() == surface)
        {
            bar.scale.integer = factor.max(1).unsigned_abs();
            self.dirty = true;
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        let Ok(pool) = SlotPool::new(4096, &self.shm) else {
            return;
        };
        let id = BarId(self.next_id);
        self.next_id += 1;
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("simple-bar"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(self.bar_height as i32 + paint::MARGIN_TOP);
        layer.set_margin(paint::MARGIN_TOP, paint::MARGIN_X, 0, paint::MARGIN_X);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, self.bar_height);
        let fractional = self
            .fractional_mgr
            .as_ref()
            .map(|mgr| mgr.get_fractional_scale(layer.wl_surface(), qh, ScaleUd(id)));
        let viewport = self
            .viewporter
            .as_ref()
            .map(|porter| porter.get_viewport(layer.wl_surface(), qh, GlobalUd));
        layer.commit();
        let dwl = match &self.ipc {
            Ipc::Dwl(manager) => Some(DwlOutput {
                proxy: manager.get_output(&output, qh, DwlUd(id)),
                state: DwlState::default(),
            }),
            Ipc::Mango { .. } => None,
        };
        self.bars.push(Bar {
            id,
            output: output.clone(),
            name: String::new(),
            output_width: 0,
            layer,
            viewport,
            _fractional: fractional,
            size: (0, self.bar_height),
            configured: false,
            scale: Scale {
                integer: 1,
                preferred: None,
            },
            created: Instant::now(),
            pool,
            buffer: None,
            painted: None,
            pills: Vec::new(),
            hits: Vec::new(),
            dwl,
        });
        self.sync_output(&output);
    }

    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.sync_output(&output);
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.bars.retain(|bar| bar.output != output);
        self.exit = self.bars.is_empty();
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.bars.retain(|bar| &bar.layer != layer);
        self.exit = self.bars.is_empty();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let bar_height = self.bar_height;
        let Some(bar) = self.bars.iter_mut().find(|bar| &bar.layer == layer) else {
            return;
        };
        let (width, height) = configure.new_size;
        let fallback = bar.output_width - 2 * paint::MARGIN_X;
        bar.size = (
            if width > 0 {
                width
            } else {
                fallback.max(1).unsigned_abs()
            },
            if height > 0 { height } else { bar_height },
        );
        bar.configured = true;
        self.dirty = true;
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability != Capability::Pointer || self.pointer.is_some() {
            return;
        }
        let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) else {
            return;
        };
        self.cursor_dev = self
            .cursor_mgr
            .as_ref()
            .map(|mgr| mgr.get_pointer(&pointer, qh, GlobalUd));
        self.pointer = Some(pointer);
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.cursor_dev = None;
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(bar) = self
                .bars
                .iter()
                .find(|bar| bar.layer.wl_surface() == &event.surface)
            else {
                continue;
            };
            let id = bar.id;
            let (x, y) = event.position;
            let target = bar.hit_at(x, y);
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    if let Some(dev) = &self.cursor_dev {
                        dev.set_shape(serial, Shape::Default);
                    }
                    self.set_hover(Some(Hover { bar: id, target }));
                }
                PointerEventKind::Motion { .. } => {
                    self.set_hover(Some(Hover { bar: id, target }));
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover.is_some_and(|hover| hover.bar == id) {
                        self.set_hover(None);
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    if let Some(hit) = target {
                        self.activate(id, hit);
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_RIGHT => {
                    if let Some(Hit::Tag(index)) = target {
                        self.toggle_tag(id, index);
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // Waybar gives the speaker widget no width unless it is
                    // muted, so scrolling only lands on the mute icon.
                    let steps = if vertical.value120 == 0 {
                        vertical.discrete
                    } else {
                        vertical.value120 / 120
                    };
                    if steps != 0 && target == Some(Hit::Speaker) {
                        self.launcher.step_volume(steps > 0);
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

/// User data for globals whose events we ignore.
struct GlobalUd;

/// User data for a bar's fractional-scale object.
struct ScaleUd(BarId);

impl Dispatch2<WpFractionalScaleV1, App> for ScaleUd {
    fn event(
        &self,
        app: &mut App,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(bar) = app.bars.iter_mut().find(|bar| bar.id == self.0)
        {
            bar.scale.preferred = Some(scale.max(1));
            app.dirty = true;
        }
    }
}

/// User data for a bar's dwl IPC output.
struct DwlUd(BarId);

impl Dispatch2<ZdwlIpcOutputV2, App> for DwlUd {
    fn event(
        &self,
        app: &mut App,
        _: &ZdwlIpcOutputV2,
        event: dwl::zdwl_ipc_output_v2::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        let Some(bar) = app.bars.iter_mut().find(|bar| bar.id == self.0) else {
            return;
        };
        let Some(dwl) = bar.dwl.as_mut() else {
            return;
        };
        if dwl.state.apply(event) {
            let mon = dwl.state.to_mon(&bar.name);
            app.dirty |= app.model.set_mon(mon);
        }
    }
}

macro_rules! ignore_events {
    ($ud:ty => $($iface:ty),+ $(,)?) => {$(
        impl Dispatch2<$iface, App> for $ud {
            fn event(
                &self,
                _: &mut App,
                _: &$iface,
                _: <$iface as Proxy>::Event,
                _: &Connection,
                _: &QueueHandle<App>,
            ) {
            }
        }
    )+};
}

ignore_events!(GlobalUd =>
    ZdwlIpcManagerV2,
    WpFractionalScaleManagerV1,
    WpViewporter,
    WpViewport,
    WpCursorShapeManagerV1,
    WpCursorShapeDeviceV1,
);

smithay_client_toolkit::delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
