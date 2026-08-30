//! Headless end-to-end proof of the idle/wake state machine, reusing
//! `SharedHeadlessSurface`'s all-black `encode_png` while powered off --
//! the whole point being that a headless screenshot taken while idle must
//! be trustworthy evidence that the real display would also be dark, per
//! the same presentation-surface-parity argument
//! `emulator/tests/surface_parity.rs` makes for "on" content.
//!
//! Unlike `core/src/run.rs`'s own idle/wake acceptance tests (which use a
//! `ControllableClock` to cross the timeout deterministically in zero real
//! time), this test drives a real `pico_link_core::run::run` against the real,
//! wall-clock-backed `emulator::platform::clock::HostClock` -- there is no
//! way to inject a fake clock into `HostPlatform` from outside `emulator`.
//! To keep this fast and non-flaky it uses a short `idle_timeout` (tens of
//! milliseconds) with a comfortable safety margin (the loop runs for
//! several times the timeout before checking), not the real
//! `DEFAULT_IDLE_TIMEOUT` (120s) `main.rs` wires up -- waiting out 120 real
//! seconds in a test would be its own kind of bad.
//!
//! Deliberately a *single* `run` call for the whole scenario (initial
//! render -> idle -> inject -> woken), not three separate calls: `run`'s
//! `Active`/`Asleep` state is local to each invocation (correct for
//! production, where it's called exactly once for the process's whole
//! lifetime) -- calling it again mid-scenario would silently reset that
//! state to `Active` even though the real display is still physically
//! `Off`, which would make this test pass without ever exercising the
//! wake-swallows-first-input path it exists to prove. Screenshots and the
//! injected `NavIntent` are instead taken/pushed from inside the
//! `should_continue` closure, which `run` calls once per iteration on this
//! same thread -- a legitimate inspection point *during* a single loop.
//!
//! No HTTP server is needed here (unlike `headless_http_drive.rs`):
//! `HttpInput` only ever needs the raw `Arc<Mutex<VecDeque<NavIntent>>>`
//! queue an `HttpServer` would otherwise hand out, so this test constructs
//! and pushes into one directly.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pico_link_core::input::NavIntent;
use pico_link_core::render::chrome::TITLE_BAR_HEIGHT;
use pico_link_core::render::theme::palette;
use pico_link_core::{run, App};
use embedded_graphics::prelude::RgbColor;
use emulator::platform::{FileStorage, HostPlatform, HttpInput, RecordingPowerControl, SharedHeadlessSurface};

const WIDTH: u32 = 240;
const HEIGHT: u32 = 240;

/// Short enough that a test loop can wait it out in real milliseconds;
/// see the module doc for why this isn't `pico_link_core::DEFAULT_IDLE_TIMEOUT`.
const TEST_IDLE_TIMEOUT: Duration = Duration::from_millis(40);
const FRAME_BUDGET: Duration = Duration::from_millis(5);

/// The iteration (1-indexed) at which the closure captures the "just
/// rendered the initial, still-Active frame" screenshot -- i.e. right
/// after iteration 1's body has completed.
const CHECKPOINT_INITIAL: u32 = 2;
/// The iteration at which the closure captures the "should be blanked by
/// now" screenshot and pushes the wake-triggering `NavIntent` -- chosen so
/// that `(CHECKPOINT_IDLE - 1) * FRAME_BUDGET` (~245ms) is comfortably
/// several multiples of `TEST_IDLE_TIMEOUT` (40ms), so ordinary CI
/// scheduling jitter can't flakily leave the loop short of the timeout.
const CHECKPOINT_IDLE_AND_INJECT: u32 = 50;
/// Deliberately **equal to** `CHECKPOINT_IDLE_AND_INJECT`, not a handful of
/// iterations past it (an earlier version of this test used
/// `CHECKPOINT_IDLE_AND_INJECT + 5` here, reasoning that a few "filler"
/// iterations were needed for the wake iteration's render+flush to
/// "definitely" complete). That reasoning was wrong and caused a real,
/// reproducible flake (`pico-link-wez`): `Runner::step` (see
/// `core/src/run.rs`) handles the wake-triggering input and performs its
/// render+flush *synchronously within the same step* that polls it -- the
/// closure pushes the intent and `run`'s loop drives that very iteration's
/// `step` immediately after, with no further iterations required. Trailing
/// filler iterations don't just fail to help; they actively reintroduce a
/// second idle-timeout race: each is a fresh chance for
/// `frame_start.saturating_duration_since(last_input) >= idle_timeout` to
/// go true again if the *real* wall-clock gap between iterations (normally
/// ~`FRAME_BUDGET`, but unbounded under OS scheduling contention -- e.g.
/// another `cargo test` invocation or emulator instance competing for CPU,
/// which is exactly what this project's agents routinely do in sibling
/// worktrees) happens to exceed `TEST_IDLE_TIMEOUT` before the loop stops.
/// That re-blanks the display *after* the wake render but *before* this
/// test reads the final screenshot, failing the "must restore the display"
/// assertion below even though the wake logic itself is correct.
/// Reproduced locally by running 8 copies of this test binary concurrently
/// under synthetic CPU load: consistently reproduced with the old `+ 5`
/// trailing window, zero failures in 200+ runs with this fixed value.
/// Stopping the loop on the exact iteration that performs the wake removes
/// the race entirely rather than papering over it with a bigger margin --
/// there is no later iteration left in which a second idle timeout could
/// ever fire.
const TOTAL_ITERATIONS: u32 = CHECKPOINT_IDLE_AND_INJECT;

fn is_all_black(image: &image::RgbImage) -> bool {
    image.pixels().all(|p| *p == image::Rgb([0, 0, 0]))
}

#[test]
fn driving_to_idle_blanks_the_headless_screenshot_and_an_injected_intent_restores_it() {
    let input_queue: Arc<Mutex<VecDeque<NavIntent>>> = Arc::new(Mutex::new(VecDeque::new()));
    let surface = SharedHeadlessSurface::new();
    let surface_handle = surface.handle();
    // A second handle purely for the closure to capture into, so the
    // `surface_handle` name above stays free for the post-`run` final
    // read without fighting the closure's own capture.
    let surface_handle_for_closure = surface.handle();

    let kv_storage_path = std::env::temp_dir().join(format!("pico-link-idle-wake-e2e-test-{}.json", uuid::Uuid::new_v4()));
    let kv_storage = FileStorage::new(kv_storage_path).expect("open a temp kv store");
    let mut platform = HostPlatform::new(surface, HttpInput::new(Arc::clone(&input_queue)), kv_storage, RecordingPowerControl::new());

    let mut app = App::new(WIDTH, HEIGHT);

    let row0_y = TITLE_BAR_HEIGHT + 2;
    // x=200: clear of the row's chip/text on this short label, and inside
    // the 240px-wide (Epic B2) panel -- see the identical comment in
    // `headless_http_drive.rs`.
    let sample_x = 200;
    let highlight = palette::SURFACE_ELEVATED;
    let highlight_rgb8 = image::Rgb([highlight.r() << 3, highlight.g() << 2, highlight.b() << 3]);

    // Home (the root screen since `pico-link-znb.8`/E7) has no focusable
    // list of its own on its status face (Up/Down is unbound there in
    // Tier 1 -- see `pico_link_core::render::home`'s module doc), so this
    // test's row-selection proof needs the Devices screen underneath it.
    // Queued before `run` starts, so iteration 1's single `poll()` drains
    // both and lands on Devices (row 0, "Scan for headphones",
    // pre-selected) before `CHECKPOINT_INITIAL` captures its screenshot:
    // centre toggles Home to its menu face (Bluetooth pre-selected),
    // centre again activates that row, pushing Devices.
    input_queue.lock().unwrap().push_back(NavIntent::Select);
    input_queue.lock().unwrap().push_back(NavIntent::Select);

    let mut initial_screenshot: Option<image::RgbImage> = None;
    let mut idle_screenshot: Option<image::RgbImage> = None;

    let mut iterations = 0u32;
    run(&mut platform, &mut app, FRAME_BUDGET, Some(TEST_IDLE_TIMEOUT), None, || {
        iterations += 1;

        if iterations == CHECKPOINT_INITIAL {
            let png = surface_handle_for_closure.lock().unwrap().encode_png().expect("iteration 1 flushed");
            initial_screenshot = Some(image::load_from_memory(&png).expect("valid PNG").to_rgb8());
        }

        if iterations == CHECKPOINT_IDLE_AND_INJECT {
            let png = surface_handle_for_closure.lock().unwrap().encode_png().expect("a frame was flushed before going idle");
            idle_screenshot = Some(image::load_from_memory(&png).expect("valid PNG").to_rgb8());

            // The wake-triggering input: queued now, so the very next
            // iteration's `poll()` (this one, about to run) sees it.
            input_queue.lock().unwrap().push_back(NavIntent::Down);
        }

        iterations <= TOTAL_ITERATIONS
    });

    let initial_screenshot = initial_screenshot.expect("checkpoint 1 must have run");
    let idle_screenshot = idle_screenshot.expect("the idle checkpoint must have run");

    assert!(!is_all_black(&initial_screenshot), "the initial render must show real content, not coincidentally start black");
    assert_eq!(*initial_screenshot.get_pixel(sample_x, row0_y), highlight_rgb8, "row 0 starts selected");

    assert!(is_all_black(&idle_screenshot), "the headless screenshot must be all-black once the idle timeout has elapsed");

    let woken_png = surface_handle.lock().unwrap().encode_png().expect("the wake iteration flushed a fresh frame");
    let woken_screenshot = image::load_from_memory(&woken_png).expect("valid PNG").to_rgb8();
    assert!(!is_all_black(&woken_screenshot), "the injected intent must restore the display, not leave it blanked");
    assert_eq!(
        woken_screenshot, initial_screenshot,
        "the wake-triggering intent must be dropped (not delivered to the app), so the restored frame is pixel-identical to the pre-idle one -- row 0 still selected, nothing navigated"
    );
}
