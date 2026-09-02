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
//! <out-dir>` (run with `home-screenshots` from the repo root to
//! regenerate the committed fixtures themselves).
//!
//! The scenario/render logic lives in `tests/support/home_fixtures.rs`,
//! included here by path rather than duplicated, so this example and
//! `tests/home_screenshot_fixtures.rs` (which proves the committed PNGs
//! still match) can never independently drift from each other -- see
//! that shared file's module doc for the drift this once caused
//! (pico-link-70b).

#[path = "../tests/support/home_fixtures.rs"]
mod home_fixtures;

use std::env;
use std::path::PathBuf;

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-home-screenshots"), PathBuf::from);
    home_fixtures::generate(&out_dir);
    for name in home_fixtures::FIXTURE_NAMES {
        println!("wrote {}", out_dir.join(format!("{name}.png")).display());
    }
}
