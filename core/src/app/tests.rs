#![cfg(test)]

use alloc::boxed::Box;
use alloc::vec;

use crate::audio::{AbrFloor, CushionPolicy};
use crate::dsp::{Band, BandKind, Preset, Program};
use crate::render::{ListItem, VerticalList};

use super::model::MAX_PAIRED_DEVICES;
use super::*;
use super::test_support::*;

// --- `App::volume_requires_dim_floor` ---

#[test]
fn volume_requires_dim_floor_is_false_with_no_volume_reading() {
    let app = App::new(240, 240);
    assert!(!app.volume_requires_dim_floor());
}

#[test]
fn volume_requires_dim_floor_is_true_when_muted_or_zero_from_any_source() {
    let mut app = App::new(240, 240);
    app.on_volume_changed(80, true, VolumeSource::Host);
    assert!(app.volume_requires_dim_floor(), "muted must require the dim floor");

    app.on_volume_changed(0, false, VolumeSource::Sink);
    assert!(app.volume_requires_dim_floor(), "0% must require the dim floor regardless of source");

    app.on_volume_changed(80, false, VolumeSource::Sink);
    assert!(!app.volume_requires_dim_floor(), "an ordinary non-zero unmuted reading must not require the floor");
}
#[test]
fn a_fresh_app_is_dirty_and_renders_the_initial_screen() {
    let app = App::new(240, 240);
    assert!(app.dirty());
}

#[test]
fn render_clears_the_dirty_flag() {
    let mut app = App::new(240, 240);
    assert!(app.dirty());
    app.render();
    assert!(!app.dirty());
}

#[test]
fn mark_dirty_sets_the_flag_even_with_no_screen_state_change() {
    let mut app = App::new(240, 240);
    app.render();
    assert!(!app.dirty());

    app.mark_dirty();
    assert!(app.dirty(), "mark_dirty should force the flag on");
}

#[test]
fn handle_input_with_no_intents_does_not_mark_dirty() {
    let mut app = App::new(240, 240);
    app.render();
    assert!(!app.dirty());
    app.handle_input(vec![]);
    assert!(!app.dirty());
}

#[test]
fn handle_input_marks_dirty_and_moving_selection_changes_the_rendered_framebuffer() {
    use crate::render::theme::palette;
    use embedded_graphics::prelude::Point;

    let mut app = App::new(240, 240);
    // Home's status face has no focusable list of its own (Up/Down
    // is unbound there in Tier 1 -- see `render::home`'s module doc),
    // so this proof needs the Devices screen's list underneath it.
    // With nothing remembered, Devices has only one row ("Pair new
    // headphones") and Down has nowhere to go -- one paired device
    // gives it a second row to move onto.
    app.handle_event(upsert([1, 2, 3, 4, 5, 6], "Test Headphones", 1));
    open_devices(&mut app);

    // x=200: past the chip/accent area and these short labels' text,
    // so it samples the row's plain elevated fill rather than a glyph
    // pixel, and still inside the 240px-wide (Epic B2) panel.
    let frame_0 = app.render().pixel(Point::new(200, 18));
    assert_eq!(frame_0, palette::SURFACE_ELEVATED, "row 0 should start selected");

    app.handle_input(vec![NavIntent::Down]);
    assert!(app.dirty(), "moving selection should mark the app dirty");

    let frame_1_row_0 = app.render().pixel(Point::new(200, 18));
    assert_ne!(frame_1_row_0, palette::SURFACE_ELEVATED, "row 0 should no longer be selected");
}

#[test]
fn navigator_starts_at_depth_one_with_the_placeholder_root_screen() {
    let app = App::new(240, 240);
    assert_eq!(app.navigator_depth(), 1);
}

// --- The navigator-preservation fix ---
//
// These are among the most valuable tests in this module: an earlier
// implementation called `Navigator::new` on every model change, which
// resets the stack to depth 1 over a brand-new root screen. That's
// invisible with only one screen ever on the stack, but fatal for a
// multi-screen design, where a Bluetooth event arriving while the user
// is browsing a pushed screen would silently eject them back to root.
// `push_screen_for_test` simulates "the user navigated away from root"
// without building any real second screen.

#[test]
fn a_bluetooth_event_mid_navigation_does_not_reset_the_screen_stack() {
    let mut app = App::new(240, 240);
    app.push_screen_for_test(Screen::new("detail", vec![]));
    assert_eq!(app.navigator_depth(), 2);
    assert_eq!(app.current_screen_title(), "detail");

    // Three different Event variants, all of which fold through
    // App::mark_model_changed.
    app.handle_event(Event::DiscoveryStateChanged { scanning: true });
    assert_eq!(app.navigator_depth(), 2, "DiscoveryStateChanged must not pop the pushed screen");
    assert_eq!(app.current_screen_title(), "detail");

    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 2, 3, 4, 5, 6], name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
    assert_eq!(app.navigator_depth(), 2, "DeviceDiscovered must not pop the pushed screen");
    assert_eq!(app.current_screen_title(), "detail");

    app.handle_event(Event::DevicesCleared);
    assert_eq!(app.navigator_depth(), 2, "DevicesCleared must not pop the pushed screen");
    assert_eq!(app.current_screen_title(), "detail");

    app.handle_event(Event::ConnectFailed { addr: [1, 2, 3, 4, 5, 6], reason: ConnectFailureReason::Timeout });
    assert_eq!(app.navigator_depth(), 2, "ConnectFailed must not pop the pushed screen");
    assert_eq!(app.current_screen_title(), "detail");
}

#[test]
fn a_paired_device_upserted_mid_navigation_does_not_reset_the_devices_screens_selection() {
    let mut app = App::new(240, 240);
    // MRU-descending: B (seq 2) sorts above A (seq 1).
    // Upserted BEFORE opening Devices so the screen's very first build
    // already reflects them -- Home's Bluetooth row always opens
    // Devices with `(prev_key: None, prev_index: 0)`, so starting
    // selection is row 0 of whatever the model holds at that moment.
    app.handle_event(upsert([1, 1, 1, 1, 1, 1], "Device A", 1));
    app.handle_event(upsert([2, 2, 2, 2, 2, 2], "Device B", 2));
    open_devices(&mut app);

    // Devices rows: 0 = Device B, 1 = Device A, 2 = "Pair new headphones".
    app.handle_input(vec![NavIntent::Down]);
    assert_eq!(app.devices_selected_index_for_test(), Some(1), "selection should be on Device A's row");

    // A third device, sorting below both, must not snap the selection
    // back to row 0 -- proving `DevicesListView`'s live model read updates
    // rows via `set_items` in place, preserving the current selection,
    // rather than resetting it.
    app.handle_event(upsert([3, 3, 3, 3, 3, 3], "Device C", 0));
    assert_eq!(app.devices_selected_index_for_test(), Some(1), "a new device must not reset the user's selection");

    // Same for a link-state change while browsing.
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert_eq!(app.devices_selected_index_for_test(), Some(1), "a link-state change must not reset the user's selection");
}

/// A defect fix: the old `App::refresh_stack` mechanism carried the
/// *selection* forward across a live-model rebuild but not `top_index`, so
/// a scrolled Devices list snapped back to the top on any unrelated event and
/// `reconcile_top_index` then re-landed the selected row at the
/// viewport's BOTTOM edge -- latent while only 4 rows fit, not latent
/// once a page scrolls. `MAX_PAIRED_DEVICES` (8) paired rows + the fixed
/// "Pair new headphones" row is 9, comfortably past this screen's
/// ~5-row viewport (206px content / 40px rows).
#[test]
fn an_unrelated_event_does_not_snap_a_scrolled_devices_list_back_to_the_top() {
    let mut app = App::new(240, 240);
    let max = u8::try_from(MAX_PAIRED_DEVICES).expect("MAX_PAIRED_DEVICES is a small constant, fits in u8");
    for i in 0..max {
        app.handle_event(upsert([i; 6], "Cans", u32::from(i)));
    }
    open_devices(&mut app);
    // Jump to the last row ("Pair new headphones") and render once so
    // `VerticalList::render`'s `reconcile_top_index` call actually
    // scrolls the viewport (scrolling is computed at render time, not
    // on `on_intent` -- see that function's doc comment).
    app.handle_input(vec![NavIntent::JumpBy(i16::from(max) + 1)]);
    app.render();
    let scroll_top_before = app.devices_scroll_top_for_test().expect("a scrolled Devices list must report a scroll-top row");
    assert!(scroll_top_before > 0, "jumping to the last of 9 rows on a ~5-row viewport must have actually scrolled");

    // An unrelated event rebuilds the Devices screen from scratch.
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    app.render();
    assert_eq!(
        app.devices_scroll_top_for_test(),
        Some(scroll_top_before),
        "an unrelated model event must not snap the scrolled list back to the top"
    );
}


// --- Bead pico-link-0cq2: `connecting` as a third, independent axis from
// `link_state`, so a failed connect ATTEMPT at a second device no longer
// wipes an already-established link to a first (the "NO LINK while the
// old link is still up" bug). ---

/// Test 1: THE REGRESSION ITSELF -- device A is connected and streaming;
/// a connect attempt at device B begins and fails. A's connected model
/// (link, addr, codec, `out_level`, `ldac_live_kbps`) must survive untouched,
/// `connecting` must end false, and the failure/wizard must record B.
#[test]
fn failed_switch_keeps_established_link() {
    let mut app = App::new(240, 240);
    let addr_a = [1; 6];
    let addr_b = [2; 6];

    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr: addr_a, degraded: false });
    app.poll_command();
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr: addr_a, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 });
    app.handle_event(Event::LdacBitrateChanged { kbps: 660 });

    app.handle_event(Event::ConnectAttemptStarted);
    app.handle_event(Event::ConnectFailed { addr: addr_b, reason: ConnectFailureReason::RadioError });

    assert_eq!(app.model().link_state, LinkState::Connected, "A's link must survive a failed attempt at B");
    assert_eq!(app.model().connected_addr, Some(addr_a));
    assert!(app.model().connected_codec.is_some(), "A's codec must survive");
    assert!(app.model().out_level.is_some(), "A's out_level must survive");
    assert_eq!(app.model().ldac_live_kbps, Some(660), "A's live bitrate must survive");
    assert!(!app.model().connecting, "the attempt is over");
    assert_eq!(app.model().last_connect_failure, Some((addr_b, ConnectFailureReason::RadioError)));
    assert_eq!(app.wizard_phase_for_test(), WizardPhase::Failed { addr: addr_b, reason: ConnectFailureReason::RadioError });
}

/// Test 2: a connect attempt beginning, on its own, must not touch
/// `link_state`/`connected_addr` -- and the Home glyph must read Live
/// throughout (`Connected` always wins over `connecting`, same rule as
/// `discovering`).
#[test]
fn attempt_does_not_touch_link() {
    let mut app = App::new(240, 240);
    let addr_a = [1; 6];
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr: addr_a, degraded: false });
    app.poll_command();

    app.handle_event(Event::ConnectAttemptStarted);

    assert_eq!(app.model().link_state, LinkState::Connected);
    assert_eq!(app.model().connected_addr, Some(addr_a));
    assert!(app.model().connecting);
}

/// Test 3: the (currently unreachable in firmware, but core-representable)
/// switch-succeeds path -- A drops mid-attempt at B, then B's connect
/// succeeds. `connecting` stays true across A's drop (the attempt is still
/// live) and only clears once B's own outcome lands.
#[test]
fn switch_success_follows_new_device() {
    let mut app = App::new(240, 240);
    let addr_a = [1; 6];
    let addr_b = [2; 6];
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr: addr_a, degraded: false });
    app.poll_command();

    app.handle_event(Event::ConnectAttemptStarted);
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert_eq!(app.model().connected_addr, None, "A's link dropping must still clear A's connected fields");
    assert!(app.model().connecting, "the attempt at B is still in flight, unaffected by A's drop");

    app.handle_event(Event::ConnectSucceeded { addr: addr_b, degraded: false });
    app.poll_command();
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr: addr_b, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));

    assert_eq!(app.model().connected_addr, Some(addr_b));
    assert!(!app.model().connecting);
}

/// Test 4: a connect attempt failing from a fully idle starting point
/// (no prior link at all) behaves exactly as before -- stays Idle,
/// `connecting` ends false.
#[test]
fn failure_from_idle_stays_idle() {
    let mut app = App::new(240, 240);
    let addr = [9, 9, 9, 9, 9, 9];

    app.handle_event(Event::ConnectAttemptStarted);
    app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink });

    assert_eq!(app.model().last_connect_failure, Some((addr, ConnectFailureReason::NoA2dpSink)));
    assert_eq!(app.model().link_state, LinkState::Idle);
    assert!(!app.model().connecting);
}

// --- Bead pico-link-sfw6: break-before-make device switching, design
// `.planning/design/2026-09-25-device-switch-break-before-make.md` sec 7
// (S1/S2). Firmware (bt.c) owns the switch state machine; these tests only
// prove core's side of the additive `ConnectStep::Disconnecting` wire item
// and the "never reconnect A on a failed switch" guarantee -- core's own
// event-fold rules (this test file's `switch_success_follows_new_device`/
// `failed_switch_keeps_established_link` above) already cover the rest,
// unchanged by this bead. ---

/// S1: a full switch success passes through `Disconnecting`. A is
/// connected and streaming; the switch starts (`ConnectAttemptStarted`,
/// then `ConnectStepChanged(Disconnecting)`); A's link drops
/// (`LinkStateChanged(Idle)`) while the attempt stays live; B is paged
/// (`ConnectStepChanged(Connecting)`) and succeeds.
#[test]
fn switch_success_passes_through_disconnecting() {
    let mut app = App::new(240, 240);
    let addr_a = [1; 6];
    let addr_b = [2; 6];

    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr: addr_a, degraded: false });
    app.poll_command(); // drain PersistDevice(A)
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr: addr_a, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    assert_eq!(app.model().link_state, LinkState::Connected, "A is up before the switch starts");

    // The switch starts: firmware pushes ConnectAttemptStarted, then the
    // new Disconnecting step, both before A's ACL has actually dropped.
    app.handle_event(Event::ConnectAttemptStarted);
    app.handle_event(Event::ConnectStepChanged(ConnectStep::Disconnecting));
    assert_eq!(app.model().link_state, LinkState::Connected, "A's link is untouched by the step change alone");
    assert!(app.model().connecting, "the attempt has started");

    // A's ACL actually drops.
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert_eq!(app.model().connected_addr, None, "A's connected fields clear on its own Idle");
    assert!(app.model().connecting, "the attempt at B is still in flight, unaffected by A's drop (Busy, not idle)");

    // B is paged and succeeds.
    app.handle_event(Event::ConnectStepChanged(ConnectStep::Connecting));
    app.handle_event(Event::ConnectSucceeded { addr: addr_b, degraded: false });
    app.poll_command(); // drain PersistDevice(B)
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr: addr_b, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));

    assert_eq!(app.model().connected_addr, Some(addr_b));
    assert_eq!(app.model().link_state, LinkState::Connected);
    assert!(!app.model().connecting, "the attempt is over");
}

/// S2: a failed switch stays disconnected and never reconnects A --
/// Andreas's ruling (bead comments, supersedes the design description's
/// original make-before-break recommendation). Same shape as S1 through
/// A's Idle, then the switch fails outright instead of succeeding: no
/// `Command::Connect` for A (or anyone) is ever queued as a side effect.
#[test]
fn switch_failure_stays_disconnected_and_never_reconnects_a() {
    let mut app = App::new(240, 240);
    let addr_a = [1; 6];
    let addr_b = [2; 6];

    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr: addr_a, degraded: false });
    app.poll_command(); // drain PersistDevice(A)

    app.handle_event(Event::ConnectAttemptStarted);
    app.handle_event(Event::ConnectStepChanged(ConnectStep::Disconnecting));
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert!(app.model().connecting, "the attempt at B is still in flight");

    app.handle_event(Event::ConnectFailed { addr: addr_b, reason: ConnectFailureReason::Timeout });

    assert_eq!(app.model().link_state, LinkState::Idle, "no reconnect to A -- the device stays disconnected");
    assert_eq!(app.model().connected_addr, None);
    assert!(!app.model().connecting, "the attempt is over");
    assert_eq!(app.wizard_phase_for_test(), WizardPhase::Failed { addr: addr_b, reason: ConnectFailureReason::Timeout });

    // The load-bearing assertion: core must never queue a Connect for A
    // (or for anyone) as a reaction to this failure. Auto-reconnect only
    // ever fires from Event::StoreLoaded at boot (a new session) -- a
    // failed switch is not a boot, so draining every queued command here
    // must find none.
    let mut queued = alloc::vec::Vec::new();
    while let Some(command) = app.poll_command() {
        queued.push(command);
    }
    assert!(queued.is_empty(), "a failed switch must queue no commands at all, above all no Connect{{addr: addr_a, ..}}: got {queued:?}");
}

#[test]
fn tick_records_now_us_instead_of_discarding_it() {
    let mut app = App::new(240, 240);
    assert_eq!(app.now_us(), 0);
    app.tick(123_456);
    assert_eq!(app.now_us(), 123_456);
}

/// A widget whose sole purpose is answering [`Widget::redraw_after`]
/// with a fixed [`Duration`] -- everything else is the trait's default
/// (a static, non-focusable, nothing-to-draw widget), so pushing one
/// via [`App::push_screen_for_test`] isolates exactly the
/// `redraw_after` -> `next_redraw_at` -> `tick` wiring under test, with
/// no other widget behaviour in the way.
struct FixedRedrawWidget(core::time::Duration);

impl crate::render::Widget for FixedRedrawWidget {
    fn measure(&self, constraints: embedded_graphics::prelude::Size, _ctx: &crate::render::RenderCtx) -> embedded_graphics::prelude::Size {
        constraints
    }
    fn render(
        &self,
        _area: embedded_graphics::primitives::Rectangle,
        _ctx: &crate::render::RenderCtx,
        _target: &mut FrameBuffer565,
    ) -> Result<(), core::convert::Infallible> {
        Ok(())
    }
    fn redraw_after(&self, _ctx: &crate::render::RenderCtx) -> Option<core::time::Duration> {
        Some(self.0)
    }
}

#[test]
fn a_widget_requesting_a_redraw_leaves_the_app_clean_before_it_is_due_and_dirty_once_it_is() {
    let mut app = App::new(240, 240);
    app.push_screen_for_test(Screen::new("T", vec![Box::new(FixedRedrawWidget(core::time::Duration::from_millis(100)))]));

    // Establishes next_redraw_at = now(0) + 100ms, and clears dirty
    // (the just-pushed screen has just been rendered).
    app.render();
    assert!(!app.dirty());

    app.tick(50_000); // 50ms: before the 100ms mark.
    assert!(!app.dirty(), "must not go dirty before the widget's requested redraw instant is reached");

    app.tick(150_000); // 150ms: past the 100ms mark.
    assert!(app.dirty(), "must go dirty once now_us reaches/passes the widget's requested redraw instant");
}

/// A widget returning `Some(Duration::ZERO)` must not re-dirty the app
/// on the immediately-following tick with no time elapsed. Unclamped,
/// `next_redraw_at` would equal exactly
/// `ctx.now()`, and `tick`'s `>=` due-check would fire on the very
/// next call regardless of elapsed time -- reinstating always-dirty
/// behaviour forever. `MIN_REDRAW_DELAY` clamps this up so the app
/// stays clean until at least that much time has actually passed.
#[test]
fn a_widget_requesting_zero_duration_redraw_does_not_redirty_on_the_next_tick() {
    let mut app = App::new(240, 240);
    app.push_screen_for_test(Screen::new("T", vec![Box::new(FixedRedrawWidget(core::time::Duration::ZERO))]));

    // Establishes next_redraw_at = now(0) + MIN_REDRAW_DELAY (clamped
    // up from the widget's literal ZERO), and clears dirty.
    app.render();
    assert!(!app.dirty());

    // Same instant, zero elapsed: with the bug (no clamp), next_redraw_at
    // == 0 == now_us, so tick's >= check would already fire here.
    app.tick(0);
    assert!(!app.dirty(), "a Some(Duration::ZERO) redraw request must not fire with zero elapsed time");

    // Still short of the clamp floor.
    app.tick(1_000); // 1ms.
    assert!(!app.dirty(), "must not go dirty before MIN_REDRAW_DELAY has actually elapsed");

    // Past the clamp floor (MIN_REDRAW_DELAY == 16ms).
    app.tick(20_000); // 20ms.
    assert!(app.dirty(), "must go dirty once the clamped redraw instant is actually reached");
}

#[test]
fn a_widget_with_no_time_driven_opinion_never_goes_dirty_from_tick_alone() {
    let mut app = App::new(240, 240);
    // MessageView never overrides redraw_after -- it inherits the
    // trait's `None` default, exactly the "nothing to do with time"
    // case this test exists to prove doesn't regress into the
    // "tick always marks dirty" hack the ADR names as the thing to
    // retire.
    app.push_screen_for_test(Screen::new("T", vec![Box::new(crate::render::MessageView::new("hi"))]));

    app.render();
    assert!(!app.dirty());

    // A tick arbitrarily far in the future must still not mark dirty:
    // there is no `next_redraw_at` to ever come due.
    app.tick(1_000_000_000);
    assert!(!app.dirty(), "tick alone must never mark a time-indifferent screen dirty");
}

#[test]
fn cancel_scan_command_round_trips_through_poll_command() {
    // Exercises the enqueue/drain path directly via the test-only helper
    // rather than through UI input -- the same shape `pl_ui_poll_command`
    // will see.
    let mut app = App::new(240, 240);
    assert_eq!(app.poll_command(), None, "no command queued yet");

    app.push_command_for_test(Command::CancelScan);
    assert_eq!(app.poll_command(), Some(Command::CancelScan));
    assert_eq!(app.poll_command(), None, "the queue drains -- one poll per queued command");
}

#[test]
fn disconnect_command_round_trips_through_poll_command() {
    // FFI surface only -- no screen queues this yet, so this exercises
    // the enqueue/drain path directly via the test-only helper.
    let mut app = App::new(240, 240);
    assert_eq!(app.poll_command(), None, "no command queued yet");

    app.push_command_for_test(Command::Disconnect);
    assert_eq!(app.poll_command(), Some(Command::Disconnect));
    assert_eq!(app.poll_command(), None, "the queue drains -- one poll per queued command");
}

// --- Home's two-face toggle ---
//
// Home has no test-only accessor for "which face is showing" (that's
// an internal `HomeView`/`HomeFace` implementation detail -- see
// `render::home`'s module doc), so these prove the toggle
// black-box, the same way `render_png_dump.rs` proves selection
// moves: by sampling the menu face's row-0 ("Bluetooth")
// selection-highlight pixel. The status face never paints
// `SURFACE_ELEVATED` at this coordinate (the hero widget draws no
// list-row fill there), so this single pixel distinguishes the two
// faces unambiguously.

/// x=200, y=18: the same "row 0's selection-highlight fill, clear of
/// any chip/glyph ink" sample point `handle_input_marks_dirty_and_
/// moving_selection_changes_the_rendered_framebuffer` and the
/// `emulator` HTTP/idle-wake e2e tests all use.
fn menu_face_row0_pixel(app: &mut App) -> embedded_graphics::pixelcolor::Rgb565 {
    use embedded_graphics::prelude::Point;
    app.render().pixel(Point::new(200, 18))
}

#[test]
fn centre_toggles_home_to_the_menu_face_and_back_without_pushing() {
    use crate::render::theme::palette;

    let mut app = App::new(240, 240);
    assert_eq!(app.navigator_depth(), 1, "Home starts alone on the stack");
    assert_ne!(menu_face_row0_pixel(&mut app), palette::SURFACE_ELEVATED, "the status face draws no selected list row");

    app.handle_input(vec![NavIntent::Select]); // status -> menu
    assert_eq!(app.navigator_depth(), 1, "toggling to the menu face must not push a screen");
    assert_eq!(
        menu_face_row0_pixel(&mut app),
        palette::SURFACE_ELEVATED,
        "the menu face's Bluetooth row (index 0) is selected by default"
    );

    app.handle_input(vec![NavIntent::Back]); // menu -> status
    assert_eq!(app.navigator_depth(), 1, "returning to the status face must not pop a screen");
    assert_ne!(
        menu_face_row0_pixel(&mut app),
        palette::SURFACE_ELEVATED,
        "back on the menu face must return to the status face, not leave the menu showing"
    );
}

#[test]
fn b_on_the_status_face_is_a_harmless_no_op_and_does_not_leave_home() {
    let mut app = App::new(240, 240);
    assert_eq!(app.navigator_depth(), 1);

    app.handle_input(vec![NavIntent::Back]);
    assert_eq!(app.navigator_depth(), 1, "B on Home's status face (nothing to back out of) must stay on Home");
    assert_eq!(app.current_screen_title(), crate::render::home::HOME_TITLE);
}

#[test]
fn navigator_depth_stays_one_across_many_toggles() {
    let mut app = App::new(240, 240);
    for _ in 0..10 {
        app.handle_input(vec![NavIntent::Select]); // status -> menu
        assert_eq!(app.navigator_depth(), 1);
        app.handle_input(vec![NavIntent::Back]); // menu -> status
        assert_eq!(app.navigator_depth(), 1);
    }
}

#[test]
#[allow(clippy::cast_possible_wrap)]
fn up_and_down_do_nothing_on_homes_status_face_in_tier_1() {
    use embedded_graphics::prelude::{OriginDimensions, Point};

    let mut app = App::new(240, 240);

    let width = app.render().size().width;
    let sample_row = |app: &mut App| -> Vec<embedded_graphics::pixelcolor::Rgb565> {
        let fb = app.render();
        (0..width).map(|x| fb.pixel(Point::new(x as i32, 100))).collect()
    };

    let frame_before = sample_row(&mut app);

    app.handle_input(vec![NavIntent::Up]);
    app.handle_input(vec![NavIntent::Down]);
    assert_eq!(app.navigator_depth(), 1, "Up/Down must not push or pop anything on Home");

    let frame_after = sample_row(&mut app);
    assert_eq!(
        frame_before, frame_after,
        "Up/Down are unbound on Home's status face in Tier 1 (no volume gauge, no binding -- design section 13's 'absent together, not inert')"
    );
}

#[test]
fn a_live_bluetooth_event_does_not_flip_the_face_or_reset_the_navigator() {
    use crate::render::theme::palette;

    let mut app = App::new(240, 240);
    app.handle_input(vec![NavIntent::Select]); // status -> menu
    assert_eq!(menu_face_row0_pixel(&mut app), palette::SURFACE_ELEVATED, "menu face showing before the event");

    // `App::mark_model_changed` runs on every one of these -- proving the
    // menu face (held in `App::home_face`, shared with the never-rebuilt
    // `HomeView` -- see `render::home`'s module doc) isn't disturbed by an
    // unrelated model event.
    app.handle_event(Event::DiscoveryStateChanged { scanning: true });
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 2, 3, 4, 5, 6], name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
    app.handle_event(Event::DevicesCleared);

    assert_eq!(app.navigator_depth(), 1, "a live Bluetooth event must not push or pop anything");
    assert_eq!(
        menu_face_row0_pixel(&mut app),
        palette::SURFACE_ELEVATED,
        "a live Bluetooth event must not flip Home back to the status face"
    );
}

// --- Does navigating back to Home disconnect the link? ---
//
// There is no Disconnect command reachable from navigating to Home at
// all: none of the command push sites in `core/` fire on a Back press,
// and the one command a Back press CAN emit (`CancelConnect`,
// wizard.rs:309) is a no-op on the C side and only fires from
// `Connecting`/`NotResponding`, never from a connected state. These
// tests turn that reading into a regression: they drive a connected
// `BtModel` through every realistic Back route to Home and assert (a)
// the command queue stays *entirely* empty -- not just free of a
// Disconnect variant that cannot exist -- and (b) the model and Home's
// own render both still say connected.

/// Route 1: Back from the wizard's success phase (Succeeded, degraded
/// or not -- both are reachable by a real Back press, only plain
/// success's *auto*-dismiss is gated to `degraded: false`) all the way
/// out to Home's status face, with a live connected link the whole
/// time.
fn back_from_wizard_success_to_home(degraded: bool) {
    let mut app = App::new(240, 240);
    connect_link(&mut app);
    assert_no_commands_queued(&mut app); // sanity: the connect events themselves queue nothing

    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
    app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
    app.handle_input(vec![NavIntent::Select]); // Scan row -> pushes the wizard, straight into Scanning
    assert_eq!(app.navigator_depth(), 3);
    app.poll_command(); // drain StartScan, queued by opening the wizard

    app.handle_event(Event::ConnectSucceeded { addr: DGX_ADDR, degraded });
    assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded });
    // A real success always queues PersistDevice -- drain exactly that
    // one command rather than asserting the queue is empty.
    assert_eq!(app.poll_command(), Some(Command::PersistDevice { addr: DGX_ADDR }));
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);

    // B: wizard Succeeded -> pop to Devices. wizard.rs's `on_intent`
    // has no `(Back, Succeeded { .. })` arm, so this falls through to
    // its `_ => Action::None` and `Navigator::dispatch` does a plain,
    // side-effect-free pop.
    app.handle_input(vec![NavIntent::Back]);
    assert_eq!(app.navigator_depth(), 2, "first Back should land on Devices");
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);

    // B: Devices -> pop to Home (root). Home's `home_face` is still
    // `Menu` here (set by the first Select above and untouched by any
    // pop), so this lands on Home's menu face, not the hero.
    app.handle_input(vec![NavIntent::Back]);
    assert_eq!(app.navigator_depth(), 1, "second Back should land on Home");
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);

    // B: Home menu face -> status face (Home's own local toggle, no
    // pop -- this is the step that actually makes the hero visible).
    app.handle_input(vec![NavIntent::Back]);
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);

    assert_home_hero_renders_connected(&mut app);
}

#[test]
fn back_from_wizard_plain_success_to_home_does_not_disconnect() {
    back_from_wizard_success_to_home(false);
}

#[test]
fn back_from_wizard_degraded_success_to_home_does_not_disconnect() {
    back_from_wizard_success_to_home(true);
}

/// Route 2: Back from an arbitrary pushed screen while connected --
/// `push_screen_for_test` simulates "the user navigated one level deep
/// and pressed Back" without needing a real second screen.
#[test]
fn back_from_a_pushed_screen_to_home_does_not_disconnect() {
    let mut app = App::new(240, 240);
    connect_link(&mut app);
    app.push_screen_for_test(Screen::new("detail", vec![]));
    assert_eq!(app.navigator_depth(), 2);
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);

    app.handle_input(vec![NavIntent::Back]);
    assert_eq!(app.navigator_depth(), 1, "Back should pop the detail screen back to Home");
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);

    // Home starts on its status face by default in this route (no
    // Select was ever pressed), so the hero is already showing.
    assert_home_hero_renders_connected(&mut app);
}

/// Route 3: `HomeView`'s live `Widget::sync` read, not a Back press at
/// all, but the other way Home's content changes while sitting at the
/// root. Confirms a live Bluetooth event folding into an already-connected
/// model, with Home as the current (root) screen the whole time, queues
/// nothing and keeps rendering connected.
#[test]
fn a_bluetooth_event_while_home_is_root_does_not_disconnect_or_queue_commands() {
    let mut app = App::new(240, 240);
    connect_link(&mut app);
    assert_eq!(app.navigator_depth(), 1);
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);
    assert_home_hero_renders_connected(&mut app);

    // A second, unrelated event folds through `mark_model_changed` again --
    // must not disturb the connected model or queue anything either.
    app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 1, 1, 1, 1, 1], name: String::from("Other"), rssi: -55, class_of_device: 0 }));
    assert_no_commands_queued(&mut app);
    assert_link_still_connected(&app);
    assert_home_hero_renders_connected(&mut app);
}

// --- StoreLoaded / PersistDevice ---

#[test]
fn store_loaded_after_paired_devices_folded_auto_reconnects_to_the_mru_max() {
    // `StoreLoaded` carries no address -- C's real boot sequence is
    // `count` x `PairedDeviceUpserted` THEN `StoreLoaded` as the
    // terminator, so this drives that same order.
    let mut app = App::new(240, 240);
    let addr_old = [1, 2, 3, 4, 5, 6];
    let addr_new = [9, 9, 9, 9, 9, 9];
    app.handle_event(upsert(addr_old, "Old", 1));
    app.handle_event(upsert(addr_new, "New", 2));
    app.handle_event(Event::StoreLoaded { status: StoreStatus::Loaded });

    assert_eq!(app.model().store_status, Some(StoreStatus::Loaded));
    assert_eq!(
        app.poll_command(),
        Some(Command::Connect { addr: addr_new, name: String::from("New") }),
        "auto-reconnect must target the highest mru_seq record, using the same Connect command a manual selection uses"
    );
    assert_eq!(app.poll_command(), None, "exactly one Connect, nothing else");
}

#[test]
fn connect_wire_path_carries_a_utf8_truncated_name_for_a_multibyte_device() {
    // Covers the same bug class as the test above but through the real
    // `Command::Connect` wire path (auto-reconnect on `StoreLoaded`),
    // the path `ui-ffi` copies byte-for-byte into the C struct -- so this
    // is also the cheapest proxy for the FFI seam without touching
    // `ui-ffi` itself.
    let mut app = App::new(240, 240);
    let addr = [9, 9, 9, 9, 9, 9];
    let long_name: String = "日".repeat(11);
    app.handle_event(upsert(addr, &long_name, 1));
    app.handle_event(Event::StoreLoaded { status: StoreStatus::Loaded });

    let expected_name = truncate_device_name(&long_name);
    assert_eq!(expected_name.len(), 30, "sanity: the fixture name must actually need truncating");
    assert_eq!(
        app.poll_command(),
        Some(Command::Connect { addr, name: expected_name }),
        "the wire-path Connect command must carry the same char-boundary-truncated name, not the raw 33-byte original"
    );
}

#[test]
fn store_loaded_with_no_paired_devices_records_status_but_queues_nothing() {
    let mut app = App::new(240, 240);
    app.handle_event(Event::StoreLoaded { status: StoreStatus::FirstBoot });

    assert_eq!(app.model().store_status, Some(StoreStatus::FirstBoot));
    assert_eq!(app.poll_command(), None, "no saved device -- nothing to auto-reconnect to");
}

#[test]
fn paired_store_full_is_folded_without_touching_the_paired_list() {
    let mut app = App::new(240, 240);
    app.handle_event(upsert([1; 6], "Existing", 1));
    app.handle_event(Event::PairedStoreFull);

    assert!(app.model().store_full, "PairedStoreFull must be recorded");
    assert_eq!(app.model().paired.len(), 1, "a refused save must not touch the existing paired list");
}

#[test]
fn connect_succeeded_queues_persist_device_for_the_events_own_address() {
    let mut app = App::new(240, 240);
    let addr = [7, 7, 7, 7, 7, 7];

    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });

    assert_eq!(
        app.poll_command(),
        Some(Command::PersistDevice { addr }),
        "a connect that actually succeeded must be queued for persistence"
    );
    assert_eq!(app.poll_command(), None);
}

#[test]
fn connect_succeeded_persists_even_with_the_wizard_closed() {
    // The debug-remote bypass path (firmware/src/bt.c's
    // pl_bt_debug_connect) never drives the wizard -- this is exactly
    // why Event::ConnectSucceeded carries its own `addr` rather than
    // requiring core to read it back off WizardPhase, which stays
    // WizardPhase::default() (NothingFound) for the whole debug-bypass
    // path. Persistence must still work.
    let mut app = App::new(240, 240);
    let addr = [42, 42, 42, 42, 42, 42];
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    assert_eq!(app.poll_command(), Some(Command::PersistDevice { addr }));
    assert_eq!(app.poll_command(), None);
}

// --- prune_stack (the ScreenId refactor) ---

/// An unidentified screen (the wizard, `ConfirmView`s, or in this test's
/// case an arbitrary probe screen standing in for either) must never be
/// truncated by `App::prune_stack`, no matter how many unrelated model
/// events fire while it's on the stack.
#[test]
fn prune_stack_never_touches_a_screen_with_no_screen_id() {
    let mut app = App::new(240, 240);
    open_devices(&mut app);
    let probe = Screen::new("PROBE", vec![Box::new(VerticalList::new(vec![ListItem::new("x")]))]);
    assert_eq!(probe.id(), None, "a screen that never calls with_id must report no ScreenId");
    app.push_screen_for_test(probe);
    assert_eq!(app.navigator_depth(), 3);
    assert_eq!(app.current_screen_title(), "PROBE");

    // A handful of unrelated Bluetooth-domain events, each of which
    // calls `App::mark_model_changed` (and therefore `prune_stack`)
    // internally.
    app.handle_event(Event::DiscoveryStateChanged { scanning: true });
    app.handle_event(upsert([9; 6], "Other", 3));
    app.handle_event(Event::PairedDeviceForgotten { addr: [9; 6] });

    assert_eq!(app.navigator_depth(), 3, "an unidentified screen must never be truncated by prune_stack");
    assert_eq!(app.current_screen_title(), "PROBE", "an unidentified screen must never be touched by prune_stack");
}

#[test]
fn prune_stack_keeps_home_and_devices_tagged_with_their_screen_ids() {
    let mut app = App::new(240, 240);
    assert_eq!(app.navigator.id_at(0), Some(ScreenId::Home));
    open_devices(&mut app);
    assert_eq!(app.navigator.id_at(1), Some(ScreenId::Devices));
    // A model event runs `prune_stack` over both -- neither has a subject
    // that can vanish, so both must keep their identity untouched (a
    // stale/lost id here would be a real regression even though it has no
    // visible symptom for these two screen kinds).
    app.handle_event(upsert([1; 6], "Cans", 1));
    assert_eq!(app.navigator.id_at(0), Some(ScreenId::Home));
    assert_eq!(app.navigator.id_at(1), Some(ScreenId::Devices));
}

/// "Forget pops two levels", structural via `App::prune_stack` rather
/// than a hand-written double pop -- this fires even when the device
/// disappears from a route *other than* the device page's own Forget
/// row (here: forgetting it from the Devices screen underneath, one
/// level below the open device page).
#[test]
fn forgetting_the_device_shown_by_an_open_device_page_unwinds_the_stack_to_devices() {
    let mut app = App::new(240, 240);
    let addr = [7; 6];
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.handle_event(upsert(addr, "Cans", 1));
    open_devices(&mut app);
    app.handle_input(vec![NavIntent::Select]); // connected row -> device page
    assert_eq!(app.navigator_depth(), 3);
    assert_eq!(app.current_screen_title(), "Cans");

    app.handle_event(Event::PairedDeviceForgotten { addr });

    assert_eq!(app.navigator_depth(), 2, "the device page must be dropped when its device vanishes");
    assert_eq!(app.current_screen_title(), DEVICES_TITLE, "unwinding must land on Devices, not Home");
}



#[test]
fn home_menu_selection_survives_a_live_refresh_while_streaming() {
    // While music plays, volume/codec/meter events fire `mark_model_changed`
    // constantly (audio events, not user input). Before the M1 fix,
    // `ScreenId::Home`'s arm rebuilt `HomeView` from scratch on every one
    // of those, snapping the menu face back to row 0 (Bluetooth) even
    // while the user was looking at Settings. As of M1, `HomeView` is
    // never rebuilt at all, so there is nothing left to reset it.
    let mut app = App::new(240, 240);
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected, row 0)
    app.handle_input(vec![NavIntent::Down]); // move to Settings (row 1) -- not activated
    assert_eq!(app.navigator.selected_index_at(0), Some(1), "Down must move the menu's own selection to row 1 before any refresh");

    // A model event that runs `mark_model_changed` but has nothing to do
    // with the user's navigation -- the exact shape of the events that
    // fire continuously while streaming.
    app.handle_event(Event::LevelsChanged { peak_l: 10, peak_r: 10, rms_l: 10, rms_r: 10 });

    assert_eq!(app.navigator.selected_index_at(0), Some(1), "a live refresh must not reset the Home menu's selection to row 0");
    assert_eq!(app.navigator.scroll_top_at(0), None, "Home's menu has no scroll concept -- always None, carried or not");
}

/// The design's T3 (`pico-link-bgnd` M1 / `pico-link-yn5i.1` B1): a
/// `LevelsChanged` event on Home's connected status face must damage only
/// the OUT meter's footprint, not the whole frame -- and in particular
/// must not touch the title bar, which nothing about an OUT-level reading
/// ever redraws. Fails on pre-M1 `main` (every model event forced
/// `force_full_damage` via the old `ScreenId::Home` rebuild arm); passes
/// once `HomeView` forwards `damage_hint`/`damage_region_key` to `hero`
/// (`Widget::sync`, plus `HomeView` never being rebuilt at all).
#[test]
fn home_level_event_does_not_damage_the_whole_frame_or_the_title_bar() {
    use embedded_graphics::prelude::{OriginDimensions, Point};

    let mut app = App::new(240, 240);
    let addr = [21; 6];
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.handle_event(upsert(addr, "Cans", 1));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.tick(1);
    // Establish a clean baseline frame -- the very first render is always
    // a full-frame cache miss (`Screen::render`'s `cache_miss` branch), so
    // it proves nothing about the OUT-level event's own damage on its own.
    app.render();

    app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 });
    let output = app.render();
    let damage = output.damage;
    let whole_frame = Rectangle::new(Point::zero(), output.size());

    assert_ne!(damage, whole_frame, "a live OUT-level reading must not damage the whole frame -- B1's damage forwarding is missing or broken");
    assert!(
        damage.top_left.y >= i32::try_from(crate::render::chrome::TITLE_BAR_HEIGHT).expect("TITLE_BAR_HEIGHT is a small constant, fits in i32"),
        "a live OUT-level reading must not touch the title bar: damage={damage:?}"
    );
}






#[test]
fn home_bitrate_line_shows_the_live_number_and_the_adaptive_tag_when_the_device_is_adaptive() {
    let mut app = App::new(240, 240);
    let addr = [32; 6];
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.poll_command();
    app.handle_event(upsert_with_quality(addr, "Cans", 1, LDAC_QUALITY_ADAPTIVE));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.handle_event(Event::LdacBitrateChanged { kbps: 660 });
    assert_eq!(app.model().ldac_live_kbps, Some(660));
    // Home's own bitrate-line assembly is exercised end to end by
    // `render/hero.rs`'s and `render/home.rs`'s own tests for the
    // `ADAPTIVE` tag's drawing/damage-key rules; this test asserts the
    // model-level fact those depend on: the live figure actually
    // reaches `BtModel`, and a codec change away from LDAC drops it.
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("SBC"), nominal_bitrate_bps: 328_000 }));
    assert_eq!(app.model().ldac_live_kbps, None, "a renegotiation away from LDAC must drop the stale live figure");
}

#[test]
fn ldac_live_kbps_is_never_snapped_to_the_nominal_ladder() {
    // A fast down-step can report a transient non-ladder rate
    // (~700 kbps) for one packet. `on_ldac_bitrate_changed` must store
    // exactly what it's given.
    let mut app = App::new(240, 240);
    app.handle_event(Event::LdacBitrateChanged { kbps: 703 });
    assert_eq!(app.model().ldac_live_kbps, Some(703), "must not snap to the nearest rung");
}

#[test]
fn ldac_live_kbps_is_cleared_on_disconnect() {
    let mut app = App::new(240, 240);
    let addr = [33; 6];
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.poll_command();
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.handle_event(Event::LdacBitrateChanged { kbps: 660 });
    assert_eq!(app.model().ldac_live_kbps, Some(660));
    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert_eq!(app.model().ldac_live_kbps, None);
}

// --- The link-state vs discovery axis split ---
//
// Test 5 (paint-key fold) lives in `render::screen`'s own test module,
// and test 6 (the malformed-wire rejection) lives in `ui-ffi`'s -- both
// own the code under test.

/// Test 1: THE REGRESSION ITSELF -- the test whose absence let the bug
/// ship. A scan started while connected must not
/// wipe any of the four connected-model fields, and `link_state` must
/// still read `Connected` throughout and after the scan.
#[test]
fn a_scan_while_connected_does_not_wipe_the_connected_model() {
    let mut app = App::new(240, 240);
    let addr = [7; 6];
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.poll_command();
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 });
    app.handle_event(Event::LdacBitrateChanged { kbps: 660 });

    assert_eq!(app.model().link_state, LinkState::Connected);
    assert!(app.model().connected_codec.is_some());
    assert_eq!(app.model().connected_addr, Some(addr));
    assert!(app.model().out_level.is_some());
    assert_eq!(app.model().ldac_live_kbps, Some(660));

    // The scan itself: start, then end.
    app.handle_event(Event::DiscoveryStateChanged { scanning: true });
    assert_eq!(app.model().link_state, LinkState::Connected, "scan start must not touch link_state");
    assert!(app.model().connected_codec.is_some(), "scan start must not clear connected_codec");
    assert_eq!(app.model().connected_addr, Some(addr), "scan start must not clear connected_addr");
    assert!(app.model().out_level.is_some(), "scan start must not clear out_level");
    assert_eq!(app.model().ldac_live_kbps, Some(660), "scan start must not clear ldac_live_kbps");

    app.handle_event(Event::DiscoveryStateChanged { scanning: false });
    assert_eq!(app.model().link_state, LinkState::Connected, "inquiry-complete must not touch link_state");
    assert!(app.model().connected_codec.is_some(), "inquiry-complete must not clear connected_codec");
    assert_eq!(app.model().connected_addr, Some(addr), "inquiry-complete must not clear connected_addr");
    assert!(app.model().out_level.is_some(), "inquiry-complete must not clear out_level");
    assert_eq!(app.model().ldac_live_kbps, Some(660), "inquiry-complete must not clear ldac_live_kbps");
}

/// Test 2: guard against over-correcting -- a real disconnect must
/// still clear all four fields, exactly as before.
#[test]
fn a_real_disconnect_still_clears_the_connected_model() {
    let mut app = App::new(240, 240);
    let addr = [8; 6];
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    app.poll_command();
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 });
    app.handle_event(Event::LdacBitrateChanged { kbps: 660 });
    assert!(app.model().connected_codec.is_some());

    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert_eq!(app.model().link_state, LinkState::Idle);
    assert_eq!(app.model().connected_codec, None);
    assert_eq!(app.model().connected_addr, None);
    assert_eq!(app.model().out_level, None);
    assert_eq!(app.model().ldac_live_kbps, None);
}

/// Test 3: the wizard's scan-end detection moves onto
/// `DiscoveryStateChanged` -- a `LinkStateChanged(Idle)` alone (e.g. a
/// connect failure or a disconnect landing while the wizard happens to
/// be on `WizardPhase::Scanning`) must no longer spuriously flip it to
/// `NothingFound`, but a genuine `DiscoveryStateChanged { scanning:
/// false }` with zero devices found still does.
#[test]
fn only_a_genuine_discovery_state_changed_ends_the_wizard_scan() {
    let mut app = App::new(240, 240);
    open_wizard(&mut app);
    assert!(matches!(app.wizard_phase_for_test(), WizardPhase::Scanning { .. }));

    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert!(
        matches!(app.wizard_phase_for_test(), WizardPhase::Scanning { .. }),
        "LinkStateChanged(Idle) alone must no longer end the wizard's scan phase"
    );

    app.handle_event(Event::DiscoveryStateChanged { scanning: false });
    assert_eq!(app.wizard_phase_for_test(), WizardPhase::NothingFound, "a genuine end-of-inquiry with zero devices found must still advance to NothingFound");
}

/// Test 4: the pending-timestamp backfill
/// (`stamp_pending_wizard_timestamp`, called unconditionally at the
/// end of `handle_event`) must still fire when the event that lands is
/// a `DiscoveryStateChanged` -- the new arm must not early-return
/// before reaching it.
#[test]
fn pending_timestamp_backfill_fires_on_a_discovery_state_changed_event() {
    let mut app = App::new(240, 240);
    *app.wizard_phase.borrow_mut() = WizardPhase::scanning_pending();
    app.tick(555_000);
    app.handle_event(Event::DiscoveryStateChanged { scanning: true });
    match app.wizard_phase_for_test() {
        WizardPhase::Scanning { started } => {
            assert_eq!(started, Instant::from_micros(555_000), "DiscoveryStateChanged must still backfill a pending started timestamp");
        }
        other => panic!("expected WizardPhase::Scanning, got {other:?}"),
    }
}

// --- Bead pico-link-8pp1.4 (S3): the cushion-policy mailbox ---

/// `App::new` defaults to `CushionPolicy::Low`, matching Andreas's
/// 2026-09-24 ruling (default is Low until proven otherwise).
#[test]
fn cushion_policy_defaults_to_low() {
    let app = App::new(240, 240);
    assert_eq!(app.cushion_policy(), CushionPolicy::Low);
}

/// Seeding (boot load, `Event::CushionPolicyLoaded`) must set `current`
/// but must NOT mark the save latch -- same discipline as
/// `DisplaySettingsState::apply_pending`/`save_pending`'s "seeding never
/// re-saves what was just loaded".
#[test]
fn cushion_policy_loaded_event_seeds_without_arming_the_save_latch() {
    let mut app = App::new(240, 240);
    app.handle_event(Event::CushionPolicyLoaded { policy: 2 });
    assert_eq!(app.cushion_policy(), CushionPolicy::Stable);
    assert_eq!(app.take_cushion_policy_to_save(), None, "a boot-load seed must never arm the save latch");
}

/// A wire value the enum doesn't understand (0 = unset, or the reserved
/// future Super-stable `3`) falls back to `Low`, the same per-field
/// fallback discipline `ScreensaverMode::from_wire` uses.
#[test]
fn cushion_policy_loaded_event_falls_back_to_low_on_an_unrecognized_wire_value() {
    let mut app = App::new(240, 240);
    app.set_cushion_policy(CushionPolicy::Stable);
    app.handle_event(Event::CushionPolicyLoaded { policy: 3 });
    assert_eq!(app.cushion_policy(), CushionPolicy::Low, "an unrecognized wire value must fall back to the default, Low");
}

/// A user-initiated pick (`request_cushion_policy`, what a future S4
/// picker calls) sets `current` AND arms the save latch, drained exactly
/// once.
#[test]
fn request_cushion_policy_arms_the_save_latch_exactly_once() {
    let mut app = App::new(240, 240);
    app.request_cushion_policy(CushionPolicy::Stable);
    assert_eq!(app.cushion_policy(), CushionPolicy::Stable);
    assert_eq!(app.take_cushion_policy_to_save(), Some(CushionPolicy::Stable));
    assert_eq!(app.take_cushion_policy_to_save(), None, "the save latch must drain to None after being taken once");
}

// --- Bead pico-link-d42g.3 (F3): the Adaptive-floor mailbox ---

/// `App::new` defaults to `AbrFloor::Kbps330`, matching the design's
/// default (330 kbps, no change from pre-bead behaviour).
#[test]
fn abr_floor_defaults_to_330() {
    let app = App::new(240, 240);
    assert_eq!(app.abr_floor(), AbrFloor::Kbps330);
}

/// Seeding (boot load, `Event::AbrFloorLoaded`) must set `current` but
/// must NOT mark the save latch -- same discipline as
/// `cushion_policy_loaded_event_seeds_without_arming_the_save_latch`.
#[test]
fn abr_floor_loaded_event_seeds_without_arming_the_save_latch() {
    let mut app = App::new(240, 240);
    app.handle_event(Event::AbrFloorLoaded { floor: 3 });
    assert_eq!(app.abr_floor(), AbrFloor::Kbps198);
    assert_eq!(app.take_abr_floor_to_save(), None, "a boot-load seed must never arm the save latch");
}

/// A wire value the enum doesn't understand (0 = unset, or anything past
/// 3) falls back to `Kbps330`, the same per-field fallback discipline
/// `CushionPolicy::from_wire` uses.
#[test]
fn abr_floor_loaded_event_falls_back_to_330_on_an_unrecognized_wire_value() {
    let mut app = App::new(240, 240);
    app.set_abr_floor(AbrFloor::Kbps198);
    app.handle_event(Event::AbrFloorLoaded { floor: 7 });
    assert_eq!(app.abr_floor(), AbrFloor::Kbps330, "an unrecognized wire value must fall back to the default, Kbps330");
}

/// A user-initiated pick (`request_abr_floor`, what a future F4 picker
/// calls) sets `current` AND arms the save latch, drained exactly once.
#[test]
fn request_abr_floor_arms_the_save_latch_exactly_once() {
    let mut app = App::new(240, 240);
    app.request_abr_floor(AbrFloor::Kbps246);
    assert_eq!(app.abr_floor(), AbrFloor::Kbps246);
    assert_eq!(app.take_abr_floor_to_save(), Some(AbrFloor::Kbps246));
    assert_eq!(app.take_abr_floor_to_save(), None, "the save latch must drain to None after being taken once");
}

// --- DSP effects editor: live preview (bead pico-link-ryw.7 review fix,
// design sec 5.1) ---

/// Matches `screens::effects::DSP_FS_HZ` (private to that module) -- the
/// one sample rate this build's DSP chain compiles at.
const TEST_DSP_FS_HZ: u32 = 48_000;

/// Design sec 5.1 rule 1: while the editor is open and not bypassed, the
/// stream plays the editor's current draft instantly -- whether or not
/// that effect is assigned to the connected device (there is none here),
/// and without waiting for the `SavePreset`/`PresetLoaded` round trip
/// (`assert_no_commands_queued` is deliberately NOT called: a queued,
/// undrained `SavePreset` must not stop `dsp_program` from already
/// reflecting the edit).
#[test]
fn editing_an_unassigned_effect_changes_dsp_program_immediately() {
    let mut app = App::new(240, 240);
    open_new_effect_editor(&mut app);

    let before = app.dsp_program(TEST_DSP_FS_HZ);
    // CROSSFEED is the editor's first row/default selection; stepping it
    // right changes the draft's saved shape (`apply_step`'s `ROW_CROSSFEED`
    // arm), which `EffectEditorView::on_intent` mirrors into
    // `App::editor_preview` before it ever queues the `SavePreset`.
    app.handle_input(vec![NavIntent::Right]);
    let after = app.dsp_program(TEST_DSP_FS_HZ);

    assert_ne!(before, after, "a Left/Right edit on an unassigned draft must change dsp_program immediately, not after a save round trip");
}

/// Design sec 5.1 rule 2: `X` (bypass) previews Off, regardless of the
/// draft's own contents.
#[test]
fn bypassing_the_open_editor_previews_off() {
    let mut app = App::new(240, 240);
    open_new_effect_editor(&mut app);
    app.handle_input(vec![NavIntent::Right]); // give the draft a non-default shape first
    assert_ne!(app.dsp_program(TEST_DSP_FS_HZ), Program::off(TEST_DSP_FS_HZ), "sanity: the un-bypassed draft must not already be Off");

    app.handle_input(vec![NavIntent::ShortcutX]); // bypass

    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), Program::off(TEST_DSP_FS_HZ), "X (bypass) must preview Off");
}

/// Design sec 5.1 rule 3: leaving the editor (by any route -- `B` here)
/// reverts playback to the connected device's assignment, even while a
/// DIFFERENT draft was being previewed a moment before.
#[test]
fn leaving_the_editor_reverts_dsp_program_to_the_connected_devices_assignment() {
    let mut app = App::new(240, 240);

    // Seed a stored preset (id 7) with a real band, as if C's boot push
    // already loaded it, and connect a device already assigned to it.
    let assigned_id = 7u16;
    let mut assigned_preset = Preset::new("Assigned");
    assigned_preset.push_band(Band { kind: BandKind::Peak, freq_hz: 1_000, gain_half_db: 12, q_idx: 4 });
    app.handle_event(Event::PresetLoaded { id: assigned_id, blob: assigned_preset.to_wire().to_vec() });
    let addr: DeviceAddr = [1, 2, 3, 4, 5, 6];
    app.model.borrow_mut().connected_addr = Some(addr);
    app.model.borrow_mut().paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: assigned_id });

    let assigned_program = Program::from_preset(&assigned_preset, TEST_DSP_FS_HZ);
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), assigned_program, "before any editor opens, rule 3 must resolve the connected device's assignment");

    // Open the effects list and edit a brand-new, still-unassigned effect.
    open_new_effect_editor(&mut app);
    app.handle_input(vec![NavIntent::Right]); // CROSSFEED step -> draft changes
    let preview = app.dsp_program(TEST_DSP_FS_HZ);
    assert_ne!(preview, assigned_program, "while editing a different, unassigned draft, the preview must not be the connected device's still-assigned preset");

    app.handle_input(vec![NavIntent::Back]); // leave the editor

    assert_eq!(
        app.dsp_program(TEST_DSP_FS_HZ),
        assigned_program,
        "leaving the editor by any route must revert dsp_program to the connected device's assignment"
    );
}

/// `ui-ffi`'s pull loop (`pl_ui_take_dsp_program`) calls `App::dsp_program`
/// every superloop iteration and change-gates on the result
/// (`PlUi::last_dsp_program`). It must therefore be a pure read of
/// current state, not something that advances or mutates on every call --
/// each Left/Right step must yield exactly one new program, not a new one
/// per subsequent call/frame with no further input.
#[test]
fn dsp_program_is_a_stable_read_between_edits() {
    let mut app = App::new(240, 240);
    open_new_effect_editor(&mut app);
    app.handle_input(vec![NavIntent::Right]);

    let first_read = app.dsp_program(TEST_DSP_FS_HZ);
    let second_read = app.dsp_program(TEST_DSP_FS_HZ);
    let third_read = app.dsp_program(TEST_DSP_FS_HZ);

    assert_eq!(first_read, second_read, "repeated dsp_program calls with no new input must return an equal program");
    assert_eq!(second_read, third_read, "repeated dsp_program calls with no new input must return an equal program");
}

/// Bead `pico-link-ryw.11`: feeds a small Equalizer APO session through
/// `App`'s debug EQ commands and checks the override it produces wins
/// over BOTH an assigned preset and an open (non-bypassed) editor
/// preview -- `dsp_program`'s doc comment says the debug override is
/// checked "AHEAD of everything else."
#[test]
fn debug_eq_override_wins_over_an_assigned_preset_and_an_open_editor() {
    let mut app = App::new(240, 240);

    // A connected device with a real assigned preset.
    let mut assigned_preset = Preset::new("Assigned");
    let assigned_id = 7;
    assigned_preset.push_band(Band { kind: BandKind::Peak, freq_hz: 1_000, gain_half_db: 12, q_idx: 4 });
    app.handle_event(Event::PresetLoaded { id: assigned_id, blob: assigned_preset.to_wire().to_vec() });
    let addr: DeviceAddr = [9, 9, 9, 9, 9, 9];
    app.model.borrow_mut().connected_addr = Some(addr);
    app.model.borrow_mut().paired.push(PairedDevice {
        addr,
        name: alloc::string::String::from("Cans"),
        mru_seq: 1,
        ldac_quality: 0,
        preset_id: assigned_id,
    });
    let assigned_program = Program::from_preset(&assigned_preset, TEST_DSP_FS_HZ);
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), assigned_program, "sanity: the assignment resolves before any override");

    // Load a debug override.
    app.debug_eq_begin();
    app.debug_eq_line("Preamp: -3 dB").unwrap();
    app.debug_eq_line("Filter 1: ON PK Fc 500 Hz Gain 2 dB Q 1").unwrap();
    app.debug_eq_end().unwrap();

    let overridden = app.dsp_program(TEST_DSP_FS_HZ);
    assert_ne!(overridden, assigned_program, "the debug override must win over the connected device's assigned preset");

    // It must ALSO win over an open, non-bypassed editor preview.
    open_new_effect_editor(&mut app);
    app.handle_input(vec![NavIntent::Right]); // CROSSFEED step -> draft changes, still not bypassed
    assert_eq!(
        app.dsp_program(TEST_DSP_FS_HZ),
        overridden,
        "the debug override must win over an open editor's live preview too"
    );
    app.handle_input(vec![NavIntent::Back]); // leave the editor

    // `EQ OFF` clears it, reverting to the connected device's assignment.
    app.debug_eq_off();
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), assigned_program, "EQ OFF must revert to the normal resolution");
}

/// Bead `pico-link-ryw.11`, orchestrator steer 2026-09-25: the override
/// is `App`-owned state, not tied to any Bluetooth connection -- it must
/// survive the connected device disconnecting and a different (or the
/// same) device reconnecting, and only clear on `EQ OFF` (or a reboot,
/// which this test can't exercise since `App` doesn't persist across
/// process boundaries -- a fresh `App::new` never has an override to
/// begin with).
#[test]
fn debug_eq_override_survives_disconnect_and_reconnect() {
    let mut app = App::new(240, 240);

    app.debug_eq_begin();
    app.debug_eq_line("Preamp: -1 dB").unwrap();
    app.debug_eq_line("Filter 1: ON LS Fc 100 Hz Gain 1 dB BW Oct 1").unwrap();
    app.debug_eq_end().unwrap();
    let overridden = app.dsp_program(TEST_DSP_FS_HZ);
    assert_ne!(overridden, Program::off(TEST_DSP_FS_HZ), "sanity: the override is a real, non-bypass program");

    let addr: DeviceAddr = [1, 1, 1, 1, 1, 1];
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), overridden, "connecting a device must not disturb the debug override");

    app.handle_event(Event::LinkStateChanged(LinkState::Idle));
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), overridden, "disconnecting must not disturb the debug override");

    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), overridden, "reconnecting must not disturb the debug override");

    app.debug_eq_off();
    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), Program::off(TEST_DSP_FS_HZ), "EQ OFF still clears it after a reconnect");
}

/// Malformed input mid-session must not clobber whatever override was
/// already active from a PREVIOUS successful `EQ END`.
#[test]
fn a_failed_eq_end_leaves_the_previous_override_in_place() {
    let mut app = App::new(240, 240);

    app.debug_eq_begin();
    app.debug_eq_line("Preamp: -2 dB").unwrap();
    app.debug_eq_line("Filter 1: ON PK Fc 300 Hz Gain 1 dB Q 1").unwrap();
    app.debug_eq_end().unwrap();
    let first = app.dsp_program(TEST_DSP_FS_HZ);

    // A second session with no Preamp: line -- END must fail.
    app.debug_eq_begin();
    app.debug_eq_line("Filter 1: ON PK Fc 400 Hz Gain 1 dB Q 1").unwrap();
    let err = app.debug_eq_end().unwrap_err();
    assert_eq!(err, DebugEqCommandError::Finish(crate::dsp::EqApoError::MissingPreamp));

    assert_eq!(app.dsp_program(TEST_DSP_FS_HZ), first, "a failed EQ END must not clear or replace the previous override");
}

#[test]
fn eq_commands_without_a_begin_report_not_in_session() {
    let mut app = App::new(240, 240);
    assert_eq!(app.debug_eq_line("Preamp: 0 dB").unwrap_err(), DebugEqCommandError::NotInSession);
    assert_eq!(app.debug_eq_end().unwrap_err(), DebugEqCommandError::NotInSession);
}
