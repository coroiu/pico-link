//! pico-link-znb.4 (E12): a headless PNG-sequence proof that
//! `VerticalList`'s identity-based selection carry-forward
//! (`with_selected_identity`) keeps the selection highlight visually
//! anchored to the *same row's content* across a rebuild that inserts
//! rows around it -- not just that the underlying index arithmetic works
//! (covered by the plain unit tests in `render::list::tests`), but that
//! the actual rendered pixels move with the selected item.
//!
//! Scoped at the widget level (`ListItem`/`VerticalList`/`Navigator`
//! directly), not through `App`, because `App`'s own device list is
//! deliberately append-only (design section 9 rule 1: never re-sort, so
//! RSSI churn can't steal the row out from under a user's thumb) -- it can
//! never itself produce the "a row is inserted *before* the selected one"
//! case this test exercises. See `core/src/app.rs`'s
//! `selecting_a_device_survives_further_devices_arriving_...` test for the
//! `App`-level (append-only) equivalent, and
//! `core/src/render/list.rs`'s `selection_follows_its_keyed_row_when_
//! other_rows_are_inserted_around_it` for the same case proven purely
//! against widget state (no rendering).
//!
//! Per the project's rendering-verification rule, PNGs are written to a
//! scratch path for optional zoomed human/agent inspection, in addition to
//! the pixel assertions below carrying the actual pass/fail.

#![allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]

use embedded_graphics::prelude::{Point, RgbColor};
use pico_link_core::platform::Instant;
use pico_link_core::render::chrome::TITLE_BAR_HEIGHT;
use pico_link_core::render::theme::palette;
use pico_link_core::render::{FrameBuffer565, ListItem, ListItemKey, Navigator, RenderCtx, Screen, VerticalList, Widget, ROW_HEIGHT};

fn test_ctx() -> RenderCtx {
    RenderCtx::at(Instant::from_micros(0))
}

fn row_top(index: i32) -> i32 {
    TITLE_BAR_HEIGHT as i32 + index * ROW_HEIGHT as i32
}

fn dump_png(framebuffer: &FrameBuffer565, name: &str) -> std::path::PathBuf {
    let mut image = image::RgbImage::new(framebuffer.width(), framebuffer.height());
    for pixel in framebuffer.pixels() {
        let color = pixel.1;
        image.put_pixel(pixel.0.x as u32, pixel.0.y as u32, image::Rgb([color.r() << 3, color.g() << 2, color.b() << 3]));
    }
    let path = std::env::temp_dir().join(format!("pico-link-core-list-identity-{name}.png"));
    image.save(&path).expect("failed to encode/write PNG");
    path
}

/// x sampled clear of the left selection accent bar and of any row's
/// (proportional-font) text -- mirrors `render_png_dump.rs`'s
/// `scene_renders_expected_chrome_colors` sampling point.
const SAMPLE_X: i32 = 200;

#[test]
fn selection_highlight_stays_on_the_same_device_row_as_new_devices_are_inserted_around_it() {
    // Frame 1: two devices, "Headphones" selected (row 1; row 0 is a
    // fixed non-device row, mirroring App's own "Scan" row).
    let frame1_items = vec![
        ListItem::new("Scan for headphones").with_key(ListItemKey::from_u64(0)),
        ListItem::new("Headphones").with_key(ListItemKey::from_u64(1)),
        ListItem::new("Other Device").with_key(ListItemKey::from_u64(2)),
    ];
    let list1 = VerticalList::new(frame1_items).with_selected_identity(None, 0);
    // Move selection onto "Headphones" (row 1).
    let mut list1 = list1;
    list1.on_intent(pico_link_core::NavIntent::Down);
    assert_eq!(list1.selected_index(), 1);
    let carried_key = list1.selected_key();
    assert_eq!(carried_key, Some(ListItemKey::from_u64(1)));

    let screen1 = Screen::new("Pico Link", vec![Box::new(list1)]);
    let mut navigator1 = Navigator::new(screen1);
    let mut fb1 = FrameBuffer565::new(240, 240);
    navigator1.render(&test_ctx(), &mut fb1).expect("core DrawTarget is Infallible");
    dump_png(&fb1, "frame1-before-growth");

    // Row 1 ("Headphones") must show the selected-row elevated fill.
    assert_eq!(fb1.pixel(Point::new(SAMPLE_X, row_top(1) + 2)), palette::SURFACE_ELEVATED);
    // Row 2 ("Other Device") must not.
    assert_eq!(fb1.pixel(Point::new(SAMPLE_X, row_top(2) + 2)), palette::BACKGROUND);

    // Frame 2: the list "grows" -- two more devices arrive, one sorting
    // *before* "Headphones" and one landing directly after it, simulating
    // live inquiry results the design says must never be allowed to steal
    // the selection out from under the user. Carry the selection forward
    // by identity, exactly as `App::rebuild_root` does.
    let frame2_items = vec![
        ListItem::new("Scan for headphones").with_key(ListItemKey::from_u64(0)),
        ListItem::new("Earbuds").with_key(ListItemKey::from_u64(3)), // new, sorts before Headphones
        ListItem::new("Headphones").with_key(ListItemKey::from_u64(1)),
        ListItem::new("Speaker").with_key(ListItemKey::from_u64(4)), // new, right after Headphones
        ListItem::new("Other Device").with_key(ListItemKey::from_u64(2)),
    ];
    let list2 = VerticalList::new(frame2_items).with_selected_identity(carried_key, 1);
    assert_eq!(list2.selected_index(), 2, "Headphones moved from row 1 to row 2");

    let screen2 = Screen::new("Pico Link", vec![Box::new(list2)]);
    let mut navigator2 = Navigator::new(screen2);
    let mut fb2 = FrameBuffer565::new(240, 240);
    navigator2.render(&test_ctx(), &mut fb2).expect("core DrawTarget is Infallible");
    dump_png(&fb2, "frame2-after-growth");

    // The highlight must now be on row 2 (Headphones' new position)...
    assert_eq!(
        fb2.pixel(Point::new(SAMPLE_X, row_top(2) + 2)),
        palette::SURFACE_ELEVATED,
        "the selection highlight must have followed Headphones to its new row"
    );
    // ...and row 1 (now "Earbuds", a device the user never selected) must
    // NOT be highlighted -- the actual failure mode a plain index-carry
    // would have produced (index 1 stayed selected, silently landing on
    // the wrong device).
    assert_eq!(
        fb2.pixel(Point::new(SAMPLE_X, row_top(1) + 2)),
        palette::BACKGROUND,
        "row 1 (Earbuds, never selected by the user) must not show the highlight"
    );
}
