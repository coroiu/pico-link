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
//! `DEFAULT_IDLE_TIMEOUT` (60s) `main.rs` wires up -- waiting out 60 real
//! seconds in a test would be its own kind of bad.
//!
//! Two scenarios (bead pico-link-4vb.3 added the second one, and adapted
//! the first): the screensaver must blank while idle **at Home root**, and
//! must **never** blank away from it -- most importantly, never in the
//! middle of the pairing wizard, where a blanked screen reads as a crash.
//! `core/src/run.rs`'s own unit tests already prove the arm/disarm/wake
//! state machine and the Home-root gate in isolation (see
//! `no_input_past_the_idle_timeout_off_home_root_never_blanks_the_display`
//! and its neighbors); what only a headless run can prove is that the real
//! `App`/`Navigator`/screen stack, driven the way a user would, ends up
//! producing an actually-blank (or actually-not-blank) framebuffer.
//!
//! Deliberately a *single* `run` call per scenario, not several: `run`'s
//! `Active`/`Asleep` state is local to each invocation (correct for
//! production, where it's called exactly once for the process's whole
//! lifetime) -- calling it again mid-scenario would silently reset that
//! state to `Active` even though the real display is still physically
//! `Off`, which would make a test pass without ever exercising the
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
use pico_link_core::platform::DisplayPower;
use pico_link_core::{run, App, Event, VolumeSource};
use emulator::platform::{FileStorage, HeadlessSurface, HostPlatform, HttpInput, RecordingPowerControl, SharedHeadlessSurface};

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

/// Mean of all three channels across every pixel -- used to prove a
/// dimmed frame is genuinely between "off" (0) and "on" (its own,
/// content-dependent mean), not just "not all black".
fn mean_luminance(image: &image::RgbImage) -> f64 {
    let mut total = 0u64;
    let mut count = 0u64;
    for p in image.pixels() {
        total += u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2]);
        count += 3;
    }
    total as f64 / count as f64
}

/// [`new_platform`]'s return type, factored out (clippy's `type_complexity`
/// lint runs at `-D warnings` in this workspace).
type NewPlatform = (HostPlatform<SharedHeadlessSurface, HttpInput>, Arc<Mutex<VecDeque<NavIntent>>>, Arc<Mutex<HeadlessSurface>>);

fn new_platform() -> NewPlatform {
    let input_queue: Arc<Mutex<VecDeque<NavIntent>>> = Arc::new(Mutex::new(VecDeque::new()));
    let surface = SharedHeadlessSurface::new();
    let surface_handle = surface.handle();
    let kv_storage_path = std::env::temp_dir().join(format!("pico-link-idle-wake-e2e-test-{}.json", uuid::Uuid::new_v4()));
    let kv_storage = FileStorage::new(kv_storage_path).expect("open a temp kv store");
    let platform = HostPlatform::new(surface, HttpInput::new(Arc::clone(&input_queue)), kv_storage, RecordingPowerControl::new());
    (platform, input_queue, surface_handle)
}

#[test]
fn driving_to_idle_at_home_root_blanks_the_headless_screenshot_and_an_injected_intent_restores_it() {
    let (mut platform, input_queue, surface) = new_platform();
    let surface_handle_for_closure = Arc::clone(&surface);

    let mut app = App::new(WIDTH, HEIGHT);
    assert!(app.is_at_home_root(), "sanity: a fresh App starts at Home root");

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

            // Down is unbound on Home's status face (Tier 1 scope
            // boundary -- see `pico_link_core::render::home`'s module
            // doc), so this both wakes the display AND is guaranteed not
            // to move anything if (incorrectly) delivered to the app --
            // the pixel-identical assertion below is a strong proof
            // either way.
            input_queue.lock().unwrap().push_back(NavIntent::Down);
        }

        iterations <= TOTAL_ITERATIONS
    });

    assert_eq!(app.navigator_depth(), 1, "sanity: never navigated away from Home root during this scenario");

    let initial_screenshot = initial_screenshot.expect("checkpoint 1 must have run");
    let idle_screenshot = idle_screenshot.expect("the idle checkpoint must have run");

    assert!(!is_all_black(&initial_screenshot), "the initial render must show real content, not coincidentally start black");
    assert!(is_all_black(&idle_screenshot), "the headless screenshot must be all-black once the idle timeout has elapsed at Home root");

    let woken_png = surface.lock().unwrap().encode_png().expect("the wake iteration flushed a fresh frame");
    let woken_screenshot = image::load_from_memory(&woken_png).expect("valid PNG").to_rgb8();
    assert!(!is_all_black(&woken_screenshot), "the injected intent must restore the display, not leave it blanked");
    assert_eq!(
        woken_screenshot, initial_screenshot,
        "the wake-triggering intent must be dropped (not delivered to the app), so the restored frame is pixel-identical to the pre-idle one"
    );
}

#[test]
fn idle_past_the_timeout_never_blanks_the_display_while_inside_the_pairing_wizard() {
    // Regression test for bead pico-link-4vb.3: a blanked screen mid-
    // pairing reads as a crash, so the screensaver must never arm once
    // the navigator has left Home root -- the pairing wizard (depth 3)
    // most of all.
    let (mut platform, input_queue, surface) = new_platform();
    let surface_handle_for_closure = Arc::clone(&surface);

    let mut app = App::new(WIDTH, HEIGHT);

    // Home status -> menu face (Bluetooth pre-selected) -> pushes Devices
    // -> "Scan for headphones" row pushes the wizard. Queued before `run`
    // starts, so iteration 1's single `poll()` drains all three and lands
    // on the wizard's Instructions phase before `CHECKPOINT_INITIAL`
    // captures its screenshot.
    {
        let mut queue = input_queue.lock().unwrap();
        queue.push_back(NavIntent::Select);
        queue.push_back(NavIntent::Select);
        queue.push_back(NavIntent::Select);
    }

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
        }

        iterations <= TOTAL_ITERATIONS
    });

    assert_eq!(app.navigator_depth(), 3, "sanity: the wizard is open at Home(1)/Devices(2)/Wizard(3)");

    let initial_screenshot = initial_screenshot.expect("checkpoint 1 must have run");
    let idle_screenshot = idle_screenshot.expect("the idle checkpoint must have run");

    assert!(!is_all_black(&initial_screenshot), "the wizard's initial render must show real content");
    assert!(
        !is_all_black(&idle_screenshot),
        "the screensaver must never blank the display while inside the pairing wizard, no matter how much idle time passes"
    );
    assert_eq!(idle_screenshot, initial_screenshot, "the wizard screen must be untouched -- no blank, no navigation, nothing queued to wake");
}

#[test]
fn driving_to_idle_while_muted_dims_the_headless_screenshot_and_a_press_restores_full_brightness_without_navigating() {
    // pico-link-qivj.2, Andreas's Q1 ruling: muted/zero volume + idle
    // lands on Dim, never Off -- proven here end to end, through a real
    // `App`/`Navigator`/`HeadlessSurface`, not just `IdlePolicy` in
    // isolation (already covered by `core::run::idle_policy_tests`).
    let (mut platform, input_queue, surface) = new_platform();
    let surface_handle_for_closure = Arc::clone(&surface);

    let mut app = App::new(WIDTH, HEIGHT);
    assert!(app.is_at_home_root(), "sanity: a fresh App starts at Home root");
    // Default mode is Off (Andreas's Q3 ruling) -- the mute/zero floor
    // must still force Dim, never Off, regardless of mode.
    app.handle_event(Event::VolumeChanged { level: 0, muted: true, source: VolumeSource::Host });

    let mut initial_screenshot: Option<image::RgbImage> = None;
    let mut idle_screenshot: Option<image::RgbImage> = None;
    let mut idle_power: Option<DisplayPower> = None;

    let mut iterations = 0u32;
    run(&mut platform, &mut app, FRAME_BUDGET, Some(TEST_IDLE_TIMEOUT), None, || {
        iterations += 1;

        if iterations == CHECKPOINT_INITIAL {
            let png = surface_handle_for_closure.lock().unwrap().encode_png().expect("iteration 1 flushed");
            initial_screenshot = Some(image::load_from_memory(&png).expect("valid PNG").to_rgb8());
        }

        if iterations == CHECKPOINT_IDLE_AND_INJECT {
            idle_power = Some(surface_handle_for_closure.lock().unwrap().power());
            let png = surface_handle_for_closure.lock().unwrap().encode_png().expect("a frame was flushed before going idle");
            idle_screenshot = Some(image::load_from_memory(&png).expect("valid PNG").to_rgb8());

            // Down is unbound on Home's status face -- wakes without
            // navigating, same reasoning as the plain idle/wake test above.
            input_queue.lock().unwrap().push_back(NavIntent::Down);
        }

        iterations <= TOTAL_ITERATIONS
    });

    assert_eq!(app.navigator_depth(), 1, "sanity: never navigated away from Home root during this scenario");

    let initial_screenshot = initial_screenshot.expect("checkpoint 1 must have run");
    let idle_screenshot = idle_screenshot.expect("the idle checkpoint must have run");
    let idle_power = idle_power.expect("the idle checkpoint must have run");

    assert!(!is_all_black(&initial_screenshot), "the initial render must show real content");
    assert_eq!(idle_power, DisplayPower::Dim, "muted + idle must land on Dim, never Off, even in the default Off mode");
    assert!(!is_all_black(&idle_screenshot), "Dim must still show content, unlike Off's all-black frame");

    let initial_luma = mean_luminance(&initial_screenshot);
    let idle_luma = mean_luminance(&idle_screenshot);
    assert!(
        idle_luma > 0.0 && idle_luma < initial_luma,
        "dimmed mean luminance ({idle_luma}) must sit strictly between black (0) and the full-brightness frame's ({initial_luma})"
    );

    let woken_power = surface.lock().unwrap().power();
    let woken_png = surface.lock().unwrap().encode_png().expect("the wake iteration flushed a fresh frame");
    let woken_screenshot = image::load_from_memory(&woken_png).expect("valid PNG").to_rgb8();
    assert_eq!(woken_power, DisplayPower::On, "a real press must restore full brightness");
    assert_eq!(
        woken_screenshot, initial_screenshot,
        "the wake-triggering press must be dropped (not delivered to the app), so the restored frame is pixel-identical to the pre-idle one"
    );
}
