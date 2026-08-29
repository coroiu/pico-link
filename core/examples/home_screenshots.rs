//! Verification fixture for bead pico-link-1v5: dumps a zoomed PNG of the
//! Home status face in its two codec states -- no link, and a live
//! connected codec -- per the project's rendering-verification rule
//! (CLAUDE.md: "Tests pass + a 1x PNG... is INSUFFICIENT evidence for a
//! render change... inspect framebuffers and PNGs at ZOOM"). Committed as
//! fixtures (`home-screenshots/`) so a future diff catches exactly this
//! bead's regression class: the hero silently reverting to `NO LINK`
//! while a device is actually connected (`core/src/render/home.rs`
//! previously hardcoded `CodecStatus::NoLink` unconditionally).
//!
//! Same shape as `wizard_screenshots.rs`: drives `pico_link_core::App`
//! directly via `Event::CodecChanged`/`Event::LinkStateChanged`, since
//! C-originated events have no emulator-side HTTP injection path today.
//! Run with `cargo run --example home_screenshots -p pico-link-core --
//! <out-dir>`.

use std::env;
use std::path::{Path, PathBuf};

use embedded_graphics::prelude::RgbColor;
use pico_link_core::{App, ConnectedCodec, DeviceEntry, Event, LinkState};

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

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-home-screenshots"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    // --- No link: the default, un-driven state (design section 15's
    // "absent, never frozen or faked" -- this is the honest state before
    // any Event::CodecChanged has ever arrived). ---
    let mut app = App::new(240, 240);
    save_zoomed_png(&mut app, &out_dir, "01_no_link");

    // --- Connected: a live LDAC link at its nominal 990 kbps, on a named
    // device -- proves the hero renders the codec word/bitrate from
    // `BtModel::connected_codec` instead of the old hardcoded `NoLink`. ---
    let mut app = App::new(240, 240);
    let addr = [0xCC; 6];
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Sony WH-1000XM5"), rssi: -40 }));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    save_zoomed_png(&mut app, &out_dir, "02_connected_ldac");

    // --- Disconnect after being connected: proves the codec is cleared,
    // never left stale (this bead's core `set_link_state` fix) -- the
    // hero must fall straight back to `NO LINK`, matching 01 exactly. ---
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    save_zoomed_png(&mut app, &out_dir, "03_disconnected_after_ldac");
}
