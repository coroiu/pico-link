//! Verifiable deliverable for pico-link-znb.10 step 6: proves the
//! `RenderCtx`/`redraw_after`/event-timestamp seam end to end through one
//! real consumer -- the pairing wizard's phase 4 (Connecting) elapsed-time
//! readout (`render::wizard::render_connecting_steps`).
//!
//! Per this project's rendering-verification rule (CLAUDE.md), "tests pass
//! plus a PNG" is insufficient evidence for a render change on its own.
//! This dumps two headless captures at two pinned `App::tick` instants,
//! and they must be inspected at zoom to confirm the elapsed-seconds text
//! actually changed, not just that some pixel comparison in a unit test
//! passed.
//!
//! Run with: `cargo run -p pico-link-core --example wizard_liveness_probe`
//! Writes `wizard_liveness_t0.png` (0s elapsed) and `wizard_liveness_t1.png`
//! (2s elapsed), both 4x nearest-neighbor upscaled, to the current
//! directory.

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::RgbColor;
use pico_link_core::app::{App, DeviceEntry, Event};
use pico_link_core::input::NavIntent;
use pico_link_core::render::FrameBuffer565;

const SCALE: u32 = 4;

fn dump_png_scaled(framebuffer: &FrameBuffer565, path: &str, scale: u32) {
    let width = framebuffer.width();
    let height = framebuffer.height();
    let mut image = image::RgbImage::new(width, height);

    for pixel in framebuffer.pixels() {
        let point = pixel.0;
        let color: Rgb565 = pixel.1;
        #[allow(clippy::cast_sign_loss)]
        image.put_pixel(point.x as u32, point.y as u32, image::Rgb([color.r() << 3, color.g() << 2, color.b() << 3]));
    }

    let scaled = image::imageops::resize(&image, width * scale, height * scale, image::imageops::FilterType::Nearest);
    scaled.save(path).expect("failed to write PNG");
}

fn main() {
    let mut app = App::new(240, 240);

    // Home status face -> menu face (Bluetooth pre-selected) -> Devices
    // ("Scan for headphones" pre-selected) -> the wizard, at phase 1
    // (Instructions).
    app.handle_input(vec![NavIntent::Select]);
    app.handle_input(vec![NavIntent::Select]);
    app.handle_input(vec![NavIntent::Select]);

    // Phase 1 -> phase 2 (Scanning): pins App's clock at t=0 first, so the
    // Scanning phase's own `started` (not under test here, but exercised
    // along the way) is deterministic too.
    app.tick(0);
    app.handle_input(vec![NavIntent::Select]);

    let addr = [0xAA; 6];
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: "Sony WH-1000XM5".into(), rssi: -40 }));

    // Phase 2 -> phase 4 (Connecting): activates the only scan-result row.
    // `WizardPhase::Connecting::started` is backfilled from this same
    // now_us == 0.
    app.handle_input(vec![NavIntent::Select]);

    // Capture 1: render right at the connect attempt's start (elapsed 0s).
    app.tick(0);
    let frame_t0 = app.render();
    dump_png_scaled(frame_t0, "wizard_liveness_t0.png", SCALE);
    println!("wrote wizard_liveness_t0.png (elapsed 0s)");

    // Capture 2: 2 seconds later. `tick` alone (no new event, no input)
    // must be enough to mark the app dirty, purely via `redraw_after` --
    // that's the seam this probe exists to prove end to end.
    app.tick(2_000_000);
    assert!(app.dirty(), "redraw_after must have marked the app dirty from tick alone, 2s into a Connecting attempt");
    let frame_t1 = app.render();
    dump_png_scaled(frame_t1, "wizard_liveness_t1.png", SCALE);
    println!("wrote wizard_liveness_t1.png (elapsed 2s)");
}
