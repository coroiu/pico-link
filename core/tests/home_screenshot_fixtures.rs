//! Structural fix for bead pico-link-70b: fails when a committed
//! `home-screenshots/*.png` fixture no longer matches what
//! `examples/home_screenshots.rs` renders today.
//!
//! Committed fixtures exist so a diff catches an unintended render
//! change. Without this test that stops being true silently: three
//! fixtures drifted out of date after `1bf0993` and were caught only by
//! a code reviewer's manual, one-off diff on an unrelated bead
//! (pico-link-ay0) -- and a stale-but-unchecked fixture trains the next
//! reviewer to read "the fixtures differ" as normal noise, which is
//! exactly how a real regression gets waved through.
//!
//! Renders through `tests/support/home_fixtures.rs::generate`, the exact
//! same code `examples/home_screenshots.rs` calls (shared via `#[path]`,
//! not reimplemented here) -- so a failure below means the *render*
//! changed, never that this test's own scenario setup fell out of sync
//! with the example's.

#[path = "support/home_fixtures.rs"]
mod home_fixtures;

use std::path::Path;

#[test]
fn committed_home_screenshots_match_a_fresh_render() {
    let tmp_dir = std::env::temp_dir().join(format!("pico-link-home-fixtures-test-{}", std::process::id()));
    home_fixtures::generate(&tmp_dir);

    let committed_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("home-screenshots");

    let mut mismatches = Vec::new();
    for name in home_fixtures::FIXTURE_NAMES {
        let committed_path = committed_dir.join(format!("{name}.png"));
        let fresh_path = tmp_dir.join(format!("{name}.png"));

        let committed = std::fs::read(&committed_path)
            .unwrap_or_else(|e| panic!("failed to read committed fixture {}: {e}", committed_path.display()));
        let fresh = std::fs::read(&fresh_path)
            .unwrap_or_else(|e| panic!("failed to read freshly rendered {}: {e}", fresh_path.display()));

        if committed != fresh {
            mismatches.push(name);
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);

    assert!(
        mismatches.is_empty(),
        "{} of {} committed home-screenshots fixture(s) no longer match a fresh render: {:?}.\n\
         Before regenerating: inspect the new render AT ZOOM against \
         `.planning/design/2026-09-01-home-alignment-grid.md` and \
         `.planning/design/2026-08-28-on-device-ui.md` section 6 -- a drifted-but-WRONG render is a \
         separate bug, not a fixture to bless.\n\
         If the new render is correct, regenerate from the repo root with:\n\
         \n    cargo run --example home_screenshots -p pico-link-core -- home-screenshots\n\
         \n then `git add home-screenshots` and commit.",
        mismatches.len(),
        home_fixtures::FIXTURE_NAMES.len(),
        mismatches,
    );
}
