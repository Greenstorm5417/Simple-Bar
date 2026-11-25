//! Cairo/Pango drawing of the waybar CSS in `~/.config/waybar/style.css`.
//!
//! Coordinates are logical CSS pixels unless a name says otherwise.
//! Drawing is split into a layout pass that turns a [`Frame`] into a list
//! of [`Pill`]s and a paint pass. The previous frame's pills are kept by
//! the caller, so [`damage`] can name the pills that changed and
//! [`Painter::paint`] repaints only those, in place, in the shm buffer that
//! is already on screen.

use std::ops::Range;

use cairo::{Context, FontOptions, Format, ImageSurface, Operator};
use pango::FontDescription;

use crate::mango::Tag;
use crate::model::{ICON_MIC, ICON_MIC_MUTE, ICON_MUTE, ICON_NIGHT, KernelClass};
use crate::tray::TrayIcon;

/// Gap between the top of the output and the bar (`margin-top`).
pub const MARGIN_TOP: i32 = 2;
/// Gap between the output's sides and the bar (`margin-left/right`).
pub const MARGIN_X: i32 = 6;

const FONT: &str = "JetBrainsMono Nerd Font Bold 13px";
const PILL_MARGIN_X: f64 = 3.0;
const PILL_MARGIN_Y: f64 = 4.0;
const PILL_PAD_X: f64 = 12.0;
const PILL_PAD_Y: f64 = 4.0;
const BORDER: f64 = 1.0;
const RADIUS: f64 = 14.0;
const GROUP_PAD: f64 = 7.0;
// Clock, title, and layout are the capsule itself. Their only inset is the
// capsule padding. Grouped modules add GROUP_PAD on top of that.
const SOLO_PAD: f64 = 0.0;
const KERNEL_RADIUS: f64 = 10.0;
const WS_EDGE: f64 = 12.0;
const WS_PAD_X: f64 = 8.0;
const WS_PAD_Y: f64 = 4.0;
const WS_MARGIN_X: f64 = 1.0;
const WS_RADIUS: f64 = 10.0;
const TRAY: f64 = 16.0;
const TRAY_GAP: f64 = 8.0;
/// Waybar `max-length` for the window title, applied as GTK's `max-width-chars`.
const TITLE_MAX_CHARS: i32 = 45;
/// How far inside a capsule's edge its flat interior starts. The rounded
/// border (radius 14, 1 px, antialiased) stays left of x = 5 between the
/// content box's top and bottom; 7 keeps a margin.
const INTERIOR_INSET: f64 = 7.0;
const MEASURE_CACHE: usize = 64;

const BG: Rgba = Rgba::new(15, 21, 17, 0.35);
const EDGE: Rgba = Rgba::new(255, 255, 255, 0.18);
const SEP: Rgba = Rgba::new(255, 255, 255, 0.22);
const FG: Rgba = Rgba::new(255, 255, 255, 1.0);
const ACCENT: Rgba = Rgba::new(0xa7, 0xeb, 0xc0, 1.0);
const WARN: Rgba = Rgba::new(0xff, 0xd2, 0xcc, 1.0);
const INK: Rgba = Rgba::new(15, 21, 17, 1.0);

/// A CSS color: 8-bit channels and an alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Rgba {
    const fn new(r: u8, g: u8, b: u8, a: f64) -> Self {
        Self {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
            a,
        }
    }

    fn apply(self, cr: &Context) {
        cr.set_source_rgba(self.r, self.g, self.b, self.a);
    }
}

/// An axis-aligned rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    /// Whether the point lies inside (left/top inclusive).
    #[must_use]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// Something on the bar that reacts to the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A workspace button, by tag index.
    Tag(u8),
    /// The layout symbol.
    Layout,
    /// The speaker mute icon (only shown while muted).
    Speaker,
    /// The microphone icon.
    Mic,
    /// The night-light toggle.
    Night,
    /// A tray icon, by position.
    Tray(usize),
}

/// A clickable area and what it does.
#[derive(Clone, Debug)]
pub struct HitRect {
    /// Area in logical pixels.
    pub rect: Rect,
    /// Target under that area.
    pub hit: Hit,
}

/// Everything one bar shows.
#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    /// Workspaces of this output.
    pub tags: &'a [Tag],
    /// Focused window title.
    pub title: &'a str,
    /// Layout label, such as "\u{f0db} Fair".
    pub layout: &'a str,
    /// Temperature text.
    pub temp: &'a str,
    /// Memory text.
    pub mem: &'a str,
    /// Battery text.
    pub battery: &'a str,
    /// Default sink is muted.
    pub sink_muted: bool,
    /// Default source is muted.
    pub source_muted: bool,
    /// Night light is running.
    pub night_on: bool,
    /// Kernel version text.
    pub kernel: &'a str,
    /// Kernel update state.
    pub kernel_class: KernelClass,
    /// Clock text.
    pub clock: &'a str,
    /// Tray icons.
    pub tray: &'a [TrayIcon],
    /// Bumped whenever the tray icon list or pixels change.
    pub tray_gen: u64,
}

/// One drawn module inside a capsule.
#[derive(Clone, Debug, PartialEq)]
struct Piece {
    text: Box<str>,
    /// Tray generation for the tray piece, so new icons repaint it.
    tray: Option<u64>,
    fg: Rgba,
    bg: Option<Rgba>,
    sep_left: bool,
    sep_right: bool,
    /// Extra horizontal padding inside a grouped module.
    pad: f64,
    w: f64,
    text_h: f64,
    /// Pango layout width to ellipsize into, for a title past `max-length`.
    ellipsize: Option<i32>,
}

#[derive(Clone, Debug, PartialEq)]
struct Button {
    label: Box<str>,
    text_w: f64,
    text_h: f64,
    fg: Rgba,
    bg: Option<Rgba>,
}

impl Button {
    fn size(&self) -> (f64, f64) {
        (self.text_w + WS_PAD_X * 2.0, self.text_h + WS_PAD_Y * 2.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Body {
    Pieces(Vec<Piece>),
    Tags(Vec<Button>),
}

/// A capsule and everything inside it. Equal pills paint identical pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Pill {
    x: f64,
    w: f64,
    body: Body,
}

/// A horizontal span in logical pixels.
pub type Span = Range<f64>;

/// What to repaint after a frame changed, from [`damage`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Repaint {
    /// Logical x-spans to clear and repaint completely.
    pub spans: Vec<Span>,
    /// Pills (indices into the new frame) whose capsule is unchanged and
    /// whose content alone is repainted, skipping the rounded outline.
    pub interiors: Vec<usize>,
}

impl Repaint {
    /// Nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty() && self.interiors.is_empty()
    }
}

/// A repainted area in buffer (device) pixels, for `wl_surface.damage_buffer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Damage {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: i32,
    /// Height.
    pub height: i32,
}

/// A premultiplied ARGB32 pixel buffer to paint into.
#[derive(Debug)]
pub struct Canvas<'a> {
    /// The pixels, `stride * height` bytes or more.
    pub pixels: &'a mut [u8],
    /// Width in device pixels.
    pub width: i32,
    /// Height in device pixels.
    pub height: i32,
    /// Bytes per row.
    pub stride: i32,
}

/// The content box of a capsule, where its pieces sit.
#[derive(Clone, Copy)]
struct Content {
    y: f64,
    h: f64,
}

impl Content {
    fn for_height(height: f64) -> Self {
        let pill_h = height - PILL_MARGIN_Y * 2.0;
        Self {
            y: PILL_MARGIN_Y + BORDER + PILL_PAD_Y,
            h: pill_h - 2.0 * (BORDER + PILL_PAD_Y),
        }
    }
}

/// A measured string.
#[derive(Clone, Copy, Debug)]
struct Metrics {
    /// Logical width in pixels.
    width: f64,
    /// Logical height in pixels.
    height: f64,
    /// Logical width in pango units, for GTK's size comparisons.
    units: i32,
}

/// Owns the one pango layout the bar uses. Font setup happens once and
/// each distinct string is shaped for measuring only once.
#[derive(Debug)]
pub struct Painter {
    layout: pango::Layout,
    probe: ImageSurface,
    scale: f64,
    cache: Vec<(Box<str>, Metrics)>,
    /// Title width limit (`max-length`) in pango units.
    title_limit: i32,
}

impl Painter {
    /// Load the font and set up text rendering like GTK does for waybar.
    ///
    /// # Errors
    /// Fails when cairo cannot allocate its 1×1 measuring surface.
    pub fn new() -> Result<Self, cairo::Error> {
        // Touch the pangocairo font map once so fontconfig is initialized
        // before cairo asks it for hinting.
        let _map = pangocairo::FontMap::default();
        let probe = ImageSurface::create(Format::ARgb32, 1, 1)?;
        let layout = {
            let cr = Context::new(&probe)?;
            pangocairo::functions::create_layout(&cr)
        };
        layout.set_font_description(Some(&FontDescription::from_string(FONT)));
        let mut options = FontOptions::new()?;
        options.set_antialias(cairo::Antialias::Gray);
        options.set_hint_style(cairo::HintStyle::Slight);
        options.set_hint_metrics(cairo::HintMetrics::On);
        pangocairo::functions::context_set_font_options(&layout.context(), Some(&options));
        // GTK sizes a `max-width-chars` label with the wider of pango's
        // approximate char and digit widths, taken in the widget's unscaled,
        // hinted context. In this monospace font both equal the advance of
        // any glyph, so measure one here, before any scale is applied.
        // (Pango's font metrics would work too, but they are cached in the
        // shared font map and come back hinted or not depending on which
        // context asked first.)
        layout.set_text("0");
        let title_limit = layout.size().0 * TITLE_MAX_CHARS;
        Ok(Self {
            layout,
            probe,
            scale: 1.0,
            cache: Vec::with_capacity(MEASURE_CACHE),
            title_limit,
        })
    }

    /// Set the device scale. Hinting depends on it, so text is measured
    /// under the same transform it is drawn with.
    pub fn set_scale(&mut self, scale: f64) {
        if scale.to_bits() == self.scale.to_bits() {
            return;
        }
        if let Ok(cr) = Context::new(&self.probe) {
            cr.scale(scale, scale);
            pangocairo::functions::update_layout(&cr, &self.layout);
        }
        self.scale = scale;
        self.cache.clear();
    }

    /// The bar height waybar settles on for this font, in logical pixels.
    pub fn bar_height(&mut self) -> u32 {
        self.set_scale(1.0);
        let line = self.measure("0").height;
        let button = line + WS_PAD_Y * 2.0;
        let outer = PILL_MARGIN_Y * 2.0 + BORDER * 2.0 + PILL_PAD_Y * 2.0 + button;
        outer.round().max(1.0) as u32
    }

    fn measure(&mut self, text: &str) -> Metrics {
        if let Some((_, metrics)) = self.cache.iter().find(|(cached, _)| &**cached == text) {
            return *metrics;
        }
        self.layout.set_width(-1);
        self.layout.set_ellipsize(pango::EllipsizeMode::None);
        self.layout.set_text(text);
        let (width, height) = self.layout.pixel_size();
        let metrics = Metrics {
            width: f64::from(width),
            height: f64::from(height),
            units: self.layout.size().0,
        };
        if self.cache.len() >= MEASURE_CACHE {
            self.cache.clear();
        }
        self.cache.push((text.into(), metrics));
        metrics
    }

    /// The window title, at most `room` logical pixels wide. Like waybar's
    /// GTK label, it is as wide as its text unless that exceeds the
    /// `max-length` limit or the room left by the centered workspaces; then
    /// it takes the smaller width and pango ellipsizes the end.
    fn title_piece(&mut self, text: &str, room: f64) -> Piece {
        let limit = self.title_limit;
        let mut piece = self.piece(text, FG, SOLO_PAD);
        let natural = self.measure(text).units;
        if natural <= limit && piece.w <= room {
            return piece;
        }
        // PANGO_PIXELS_CEIL, as GTK sizes the label.
        let capped = (limit.min(natural) + pango::SCALE - 1) / pango::SCALE;
        let width = f64::from(capped).min(room);
        piece.w = width;
        piece.ellipsize = Some(width as i32 * pango::SCALE);
        piece
    }

    fn piece(&mut self, text: &str, fg: Rgba, pad: f64) -> Piece {
        let Metrics {
            width: text_w,
            height: text_h,
            ..
        } = self.measure(text);
        Piece {
            text: text.into(),
            tray: None,
            fg,
            bg: None,
            sep_left: false,
            sep_right: false,
            pad,
            w: pad * 2.0 + text_w,
            text_h,
            ellipsize: None,
        }
    }

    /// A grouped module with a separator line on its left.
    fn grouped(&mut self, text: &str, fg: Rgba) -> Piece {
        let mut piece = self.piece(text, fg, GROUP_PAD);
        piece.sep_left = true;
        piece.w += 1.0;
        piece
    }

    /// Lay the bar out. Returns the pills to paint and the click targets.
    pub fn layout(
        &mut self,
        width: f64,
        height: f64,
        frame: &Frame<'_>,
        hover: Option<&Hit>,
    ) -> (Vec<Pill>, Vec<HitRect>) {
        let content = Content::for_height(height);
        let mut pills = Vec::with_capacity(6);
        let mut hits = Vec::new();
        // Waybar's workspaces are the bar box's center widget: GTK centers
        // them and the left box only gets the space up to their margin.
        let tags = (!frame.tags.is_empty())
            .then(|| self.layout_tags(width, frame.tags, hover, content, &mut hits));
        let left_end = tags.as_ref().map_or(width, |pill| pill.x - PILL_MARGIN_X);
        self.layout_left(frame, content, left_end, &mut pills, &mut hits);
        pills.extend(tags);
        self.layout_right(width, frame, content, &mut pills, &mut hits);
        (pills, hits)
    }

    /// The system group (tray, temperature, memory) and the window title.
    fn layout_left(
        &mut self,
        frame: &Frame<'_>,
        content: Content,
        left_end: f64,
        pills: &mut Vec<Pill>,
        hits: &mut Vec<HitRect>,
    ) {
        let mut left = 0.0;
        let mut system = Vec::new();
        if !frame.tray.is_empty() {
            system.push(Piece {
                text: "".into(),
                tray: Some(frame.tray_gen),
                fg: FG,
                bg: None,
                sep_left: false,
                sep_right: false,
                pad: GROUP_PAD,
                w: GROUP_PAD * 2.0 + tray_width(frame.tray.len()),
                text_h: 0.0,
                ellipsize: None,
            });
            let mut x = PILL_MARGIN_X + BORDER + PILL_PAD_X + GROUP_PAD;
            let y = content.y + (content.h - TRAY) / 2.0;
            for index in 0..frame.tray.len() {
                hits.push(HitRect {
                    rect: Rect::new(x, y, TRAY, TRAY),
                    hit: Hit::Tray(index),
                });
                x += TRAY + TRAY_GAP;
            }
        }
        for text in [frame.temp, frame.mem] {
            if !text.is_empty() {
                system.push(self.grouped(text, FG));
            }
        }
        if !system.is_empty() {
            let pill = pieces_pill(left + PILL_MARGIN_X, system);
            left += PILL_MARGIN_X + pill.w + PILL_MARGIN_X;
            pills.push(pill);
        }
        if !frame.title.is_empty() {
            let x = left + PILL_MARGIN_X;
            // The title is the only module that can shrink (it ellipsizes),
            // so it absorbs any shortfall in the left box.
            let room = (left_end - PILL_MARGIN_X - x - 2.0 * (BORDER + PILL_PAD_X)).floor();
            let piece = self.title_piece(frame.title, room.max(0.0));
            pills.push(pieces_pill(x, vec![piece]));
        }
    }

    /// The centered workspace buttons.
    fn layout_tags(
        &mut self,
        width: f64,
        tags: &[Tag],
        hover: Option<&Hit>,
        content: Content,
        hits: &mut Vec<HitRect>,
    ) -> Pill {
        let mut buttons = Vec::with_capacity(tags.len());
        let mut w = 2.0 * (BORDER + WS_EDGE);
        for tag in tags {
            let label = tag.index.to_string();
            let Metrics {
                width: text_w,
                height: text_h,
                ..
            } = self.measure(&label);
            let hovered = hover == Some(&Hit::Tag(tag.index));
            let (fg, bg) = if tag.urgent {
                (INK, Some(WARN))
            } else if tag.active {
                (INK, Some(ACCENT))
            } else {
                (if hovered { ACCENT } else { FG }, None)
            };
            let button = Button {
                label: label.into(),
                text_w,
                text_h,
                fg,
                bg,
            };
            w += WS_MARGIN_X * 2.0 + button.size().0;
            buttons.push(button);
        }
        let x = (width - w) / 2.0;
        let mut cursor = x + BORDER + WS_EDGE;
        for (tag, button) in tags.iter().zip(&buttons) {
            let (bw, bh) = button.size();
            hits.push(HitRect {
                rect: Rect::new(
                    cursor + WS_MARGIN_X,
                    content.y + (content.h - bh) / 2.0,
                    bw,
                    bh,
                ),
                hit: Hit::Tag(tag.index),
            });
            cursor += WS_MARGIN_X * 2.0 + bw;
        }
        Pill {
            x,
            w,
            body: Body::Tags(buttons),
        }
    }

    /// Clock, controls group, and layout symbol, from the right edge in.
    fn layout_right(
        &mut self,
        width: f64,
        frame: &Frame<'_>,
        content: Content,
        pills: &mut Vec<Pill>,
        hits: &mut Vec<HitRect>,
    ) {
        let mut right = width;
        if !frame.clock.is_empty() {
            let piece = self.piece(frame.clock, FG, SOLO_PAD);
            right -= PILL_MARGIN_X;
            pills.push(pieces_pill_ending(right, vec![piece]));
            right = pills[pills.len() - 1].x - PILL_MARGIN_X;
        }

        let (controls, targets) = self.controls(frame);
        right -= PILL_MARGIN_X;
        let pill = pieces_pill_ending(right, controls);
        if let Body::Pieces(pieces) = &pill.body {
            let mut x = pill.x + BORDER + PILL_PAD_X;
            for (piece, target) in pieces.iter().zip(targets) {
                if let Some(hit) = target {
                    hits.push(HitRect {
                        rect: Rect::new(x, content.y, piece.w, content.h),
                        hit,
                    });
                }
                x += piece.w;
            }
        }
        right = pill.x - PILL_MARGIN_X;
        pills.push(pill);

        if !frame.layout.is_empty() {
            let piece = self.piece(frame.layout, FG, SOLO_PAD);
            let piece_w = piece.w;
            let pill = pieces_pill_ending(right - PILL_MARGIN_X, vec![piece]);
            hits.push(HitRect {
                rect: Rect::new(pill.x + BORDER + PILL_PAD_X, content.y, piece_w, content.h),
                hit: Hit::Layout,
            });
            pills.push(pill);
        }
    }

    /// The controls group: speaker (when muted), mic, night light, battery,
    /// kernel. Each piece comes with its click target, if any.
    fn controls(&mut self, frame: &Frame<'_>) -> (Vec<Piece>, Vec<Option<Hit>>) {
        let mut pieces = Vec::with_capacity(5);
        let mut targets = Vec::with_capacity(5);
        if frame.sink_muted {
            let mut speaker = self.piece(ICON_MUTE, WARN, GROUP_PAD);
            speaker.sep_right = true;
            speaker.w += 1.0;
            pieces.push(speaker);
            targets.push(Some(Hit::Speaker));
        }
        let (mic, mic_fg) = if frame.source_muted {
            (ICON_MIC_MUTE, WARN)
        } else {
            (ICON_MIC, FG)
        };
        pieces.push(self.piece(mic, mic_fg, GROUP_PAD));
        targets.push(Some(Hit::Mic));
        let night_fg = if frame.night_on { ACCENT } else { FG };
        pieces.push(self.grouped(ICON_NIGHT, night_fg));
        targets.push(Some(Hit::Night));
        if !frame.battery.is_empty() {
            pieces.push(self.grouped(frame.battery, FG));
            targets.push(None);
        }
        if !frame.kernel.is_empty() {
            let (fg, bg) = match frame.kernel_class {
                KernelClass::Update => (INK, Some(ACCENT)),
                KernelClass::Unknown => (INK, Some(WARN)),
                KernelClass::Current => (FG, None),
            };
            let mut kernel = self.grouped(frame.kernel, fg);
            kernel.bg = bg;
            pieces.push(kernel);
            targets.push(None);
        }
        (pieces, targets)
    }

    /// Paint pills into `canvas`. With a `repaint` plan only the changed
    /// areas are redrawn, on top of the previous frame's pixels; without one
    /// the whole canvas is. Returns the device-pixel damage to report to
    /// the compositor.
    ///
    /// # Errors
    /// Returns cairo's error if the canvas is too small for its stated size
    /// or drawing fails.
    pub fn paint(
        &mut self,
        canvas: Canvas<'_>,
        height: f64,
        pills: &[Pill],
        repaint: Option<&Repaint>,
        tray: &[TrayIcon],
    ) -> Result<Vec<Damage>, cairo::Error> {
        let Canvas {
            pixels,
            width,
            height: rows,
            stride,
        } = canvas;
        if pixels.len() < (stride as usize) * (rows as usize) {
            return Err(cairo::Error::InvalidSize);
        }
        // SAFETY: `pixels` holds at least `stride * rows` bytes (checked
        // above) and outlives the surface, which is finished before return.
        let surface = unsafe {
            ImageSurface::create_for_data_unsafe(
                pixels.as_mut_ptr(),
                Format::ARgb32,
                width,
                rows,
                stride,
            )
        }?;
        let result = self.paint_changes(&surface, height, pills, repaint, tray);
        surface.flush();
        surface.finish();
        result
    }

    fn paint_changes(
        &mut self,
        surface: &ImageSurface,
        height: f64,
        pills: &[Pill],
        repaint: Option<&Repaint>,
        tray: &[TrayIcon],
    ) -> Result<Vec<Damage>, cairo::Error> {
        let cr = Context::new(surface)?;
        let scale = self.scale;
        let device = (surface.width(), surface.height());
        let full = [0.0..f64::from(device.0) / scale];
        let (spans, interiors) = match repaint {
            Some(plan) => (&plan.spans[..], &plan.interiors[..]),
            None => (&full[..], &[][..]),
        };
        let mut damage = Vec::with_capacity(spans.len() + interiors.len());
        for &index in interiors {
            let Some(pill) = pills.get(index) else {
                continue;
            };
            if let Some(rect) = self.interior(pill, height) {
                self.paint_interior(&cr, pill, rect, height, tray)?;
                damage.push(rect);
            } else {
                // Off the device grid: repaint the whole capsule instead.
                let span = pill.x..pill.x + pill.w;
                damage.extend(self.paint_span(&cr, device, pills, &span, height, tray)?);
            }
        }
        for span in spans {
            damage.extend(self.paint_span(&cr, device, pills, span, height, tray)?);
        }
        Ok(damage)
    }

    /// Clear a span to transparent and repaint every pill that touches it.
    fn paint_span(
        &mut self,
        cr: &Context,
        (width, rows): (i32, i32),
        pills: &[Pill],
        span: &Span,
        height: f64,
        tray: &[TrayIcon],
    ) -> Result<Option<Damage>, cairo::Error> {
        let scale = self.scale;
        // Widen to whole device pixels plus one for antialiasing.
        let x0 = ((span.start * scale).floor() as i32 - 1).clamp(0, width);
        let x1 = ((span.end * scale).ceil() as i32 + 1).clamp(0, width);
        if x1 <= x0 {
            return Ok(None);
        }
        cr.save()?;
        cr.rectangle(f64::from(x0), 0.0, f64::from(x1 - x0), f64::from(rows));
        cr.clip();
        clear(cr)?;
        cr.scale(scale, scale);
        let visible = f64::from(x0) / scale..f64::from(x1) / scale;
        for pill in pills
            .iter()
            .filter(|pill| pill.x < visible.end && pill.x + pill.w > visible.start)
        {
            self.paint_pill(cr, pill, height, tray)?;
        }
        cr.restore()?;
        Ok(Some(Damage {
            x: x0,
            y: 0,
            width: x1 - x0,
            height: rows,
        }))
    }

    /// The device rectangle inside a capsule that holds only its flat fill
    /// and content: clear of the rounded border and its antialiasing. `None`
    /// unless it falls exactly on the device pixel grid.
    fn interior(&self, pill: &Pill, height: f64) -> Option<Damage> {
        let content = Content::for_height(height);
        let logical = [
            pill.x + INTERIOR_INSET,
            content.y,
            pill.x + pill.w - INTERIOR_INSET,
            content.y + content.h,
        ];
        let mut device = [0i32; 4];
        for (out, value) in device.iter_mut().zip(logical) {
            let scaled = value * self.scale;
            if scaled.fract() != 0.0 {
                return None;
            }
            *out = scaled as i32;
        }
        let [x0, y0, x1, y1] = device;
        (x1 > x0 && y1 > y0).then_some(Damage {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }

    /// Repaint a capsule's content in place: its flat fill, then the pieces.
    /// Inside the border the capsule is exactly its fill color, so painting
    /// that color with `SOURCE` reproduces the full drawing's pixels.
    fn paint_interior(
        &mut self,
        cr: &Context,
        pill: &Pill,
        rect: Damage,
        height: f64,
        tray: &[TrayIcon],
    ) -> Result<(), cairo::Error> {
        cr.save()?;
        cr.rectangle(
            f64::from(rect.x),
            f64::from(rect.y),
            f64::from(rect.width),
            f64::from(rect.height),
        );
        cr.clip();
        cr.set_operator(Operator::Source);
        BG.apply(cr);
        cr.paint()?;
        cr.set_operator(Operator::Over);
        cr.scale(self.scale, self.scale);
        self.paint_content(cr, pill, height, tray)?;
        cr.restore()
    }

    fn paint_pill(
        &mut self,
        cr: &Context,
        pill: &Pill,
        height: f64,
        tray: &[TrayIcon],
    ) -> Result<(), cairo::Error> {
        let shell = Rect::new(pill.x, PILL_MARGIN_Y, pill.w, height - PILL_MARGIN_Y * 2.0);
        capsule(cr, shell)?;
        self.paint_content(cr, pill, height, tray)
    }

    /// The pieces or workspace buttons inside a capsule.
    fn paint_content(
        &mut self,
        cr: &Context,
        pill: &Pill,
        height: f64,
        tray: &[TrayIcon],
    ) -> Result<(), cairo::Error> {
        let content = Content::for_height(height);
        match &pill.body {
            Body::Pieces(pieces) => {
                let mut x = pill.x + BORDER + PILL_PAD_X;
                for piece in pieces {
                    let slot = Rect::new(x, content.y, piece.w, content.h);
                    self.paint_piece(cr, piece, slot, tray)?;
                    x += piece.w;
                }
            }
            Body::Tags(buttons) => {
                let mut cursor = pill.x + BORDER + WS_EDGE;
                for button in buttons {
                    let (bw, bh) = button.size();
                    let rect = Rect::new(
                        cursor + WS_MARGIN_X,
                        content.y + (content.h - bh) / 2.0,
                        bw,
                        bh,
                    );
                    if let Some(bg) = button.bg {
                        fill_round(cr, rect, WS_RADIUS, bg)?;
                    }
                    self.draw_text(
                        cr,
                        &button.label,
                        rect.x + (bw - button.text_w) / 2.0,
                        rect.y + (bh - button.text_h) / 2.0,
                        button.fg,
                    );
                    cursor += WS_MARGIN_X * 2.0 + bw;
                }
            }
        }
        Ok(())
    }

    fn paint_piece(
        &mut self,
        cr: &Context,
        piece: &Piece,
        slot: Rect,
        tray: &[TrayIcon],
    ) -> Result<(), cairo::Error> {
        let scale = self.scale;
        if let Some(bg) = piece.bg {
            fill_round(cr, slot, KERNEL_RADIUS, bg)?;
        }
        // A CSS border-left/right spans the module's full content box.
        if piece.sep_left {
            vline(cr, snap(slot.x, scale), slot.y, slot.h)?;
        }
        let inner_x = slot.x + f64::from(u8::from(piece.sep_left)) + piece.pad;
        if piece.tray.is_some() {
            let y = snap(slot.y + (slot.h - TRAY) / 2.0, scale);
            let mut x = inner_x;
            for icon in tray {
                blit(cr, icon, snap(x, scale), y)?;
                x += TRAY + TRAY_GAP;
            }
        } else {
            let y = slot.y + (slot.h - piece.text_h) / 2.0;
            if let Some(width) = piece.ellipsize {
                self.layout.set_width(width);
                self.layout.set_ellipsize(pango::EllipsizeMode::End);
            }
            self.draw_text(cr, &piece.text, inner_x, y, piece.fg);
            if piece.ellipsize.is_some() {
                self.layout.set_width(-1);
                self.layout.set_ellipsize(pango::EllipsizeMode::None);
            }
        }
        if piece.sep_right {
            vline(cr, snap(slot.x + piece.w - 1.0, scale), slot.y, slot.h)?;
        }
        Ok(())
    }

    /// Draw text with its origin snapped to the device pixel grid.
    fn draw_text(&mut self, cr: &Context, text: &str, x: f64, y: f64, color: Rgba) {
        color.apply(cr);
        self.layout.set_text(text);
        cr.move_to(snap(x, self.scale), snap(y, self.scale));
        pangocairo::functions::show_layout(cr, &self.layout);
    }
}

/// Work out what to repaint to turn the `old` frame into the `new` one.
///
/// A pill whose capsule kept its position, size and piece layout only needs
/// its content repainted. Everything else that differs is repainted as a
/// span, covering both where it was and where it is now.
#[must_use]
pub fn damage(old: &[Pill], new: &[Pill]) -> Repaint {
    let mut plan = Repaint::default();
    let mut spans = Vec::new();
    let mut kept = vec![false; old.len()];
    for (index, pill) in new.iter().enumerate() {
        if let Some(same) = old.iter().position(|was| was == pill) {
            kept[same] = true;
            continue;
        }
        match old.iter().position(|was| was.same_frame(pill)) {
            Some(frame) => {
                kept[frame] = true;
                plan.interiors.push(index);
            }
            None => spans.push(pill.x..pill.x + pill.w),
        }
    }
    spans.extend(
        old.iter()
            .zip(&kept)
            .filter(|&(_, &kept)| !kept)
            .map(|(pill, _)| pill.x..pill.x + pill.w),
    );
    spans.sort_by(|a, b| a.start.total_cmp(&b.start));
    for span in spans {
        match plan.spans.last_mut() {
            // Spans closer than the antialiasing margin share a repaint.
            Some(last) if span.start <= last.end + 2.0 => last.end = last.end.max(span.end),
            _ => plan.spans.push(span),
        }
    }
    plan
}

impl Pill {
    /// Same capsule and same piece slots; only what is inside them differs.
    fn same_frame(&self, other: &Self) -> bool {
        let slots = |piece: &Piece| {
            (
                piece.w.to_bits(),
                piece.pad.to_bits(),
                piece.sep_left,
                piece.sep_right,
                piece.tray.is_some(),
            )
        };
        match (&self.body, &other.body) {
            (Body::Pieces(a), Body::Pieces(b)) => {
                self.x.to_bits() == other.x.to_bits()
                    && self.w.to_bits() == other.w.to_bits()
                    && a.len() == b.len()
                    && a.iter().map(slots).eq(b.iter().map(slots))
            }
            _ => false,
        }
    }
}

fn pieces_pill(x: f64, pieces: Vec<Piece>) -> Pill {
    let w = pieces.iter().map(|piece| piece.w).sum::<f64>() + 2.0 * (BORDER + PILL_PAD_X);
    Pill {
        x,
        w,
        body: Body::Pieces(pieces),
    }
}

/// A pill whose right edge sits at `right`.
fn pieces_pill_ending(right: f64, pieces: Vec<Piece>) -> Pill {
    let pill = pieces_pill(0.0, pieces);
    Pill {
        x: right - pill.w,
        ..pill
    }
}

fn tray_width(count: usize) -> f64 {
    match count {
        0 => 0.0,
        n => n as f64 * TRAY + (n - 1) as f64 * TRAY_GAP,
    }
}

/// Make the current clip region fully transparent.
fn clear(cr: &Context) -> Result<(), cairo::Error> {
    cr.set_operator(Operator::Source);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
    cr.paint()?;
    cr.set_operator(Operator::Over);
    Ok(())
}

/// Draw a tray icon scaled to the tray size.
fn blit(cr: &Context, icon: &TrayIcon, x: f64, y: f64) -> Result<(), cairo::Error> {
    let (Ok(w), Ok(h)) = (usize::try_from(icon.width), usize::try_from(icon.height)) else {
        return Ok(());
    };
    if w == 0 || h == 0 || icon.bgra.len() < w * h * 4 {
        return Ok(());
    }
    // SAFETY: cairo only reads from a source surface, the length was checked
    // above, and the pixels outlive the surface, which is finished below.
    let surface = unsafe {
        ImageSurface::create_for_data_unsafe(
            icon.bgra.as_ptr().cast_mut(),
            Format::ARgb32,
            icon.width,
            icon.height,
            icon.width * 4,
        )
    }?;
    cr.save()?;
    cr.translate(x, y);
    cr.scale(TRAY / f64::from(icon.width), TRAY / f64::from(icon.height));
    cr.set_source_surface(&surface, 0.0, 0.0)?;
    let painted = cr.paint();
    cr.restore()?;
    surface.finish();
    painted
}

/// The CSS capsule: translucent fill and a 1px inner border.
fn capsule(cr: &Context, rect: Rect) -> Result<(), cairo::Error> {
    fill_round(cr, rect, RADIUS, BG)?;
    EDGE.apply(cr);
    let inset = Rect::new(
        rect.x + 0.5,
        rect.y + 0.5,
        (rect.w - 1.0).max(0.0),
        (rect.h - 1.0).max(0.0),
    );
    rounded(cr, inset, (RADIUS - 0.5).max(0.0));
    cr.set_line_width(1.0);
    cr.stroke()
}

fn fill_round(cr: &Context, rect: Rect, radius: f64, color: Rgba) -> Result<(), cairo::Error> {
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return Ok(());
    }
    color.apply(cr);
    rounded(cr, rect, radius);
    cr.fill()
}

/// A 1px CSS border line.
fn vline(cr: &Context, x: f64, y: f64, h: f64) -> Result<(), cairo::Error> {
    SEP.apply(cr);
    cr.rectangle(x, y, 1.0, h);
    cr.fill()
}

fn rounded(cr: &Context, rect: Rect, radius: f64) {
    let Rect { x, y, w, h } = rect;
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    if radius == 0.0 {
        cr.rectangle(x, y, w, h);
        return;
    }
    let quarter = std::f64::consts::FRAC_PI_2;
    cr.arc(x + w - radius, y + radius, radius, -quarter, 0.0);
    cr.arc(x + w - radius, y + h - radius, radius, 0.0, quarter);
    cr.arc(x + radius, y + h - radius, radius, quarter, 2.0 * quarter);
    cr.arc(x + radius, y + radius, radius, 2.0 * quarter, 3.0 * quarter);
    cr.close_path();
}

/// Round a logical coordinate to the device pixel grid.
fn snap(value: f64, scale: f64) -> f64 {
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(clock: &str) -> Frame<'_> {
        Frame {
            tags: &[],
            title: "title",
            layout: "",
            temp: "t",
            mem: "m",
            battery: "b",
            sink_muted: false,
            source_muted: false,
            night_on: false,
            kernel: "k",
            kernel_class: KernelClass::Current,
            clock,
            tray: &[],
            tray_gen: 0,
        }
    }

    fn wrap(pixels: &mut [u8], width: i32, height: i32, stride: i32) -> Canvas<'_> {
        Canvas {
            pixels,
            width,
            height,
            stride,
        }
    }

    #[test]
    fn long_title_yields_to_centered_workspaces() {
        let mut painter = Painter::new().unwrap();
        painter.set_scale(2.0);
        let tags: Vec<Tag> = (1..=10)
            .map(|index| Tag {
                index,
                active: index == 2,
                urgent: false,
            })
            .collect();
        let frame = Frame {
            tags: &tags,
            title: "Overview \u{b7} Greenstorm5417/kisslock \u{2014} LibreWolf",
            temp: "\u{f2c9} 53\u{b0}C",
            mem: "\u{f035b} 4.4GB",
            ..frame("03:41:05 PM")
        };
        // eDP-1 is 1422 logical px wide; the bar has 6 px side margins.
        let (pills, _) = painter.layout(1410.0, 44.0, &frame, None);
        let title = &pills[1];
        let workspaces = &pills[2];
        assert!(matches!(&title.body, Body::Pieces(p) if p[0].ellipsize.is_some()));
        // Margin boxes touch: title right margin + workspace left margin.
        let gap = workspaces.x - (title.x + title.w);
        assert!(
            (2.0 * PILL_MARGIN_X..2.0 * PILL_MARGIN_X + 1.0).contains(&gap),
            "{gap}"
        );
    }

    #[test]
    fn title_limit_is_45_hinted_cells() {
        let painter = Painter::new().unwrap();
        assert_eq!(painter.title_limit, 45 * 8 * pango::SCALE);
    }

    #[test]
    fn short_title_keeps_natural_width() {
        let mut painter = Painter::new().unwrap();
        painter.set_scale(2.0);
        let piece = painter.title_piece("Custom bar alignment with Waybar", 500.0);
        assert!(piece.ellipsize.is_none());
    }

    #[test]
    fn reports_logical_height() {
        let height = Painter::new().unwrap().bar_height();
        assert_eq!(height, 44, "waybar settled on 44px for this font");
    }

    #[test]
    fn clock_tick_repaints_only_the_clock_interior() {
        let mut painter = Painter::new().unwrap();
        painter.set_scale(2.0);
        let (a, _) = painter.layout(1000.0, 44.0, &frame("01:00:00 PM"), None);
        let (b, _) = painter.layout(1000.0, 44.0, &frame("01:00:01 PM"), None);
        let plan = damage(&a, &b);
        assert!(plan.spans.is_empty(), "{plan:?}");
        assert_eq!(plan.interiors.len(), 1);
        assert!(b[plan.interiors[0]].x > 800.0, "the clock pill");
    }

    #[test]
    fn resized_pills_repaint_as_spans() {
        let mut painter = Painter::new().unwrap();
        painter.set_scale(2.0);
        let (a, _) = painter.layout(1000.0, 44.0, &frame("01:00:00 PM"), None);
        let wider = Frame {
            title: "a much longer window title",
            ..frame("01:00:00 PM")
        };
        let (b, _) = painter.layout(1000.0, 44.0, &wider, None);
        let plan = damage(&a, &b);
        assert_eq!(plan.interiors, Vec::<usize>::new());
        assert_eq!(plan.spans.len(), 1);
    }

    /// The bar as it looks on the real screen, with icons in every module.
    fn real_frame<'a>(tags: &'a [Tag], clock: &'a str, battery: &'a str) -> Frame<'a> {
        Frame {
            tags,
            title: "\u{25d0} Custom bar alignment with Waybar",
            layout: "\u{f0db} Fair",
            temp: "\u{f2c9} 46\u{b0}C",
            mem: "\u{f035b} 5.4GB",
            battery,
            sink_muted: true,
            source_muted: true,
            night_on: false,
            kernel: "\u{f17c} 7.3.6",
            kernel_class: KernelClass::Current,
            clock,
            tray: &[],
            tray_gen: 0,
        }
    }

    #[test]
    fn partial_repaints_match_full_repaints() {
        let tags: Vec<Tag> = (1..=10)
            .map(|index| Tag {
                index,
                active: index == 1,
                urgent: false,
            })
            .collect();
        let hover_tags: Vec<Tag> = (1..=10)
            .map(|index| Tag {
                index,
                active: index == 3,
                urgent: index == 7,
            })
            .collect();
        let frames = [
            real_frame(&tags, "03:41:05 PM", "\u{f007c} 36%"),
            // Clock tick: interior of the clock capsule.
            real_frame(&tags, "03:41:06 PM", "\u{f007c} 36%"),
            // Battery level and icon change at the same width.
            real_frame(&tags, "03:41:07 PM", "\u{f007b} 29%"),
            // Charging bolt.
            real_frame(&tags, "03:41:08 PM", "\u{f0084} 29%"),
            // Mic unmuted, kernel highlighted, night light on, tags moved.
            Frame {
                source_muted: false,
                night_on: true,
                kernel_class: KernelClass::Update,
                ..real_frame(&hover_tags, "12:00:00 AM", "\u{f0084} 100%")
            },
            // Title spinner and a longer title that pushes nothing else.
            Frame {
                title: "\u{25d3} Custom bar alignment with Waybar and more",
                ..real_frame(&hover_tags, "12:00:01 AM", "\u{f0084} 100%")
            },
        ];
        let mut painter = Painter::new().unwrap();
        painter.set_scale(2.0);
        let (w, h) = (1410_i32, 44_i32);
        let (bw, bh, stride) = (w * 2, h * 2, w * 2 * 4);
        let size = (stride * bh) as usize;
        let mut on_screen = vec![0u8; size];
        let mut previous: Vec<Pill> = Vec::new();
        let mut interiors = 0;
        for (step, frame) in frames.iter().enumerate() {
            let (pills, _) = painter.layout(f64::from(w), f64::from(h), frame, None);
            let plan = (step > 0).then(|| damage(&previous, &pills));
            painter
                .paint(
                    wrap(&mut on_screen, bw, bh, stride),
                    f64::from(h),
                    &pills,
                    plan.as_ref(),
                    &[],
                )
                .unwrap();
            let mut fresh = vec![0u8; size];
            painter
                .paint(
                    wrap(&mut fresh, bw, bh, stride),
                    f64::from(h),
                    &pills,
                    None,
                    &[],
                )
                .unwrap();
            assert!(on_screen == fresh, "frame {step} differs after {plan:?}");
            interiors += plan.map_or(0, |plan| plan.interiors.len());
            previous = pills;
        }
        assert!(
            interiors >= 4,
            "the in-place path ran only {interiors} times"
        );
    }
}
