//! Verification fixture for pico-link-4vb.4 (T5, the remembered-devices
//! Devices screen): dumps a zoomed PNG of every state design section 6/8
//! (`.planning/design/2026-09-01-remembered-devices.md`) calls out, per the
//! project's rendering-verification rule (CLAUDE.md: "Tests pass + a 1x
//! PNG... is INSUFFICIENT evidence for a render change... inspect
//! framebuffers and PNGs at ZOOM"). Not a test -- a standalone example, run
//! on demand:
//!
//! ```sh
//! cargo run --example devices_screenshots -p pico-link-core -- <out-dir>
//! ```
//!
//! Drives `pico_link_core::App` directly (not through the emulator's
//! headless HTTP surface), because the `PairedDevice*`/`StoreLoaded` events
//! this screen depends on have no emulator-side injection path -- only
//! `NavIntent`s do. Mirrors `wizard_screenshots.rs`'s own shape.

use std::env;
use std::path::{Path, PathBuf};

use embedded_graphics::prelude::RgbColor;
use pico_link_core::input::NavIntent;
use pico_link_core::{App, Event, LinkState, PairedDevice, StoreStatus};

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

/// Home(1) -> Devices(2): Home starts on its status face; the first
/// `Select` toggles it to the menu face (Bluetooth pre-selected), the
/// second activates that row, pushing Devices.
fn open_devices(app: &mut App) {
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
}

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-devices-screenshots"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    // --- Home, menu face (design row 2): the first `Select` out of
    // `open_devices` toggles Home's status face to its menu face
    // (Bluetooth pre-selected); captured here rather than only passed
    // through, since no other fixture set renders this face on its own. A
    // reads `open` (design doc section 4: both Bluetooth/Settings rows
    // push a deeper screen and draw a caret). ---
    let mut app = App::new(240, 240);
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    save_zoomed_png(&mut app, &out_dir, "00_home_menu_face");

    // --- First run: nothing remembered yet -- the only row is "Pair new
    // headphones" (design section 4). Proves the empty state is navigable,
    // not a dead/blank-looking single-row screen. ---
    let mut app = App::new(240, 240);
    open_devices(&mut app);
    save_zoomed_png(&mut app, &out_dir, "01_first_run_pair_new_only");

    // --- One nameless paired row: renders "(unknown device)" plus the
    // address's last three bytes as the discriminator (design section
    // 4/13), sublabelled "Paired". ---
    let mut app = App::new(240, 240);
    app.handle_event(upsert([0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33], "", 1));
    open_devices(&mut app);
    save_zoomed_png(&mut app, &out_dir, "02_nameless_paired_row");

    // --- Connected pinned above two others: the connected device (design
    // section 4: link_state == Connected and its addr is in `paired`)
    // pins first sublabelled "Connected"; the remaining two sort
    // MRU-descending below it, sublabelled "Paired". ---
    let mut app = App::new(240, 240);
    let connected_addr = [1, 1, 1, 1, 1, 1];
    app.handle_event(upsert(connected_addr, "Connected Cans", 1));
    app.handle_event(upsert([2, 2, 2, 2, 2, 2], "Older Pair", 2));
    app.handle_event(upsert([3, 3, 3, 3, 3, 3], "Newer Pair", 3));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr: connected_addr, degraded: false });
    app.poll_command(); // drain PersistDevice -- irrelevant to rendering
    // C echoes the upsert once the persist write actually lands (design
    // section 7's hazard 4) -- this is what actually refreshes the pinned
    // row with the live `connected_addr`.
    app.handle_event(upsert(connected_addr, "Connected Cans", 4));
    open_devices(&mut app);
    save_zoomed_png(&mut app, &out_dir, "03_connected_pinned_above_two_others");

    // --- 8-full: pick-one-to-forget (design section 4). Selecting "Pair
    // new headphones" at capacity opens the picker instead of the wizard --
    // gated before any radio work. ---
    let mut app = App::new(240, 240);
    for i in 0..8u8 {
        app.handle_event(upsert([i, i, i, i, i, i], &format!("Device {i}"), u32::from(i)));
    }
    open_devices(&mut app);
    save_zoomed_png(&mut app, &out_dir, "04a_eight_full_devices_screen");
    // Navigate onto "Pair new headphones" (row 8, the last row) and select it.
    for _ in 0..8 {
        app.handle_input(vec![NavIntent::Down]);
    }
    app.handle_input(vec![NavIntent::Select]);
    save_zoomed_png(&mut app, &out_dir, "04b_eight_full_pick_one_to_forget");

    // --- Store-corrupt: a boot with a corrupt record (design point 5's
    // "never renders identically to a healthy new one" -- not yet surfaced
    // by any screen, but the Devices screen must still render sanely, not
    // blank or panic, on a corrupt-store boot with nothing remembered). ---
    let mut app = App::new(240, 240);
    app.handle_event(Event::StoreLoaded { status: StoreStatus::RecordCorrupt });
    open_devices(&mut app);
    save_zoomed_png(&mut app, &out_dir, "05_store_corrupt_boot");

    // --- X on a paired row: the forget-confirm screen (design section 4). ---
    let mut app = App::new(240, 240);
    app.handle_event(upsert([6, 6, 6, 6, 6, 6], "Cans", 1));
    open_devices(&mut app);
    app.handle_input(vec![NavIntent::ShortcutX]);
    save_zoomed_png(&mut app, &out_dir, "06_forget_confirm");

    // --- A on the connected row: the stub device-detail screen (design
    // section 4, the `build_settings_screen` precedent). ---
    let mut app = App::new(240, 240);
    let addr = [4, 4, 4, 4, 4, 4];
    app.handle_event(upsert(addr, "Connected Cans", 1));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.poll_command();
    app.handle_event(upsert(addr, "Connected Cans", 2));
    open_devices(&mut app);
    app.handle_input(vec![NavIntent::Select]);
    save_zoomed_png(&mut app, &out_dir, "07_device_detail_stub");

    // --- Settings (stub, design row 7): reached from Home's status face
    // via ShortcutY, per `build_settings_screen`'s doc comment. No
    // widgets, so A is Inert -- design doc
    // .planning/design/2026-09-02-a-button-label-rule.md section 4's
    // "Device detail (stub) / Settings (stub)" row.
    let mut app = App::new(240, 240);
    app.handle_input(vec![NavIntent::ShortcutY]); // Home status face -> Settings
    save_zoomed_png(&mut app, &out_dir, "08_settings_stub");
}
