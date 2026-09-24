//! `App`: the platform-free application state the unified main loop
//! ([`crate::run::run`]) drives every frame.
//!
//! Owns the [`Navigator`] (built over the Home root screen), the live
//! [`BtModel`], and the single [`FrameBuffer565`] it renders into. Bluetooth
//! events fold into the model via [`App::handle_event`]; user input reaches
//! the navigator via [`App::handle_input`]; screens read the model to render
//! and queue [`Command`]s back out through [`App::poll_command`].

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
#[cfg(test)]
use crate::render::Screen;

mod events;
mod fault;
mod fold;
mod inspect;
mod model;
mod screen_id;
mod screens;
mod ui_state;

pub use events::{Command, ConnectFailureReason, ConnectStep, Event, StoreStatus, VolumeSource, VolumeState};
pub use fault::{FaultEntry, FaultGlyphClass, FaultKey, FaultLog, FaultSeverity, FaultValue};
pub use model::{BtModel, ConnectedCodec, DeviceAddr, DeviceEntry, LinkState, OutLevelSample, PairedDevice};
pub(crate) use model::{decay_peak, is_audio_sink, truncate_device_name, MAX_SCAN_LIST_ITEMS};
pub use screen_id::{PickerKind, ScreenId, SettingsPickerKind};
pub(crate) use screens::device_page::build_device_page_screen;
pub(crate) use screens::devices::build_devices_screen;
#[cfg(test)]
pub(crate) use screens::devices::DEVICES_TITLE;
pub(crate) use screens::ldac_quality::LDAC_QUALITY_ADAPTIVE;
pub(crate) use screens::settings::build_settings_screen;
pub(crate) use screens::why_page::build_why_page_screen;
pub use ui_state::{DisplaySettingsState, HomeFace, WizardPhase};

/// Floor applied to [`Widget::redraw_after`]'s returned [`Duration`]
/// before it is added to `ctx.now()` to produce [`App::next_redraw_at`]
/// (see [`App::render`]). Guards against `Some(Duration::ZERO)` (or any
/// sub-frame duration): unclamped, that would make `next_redraw_at`
/// exactly equal to (or barely past) the instant just rendered, and
/// [`App::tick`]'s due-check uses `>=`, so it would come due on the very
/// next tick with effectively no time elapsed -- forever, reinstating an
/// always-dirty screen that costs the flush-skip and contends with audio
/// over SPI. Not a live bug (no production widget returns a sub-frame
/// duration today) -- a guard against a future one.
///
/// A conservative floor, not a measured frame cadence: `run::run`'s
/// `frame_budget` is a runtime parameter chosen per platform, not a
/// compile-time constant `App` can see. `Duration::from_millis(16)`
/// (~60Hz) is comfortably below any real frame period on this hardware.
///
/// Deliberately *not* gated behind `debug_assert!`: this project's firmware
/// builds `NDEBUG`-on by default, which compiles out `debug_assert!` too --
/// a debug-only guard would protect the host test suite but not the
/// shipped firmware, exactly where this regression is costly. The clamp
/// below is unconditional in every build.
const MIN_REDRAW_DELAY: Duration = Duration::from_millis(16);
/// Shared, interior-mutable handle to the live [`BtModel`]. `App` owns the
/// one `Rc`; screens hold their own clone so they can read live state at
/// render/sync time instead of being rebuilt from a snapshot. `RefCell`,
/// not a plain `Rc<BtModel>`: `App`'s own event-folding methods need to
/// mutate through it. **Borrow rule**: never hold a live `Ref`/`RefMut`
/// across a call back into `App` (e.g. `mark_model_changed`) -- borrow,
/// read/write, drop, *then* call back in, or the `RefCell` panics at
/// runtime. Model writes happen only inside `App::handle_event`'s fold
/// methods, never during `sync`/`render`/`dispatch`, so such a panic would
/// indicate a real bug, not a false positive.
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
    /// See [`ModelHandle`]'s doc comment for the borrow rule.
    model: ModelHandle,
    /// C's own clock, threaded through from [`App::tick`] (`pl_ui_tick`'s
    /// `now_us` in the FFI surface). `core` never reads a hardware timer
    /// itself (the platform seam owns that); this is purely the latest
    /// value C has told it.
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
    /// The pairing wizard's phase -- shared with whatever
    /// `PairingWizardView` widget instance is currently on the navigator's
    /// stack, the same `Rc<RefCell<_>>`-mailbox shape `commands` uses
    /// above. Two-way: the widget itself mutates this directly for
    /// user-input-driven transitions (pressing A/X), and `App` mutates it
    /// directly from C events (`on_connect_step_changed` and friends) --
    /// either side's write is picked up by the widget's next `render`
    /// because it reads through the same `Rc` rather than a snapshot, so
    /// **no screen replacement is needed** for a phase transition alone --
    /// the wizard screen, once pushed, is never rebuilt or replaced; only
    /// its shared state changes underneath it (the same "long-lived, reads
    /// live state" shape every screen has followed since bead
    /// `pico-link-bgnd`'s M1-M4).
    wizard_phase: Rc<RefCell<WizardPhase>>,
    /// Which of Home's two faces is currently showing -- shared with
    /// whatever `HomeView` widget instance is currently the root screen's
    /// content, the same `Rc<RefCell<_>>`-mailbox shape [`App::wizard_phase`]
    /// uses. Required not because `HomeView` is rebuilt (it isn't -- built
    /// once at [`App::new`] and never again, see `render::home`'s module
    /// doc), but because `App` itself is a genuine *second writer*:
    /// `on_wizard_auto_dismiss` forces the face back to `Status` from a
    /// fold method, with no other path back to the live `HomeView` instance
    /// sitting inside the `Navigator`'s stack -- the same shape
    /// [`App::wizard_phase`] has for its own second-writer fold methods.
    home_face: Rc<RefCell<HomeFace>>,
    /// The screensaver dim/off + timeout setting's shared mailbox -- same
    /// `Rc<RefCell<_>>` shape as `home_face`/`wizard_phase`, for the same
    /// reason (the Settings screen and its pickers are pushed
    /// `Action::PushView` closures with no path back to `App`).
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
        let home_face = Rc::new(RefCell::new(HomeFace::default()));
        let display_settings = Rc::new(RefCell::new(DisplaySettingsState::default()));
        let model: ModelHandle = Rc::new(RefCell::new(BtModel::default()));
        let navigator = Navigator::new(build_home_screen(&model, &home_face, &commands, &wizard_phase, Instant::from_micros(0), &display_settings));
        Self {
            navigator,
            framebuffer: FrameBuffer565::new(width, height),
            dirty: true,
            model,
            now_us: 0,
            next_redraw_at: None,
            commands,
            wizard_phase,
            home_face,
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
    /// `IdlePolicy`, but deliberately does NOT mark it pending *save*:
    /// seeding is "here is what's already stored/defaulted," not a user
    /// edit, and re-saving a value that was just loaded would be a
    /// pointless (if harmless) write on every boot. No stack refresh to
    /// mark either (bead `pico-link-bgnd` M3): the Settings screen/pickers
    /// read this same `Rc<RefCell<_>>` handle live via `Widget::sync`.
    pub fn set_display_settings(&mut self, settings: DisplaySettings) {
        let mut state = self.display_settings.borrow_mut();
        state.current = settings;
        state.apply_pending = true;
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

    /// Drops any screen on the [`Navigator`]'s stack (and everything above
    /// it) whose subject has vanished from the model -- e.g. a
    /// [`ScreenId::DevicePage`]/[`ScreenId::Picker`]`(LdacQuality, _)` for a
    /// device that was just forgotten. Called by [`Self::mark_model_changed`]
    /// after every `BtModel`-mutating fold, since a device can vanish via
    /// *any* such event, not only the one the user is looking at. Screens
    /// that never call [`Screen::with_id`] (the wizard, `ConfirmView`s) are
    /// skipped: [`Navigator::id_at`] returns `None` for those. Every other
    /// identified screen kind (`Home`, `Devices`, `WhyPage`, `Settings`,
    /// `SettingsPicker`) has no subject that can vanish, so this is a no-op
    /// for them -- see [`ScreenId`]'s doc comment for the full "identity/
    /// liveness tag, not a rebuild trigger" rule.
    fn prune_stack(&mut self) {
        let mut truncate_at: Option<usize> = None;
        for index in 0..self.navigator.depth() {
            let gone = match self.navigator.id_at(index) {
                Some(ScreenId::DevicePage(addr) | ScreenId::Picker(PickerKind::LdacQuality, addr)) => {
                    !self.model.borrow().paired.iter().any(|d| d.addr == addr)
                }
                _ => false,
            };
            if gone {
                truncate_at = Some(index);
                break;
            }
        }
        if let Some(index) = truncate_at {
            self.navigator.truncate_to(index.saturating_sub(1));
        }
    }

    /// Every `BtModel`-mutating fold method calls this exactly once, in
    /// place of the old `refresh_stack` rebuild -- runs [`Self::prune_stack`]
    /// (a forgotten device's page/picker must still unwind cleanly) and
    /// marks the app dirty so the next [`Self::render`] picks up the
    /// change. No screen is rebuilt or replaced here: every identified
    /// screen kind reads the live model itself via `Widget::sync` on the
    /// next render/input dispatch (see [`Navigator::sync_top`]).
    fn mark_model_changed(&mut self) {
        self.prune_stack();
        self.dirty = true;
    }

    /// Pops the oldest queued user command, if any
    /// (`pl_ui_poll_command`'s core-side implementation). `core` never acts
    /// on these itself -- see [`Command`]'s doc comment.
    pub fn poll_command(&mut self) -> Option<Command> {
        self.commands.borrow_mut().pop_front()
    }

    /// Records C's latest clock reading (`pl_ui_tick`'s core-side
    /// implementation). Marks the app dirty exactly when `now_us` reaches
    /// or passes [`App::next_redraw_at`] -- a widget declares when it would
    /// next look different, rather than this unconditionally marking dirty
    /// on every tick. Deliberately does **not** clear `next_redraw_at`
    /// here: the next [`App::render`] recomputes it from scratch, and
    /// until then it stays accurate for any repeated `tick` call in the
    /// same frame.
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
            // doc comment.
            self.navigator.sync_top(&ctx);
            self.navigator.dispatch(intent);
        }
        // The Settings screen/its pickers pick up a same-frame change to
        // `display_settings` themselves via `Widget::sync` (bead
        // `pico-link-bgnd` M3) -- there used to be a `refresh_pending` latch
        // forcing a `refresh_stack` call here; it's gone, because nothing
        // needs to force a rebuild any more.
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
    /// (`Navigator::force_full_damage`): the panel was off, so nothing on
    /// it can be trusted to already show the current screen's pixels, and a
    /// plain damage diff (which only compares *this app's* last painted
    /// state, not what's physically on the blanked panel) would otherwise see
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
/// into, plus the frame damage rect the damage pass actually painted this
/// frame -- `Rectangle::zero()` on a frame that changed nothing on screen.
///
/// `Deref`s to [`FrameBuffer565`] so every existing call site that only
/// ever wanted the framebuffer itself (`.pixel(..)`, `.pixels()`,
/// `.size()`, a `DisplaySurface::flush(&output)`) keeps compiling
/// unchanged -- only a caller that actually needs the rect (the FFI seam)
/// reads [`Self::damage`] directly.
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
