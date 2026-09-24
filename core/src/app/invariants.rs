#![cfg(test)]

use alloc::vec;

use crate::render::{ButtonLabel, Verb};

use super::model::MAX_PAIRED_DEVICES;
use super::*;
use super::test_support::*;

// --- pico-link-vxc, design doc §6.1: the freshness invariant ---
//
// This is the acceptance mechanism for the FFI dirty gate
// (`pl_ui_dirty`, `ui-ffi/src/lib.rs`): C is now allowed to skip
// render+blit whenever `App::dirty()` is false, so every screen the
// product can build must be provably safe to leave unrendered for an
// arbitrary stretch of wall-clock time unless it has explicitly opted
// into a `Widget::redraw_after` request. This table is what proves it,
// and is exactly the test Ada's audit (see the design doc) says would
// have caught D2 (a composite widget silently swallowing a child's
// `redraw_after`) had that bug shipped instead of being fixed in the
// same change.
//
// ADD YOUR NEW SCREEN BUILDER TO `freshness_cases()` BELOW whenever you
// add one -- see that function's doc comment.

/// A ten-minute span, deliberately far longer than any real idle gap
/// this device would ever sit unattended for -- if a screen is going
/// to leak a missing `redraw_after`, this window makes it unmistakable
/// rather than a maybe-it-was-close flake.
const FRESHNESS_TEN_MINUTES_US: u64 = 10 * 60 * 1_000_000;

/// What a screen builder in [`freshness_cases`] promises about its own
/// staleness.
enum Freshness {
    /// Nothing about this screen can change without an event or
    /// input -- rendering it now and rendering it again after
    /// [`FRESHNESS_TEN_MINUTES_US`] of untouched wall-clock time must
    /// produce byte-identical pixels.
    Static,
    /// This screen has a genuinely time-driven element that requests
    /// its own redraw after the first `Duration` -- rendering it now
    /// and again after the *second* `Duration` must produce
    /// *different* pixels (proving the request isn't spurious). The
    /// two durations can differ: the wizard's elapsed-seconds readout
    /// requests a redraw every 250ms but only actually changes on a
    /// whole-second boundary (§8's "minor, non-blocking" risk note),
    /// so its assertion window is 2s, matching the exact scenario
    /// `wizard.rs`'s own
    /// `connecting_phase_liveness_end_to_end_tick_alone_marks_dirty_and_the_elapsed_readout_changes`
    /// test already proves -- folded in here per the design doc's
    /// §6.1 instruction, not duplicated.
    Live { redraw_after: Duration, assert_differs_after: Duration },
}

/// Every screen builder the product has, paired with its
/// [`Freshness`] promise. **Add every new screen builder here** --
/// this table is the single place pico-link-vxc's freshness-invariant
/// test (`dirty_gate_freshness_invariant_holds_for_every_screen`)
/// draws its cases from, and an entry missing here is an entry the
/// dirty gate has no proof about.
/// One row of [`freshness_cases`]'s table: a case name, a builder that
/// constructs the `App` already navigated to the screen under test,
/// and that screen's [`Freshness`] promise.
type FreshnessCase = (&'static str, fn() -> App, Freshness);

#[allow(clippy::too_many_lines)] // one function per screen builder is the point -- see freshness_cases's own doc comment
fn freshness_cases() -> Vec<FreshnessCase> {
    fn home_status_face() -> App {
        App::new(240, 240)
    }
    fn home_menu_face() -> App {
        let mut app = App::new(240, 240);
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app
    }
    fn devices_list() -> App {
        let mut app = App::new(240, 240);
        open_devices(&mut app);
        app
    }
    fn wizard_scanning() -> App {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::DeviceDiscovered(DeviceEntry {
            addr: [1; 6],
            name: String::from("Cans"),
            rssi: -40,
            class_of_device: 0x24_04_04,
        }));
        app
    }
    fn wizard_nothing_found() -> App {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        // Bead pico-link-88xs: a genuine end-of-inquiry, not a
        // `LinkStateChanged(Idle)` -- see `on_scan_ended_if_applicable`'s
        // doc comment for why `LinkStateChanged(Idle)` alone must no
        // longer trigger this transition.
        app.handle_event(Event::DiscoveryStateChanged { scanning: false });
        app
    }
    fn wizard_connecting() -> App {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::DeviceDiscovered(DeviceEntry {
            addr: [2; 6],
            name: String::from("Cans"),
            rssi: -40,
            class_of_device: 0x24_04_04,
        }));
        app.handle_input(vec![NavIntent::Select]); // Scanning -> Connecting
        app
    }
    fn wizard_not_responding() -> App {
        let mut app = wizard_connecting();
        app.handle_event(Event::ConnectRetrying { attempt: 1 });
        app
    }
    fn wizard_succeeded() -> App {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::ConnectSucceeded { addr: [3; 6], degraded: false });
        app
    }
    fn wizard_failed() -> App {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::ConnectFailed { addr: [4; 6], reason: ConnectFailureReason::Timeout });
        app
    }
    fn forget_picker() -> App {
        let mut app = App::new(240, 240);
        let max = u8::try_from(MAX_PAIRED_DEVICES).expect("MAX_PAIRED_DEVICES is a small constant, fits in u8");
        for i in 0..max {
            app.handle_event(upsert([i; 6], "Cans", u32::from(i)));
        }
        open_devices(&mut app);
        // Overshoots on purpose -- VerticalList::on_intent's JumpBy
        // clamps at the last row ("Pair new headphones") regardless of
        // exactly how many paired rows precede it.
        app.handle_input(vec![NavIntent::JumpBy(i16::from(max) + 1)]);
        app.handle_input(vec![NavIntent::Select]); // at the cap -> forget picker, not the wizard
        app
    }
    fn forget_confirm() -> App {
        let mut app = App::new(240, 240);
        app.handle_event(upsert([5; 6], "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::ShortcutX]); // the one paired row -> forget confirm
        app
    }
    fn device_detail() -> App {
        let mut app = App::new(240, 240);
        let addr = [6; 6];
        // `ConnectSucceeded` before `upsert` deliberately: the former
        // sets `connected_addr` but does not itself `rebuild_root`
        // (see `on_connect_succeeded`'s doc comment), so `HomeView`'s
        // captured `model` snapshot would otherwise still read
        // `connected_addr: None` when `open_devices` pushes the
        // Devices screen off of it, and `on_activate_index` would take
        // the reconnect-to-a-non-connected-row branch (pushing the
        // wizard's Connecting phase) instead of device detail. Ordered
        // this way, the `upsert` event's own `rebuild_root` is the one
        // that captures the fresh, already-connected model.
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // the connected (pinned-first) row -> device detail
        app
    }
    fn settings() -> App {
        let mut app = App::new(240, 240);
        // `pico-link-hr30` repurposed Home's `ShortcutY` to the device
        // page, so Settings is reached the ordinary way now: A/centre
        // to the menu face, Down to the Settings row, A/centre to
        // activate it.
        app.handle_input(vec![NavIntent::Select, NavIntent::Down, NavIntent::Select]);
        app
    }
    /// Bead pico-link-du0: Home's status face, connected, with a live
    /// OUT-meter reading. Deliberately `tick`s to a nonzero `now_us`
    /// *before* the `LevelsChanged` event so `OutLevelSample::
    /// received_at` isn't `Instant::from_micros(0)` -- otherwise this
    /// case couldn't be told apart from "app never ticked at all",
    /// and `dirty_gate_freshness_invariant_holds_for_every_screen`'s
    /// own `app.tick(0)` at the top of the test would already be
    /// t0 == received_at, not a meaningfully "just arrived" reading.
    fn home_connected_with_out_level() -> App {
        let mut app = App::new(240, 240);
        let addr = [7; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.handle_event(upsert(addr, "Cans", 1));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        app.tick(1);
        app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 });
        app
    }

    vec![
        ("home, status face", home_status_face, Freshness::Static),
        ("home, menu face", home_menu_face, Freshness::Static),
        ("devices list", devices_list, Freshness::Static),
        ("wizard: scanning", wizard_scanning, Freshness::Static),
        ("wizard: nothing found", wizard_nothing_found, Freshness::Static),
        (
            "wizard: connecting",
            wizard_connecting,
            Freshness::Live {
                redraw_after: crate::render::wizard::ELAPSED_REDRAW_INTERVAL,
                assert_differs_after: Duration::from_secs(2),
            },
        ),
        ("wizard: not responding", wizard_not_responding, Freshness::Static),
        ("wizard: succeeded", wizard_succeeded, Freshness::Static),
        ("wizard: failed", wizard_failed, Freshness::Static),
        ("forget picker", forget_picker, Freshness::Static),
        ("forget confirm", forget_confirm, Freshness::Static),
        ("device detail", device_detail, Freshness::Static),
        ("settings", settings, Freshness::Static),
        (
            "home, status face, connected with live out level",
            home_connected_with_out_level,
            Freshness::Live {
                redraw_after: crate::render::hero::OUT_LEVEL_REFRESH_INTERVAL,
                // Past `OUT_LEVEL_STALE_AFTER` (600ms) so the meter
                // has gone from drawn to absent by t1 -- proving the
                // "absent, never frozen" rule (design section 15)
                // actually fires via `redraw_after` with no new event.
                assert_differs_after: Duration::from_millis(700),
            },
        ),
    ]
}

/// The test itself: see the block comment above `freshness_cases` for
/// what this is proving and why. Table-driven so a missing case is a
/// missing table row, not a missing hand-written test function.
#[test]
fn dirty_gate_freshness_invariant_holds_for_every_screen() {
    for (name, build, freshness) in freshness_cases() {
        let mut app = build();
        app.tick(0);
        let t0: Vec<_> = app.render().pixels().collect();
        match freshness {
            Freshness::Static => {
                app.tick(FRESHNESS_TEN_MINUTES_US);
                let t1: Vec<_> = app.render().pixels().collect();
                assert_eq!(
                    t0, t1,
                    "{name}: pixels changed after 10 minutes with no input/event -- a widget is reading the \
                     clock without a matching Widget::redraw_after (pico-link-vxc D1/D2)"
                );
            }
            Freshness::Live { redraw_after, assert_differs_after } => {
                assert!(
                    assert_differs_after >= redraw_after,
                    "{name}: table error -- assert_differs_after ({assert_differs_after:?}) must be at least the \
                     claimed redraw_after ({redraw_after:?}), or this isn't actually proving the request fires"
                );
                app.tick(
                    u64::try_from(assert_differs_after.as_micros()).expect("test-only duration fits in u64 micros"),
                );
                let t1: Vec<_> = app.render().pixels().collect();
                assert_ne!(
                    t0, t1,
                    "{name}: pixels are unchanged at t+{assert_differs_after:?} -- the redraw_after request is spurious"
                );
            }
        }
    }
}

/// Bead `pico-link-7h5.4`'s acceptance criterion A4 (the damage-rect
/// render design, `.planning/design/2026-09-06-damage-rect-render-and-
/// partial-blit.md` section 10): for every screen, a damage-rendered
/// frame must be pixel-identical to a full-frame render of the same
/// state, and every pixel *outside* the reported damage rect must be
/// byte-identical to the previous frame. Modeled on
/// [`dirty_gate_freshness_invariant_holds_for_every_screen`] just
/// above -- same "prove it for every screen, not the one you thought
/// of" table-driven shape, reusing [`freshness_cases`] itself: each
/// case's [`Freshness`] promise doubles as this test's recipe for a
/// real, screen-appropriate state mutation (`Live` cases tick forward
/// to `assert_differs_after`, already proven elsewhere to change
/// pixels; `Static` cases get a `NavIntent::Down`, which is a no-op on
/// the handful of screens with nothing to move -- this test's
/// assertions then hold trivially for those, rather than not holding
/// at all).
///
/// The "full-frame render of the same state" comparator is a second,
/// otherwise-untouched `App` built and driven through the exact same
/// setup-plus-mutation as the app under test, then rendered exactly
/// once: a freshly built `Navigator` starts with `force_full_damage:
/// true` (see that field's doc comment), so that single render is
/// guaranteed to be a full repaint -- no separate "full render" code
/// path needs to exist anywhere in production code for this test to
/// use.
#[test]
fn damage_rendered_frame_matches_a_full_frame_render_of_the_same_state_for_every_screen() {
    fn apply_mutation(app: &mut App, freshness: &Freshness) {
        match *freshness {
            Freshness::Static => {
                app.handle_input(vec![NavIntent::Down]);
            }
            Freshness::Live { assert_differs_after, .. } => {
                let micros = u64::try_from(assert_differs_after.as_micros()).expect("test-only duration fits in u64 micros");
                app.tick(micros);
            }
        }
    }

    for (name, build, freshness) in freshness_cases() {
        // The app under test: frame N and frame N+1 both go through
        // the production damage path (the same `App`/`Navigator`, so
        // frame N+1's diff is a real incremental diff against N, not
        // another cache miss).
        let mut app = build();
        app.tick(0);
        let frame_n: Vec<_> = app.render().pixels().collect();

        apply_mutation(&mut app, &freshness);
        let output = app.render();
        let damage = output.damage;
        let frame_n_plus_1: Vec<_> = output.pixels().collect();

        // The comparator, per the doc comment above.
        let mut full = build();
        full.tick(0);
        apply_mutation(&mut full, &freshness);
        let full_frame: Vec<_> = full.render().pixels().collect();

        assert_eq!(
            frame_n_plus_1, full_frame,
            "{name}: a damage-rendered frame differs from a full-frame render of the identical state"
        );

        assert_eq!(
            frame_n.len(),
            frame_n_plus_1.len(),
            "{name}: pixel count must not change frame to frame"
        );
        for (pixel_n, pixel_n_plus_1) in frame_n.iter().zip(frame_n_plus_1.iter()) {
            let point = pixel_n.0;
            if !damage.contains(point) {
                assert_eq!(
                    pixel_n.1, pixel_n_plus_1.1,
                    "{name}: pixel {point:?} outside the reported damage rect {damage:?} changed anyway"
                );
            }
        }
    }
}

/// Design rule 4 (`.planning/design/2026-09-02-a-button-label-rule.md`
/// §5(d)): "A's liveness and A's label are the same fact." This is the
/// **one central test**, not one per screen -- a per-screen assertion
/// is the exact scatter that caused the bug this rule fixes.
///
/// Two things are checked per screen state, both against
/// [`Screen::focused_activation`] as the single source of truth:
///
/// 1. `Screen::resolve_a` (what the rail actually renders) agrees with
///    `focused_activation().is_some()`. This is a regression tripwire
///    on the *mechanism*: today the two are one call apart by
///    construction (`screen.rs`'s `activate_focused`/`resolve_a` both
///    read `focused_activation`), so this cannot fail without someone
///    reintroducing a second channel for A -- see the design doc's "what
///    is NOT enforceable" note on `Box<dyn Widget>` wrapper forwarding,
///    which is exactly the gap this line stands guard over.
/// 2. The rendered word matches Uma's assignment table (design doc §4)
///    verbatim, which *is* capable of failing on an ordinary per-screen
///    regression (wrong verb, or a screen silently losing its verb).
///
/// Reuses [`freshness_cases`]'s table of screen-state builders rather
/// than hand-rolling a second one -- one table of "every production
/// screen state", not two that can drift apart.
#[test]
fn a_rail_liveness_matches_activation_for_every_screen() {
    fn store_corrupt_boot_devices() -> App {
        let mut app = App::new(240, 240);
        app.handle_event(Event::StoreLoaded { status: StoreStatus::RecordCorrupt });
        open_devices(&mut app);
        app
    }

    // A paired device row focused on Devices -- design section 4's
    // `open` row (row 3 of the audit table), distinct from
    // `devices_list`'s empty-store "Pair new headphones" row.
    fn devices_list_paired_row_focused() -> App {
        let mut app = App::new(240, 240);
        app.handle_event(upsert([9; 6], "Cans", 1));
        open_devices(&mut app);
        app
    }

    type ActivationCase = (&'static str, fn() -> App, Option<Verb>);

    let mut cases: Vec<ActivationCase> = freshness_cases()
        .into_iter()
        .map(|(name, build, _)| {
            let expected = match name {
                "home, status face" | "home, status face, connected with live out level" => Some(Verb::Exception("devs")),
                "home, menu face" => Some(Verb::Open),
                // `devices_list`'s builder (see `freshness_cases`) has
                // no paired devices, so the only row is "Pair new
                // headphones" -- design doc section 4's `pair` row, not
                // its `open` row. `devices_list_paired_row_focused`
                // below covers the `open` case with an actual device
                // row focused.
                "devices list" => Some(Verb::Pair),
                "forget picker" => Some(Verb::Select),
                "forget confirm" => Some(Verb::Select),
                "device detail" => None,
                // Settings gained real rows with pico-link-qivj.2 (the
                // screensaver dim/off + timeout setting) -- its first
                // row ("IDLE SCREEN") is a focusable `Action` row that
                // pushes a picker, so `A` is correctly live now (design
                // doc's own rule: a real destination behind a row means
                // `A` must say so).
                "settings" => Some(Verb::Open),
                "wizard: scanning" => Some(Verb::Pair),
                "wizard: nothing found" => Some(Verb::Scan),
                "wizard: connecting" | "wizard: not responding" | "wizard: failed" | "wizard: succeeded" => None,
                other => panic!(
                    "{other}: no expected A-verb entry in this test -- add one from design doc \
                     .planning/design/2026-09-02-a-button-label-rule.md section 4's assignment table, don't skip it"
                ),
            };
            (name, build, expected)
        })
        .collect();
    // No devices survive a corrupt store, so (like `devices_list`) the
    // only row is "Pair new headphones" -- `pair`, not `open`. This
    // case exists to cover design row 14 (same defect class as row 3,
    // a Devices screen with A silent), not to exercise a different
    // verb.
    cases.push(("store-corrupt boot, devices", store_corrupt_boot_devices, Some(Verb::Pair)));
    cases.push(("devices list, paired row focused", devices_list_paired_row_focused, Some(Verb::Open)));

    for (name, build, expected) in cases {
        let app = build();
        let screen = app.navigator.current();
        let rendered_live = matches!(screen.resolve_a(), ButtonLabel::Live(_));
        let activation = screen.focused_activation();

        assert_eq!(
            rendered_live,
            activation.is_some(),
            "{name}: rail A liveness ({rendered_live}) disagrees with focused_activation \
             ({activation:?}) -- design rule 4 says these are the same fact"
        );
        assert_eq!(
            activation, expected,
            "{name}: A's verb is {activation:?}, expected {expected:?} per design doc section 4's assignment table"
        );
    }
}
