//! Shared scenario/render logic for the Home status-face screenshot
//! fixtures (bead pico-link-1v5, alignment reworked by
//! `.planning/design/2026-09-01-home-alignment-grid.md`).
//!
//! **Included via `#[path]` into two different crates**:
//! `examples/home_screenshots.rs` (regenerates the committed
//! `home-screenshots/*.png` fixtures for real) and
//! `tests/home_screenshot_fixtures.rs` (proves a fresh render still
//! matches them). Both call exactly this code, so the scenario that
//! *produces* a fixture and the scenario the test *checks against* can
//! never independently drift from each other.
//!
//! That specific class of drift is why this file exists at all: before
//! bead pico-link-70b, the example still drove `Event::DeviceDiscovered`
//! alone to populate the connected device's name, but `HomeView` had
//! since been changed (bead pico-link-4vb.4/T4) to read the name from
//! `BtModel::paired`, populated only by `Event::PairedDeviceUpserted`.
//! The example silently started rendering an empty device-name line, the
//! committed `02_connected_ldac.png` fixture was never regenerated to
//! match, and the mismatch was caught only by a code reviewer's manual
//! diff on an unrelated bead (pico-link-ay0). A single shared source for
//! the scenario removes the possibility of the example and its own proof
//! disagreeing about what the scenario even is.

use std::path::Path;

use embedded_graphics::prelude::RgbColor;
use pico_link_core::{App, ConnectedCodec, DeviceEntry, Event, LinkState, PairedDevice, VolumeSource};

pub const ZOOM: u32 = 3;

/// Names of the fixtures `generate` writes, in the order it writes them --
/// also the base filenames (without `.png`) under `home-screenshots/` at
/// the repo root. `05`-`08` are VT6 (bead pico-link-4v2.6, design
/// `.planning/design/2026-09-07-volume-on-display.md`): the title-bar
/// volume element and its exceptional-state banners.
pub const FIXTURE_NAMES: [&str; 8] = [
    "01_no_link",
    "02_connected_ldac",
    "03_disconnected_after_ldac",
    "04_connected_with_out_level",
    "05_volume_62_percent",
    "06_volume_zero_banner",
    "07_muted_banner_host",
    "08_muted_with_out_level_still_swinging",
];

pub fn save_zoomed_png(app: &mut App, out_dir: &Path, name: &str) {
    let framebuffer = app.render();
    let mut image = image::RgbImage::new(framebuffer.width(), framebuffer.height());
    for pixel in framebuffer.pixels() {
        let color = pixel.1;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
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
}

/// Renders all three of [`FIXTURE_NAMES`] into `out_dir`, in the exact
/// scenario order/state bead pico-link-1v5 established (see the two
/// `App` mutations below for what each one proves).
pub fn generate(out_dir: &Path) {
    std::fs::create_dir_all(out_dir).expect("failed to create output directory");

    // --- No link: the default, un-driven state (design section 15's
    // "absent, never frozen or faked" -- this is the honest state before
    // any Event::CodecChanged has ever arrived). ---
    let mut app = App::new(240, 240);
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[0]);

    // --- Connected: a live LDAC link at its nominal 990 kbps, on a named
    // device -- proves the hero renders the codec word/bitrate from
    // `BtModel::connected_codec` instead of the old hardcoded `NoLink`. ---
    let mut app = App::new(240, 240);
    let addr = [0xCC; 6];
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Sony WH-1000XM5"), rssi: -40, class_of_device: 0 }));
    // `HomeView` reads the connected device's name from `BtModel::paired`,
    // not `discovered` (bead pico-link-4vb.4/T4: a name must survive long
    // after the scan that first found it is gone) -- so the scenario must
    // upsert it into `paired` the same way a real pairing does, or the
    // hero widget's device-name line silently renders empty (pico-link-70b).
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Sony WH-1000XM5"), mru_seq: 1 }));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[1]);

    // --- Disconnect after being connected: proves the codec is cleared,
    // never left stale -- the hero must fall straight back to `NO LINK`,
    // matching 01 exactly. ---
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[2]);

    // --- Connected, with a live stereo OUT level meter (bead
    // pico-link-du0, design section 21 E17): a fresh connected `App`
    // (not a reuse of the one above, which is now disconnected), ticked
    // to a nonzero `now_us` so `OutLevelSample::received_at` reads as a
    // real timestamp, then fed one `Event::LevelsChanged` reading.
    // Deliberately MID-RMS (bead pico-link-ajj code review): `rms_l`/
    // `rms_r` are chosen on the dBFS scale to land mid-column (9 and 10
    // of 16 segments) rather than near-full -- a near-full fixture can't
    // tell a correct dBFS mapping apart from the old buggy linear one at
    // a glance, which is exactly the failure mode this bead exists to
    // fix. `peak_l` is intentionally well above `rms_l` so the L
    // channel's peak-hold cap (the bright/red single segment) visibly
    // sits above its RMS-driven bar fill in the fixture -- proving the
    // two aren't the same number rendered twice. `peak_r`/`rms_r` are
    // close together, so the R channel shows an ordinary reading with
    // its hold cap right at the bar's edge. ---
    let mut app = App::new(240, 240);
    let addr = [0xDD; 6];
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Sony WH-1000XM5"), mru_seq: 1 }));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.tick(1);
    app.handle_event(Event::LevelsChanged { peak_l: 90, peak_r: 45, rms_l: 26, rms_r: 40 });
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[3]);

    // --- VT6 (design `.planning/design/2026-09-07-volume-on-display.md`):
    // a normal, unmuted, nonzero volume reading -- level 79 maps to
    // exactly 62% via `VolumeState::percent`'s formula, matching the
    // design's own worked example ("62%") so the fixture is directly
    // comparable to the design doc's ASCII sketch (section 8). ---
    let mut app = App::new(240, 240);
    let addr = [0xEE; 6];
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Sony WH-1000XM5"), mru_seq: 1 }));
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 909_000 }));
    app.handle_event(Event::VolumeChanged { level: 79, muted: false, source: VolumeSource::Sink });
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[4]);

    // --- VT6: unmuted but at 0% -- the VOLUME 0 banner ("HEADPHONE VOLUME
    // AT 0"), reachable in practice only from the headphones' own dial
    // (design section 4.3: macOS sends `muted` and 0% together, so a
    // host-originated zero is caught by the MUTED banner instead). The
    // OUT meter is deliberately absent here (no `LevelsChanged` event) --
    // fixture 08 below is where "the meter keeps moving while muted/zero"
    // is proven, so this one stays focused on the banner/title-bar text
    // alone. ---
    app.handle_event(Event::VolumeChanged { level: 0, muted: false, source: VolumeSource::Sink });
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[5]);

    // --- VT6: MUTED from the host -- title bar reads amber "MUTE", banner
    // reads "MUTED  Unmute on Mac" (the source-selected remedy that
    // replaces the old, defective "Press Up to raise" string -- bead
    // pico-link-31ey; the exact wording is trimmed slightly from the
    // design doc's literal string -- see `HeroStatusView::render`'s banner
    // match for the measured reason). ---
    app.handle_event(Event::VolumeChanged { level: 0, muted: true, source: VolumeSource::Host });
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[6]);

    // --- VT6: MUTED, but the OUT meter is STILL SWINGING -- design section
    // 2.1's central claim, pinned as a diffable artefact so a future
    // reviewer cannot quietly "fix" it by scaling the meter down to zero
    // when muted. The meter reads PCM entering the LDAC encoder, upstream
    // of the sink's own gain -- volume 0/muted changes nothing about how
    // much audio is reaching the encoder. ---
    app.tick(1);
    app.handle_event(Event::LevelsChanged { peak_l: 90, peak_r: 45, rms_l: 26, rms_r: 40 });
    save_zoomed_png(&mut app, out_dir, FIXTURE_NAMES[7]);
}
