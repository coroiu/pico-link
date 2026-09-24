//! Verification fixture for bead `pico-link-bgnd` M4 (the `why?` page reads
//! the live model instead of being rebuilt on every fault event): dumps a
//! zoomed PNG before and after a live model change with the page already
//! open, per the project's rendering-verification rule (CLAUDE.md: "Tests
//! pass + a 1x PNG... is INSUFFICIENT evidence for a render change...
//! inspect framebuffers and PNGs at ZOOM"). Not a test -- a standalone
//! example (`cargo run --example why_page_screenshots -p pico-link-core --
//! <out-dir>`).

use std::env;
use std::path::{Path, PathBuf};

use embedded_graphics::prelude::RgbColor;
use pico_link_core::app::{FaultKey, FaultValue};
use pico_link_core::input::NavIntent;
use pico_link_core::App;

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
            image::Rgb([(color.r() << 3) | (color.r() >> 2), (color.g() << 2) | (color.g() >> 4), (color.b() << 3) | (color.b() >> 2)]),
        );
    }
    let zoomed = image::imageops::resize(&image, framebuffer.width() * ZOOM, framebuffer.height() * ZOOM, image::imageops::FilterType::Nearest);
    let path = out_dir.join(format!("{name}.png"));
    zoomed.save(&path).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    println!("wrote {}", path.display());
}

fn main() {
    let out_dir: PathBuf = env::args().nth(1).map_or_else(|| env::temp_dir().join("pico-link-why-page-screenshots"), PathBuf::from);
    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    let mut app = App::new(240, 240);
    app.handle_event(pico_link_core::Event::FaultRaised { key: FaultKey::BufOverflow, value: Some(FaultValue::Count(14)), count: 14 });
    app.handle_event(pico_link_core::Event::FaultRaised { key: FaultKey::AirLinkLost, value: Some(FaultValue::Count(2)), count: 2 });
    app.handle_input(vec![NavIntent::ShortcutX]); // open the why? page
    save_zoomed_png(&mut app, &out_dir, "01_before_live_change");

    // A live count bump on an already-shown key, WITHOUT popping/re-pushing
    // the page -- the whole point of bead pico-link-bgnd M4.
    app.handle_event(pico_link_core::Event::FaultRaised { key: FaultKey::BufOverflow, value: Some(FaultValue::Count(140)), count: 140 });
    save_zoomed_png(&mut app, &out_dir, "02_after_live_count_bump_same_screen");

    // A brand-new key firing while the page is open must append at the
    // BOTTOM, not jump to the top (the frozen-order rule).
    app.handle_event(pico_link_core::Event::FaultRaised { key: FaultKey::BufStarved, value: None, count: 1 });
    save_zoomed_png(&mut app, &out_dir, "03_after_a_new_key_fires_appends_at_bottom");

    println!("done -- 3 PNGs written to {}", out_dir.display());
}
