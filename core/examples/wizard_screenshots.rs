//! Verification fixture for pico-link-znb.7 (E5, the pairing wizard): dumps
//! a zoomed PNG of every phase (and every named failure) to a scratch
//! directory, per the bead's "DONE LOOKS LIKE" ask and the project's
//! rendering-verification rule (CLAUDE.md: "Tests pass + a 1x PNG... is
//! INSUFFICIENT evidence for a render change... inspect framebuffers and
//! PNGs at ZOOM"). Not a test -- a standalone example so it can be run
//! on demand (`cargo run --example wizard_screenshots -p pico-link-core --
//! <out-dir>`) without slowing down `cargo test`.
//!
//! Drives `pico_link_core::App` directly (not through the emulator's
//! headless HTTP surface), because C-originated `Event`s (device
//! discovery, connect sub-steps, retries, outcomes) have no emulator-side
//! injection path today -- only `NavIntent`s do. This is exactly the same
//! level `core`'s own `#[cfg(test)]` modules already drive `App` at.

use std::env;
use std::path::{Path, PathBuf};

use embedded_graphics::prelude::RgbColor;
use pico_link_core::input::NavIntent;
use pico_link_core::{App, ConnectFailureReason, DeviceEntry, Event};

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

/// Home(1) -> Devices(2) -> Wizard(3), all three `Select`s. Fixed
/// (pico-link-0r3) to match the post-E7 navigator: `App::new` starts on
/// Home's status face, not on Devices, so reaching the wizard now takes
/// three `Select`s (Home status -> menu face with Bluetooth pre-selected;
/// menu Bluetooth row -> pushes Devices; Devices' "Scan for headphones"
/// row -> pushes the wizard), not the one this function used to send --
/// see `wizard.rs`'s own `#[cfg(test)]` `open_wizard` for the up-to-date
/// version this now mirrors. The stale one-`Select` version silently
/// landed every fixture below on the wrong screen/phase instead of
/// failing loudly, since a `Select` is always accepted by *something*
/// (the Home status face, then the Devices list) -- it just wasn't the
/// wizard.
///
/// pico-link-4vb.2 deleted the old instructions phase 1 -- the third
/// `Select` above now lands directly in `WizardPhase::Scanning` (and
/// queues `Command::StartScan` itself), so there is no separate
/// `start_scan` step any more; every call site below that used to call
/// `start_scan(&mut app)` right after `open_wizard` had that line deleted.
fn open_wizard(app: &mut App) {
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
    app.handle_input(vec![NavIntent::Select]); // "Scan for headphones" row -> pushes the wizard, straight into Scanning
}

fn select_device(app: &mut App) {
    app.handle_input(vec![NavIntent::Select]); // Scanning row 0 -> Connecting
}

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-wizard-screenshots"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    // --- Phase 2: scanning, with a late-arriving name AND a permanently
    // nameless device (design's own "DONE LOOKS LIKE" fixture ask) ---
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    let named_addr = [0xAA; 6];
    let nameless_addr = [0xBB; 6];
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: named_addr, name: String::new(), rssi: -45 }));
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: nameless_addr, name: String::new(), rssi: -82 }));
    // The name resolves later for the first device only.
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: named_addr, name: String::from("Sony WH-1000XM5"), rssi: -45 }));
    save_zoomed_png(&mut app, &out_dir, "02_scanning_late_name_and_nameless");

    // --- Phase 2b: real-world long device names (pico-link-ok1) --
    // `VerticalList` row labels come from live BT scan results, so their
    // length is never in this widget's control -- unlike "Sony
    // WH-1000XM5" (phase 2, ~15 chars, the committed fixture that used to
    // make the missing width clamp invisible), these two overflow the
    // pre-fix ~14-15-character budget and must now truncate with an
    // ellipsis rather than running into the disclosure caret.
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    app.handle_event(Event::DeviceDiscovered(DeviceEntry {
        addr: [0xCC; 6],
        name: String::from("Sennheiser Momentum 4 Wireless"),
        rssi: -50,
    }));
    app.handle_event(Event::DeviceDiscovered(DeviceEntry {
        addr: [0xDD; 6],
        name: String::from("Bang and Olufsen Beoplay H95"),
        rssi: -60,
    }));
    save_zoomed_png(&mut app, &out_dir, "02b_scanning_long_device_names");

    // --- Phase 2c: F10, the real 4-bar signal glyph (pico-link-0r3) --
    // five rows spanning every bar count the RSSI->level mapping
    // (`signal_bar_level`) produces, including both ends (0 and 4 bars),
    // so a zoomed screenshot shows the glyph at every distinct fill
    // level side by side.
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [0x10; 6], name: String::from("4 bars"), rssi: -40 }));
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [0x11; 6], name: String::from("3 bars"), rssi: -55 }));
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [0x12; 6], name: String::from("2 bars"), rssi: -65 }));
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [0x13; 6], name: String::from("1 bar"), rssi: -75 }));
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [0x14; 6], name: String::from("0 bars"), rssi: -95 }));
    save_zoomed_png(&mut app, &out_dir, "02c_scanning_signal_bar_levels");

    // --- Phase 3: nothing found ---
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    app.handle_event(Event::LinkStateChanged(pico_link_core::LinkState::Idle));
    save_zoomed_png(&mut app, &out_dir, "03_nothing_found");

    // --- Phase 4: all four named connecting sub-steps ---
    let steps = [
        (pico_link_core::app::ConnectStep::Connecting, "04a_connecting_acl"),
        (pico_link_core::app::ConnectStep::Pairing, "04b_connecting_pairing"),
        (pico_link_core::app::ConnectStep::SettingUpAudio, "04c_connecting_audio"),
        (pico_link_core::app::ConnectStep::NegotiatingCodec, "04d_connecting_codec"),
    ];
    for (step, name) in steps {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1; 6], name: String::from("Cans"), rssi: -50 }));
        select_device(&mut app);
        app.handle_event(Event::ConnectStepChanged(step));
        save_zoomed_png(&mut app, &out_dir, name);
    }

    // --- Phase 5: "Still trying (2)" -- the 6s surfacing point ---
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [2; 6], name: String::from("Cans"), rssi: -50 }));
    select_device(&mut app);
    app.handle_event(Event::ConnectRetrying { attempt: 1 });
    app.handle_event(Event::ConnectRetrying { attempt: 2 });
    save_zoomed_png(&mut app, &out_dir, "05_not_responding_still_trying_2");

    // --- Phase 6: each of the five named failures ---
    let failures = [
        (ConnectFailureReason::Timeout, "06a_failed_timeout"),
        (ConnectFailureReason::Rejected, "06b_failed_rejected"),
        (ConnectFailureReason::NoA2dpSink, "06c_failed_no_a2dp_sink_back_only"),
        (ConnectFailureReason::NeedsPin, "06d_failed_needs_pin_back_only"),
        (ConnectFailureReason::RadioError, "06e_failed_radio_error"),
    ];
    for (reason, name) in failures {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [3; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Cans"), rssi: -50 }));
        select_device(&mut app);
        app.handle_event(Event::ConnectFailed { addr, reason });
        save_zoomed_png(&mut app, &out_dir, name);
    }

    // --- Phase 6: plain success ---
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [4; 6], name: String::from("Cans"), rssi: -50 }));
    select_device(&mut app);
    app.handle_event(Event::ConnectSucceeded { addr: [4; 6], degraded: false });
    save_zoomed_png(&mut app, &out_dir, "06f_succeeded_plain");

    // --- Phase 6: degraded success ---
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [5; 6], name: String::from("Cans"), rssi: -50 }));
    select_device(&mut app);
    app.handle_event(Event::ConnectSucceeded { addr: [5; 6], degraded: true });
    save_zoomed_png(&mut app, &out_dir, "06g_succeeded_degraded");

    println!("done -- {} PNGs written to {}", 1 + 1 + 1 + 4 + 1 + 5 + 1 + 1, out_dir.display());
}
