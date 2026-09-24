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
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;

    use crate::render::{ButtonLabel, ListItem, Verb, VerticalList};

    use super::model::MAX_PAIRED_DEVICES;
    use super::*;
    use super::test_support::*;

    // --- VT6 design section 5.4/6.1: `App::volume_requires_dim_floor` ---

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
        // Bead pico-link-4vb.4 (T5): with nothing remembered, Devices has
        // only one row ("Pair new headphones") and Down has nowhere to go
        // -- one paired device gives it a second row to move onto.
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

    // --- pico-link-a67: the navigator-preservation fix ---
    //
    // These are the single most valuable tests in this bead: the old
    // `rebuild_root` called `Navigator::new`, which resets the stack to
    // depth 1 over a brand-new root screen. That's invisible with only one
    // screen ever on the stack (this crate's current shipped behavior) but
    // fatal for the approved multi-screen design, where a Bluetooth event
    // arriving while the user is browsing a pushed screen would silently
    // eject them back to root. `push_screen_for_test` simulates "the user
    // navigated away from root" without this bead building any real second
    // screen.

    #[test]
    fn a_bluetooth_event_mid_navigation_does_not_reset_the_screen_stack() {
        let mut app = App::new(240, 240);
        app.push_screen_for_test(Screen::new("detail", vec![]));
        assert_eq!(app.navigator_depth(), 2);
        assert_eq!(app.current_screen_title(), "detail");

        // Three different Event variants, all of which used to rebuild the
        // whole Navigator via App::rebuild_root.
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
        // MRU-descending (design section 4): B (seq 2) sorts above A (seq 1).
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
        // back to row 0 -- proving `App::rebuild_root`'s
        // `Navigator::replace_at(1, ...)` path carries the selection
        // forward the way `replace_root` always has.
        app.handle_event(upsert([3, 3, 3, 3, 3, 3], "Device C", 0));
        assert_eq!(app.devices_selected_index_for_test(), Some(1), "a new device must not reset the user's selection");

        // Same for a link-state change while browsing.
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        assert_eq!(app.devices_selected_index_for_test(), Some(1), "a link-state change must not reset the user's selection");
    }

    /// Field-list widget ruling §4.7's defect fix: `App::rebuild_root`
    /// carried the *selection* forward across a live-model rebuild but not
    /// `top_index`, so a scrolled Devices list snapped back to the top on
    /// any unrelated event and `reconcile_top_index` then re-landed the
    /// selected row at the viewport's BOTTOM edge -- latent while only 4
    /// rows fit, not latent once a page scrolls. `MAX_PAIRED_DEVICES` (8)
    /// paired rows + the fixed "Pair new headphones" row is 9, comfortably
    /// past this screen's ~5-row viewport (206px content / 40px rows).
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


    #[test]
    fn connect_failed_event_updates_the_model_and_returns_the_link_to_idle() {
        let mut app = App::new(240, 240);
        app.set_link_state(LinkState::Connecting);
        let addr = [9, 9, 9, 9, 9, 9];

        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink });

        assert_eq!(app.model().last_connect_failure, Some((addr, ConnectFailureReason::NoA2dpSink)));
        assert_eq!(app.model().link_state, LinkState::Idle);
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

    /// pico-link-6wz: a widget returning `Some(Duration::ZERO)` must not
    /// re-dirty the app on the immediately-following tick with no time
    /// elapsed. Unclamped, `next_redraw_at` would equal exactly
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
        // pico-link-znb.2 (E1): the wizard screen (pico-link-znb.7) that
        // binds B to this doesn't exist yet, so this exercises the
        // enqueue/drain path directly via the test-only helper rather than
        // through UI input -- the same shape `pl_ui_poll_command` will see.
        let mut app = App::new(240, 240);
        assert_eq!(app.poll_command(), None, "no command queued yet");

        app.push_command_for_test(Command::CancelScan);
        assert_eq!(app.poll_command(), Some(Command::CancelScan));
        assert_eq!(app.poll_command(), None, "the queue drains -- one poll per queued command");
    }

    #[test]
    fn disconnect_command_round_trips_through_poll_command() {
        // Bead pico-link-44w: FFI surface only -- no screen queues this
        // yet (design-of-record rule 2 forbids a labelled-but-dead
        // affordance), so this exercises the enqueue/drain path directly
        // via the test-only helper, same shape as `CancelScan` above
        // before its wizard binding existed.
        let mut app = App::new(240, 240);
        assert_eq!(app.poll_command(), None, "no command queued yet");

        app.push_command_for_test(Command::Disconnect);
        assert_eq!(app.poll_command(), Some(Command::Disconnect));
        assert_eq!(app.poll_command(), None, "the queue drains -- one poll per queued command");
    }

    // --- pico-link-znb.8 (E7): Home's two-face toggle ---
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

        // `App::rebuild_root` runs on every one of these -- proving the
        // menu face (held in `App::home_face`, shared with the freshly
        // rebuilt `HomeView` -- see `render::home`'s module doc) survives
        // a root rebuild the same way pico-link-a67 already proved pushed
        // screens survive one.
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

    // --- pico-link-dgx: does navigating back to Home disconnect the link? ---
    //
    // Orchestrator investigation (bead comment, 2026-08-30) already refuted
    // both hypotheses in the bead's description by reading the code: there
    // is no Disconnect command in the FFI at all, none of the five command
    // push sites in `core/` is reachable from navigating to Home, and the
    // one command a Back press CAN emit (`CancelConnect`, wizard.rs:309) is
    // a no-op on the C side and only fires from `Connecting`/`NotResponding`,
    // never from a connected state. These tests turn that reading into a
    // regression: they drive a connected `BtModel` through every realistic
    // Back route to Home and assert (a) the command queue stays *entirely*
    // empty -- not just free of a Disconnect variant that cannot exist --
    // and (b) the model and Home's own render both still say connected.

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
        // Bead pico-link-cz0.6 (M5 persistence): a real success now always
        // queues PersistDevice -- drain exactly that one command rather
        // than asserting the queue is empty (which this test did before
        // that bead landed).
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

    /// Route 2: Back from an arbitrary pushed screen (standing in for a
    /// future device-detail screen, per this bead's brief -- no such screen
    /// exists in `core/` yet, so `push_screen_for_test` is the only way to
    /// simulate "the user navigated one level deep and pressed Back") while
    /// connected.
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

    /// Route 3: the `Navigator::replace_root` path `App::rebuild_root`
    /// drives (app.rs:630) -- not a Back press at all, but the other way
    /// Home's content changes while sitting at the root. Confirms a live
    /// Bluetooth event folding into an already-connected model, with Home
    /// as the current (root) screen the whole time, queues nothing and
    /// keeps rendering connected.
    #[test]
    fn a_bluetooth_event_while_home_is_root_does_not_disconnect_or_queue_commands() {
        let mut app = App::new(240, 240);
        connect_link(&mut app);
        assert_eq!(app.navigator_depth(), 1);
        assert_no_commands_queued(&mut app);
        assert_link_still_connected(&app);
        assert_home_hero_renders_connected(&mut app);

        // A second, unrelated event folds through `rebuild_root` again --
        // must not disturb the connected model or queue anything either.
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 1, 1, 1, 1, 1], name: String::from("Other"), rssi: -55, class_of_device: 0 }));
        assert_no_commands_queued(&mut app);
        assert_link_still_connected(&app);
        assert_home_hero_renders_connected(&mut app);
    }

    // --- beads pico-link-cz0.6 / pico-link-4vb.4 (T4): StoreLoaded / PersistDevice ---

    #[test]
    fn store_loaded_after_paired_devices_folded_auto_reconnects_to_the_mru_max() {
        // Reshaped by bead pico-link-4vb.4 (T4), design section 5.2:
        // `StoreLoaded` no longer carries an address -- C's real boot
        // sequence is `count` x `PairedDeviceUpserted` THEN `StoreLoaded` as
        // the terminator, so this drives that same order.
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
        // why Event::ConnectSucceeded carries its own `addr` (bead
        // pico-link-cz0.6) rather than requiring core to read it back off
        // WizardPhase, which stays WizardPhase::default() (NothingFound)
        // for the whole debug-bypass path. Persistence must still work.
        let mut app = App::new(240, 240);
        let addr = [42, 42, 42, 42, 42, 42];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        assert_eq!(app.poll_command(), Some(Command::PersistDevice { addr }));
        assert_eq!(app.poll_command(), None);
    }

    // --- pico-link-7jol.4: refresh_stack (the ScreenId refactor) ---

    /// Fern's design §7 step 1's explicit ask: an unidentified screen (the
    /// wizard, `ConfirmView`s, Settings, or in this test's case an
    /// arbitrary probe screen standing in for any of them) must never be
    /// replaced OR truncated by `refresh_stack`, no matter how many
    /// unrelated model events fire while it's on the stack.
    #[test]
    fn refresh_stack_never_touches_a_screen_with_no_screen_id() {
        let mut app = App::new(240, 240);
        open_devices(&mut app);
        let probe = Screen::new("PROBE", vec![Box::new(VerticalList::new(vec![ListItem::new("x")]))]);
        assert_eq!(probe.id(), None, "a screen that never calls with_id must report no ScreenId");
        app.push_screen_for_test(probe);
        assert_eq!(app.navigator_depth(), 3);
        assert_eq!(app.current_screen_title(), "PROBE");

        // A handful of unrelated Bluetooth-domain events, each of which
        // calls `refresh_stack` internally.
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });
        app.handle_event(upsert([9; 6], "Other", 3));
        app.handle_event(Event::PairedDeviceForgotten { addr: [9; 6] });

        assert_eq!(app.navigator_depth(), 3, "an unidentified screen must never be popped/truncated by refresh_stack");
        assert_eq!(app.current_screen_title(), "PROBE", "an unidentified screen must never be replaced by refresh_stack");
    }

    #[test]
    fn refresh_stack_keeps_home_and_devices_tagged_with_their_screen_ids() {
        let mut app = App::new(240, 240);
        assert_eq!(app.navigator.id_at(0), Some(ScreenId::Home));
        open_devices(&mut app);
        assert_eq!(app.navigator.id_at(1), Some(ScreenId::Devices));
        // A model event refreshes both -- both must keep their identity
        // (a stale/lost id here would silently stop refresh_stack from
        // ever refreshing them again).
        app.handle_event(upsert([1; 6], "Cans", 1));
        assert_eq!(app.navigator.id_at(0), Some(ScreenId::Home));
        assert_eq!(app.navigator.id_at(1), Some(ScreenId::Devices));
    }

    /// "Forget pops two levels" (device-page design §3.7), now structural
    /// via `Refresh::Gone` rather than a hand-written double pop -- this
    /// fires even when the device disappears from a route *other than*
    /// the device page's own Forget row (here: forgetting it from the
    /// Devices screen underneath, one level below the open device page).
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
        // Bead pico-link-hu97: while music plays, volume/codec/meter
        // events fire `refresh_stack` constantly (audio events, not user
        // input). Before the fix, `ScreenId::Home`'s arm rebuilt
        // `HomeView` without the `ScreenCarry` it had just read, so every
        // one of those refreshes snapped the menu face back to row 0
        // (Bluetooth) even while the user was looking at Settings.
        let mut app = App::new(240, 240);
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected, row 0)
        app.handle_input(vec![NavIntent::Down]); // move to Settings (row 1) -- not activated
        assert_eq!(app.navigator.selected_index_at(0), Some(1), "Down must move the menu's own selection to row 1 before any refresh");

        // A model event that runs `refresh_stack` but has nothing to do
        // with the user's navigation -- the exact shape of the events that
        // fire continuously while streaming.
        app.handle_event(Event::LevelsChanged { peak_l: 10, peak_r: 10, rms_l: 10, rms_r: 10 });

        assert_eq!(app.navigator.selected_index_at(0), Some(1), "a live refresh must not reset the Home menu's selection to row 0");
        assert_eq!(app.navigator.scroll_top_at(0), None, "Home's menu has no scroll concept -- always None, carried or not");
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
        // pico-link-qx8's trap: a fast down-step can report a transient
        // non-ladder rate (~700 kbps) for one packet. `on_ldac_bitrate_
        // changed` must store exactly what it's given.
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

    // --- pico-link-88xs: the link-state vs discovery axis split ---
    //
    // Design `.planning/design/2026-09-08-link-state-vs-discovery-axis.md`
    // section 8.5's six owed tests. Test 5 (paint-key fold) lives in
    // `render::screen`'s own test module, and test 6 (the malformed-wire
    // rejection) lives in `ui-ffi`'s -- both own the code under test.

    /// Test 1 (design 8.5.1): THE REGRESSION ITSELF -- the test whose
    /// absence let the bug ship. A scan started while connected must not
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

    /// Test 2 (design 8.5.2): guard against over-correcting -- a real
    /// disconnect must still clear all four fields, exactly as before.
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

    /// Test 3 (design 8.5.3): the wizard's scan-end detection moves onto
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

    /// Test 4 (design 8.5.4): the pending-timestamp backfill
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
}
