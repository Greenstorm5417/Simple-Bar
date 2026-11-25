//! Benchmarks for the work simple-bar does on every event: parsing Mango
//! snapshots and pulse replies, laying the bar out, and painting it either
//! fully or only where it changed.
//!
//! Run with `cargo bench`. Painting renders real glyphs, so the font from
//! the bar's CSS (`JetBrainsMono Nerd Font`) should be installed.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use simple_bar::mango::{Tag, parse_monitors};
use simple_bar::model::KernelClass;
use simple_bar::paint::{Canvas, EmptyTitle, Frame, Painter, damage};
use simple_bar::{json, pulse};

/// A real `watch all-monitors` line captured from Mango.
const MANGO_EVENT: &str = r#"{"monitors":[{"name":"eDP-1","active":true,"is_hdr":false,"is_vrr":false,"x":0,"y":0,"width":1422,"height":888,"scale":1.3500000238418579,"transform":"normal","layout_index":12,"layout_symbol":"F","last_open_surface":"simple-bar","tag_num":10,"hide_clients":0,"tags":[{"index":1,"is_active":true,"is_urgent":false,"layout":"F","client_count":1},{"index":2,"is_active":false,"is_urgent":false,"layout":"F","client_count":1},{"index":3,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":4,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":5,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":6,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":7,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":8,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":9,"is_active":false,"is_urgent":false,"layout":"F","client_count":0},{"index":10,"is_active":false,"is_urgent":false,"layout":"F","client_count":0}],"active_tags":[1],"active_client":{"id":18,"title":"◐ Custom bar alignment with Waybar","appid":"foot"},"keymode":"default","keyboardlayout":"English (US)"}]}"#;

const WIDTH: i32 = 1410;
const HEIGHT: i32 = 44;
const SCALE: i32 = 2;

fn tags() -> Vec<Tag> {
    (1..=10)
        .map(|index| Tag {
            index,
            active: index == 2,
            urgent: false,
        })
        .collect()
}

fn frame<'a>(tags: &'a [Tag], clock: &'a str) -> Frame<'a> {
    Frame {
        tags,
        title: "Greenstorm5417/kisslock \u{2014} LibreWolf",
        empty_title: EmptyTitle::Hide,
        layout: "\u{f0db} Fair",
        temp: "\u{f2c9} 38\u{b0}C",
        mem: "\u{f035b} 3.5GB",
        battery: "\u{f007c} 36%",
        sink_muted: true,
        source_muted: false,
        night_on: false,
        kernel: "\u{f17c} 7.3.6",
        kernel_class: KernelClass::Current,
        clock,
        tray: &[],
        tray_gen: 0,
    }
}

fn canvas(pixels: &mut [u8]) -> Canvas<'_> {
    Canvas {
        pixels,
        width: WIDTH * SCALE,
        height: HEIGHT * SCALE,
        stride: WIDTH * SCALE * 4,
    }
}

fn parsing(c: &mut Criterion) {
    c.bench_function("json/mango_event", |b| {
        b.iter(|| json::parse(black_box(MANGO_EVENT)));
    });
    c.bench_function("mango/parse_monitors", |b| {
        b.iter(|| parse_monitors(black_box(MANGO_EVENT)));
    });

    // index, name, description, sample spec, channel map, owner, volume, mute
    let mut reply = vec![b'L', 0, 0, 0, 5];
    for text in [
        "alsa_output.pci-0000_00_1f.3.analog-stereo",
        "Built-in Audio",
    ] {
        reply.push(b't');
        reply.extend_from_slice(text.as_bytes());
        reply.push(0);
    }
    reply.extend_from_slice(&[b'a', 3, 2, 0, 0, 0xbb, 0x80, b'm', 2, 1, 2]);
    reply.extend_from_slice(&[b'L', 0xff, 0xff, 0xff, 0xff]);
    reply.extend_from_slice(&[b'v', 2, 0, 1, 0, 0, 0, 1, 0, 0, b'1']);
    c.bench_function("pulse/reply_muted", |b| {
        b.iter(|| pulse::reply_muted(black_box(&reply)));
    });
}

fn drawing(c: &mut Criterion) {
    let tags = tags();
    let mut painter = Painter::new().expect("cairo");
    painter.set_scale(f64::from(SCALE));
    let (w, h) = (f64::from(WIDTH), f64::from(HEIGHT));

    c.bench_function("paint/layout", |b| {
        b.iter(|| painter.layout(w, h, black_box(&frame(&tags, "03:22:42 PM")), None));
    });

    let mut pixels = vec![0u8; (WIDTH * SCALE * 4 * HEIGHT * SCALE) as usize];
    let (before, _) = painter.layout(w, h, &frame(&tags, "03:22:42 PM"), None);
    let (after, _) = painter.layout(w, h, &frame(&tags, "03:22:43 PM"), None);

    c.bench_function("paint/full_frame", |b| {
        b.iter(|| {
            painter
                .paint(canvas(&mut pixels), h, black_box(&after), None, &[])
                .unwrap()
        });
    });

    c.bench_function("paint/damage", |b| {
        b.iter(|| damage(black_box(&before), black_box(&after)));
    });

    let plan = damage(&before, &after);
    c.bench_function("paint/clock_tick", |b| {
        b.iter(|| {
            painter
                .paint(canvas(&mut pixels), h, black_box(&after), Some(&plan), &[])
                .unwrap()
        });
    });
}

criterion_group!(benches, parsing, drawing);
criterion_main!(benches);
