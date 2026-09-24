//! `App`: the platform-free application state the unified main loop
//! ([`crate::run::run`]) drives every frame.
//!
//! This is the minimal shell left after stripping the previous product
//! layer down to a generic UI-framework template: a [`Navigator`] built
//! once over a placeholder root screen, plus the single [`FrameBuffer565`]
//! it renders into. There is no domain model, no sync, and no output seam
//! wired up here — those are exactly the pieces a concrete product adds on
//! top of this shell: build real content [`Screen`]s, wire `Action`s to
//! push/pop them, and hand `App` whatever live state those screens need to
//! read.

use alloc::collections::VecDeque;
use alloc::rc::Rc;
#[cfg(test)]
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::time::Duration;

use embedded_graphics::primitives::Rectangle;

use crate::input::NavIntent;
use crate::power::DisplaySettings;
use crate::render::home::build_home_screen;
use crate::render::{FrameBuffer565, Instant, Navigator, RenderCtx};
// Only the `#[cfg(test)]` methods below (`push_screen_for_test`,
// `replace_root_for_test`) name `Screen` directly -- a plain `cargo build`
// never uses it, hence the otherwise-unwarranted `allow`.
#[allow(unused_imports)]
use crate::render::Screen;

mod events;
mod fault;
mod fold;
mod inspect;
mod model;
mod refresh;
mod screen_id;
mod screens;
mod ui_state;

pub use events::{Command, ConnectFailureReason, ConnectStep, Event, StoreStatus, VolumeSource, VolumeState};
pub use fault::{FaultEntry, FaultGlyphClass, FaultKey, FaultLog, FaultSeverity, FaultValue};
pub use model::{BtModel, ConnectedCodec, DeviceAddr, DeviceEntry, LinkState, OutLevelSample, PairedDevice};
pub(crate) use model::{decay_peak, is_audio_sink, truncate_device_name, MAX_SCAN_LIST_ITEMS};
pub(crate) use refresh::{Refresh, ScreenCarry};
pub use screen_id::{PickerKind, ScreenId, SettingsPickerKind};
pub(crate) use screens::device_page::build_device_page_screen;
pub(crate) use screens::devices::build_devices_screen;
// Only test code (this module's own `mod tests` and `render::wizard`'s)
// reaches this via `crate::app::DEVICES_TITLE` -- a plain `cargo build`
// never uses it, hence the otherwise-unwarranted `allow`.
#[allow(unused_imports)]
pub(crate) use screens::devices::DEVICES_TITLE;
use screens::ldac_quality::build_ldac_quality_picker_screen;
pub(crate) use screens::ldac_quality::LDAC_QUALITY_ADAPTIVE;
pub(crate) use screens::settings::{build_settings_picker_screen, build_settings_screen};
pub(crate) use screens::why_page::build_why_page_screen;
pub use ui_state::{DisplaySettingsState, HomeFace, WizardPhase};

/// Floor applied to [`Widget::redraw_after`]'s returned [`Duration`]
/// before it is added to `ctx.now()` to produce [`App::next_redraw_at`]
/// (see [`App::render`]). Guards against `Some(Duration::ZERO)` (or any
/// sub-frame duration): unclamped, that would make `next_redraw_at`
/// exactly equal to (or barely past) the instant just rendered, and
/// [`App::tick`]'s due-check uses `>=`, so it would come due on the very
/// next tick with effectively no time elapsed -- forever. That silently
/// reinstates the always-dirty behaviour
/// `.planning/decisions/2026-08-31-render-ctx-frame-scoped-clock.md`
/// forbids: it costs the flush-skip on every screen and puts full-frame
/// blits into SPI contention with audio (see pico-link-6wz). Not a live
/// bug -- no production widget returns a sub-frame duration today -- this
/// is a guard against a future one.
///
/// This is a conservative floor, not a measured frame cadence: no
/// existing `core`-visible constant was available to reuse.
/// `run::run`'s `frame_budget` is a runtime parameter chosen per platform
/// (the emulator passes 33ms; firmware's superloop has no fixed period at
/// all, see `firmware/src/main.c`'s per-frame `pl_ui_tick` call), not a
/// compile-time constant `App` can see. `Duration::from_millis(16)`
/// (~60Hz) is comfortably below any real frame period on this hardware,
/// so it never meaningfully delays a widget with a genuinely short but
/// non-pathological redraw interval -- it only rules out the
/// exactly-or-near-zero case that re-dirties every tick.
///
/// Deliberately *not* gated behind `debug_assert!`/`cfg(debug_assertions)`:
/// this project's firmware builds `NDEBUG`-on by default (pico-sdk forces
/// `CMAKE_BUILD_TYPE=Release` when the caller sets none), which also
/// compiles out Rust's `debug_assert!` -- so a debug_assert-only guard
/// would protect the host test suite and emulator but not the shipped
/// firmware, exactly the environment where this regression is costly. The
/// clamp below is unconditional in every build and is the actual guard;
/// there is no accompanying `debug_assert!`, deliberately -- returning a
/// sub-floor duration is a normal, silently-handled case (see
/// [`Widget::redraw_after`](crate::render::Widget::redraw_after)'s doc
/// comment), not a bug to flag loudly, and this crate's own test suite
/// exercises exactly that case.
const MIN_REDRAW_DELAY: Duration = Duration::from_millis(16);



/// Shared, interior-mutable handle to the live [`BtModel`] -- the M0 step
/// of the live-widgets refactor (bead `pico-link-bgnd`,
/// `.planning/design/2026-09-24-live-widgets-retire-refresh-stack.md`
/// section 2). `App` owns the one `Rc`; future app-view widgets (Home,
/// Devices, ...) will hold their own clone of this same handle so they can
/// read live state at render/sync time instead of being rebuilt from a
/// snapshot on every model change. `RefCell`, not a plain `Rc<BtModel>`:
/// `App`'s own event-folding methods need to mutate through it. **Borrow
/// rule**: never hold a live `Ref`/`RefMut` across a call back into `App`
/// (e.g. `refresh_stack`) -- borrow, read/write, drop, *then* call back in,
/// or the `RefCell` panics at runtime. Model writes happen only inside
/// `App::handle_event`'s fold methods, never during `sync`/`render`/
/// `dispatch`, so such a panic would indicate a real bug, not a false
/// positive.
pub(crate) type ModelHandle = Rc<RefCell<BtModel>>;

/// The application core: a [`Navigator`] built once over the devices root
/// screen, the [`BtModel`] that screen (and any future ones) reads to
/// render itself, and the single [`FrameBuffer565`] rendered into.
pub struct App {
    navigator: Navigator,
    framebuffer: FrameBuffer565,
    /// Whether the current screen state has changed since the last
    /// [`App::render`] call. The run loop uses this to skip
    /// `DisplaySurface::flush` on frames where nothing changed.
    dirty: bool,
    /// The live Bluetooth device/link state, folded in from [`Event`]s via
    /// [`App::handle_event`]. Screens are built by *reading* this, not by
    /// owning fragments of it themselves -- see [`BtModel`]'s doc comment.
    /// A [`ModelHandle`] (`Rc<RefCell<BtModel>>`) as of bead
    /// `pico-link-bgnd` M0, not a bare `BtModel` -- see that type alias's
    /// doc comment for the borrow rule and why.
    model: ModelHandle,
    /// C's own clock, threaded through from [`App::tick`]
    /// (`pl_ui_tick`'s `now_us` in the FFI surface -- previously received
    /// and silently discarded, see pico-link-a67). `core` never reads a
    /// hardware timer itself (the platform seam owns that); this is purely
    /// the latest value C has told it. Not yet consumed by any screen in
    /// this bead's scope -- storing it is the fix pico-link-a67 asks for;
    /// wiring a liveness/timeout indicator to it is future UI work.
    now_us: u64,
    /// The next instant, if any, at which some widget on the current
    /// screen says its own appearance would differ purely from elapsed
    /// time -- recomputed by every [`App::render`] call from
    /// [`Navigator::redraw_after`], and consulted by [`App::tick`] to mark
    /// the app dirty exactly when it comes due. `None` means nothing
    /// currently on screen has a time-driven opinion, so `tick` alone will
    /// never mark this app dirty (see the frame-scoped clock ADR's "hacks
    /// to retire" section for why `tick` does not just mark dirty
    /// unconditionally).
    next_redraw_at: Option<Instant>,
    /// Queued by the devices screen's `on_activate_index` closures (see
    /// [`build_devices_screen`]), drained by [`App::poll_command`]. `Rc`+
    /// `RefCell` because the closures live inside the `Navigator`'s screen
    /// stack, with no path back to `App` itself -- this is the shared
    /// mailbox between them.
    commands: Rc<RefCell<VecDeque<Command>>>,
    /// The pairing wizard's phase (pico-link-znb.7 / E5) -- shared with
    /// whatever `PairingWizardView` widget instance is currently on the
    /// navigator's stack, the same `Rc<RefCell<_>>`-mailbox shape
    /// `commands` uses above. Two-way: the widget itself mutates this
    /// directly for user-input-driven transitions (pressing A/X), and
    /// `App` mutates it directly from C events (`on_connect_step_changed`
    /// and friends) -- either side's write is picked up by the widget's
    /// next `render` because it reads through the same `Rc` rather than a
    /// snapshot, so **no screen replacement is needed** for a phase
    /// transition alone (contrast [`App::rebuild_root`], which fully
    /// rebuilds the *devices* screen on every model change -- the wizard
    /// screen, once pushed, is never rebuilt or replaced; only its shared
    /// state changes underneath it). This is what design section 14/F8
    /// means by "phase/wizard screen replaced by C events without
    /// pushing" at the state level: no `Navigator` stack operation is
    /// involved in a phase advance at all, pushing or otherwise.
    wizard_phase: Rc<RefCell<WizardPhase>>,
    /// A live mirror of `model.discovered`, shared with the wizard widget the
    /// same way `wizard_phase` is -- kept in lockstep by [`App::add_device`]/
    /// [`App::clear_devices`] purely so the wizard's scan-list rendering
    /// doesn't need a borrowed reference into `App` itself (which nothing
    /// living inside `Navigator`'s stack can hold). A small, bounded clone
    /// on every device event (the design's own Class-of-Device filter, E9,
    /// keeps this list under ~12 entries) -- simplicity over cleverness,
    /// matching this crate's existing "rebuilt from scratch" philosophy
    /// for small lists (see [`build_devices_screen`]'s doc comment).
    wizard_devices: Rc<RefCell<Vec<DeviceEntry>>>,
    /// Which of Home's two faces (design section 4/7) is currently
    /// showing -- shared with whatever `HomeView` widget instance is
    /// currently the root screen's content, the same `Rc<RefCell<_>>`-
    /// mailbox shape [`App::wizard_phase`] uses. This indirection is
    /// required, not just consistent-for-its-own-sake: unlike the wizard
    /// (pushed once, never rebuilt -- see `wizard.rs`'s module doc),
    /// Home *is* rebuilt on every model change (it's the root screen, see
    /// [`App::rebuild_root`]), so a face toggle recorded only on a
    /// `HomeView` field would be silently discarded the next time a
    /// Bluetooth event fires while the menu face is showing -- exactly
    /// the defect class `pico-link-a67`'s `Navigator::replace_root` fix
    /// already closed for screen-stack depth; this closes the same class
    /// for this one piece of intra-screen state. Read (not written) fresh
    /// by every freshly built `HomeView`, so the toggle survives a
    /// rebuild with no navigator involvement.
    home_face: Rc<RefCell<HomeFace>>,
    /// The `why?` page's frozen block order (design
    /// `.planning/design/2026-09-07-home-fault-strip.md` §8.1, orchestrator
    /// ruling on `pico-link-9eq2.3.3`) -- shared with `HomeView`/
    /// `build_why_page_screen` the same `Rc<RefCell<_>>`-mailbox shape
    /// `home_face` uses, for the same reason: the page's ordering must
    /// survive [`App::refresh_stack`] rebuilding it on every subsequent
    /// fault event while it's open, only ever appending, never re-sorting
    /// (see [`build_why_page_screen`]'s doc comment).
    why_page_order: Rc<RefCell<Vec<FaultKey>>>,
    /// The screensaver dim/off + timeout setting's shared mailbox (bead
    /// pico-link-qivj.2) -- same `Rc<RefCell<_>>` shape as `home_face`/
    /// `wizard_phase`, for the same reason (the Settings screen and its
    /// pickers are pushed `Action::PushView` closures with no path back to
    /// `App`).
    display_settings: Rc<RefCell<DisplaySettingsState>>,
}

impl App {
    /// Builds the app, rendering into a `width`x`height` framebuffer.
    /// `width`/`height` should match whatever the platform's
    /// `DisplaySurface` actually presents — the core has no way to
    /// discover this itself, so callers (each run mode's `main.rs`) pass
    /// in whatever their concrete surface is sized for.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let wizard_phase = Rc::new(RefCell::new(WizardPhase::default()));
        let wizard_devices = Rc::new(RefCell::new(Vec::new()));
        let home_face = Rc::new(RefCell::new(HomeFace::default()));
        let why_page_order = Rc::new(RefCell::new(Vec::new()));
        let display_settings = Rc::new(RefCell::new(DisplaySettingsState::default()));
        let model: ModelHandle = Rc::new(RefCell::new(BtModel::default()));
        let navigator = Navigator::new(build_home_screen(
            &model.borrow(),
            &home_face,
            &commands,
            &wizard_phase,
            &wizard_devices,
            Instant::from_micros(0),
            &why_page_order,
            &display_settings,
            &ScreenCarry::default(),
        ));
        Self {
            navigator,
            framebuffer: FrameBuffer565::new(width, height),
            dirty: true,
            model,
            now_us: 0,
            next_redraw_at: None,
            commands,
            wizard_phase,
            wizard_devices,
            home_face,
            why_page_order,
            display_settings,
        }
    }

    /// The live screensaver dim/off + timeout setting.
    #[must_use]
    pub fn display_settings(&self) -> DisplaySettings {
        self.display_settings.borrow().current
    }

    /// Seeds the live setting (e.g. from `Event::DisplaySettingsLoaded`, or
    /// the emulator's own `DisplaySettings::load` at startup) -- sets
    /// `current` and marks it pending *application* to the live
    /// `IdlePolicy` and a stack refresh, but deliberately does NOT mark it
    /// pending *save*: seeding is "here is what's already stored/
    /// defaulted," not a user edit, and re-saving a value that was just
    /// loaded would be a pointless (if harmless) write on every boot.
    pub fn set_display_settings(&mut self, settings: DisplaySettings) {
        let mut state = self.display_settings.borrow_mut();
        state.current = settings;
        state.apply_pending = true;
        state.refresh_pending = true;
    }

    /// Drains the "apply to `IdlePolicy`" latch -- `Some` at most once per
    /// change, consumed by `crate::run::Runner::step`/`ui-ffi`'s
    /// `pl_ui_tick`. `pub`, not `pub(crate)`, because `ui-ffi` is a
    /// separate crate that needs this exact seam -- see
    /// `crate::run::Runner::step`'s own doc comment for the shape both
    /// callers share (design D9).
    pub fn take_display_settings_to_apply(&mut self) -> Option<DisplaySettings> {
        let mut state = self.display_settings.borrow_mut();
        if state.apply_pending {
            state.apply_pending = false;
            Some(state.current)
        } else {
            None
        }
    }

    /// Drains the "persist" latch -- `Some` at most once per user pick,
    /// consumed by `crate::run::Runner::step`'s `Storage` adapter / `ui-
    /// ffi`'s `pl_ui_poll_command`. `pub` for the same cross-crate reason
    /// as [`App::take_display_settings_to_apply`].
    pub fn take_display_settings_to_save(&mut self) -> Option<DisplaySettings> {
        let mut state = self.display_settings.borrow_mut();
        if state.save_pending {
            state.save_pending = false;
            Some(state.current)
        } else {
            None
        }
    }

    /// Refreshes every screen on the [`Navigator`]'s stack that carries a
    /// [`ScreenId`] to reflect the current [`BtModel`], via
    /// [`Navigator::replace_at`] one index at a time -- the generalization
    /// of the old `rebuild_root` (renamed by
    /// `pico-link-7jol.4`/`.planning/design/2026-09-07-device-page-and-
    /// single-select-picker.md` §1.1) from "refresh index 0, and index 1
    /// if it happens to be Devices, identified by title string" to "refresh
    /// every identified screen, at any depth, identified by
    /// [`ScreenId`]".
    ///
    /// **Behaviour-preserving for every screen that never calls
    /// [`Screen::with_id`]** (the wizard, `ConfirmView`s, Settings):
    /// [`Navigator::id_at`] returns `None` for those, and this loop skips
    /// them exactly as before -- they were never in `rebuild_root`'s old
    /// two-branch check either, so nothing about their behaviour changes.
    /// [`ScreenId::Home`] and [`ScreenId::Devices`] replace the old
    /// `stack[0]`-is-always-Home assumption and the
    /// `title_at(1) == Some(DEVICES_TITLE)` check respectively, with
    /// identical net effect; [`ScreenId::DevicePage`] is new with this
    /// bead.
    ///
    /// If a screen's subject has vanished from the model (e.g. a
    /// [`ScreenId::DevicePage`] for a device that was just forgotten from
    /// one level up), [`Self::build_identified_screen`] returns
    /// [`Refresh::Gone`] and the stack unwinds to just below it via
    /// [`Navigator::truncate_to`] -- "Forget pops two levels" (device-page
    /// design §3.7) becomes structural this way rather than a hand-written
    /// double pop in a confirm's callback, and it fires from *either*
    /// route a device can disappear by, not only the one the user is
    /// looking at.
    fn refresh_stack(&mut self) {
        let mut truncate_at: Option<usize> = None;
        for index in 0..self.navigator.depth() {
            let Some(id) = self.navigator.id_at(index) else { continue };
            let carry = ScreenCarry {
                selected_key: self.navigator.selected_key_at(index),
                selected_index: self.navigator.selected_index_at(index).unwrap_or(0),
                scroll_top: self.navigator.scroll_top_at(index),
            };
            match self.build_identified_screen(id, &carry) {
                Refresh::Rebuild(screen) => self.navigator.replace_at(index, screen),
                Refresh::Gone => {
                    truncate_at = Some(index);
                    break;
                }
                // Leave the screen already on the stack in place -- no
                // `replace_at`, no forced full-frame damage (bead
                // pico-link-bgnd M0). Dead in practice today: no
                // `build_identified_screen` arm returns `Keep` yet.
                Refresh::Keep => {}
            }
        }
        if let Some(index) = truncate_at {
            self.navigator.truncate_to(index.saturating_sub(1));
        }
        self.dirty = true;
    }

    /// The one mapping from [`ScreenId`] to screen builder --
    /// [`Self::refresh_stack`]'s only caller. Every screen kind that can be
    /// live-refreshed is one match arm here; adding a new refreshable
    /// screen kind means adding a [`ScreenId`] variant and one arm, nothing
    /// else.
    fn build_identified_screen(&self, id: ScreenId, carry: &ScreenCarry) -> Refresh {
        match id {
            ScreenId::Home => Refresh::Rebuild(build_home_screen(
                &self.model.borrow(),
                &self.home_face,
                &self.commands,
                &self.wizard_phase,
                &self.wizard_devices,
                Instant::from_micros(self.now_us),
                &self.why_page_order,
                &self.display_settings,
                carry,
            )),
            ScreenId::Devices => Refresh::Rebuild(build_devices_screen(
                &self.model.borrow(),
                carry.selected_key,
                carry.selected_index,
                carry.scroll_top,
                &self.commands,
                &self.wizard_phase,
                &self.wizard_devices,
            )),
            ScreenId::DevicePage(addr) => build_device_page_screen(&self.model.borrow(), addr, carry, &self.commands),
            ScreenId::Picker(PickerKind::LdacQuality, addr) => build_ldac_quality_picker_screen(&self.model.borrow(), addr, carry, &self.commands),
            ScreenId::WhyPage => build_why_page_screen(&self.model.borrow(), Instant::from_micros(self.now_us), &self.why_page_order, carry),
            ScreenId::Settings => Refresh::Rebuild(build_settings_screen(&self.display_settings, carry)),
            ScreenId::SettingsPicker(kind) => Refresh::Rebuild(build_settings_picker_screen(kind, &self.display_settings, carry)),
        }
    }

    /// Pops the oldest queued user command, if any
    /// (`pl_ui_poll_command`'s core-side implementation). `core` never acts
    /// on these itself -- see [`Command`]'s doc comment.
    pub fn poll_command(&mut self) -> Option<Command> {
        self.commands.borrow_mut().pop_front()
    }

    /// Records C's latest clock reading (`pl_ui_tick`'s core-side
    /// implementation -- previously a no-op that discarded `now_us`
    /// entirely, see pico-link-a67). Marks the app dirty exactly when
    /// `now_us` reaches or passes [`App::next_redraw_at`] -- the
    /// `redraw_after` seam's whole point (see the frame-scoped clock ADR):
    /// a widget declares when it would next look different, rather than
    /// this unconditionally marking dirty on every tick (which would
    /// permanently cost the flush-skip and full-frame-blit a static screen
    /// every tick, contending with audio over SPI on real hardware -- see
    /// the ADR's "hacks to retire" section). Deliberately does **not**
    /// clear `next_redraw_at` here: the next [`App::render`] recomputes it
    /// from scratch, and until then it stays accurate for any repeated
    /// `tick` call in the same frame.
    pub fn tick(&mut self, now_us: u64) {
        self.now_us = now_us;
        if let Some(due) = self.next_redraw_at {
            if Instant::from_micros(now_us) >= due {
                self.dirty = true;
            }
        }
    }

    /// Dispatches every polled `NavIntent` to the navigator, in order.
    /// A no-op (including leaving `dirty` untouched) if `intents` is empty.
    pub fn handle_input(&mut self, intents: Vec<NavIntent>) {
        if intents.is_empty() {
            return;
        }
        let ctx = RenderCtx::at(Instant::from_micros(self.now_us));
        for intent in intents {
            // Sync before EACH dispatch, not once before the loop: a single
            // intent can pop the stack, exposing a screen underneath that
            // was not synced yet this frame -- see `Navigator::sync_top`'s
            // doc comment (bead `pico-link-bgnd` M0).
            self.navigator.sync_top(&ctx);
            self.navigator.dispatch(intent);
        }
        // A Settings picker's `on_pick` may have set `refresh_pending`
        // (design S6) -- the checkmark/row value must move on the SAME
        // frame as the press, which needs `refresh_stack`, not anything
        // `Runner`/`pl_ui_tick` does downstream.
        let refresh_pending = {
            let mut state = self.display_settings.borrow_mut();
            core::mem::take(&mut state.refresh_pending)
        };
        if refresh_pending {
            self.refresh_stack();
        }
        self.stamp_pending_wizard_timestamp();
        self.dirty = true;
    }

    /// Whether [`App::render`] would draw something different from the
    /// last time it was called.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Forces the next [`App::render`] to redraw, without any actual
    /// screen-state change. Platform-free: this is plumbing for the
    /// idle-screensaver run loop to force a repaint on wake (the
    /// framebuffer's *content* never changed while the display was off,
    /// but the display itself needs a fresh flush once it's powered back
    /// on).
    ///
    /// Also forces the *whole framebuffer* damaged on that next render
    /// (`Navigator::force_full_damage`) -- one of the damage pass's
    /// enumerated full-damage triggers (design section 3.4, "wake from
    /// display blank"): the panel was off, so nothing on it can be trusted
    /// to already show the current screen's pixels, and a plain damage
    /// diff (which only compares *this app's* last painted state, not
    /// what's physically on the blanked panel) would otherwise see
    /// nothing dirty and skip repainting entirely.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.navigator.force_full_damage();
    }

    /// Renders the current screen into the app's framebuffer and clears
    /// the dirty flag, returning the freshly rendered framebuffer (plus
    /// the frame damage rect the damage pass actually painted -- see
    /// [`RenderOutput`]) for the caller to hand to a `DisplaySurface::flush`.
    ///
    /// Also recomputes [`App::next_redraw_at`] from
    /// [`Navigator::redraw_after`] at this frame's `ctx` -- so a widget's
    /// "I'll look different again in N" answer is always relative to the
    /// instant that was actually just rendered, not a stale one. The
    /// returned duration is floored at [`MIN_REDRAW_DELAY`] before being
    /// added to `ctx.now()` -- see that constant's doc comment for why.
    ///
    /// # Panics
    ///
    /// Never, in practice: `Navigator::render`'s `Result` is over
    /// `Infallible`'s uninhabited error type (the core `DrawTarget` can
    /// never fail to draw). The `expect` exists only because
    /// `Result::expect` is how that's asserted at the call site.
    pub fn render(&mut self) -> RenderOutput<'_> {
        let ctx = RenderCtx::at(Instant::from_micros(self.now_us));
        self.navigator.sync_top(&ctx);
        let damage = self
            .navigator
            .render(&ctx, &mut self.framebuffer)
            .expect("core DrawTarget is Infallible");
        self.next_redraw_at = self.navigator.redraw_after(&ctx).map(|duration| ctx.now() + duration.max(MIN_REDRAW_DELAY));
        self.dirty = false;
        RenderOutput { framebuffer: &self.framebuffer, damage }
    }
}

/// The result of one [`App::render`] call: the framebuffer that was drawn
/// into, plus the frame damage rect the damage pass
/// (`.planning/design/2026-09-06-damage-rect-render-and-partial-blit.md`
/// section 3.3) actually painted this frame -- `Rectangle::zero()` on a
/// frame that changed nothing on screen.
///
/// `Deref`s to [`FrameBuffer565`] so every existing call site that only
/// ever wanted the framebuffer itself (`.pixel(..)`, `.pixels()`,
/// `.size()`, a `DisplaySurface::flush(&output)`) keeps compiling
/// unchanged -- only a caller that actually needs the rect (the FFI seam,
/// bead `pico-link-7h5.6`; this bead's own A4 property test below) reads
/// [`Self::damage`] directly.
pub struct RenderOutput<'a> {
    framebuffer: &'a FrameBuffer565,
    pub damage: Rectangle,
}

impl core::ops::Deref for RenderOutput<'_> {
    type Target = FrameBuffer565;

    fn deref(&self) -> &FrameBuffer565 {
        self.framebuffer
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod invariants;

#[cfg(test)]
mod tests;
