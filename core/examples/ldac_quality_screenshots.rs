//! Verification fixture for pico-link-7jol.5 (the `QUALITY` row and its
//! picker): dumps a zoomed PNG of the four states Tess's design handoff
//! (`.planning/design/2026-09-07-ldac-quality-selector.md` §9, "For
//! Tess") asks for on each of the device page and the picker, plus Home's
//! bitrate line with/without the `ADAPTIVE` tag. Not a test -- a
//! standalone example, run on demand:
//!
//! ```sh
//! cargo run --example ldac_quality_screenshots -p pico-link-core -- <out-dir>
//! ```

use std::env;
use std::path::{Path, PathBuf};

use embedded_graphics::prelude::RgbColor;
use pico_link_core::input::NavIntent;
use pico_link_core::{App, ConnectedCodec, Event, LinkState, PairedDevice};

const ZOOM: u32 = 3;

fn save_zoomed_png(app: &mut App, out_dir: &Path, name: &str) {
    let framebuffer = app.render();
    let mut image = image::RgbImage::new(framebuffer.width(), framebuffer.height());
    for pixel in framebuffer.pixels() {
        let color = pixel.1;
        #[allow(clippy::cast_sign_loss)]
        image.put_pixel(
            pixel.0.x as u32,
            pixel.0.y as u32,
            image::Rgb([
                (color.r() << 3) | (color.r() >> 2),
                (color.g() << 2) | (color.g() >> 4),
                (color.b() << 3) | (color.b() >> 2),
            ]),
        );
    }
    let zoomed = image::imageops::resize(
        &image,
        framebuffer.width() * ZOOM,
        framebuffer.height() * ZOOM,
        image::imageops::FilterType::Nearest,
    );
    let path = out_dir.join(format!("{name}.png"));
    zoomed.save(&path).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    println!("wrote {}", path.display());
}

fn upsert(addr: [u8; 6], name: &str, mru_seq: u32, ldac_quality: u8) -> Event {
    Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality, preset_id: 0 })
}

fn open_devices(app: &mut App) {
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
}

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-ldac-quality-screenshots"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    let addr = [0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2];

    // === Device page's QUALITY row, four states (design §9 "For Tess" 4/5) ===

    // --- 1. Fixed pick (never chosen -> renders the effective default,
    // checked, design §7), connected and streaming LDAC. ---
    {
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, 0));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        save_zoomed_png(&mut app, &out_dir, "row_01_fixed_990_default");
    }

    // --- 2. Adaptive, streaming: the row's live-updating "Adaptive · N"
    // form (design §4.2 amendment 2). ---
    {
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, 4));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        app.handle_event(Event::LdacBitrateChanged { kbps: 660 });
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        save_zoomed_png(&mut app, &out_dir, "row_02_adaptive_streaming_660");
    }

    // --- 3. Adaptive, disconnected: plain "Adaptive", never a stale
    // number (design §8: picking while disconnected is allowed, but the
    // row must never claim a live figure it doesn't have). ---
    {
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, 4));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        app.handle_input(vec![NavIntent::ShortcutX]); // drop
        app.poll_command();
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        save_zoomed_png(&mut app, &out_dir, "row_03_adaptive_disconnected_plain");
    }

    // --- 4. A fixed pick (330 kbps), connected, streaming -- the row's
    // trailing value is the stored/checked pin, unconditionally, "any
    // link state" (design §4.2). ---
    {
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, 3));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        save_zoomed_png(&mut app, &out_dir, "row_04_fixed_330_pinned");
    }

    // === The picker, four states (design §9 "For Tess" 6/7/8) ===

    // Reusable across the four picker screenshots below: a device that has
    // actually been connected once (so the device page, and therefore the
    // picker beneath it, are reachable per this bead's scope -- see
    // device-page-and-single-select-picker.md §9's "reachable only for the
    // connected device" note) and is optionally dropped back to
    // disconnected afterwards, with a given `ldac_quality` already chosen.
    // that has actually been connected once (so the device page, and
    // therefore the picker beneath it, are reachable) and is now
    // disconnected with a fixed pick already chosen.
    fn open_picker(ldac_quality: u8, adaptive_live_kbps: Option<u32>, connected: bool) -> App {
        let addr = [0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2];
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, ldac_quality));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        if let Some(kbps) = adaptive_live_kbps {
            app.handle_event(Event::LdacBitrateChanged { kbps });
        }
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        if !connected {
            app.handle_input(vec![NavIntent::ShortcutX]); // drop
            app.poll_command();
            app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        }
        app.handle_input(vec![NavIntent::Down]); // focus QUALITY (row 1)
        app.handle_input(vec![NavIntent::Select]); // -> picker
        app
    }

    // --- 6. Check on 990 kbps, disconnected -- Adaptive trailing "varies". ---
    let mut app = open_picker(1, None, false);
    save_zoomed_png(&mut app, &out_dir, "picker_06_check_on_990_disconnected_varies");

    // --- 7. Check on Adaptive, streaming -- trailing note "N now". ---
    let mut app = open_picker(4, Some(660), true);
    save_zoomed_png(&mut app, &out_dir, "picker_07_check_on_adaptive_660_now");

    // --- 8. Fresh device (ldac_quality == 0, "never chosen") -- check
    // sits on the firmware default (990 kbps), not blank (design §7). ---
    let mut app = open_picker(0, None, true);
    save_zoomed_png(&mut app, &out_dir, "picker_08_fresh_device_check_on_default");

    // --- 9. Focus moved to a different row than the check, proving focus
    // and check are independent (design: caret never moves the value). ---
    app.handle_input(vec![NavIntent::Down, NavIntent::Down]); // focus "330 kbps"
    save_zoomed_png(&mut app, &out_dir, "picker_09_focus_separate_from_check");

    // === Bonus: Home's bitrate line, with and without the ADAPTIVE tag
    // (design §6/§6.2) -- verifies hero.rs's new drawing code at zoom, per
    // this project's rendering-verification rule. ===

    // --- 10. Adaptive, streaming: "660 kbps  ADAPTIVE", tag dim, number
    // anchored at the same x fixed mode uses. ---
    {
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, 4));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        app.handle_event(Event::LdacBitrateChanged { kbps: 660 });
        save_zoomed_png(&mut app, &out_dir, "home_10_adaptive_streaming_tag");
    }

    // --- 11. Fixed 990, for contrast: identical number position, no tag. ---
    {
        let mut app = App::new(240, 240);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 1, 1));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        save_zoomed_png(&mut app, &out_dir, "home_11_fixed_990_no_tag");
    }
}
