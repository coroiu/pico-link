//! Verification fixture for pico-link-7jol.4 (the device page): dumps a
//! zoomed PNG of every state reachable through the public `App` surface,
//! per the project's rendering-verification rule (CLAUDE.md: "Tests
//! pass... is INSUFFICIENT evidence for a render change... inspect
//! framebuffers and PNGs at ZOOM"). Not a test -- a standalone example, run
//! on demand:
//!
//! ```sh
//! cargo run --example device_page_screenshots -p pico-link-core -- <out-dir>
//! ```
//!
//! Only the CONNECTED device's page is reachable this way in this bead
//! (Devices' `X` still opens the forget confirm, not the device page --
//! rebinding it is its own bead, per
//! `.planning/design/2026-09-07-device-page-and-single-select-picker.md`
//! §9). Mirrors `devices_screenshots.rs`'s own shape.

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

fn upsert(addr: [u8; 6], name: &str, mru_seq: u32) -> Event {
    Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality: 0, preset_id: 0 })
}

/// Home(1) -> Devices(2), same shape as `devices_screenshots.rs`'s own
/// helper.
fn open_devices(app: &mut App) {
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
}

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-device-page-screenshots"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    // --- Connected device, streaming LDAC: CODEC shows the live codec,
    // SAMPLE RATE/USB IN/A2DP dash honestly, ADDRESS in small font, X =
    // drop. ---
    let addr = [0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2];
    let mut app = App::new(240, 240);
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.poll_command(); // drain PersistDevice
    app.handle_event(upsert(addr, "Sony WH-1000XM5", 1));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    open_devices(&mut app);
    app.handle_input(vec![NavIntent::Select]); // connected row -> device page
    save_zoomed_png(&mut app, &out_dir, "01_connected_ldac");

    // --- Focus moved down to ADDRESS, proving the small-value font row
    // and that focus traversal lands on every Readonly row. Bead
    // pico-link-7jol.5: the row order is now CODEC(0), QUALITY(1),
    // SAMPLE RATE(2), USB IN(3), A2DP(4), ADDRESS(5) -- one more Down than
    // before QUALITY existed. ---
    app.handle_input(vec![NavIntent::Down, NavIntent::Down, NavIntent::Down, NavIntent::Down, NavIntent::Down]);
    save_zoomed_png(&mut app, &out_dir, "02_focus_on_address_row");

    // --- Focus on Forget this device (last row, red, the only pressable
    // one). ---
    app.handle_input(vec![NavIntent::Down]);
    save_zoomed_png(&mut app, &out_dir, "03_focus_on_forget_row");

    // --- X (drop): queues Command::Disconnect, then the link actually
    // drops -- refresh_stack must flip CODEC back to Automatic and the
    // rail from `drop` to `link`, live, without leaving this screen.
    // QUALITY disappears too (design §2/§8: absent, not dim, once the
    // stored ldac_quality is 0/never-chosen and the device is
    // disconnected). ---
    app.handle_input(vec![NavIntent::Up, NavIntent::Up, NavIntent::Up, NavIntent::Up, NavIntent::Up, NavIntent::Up]); // back to CODEC (row 0)
    app.handle_input(vec![NavIntent::ShortcutX]);
    app.poll_command(); // drain the queued Disconnect
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    save_zoomed_png(&mut app, &out_dir, "04_disconnected_automatic_link_rail");

    // --- Forget confirm, reached through the device page (not Devices'
    // X): proves the shared ConfirmView still opens correctly from here.
    // QUALITY is gone now (disconnected, never chosen), so the page is
    // back to 6 rows: CODEC, SAMPLE RATE, USB IN, A2DP, ADDRESS, Forget. ---
    app.handle_input(vec![NavIntent::Down, NavIntent::Down, NavIntent::Down, NavIntent::Down, NavIntent::Down]); // -> Forget row
    app.handle_input(vec![NavIntent::Select]);
    save_zoomed_png(&mut app, &out_dir, "05_forget_confirm_from_device_page");
}
