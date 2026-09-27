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
use crate::audio::{AbrFloor, CushionPolicy};
use crate::dsp::{import_preset, EqApoError, EqApoOverride, EqApoSession, ImportError, ImportOutcome, Preset, Program, PresetStore};
use crate::render::ListItemKey;

/// What can go wrong dispatching one `EQ BEGIN`/`EQ <line>`/`EQ END`
/// console command against [`App`]'s session state -- a thin wrapper
/// around [`crate::dsp::EqApoError`]/[`crate::dsp::EqApoLineError`] that
/// adds the one failure mode the parser itself can't see:
/// [`Self::NotInSession`], a command arriving with no `EQ BEGIN` open (or
/// one already consumed by a prior `EQ END`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugEqCommandError {
    /// `EQ <line>` or `EQ END` arrived with no in-progress session.
    NotInSession,
    /// `EQ <line>` failed to parse -- see [`crate::dsp::EqApoLineError`]
    /// for the line number this line was within the session.
    Line(crate::dsp::EqApoLineError),
    /// `EQ END` finished a session that had no `Preamp:` line
    /// ([`EqApoError::MissingPreamp`]) or no enabled filters
    /// ([`EqApoError::NoBands`]) -- session-wide, not one line's fault.
    Finish(EqApoError),
}
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
mod telemetry;
#[cfg(test)]
mod telemetry_fixtures;
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
pub(crate) use screens::effects::{build_effects_list_screen, resolve_effect_name};
pub(crate) use screens::ldac_quality::LDAC_QUALITY_ADAPTIVE;
pub(crate) use screens::settings::build_settings_screen;
pub(crate) use screens::why_page::build_why_page_screen;
pub use ui_state::{AbrFloorState, CushionPolicyState, DisplaySettingsState, HomeFace, WizardPhase};

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
    /// The global congestion-cushion policy's shared mailbox -- bead
    /// pico-link-8pp1.4 (S3). Same `Rc<RefCell<_>>` shape as
    /// `display_settings` above, for a future Settings row/picker (S4).
    /// See [`CushionPolicyState`]'s doc comment for why it carries only
    /// one flag, not two.
    cushion_policy: Rc<RefCell<CushionPolicyState>>,
    /// The global LDAC Adaptive floor's shared mailbox -- bead
    /// pico-link-d42g.3 (F3). Same `Rc<RefCell<_>>` shape as
    /// `cushion_policy` above, for a future Settings row/picker (F4). See
    /// [`AbrFloorState`]'s doc comment for why it carries only one flag,
    /// not two.
    abr_floor: Rc<RefCell<AbrFloorState>>,
    /// The global DSP effects preset store -- bead `pico-link-ryw.5`,
    /// design sec 2.2/3.1. Populated purely by folding
    /// [`Event::PresetLoaded`]/[`Event::PresetDeleted`] (boot load AND
    /// every [`Command::SavePreset`] echo use the same incremental
    /// [`PresetStore::load`] call -- see [`App::on_preset_loaded`]'s doc
    /// comment for why this bead does not use [`PresetStore::from_loaded`]'s
    /// batch constructor the way a one-shot boot sequence could). No
    /// `save_pending`/`apply_pending` latch shape here, unlike
    /// `cushion_policy`/`abr_floor` above: [`Command::SavePreset`]/
    /// [`Command::DeletePreset`]/[`Command::AssignPreset`] are ordinary
    /// queued [`Command`]s (Andreas's "every value change in the editor
    /// saves immediately" ruling means a future editor screen pushes one
    /// per edit, not a single coalesced latch -- see [`Command::SavePreset`]'s
    /// doc comment).
    ///
    /// `Rc<RefCell<_>>`, not a plain field, as of bead `pico-link-ryw.7`:
    /// the effects list, the device page's `EFFECT` row/picker, and Home's
    /// `FX <name>` line all need to read the live store from inside a
    /// pushed screen's own `Widget::sync`, the same [`ModelHandle`] shape
    /// every other piece of live app state already uses.
    presets: Rc<RefCell<PresetStore>>,
    /// The DSP effects editor's live preview, while an editor screen is
    /// open -- bead `pico-link-ryw.7` review fix, design sec 5.1. `None`
    /// when no editor is open (rule 3: [`App::dsp_program`] resolves the
    /// connected device's assignment, unchanged). `Some((draft, bypassed))`
    /// while an editor is open: rule 1 (not bypassed) previews `draft`
    /// instantly; rule 2 (bypassed, `X`) previews Off. Deliberately
    /// separate from the editor's own currently-assigned preset id (which
    /// names *where a save goes*, not *what plays* -- see
    /// `screens::effects::EffectEditorView::editor_preset_id`'s doc
    /// comment; `App` itself holds no field for that any more, since bead
    /// `pico-link-ryw.14` made it always the real, already-Rust-allocated
    /// id from the moment the editor opens, with nothing left for `App` to
    /// adopt from an echo) and from the draft this bead's `EditorState`
    /// owns inside the pushed `EffectEditorView` itself -- `EditorState`
    /// cannot be read from here (no path back to a screen buried in the
    /// `Navigator`'s stack), so the editor widget mirrors its own draft
    /// into this `App`-owned mailbox on every change, the same "second
    /// writer reaches into a live pushed screen via a shared mailbox" shape
    /// `wizard_phase`/`home_face` already use -- except here the pushed
    /// screen is the writer and `App` is the reader. Updated independently
    /// of the `SavePreset` round trip: preview must not wait on flash +
    /// the `PresetLoaded` echo (Andreas's 12:48 ruling: "applies to the
    /// stream instantly, independent of the save").
    editor_preview: Rc<RefCell<Option<(Preset, bool)>>>,
    /// In-progress `EQ BEGIN` .. `EQ END` console session (bead
    /// `pico-link-ryw.11`), or `None` between sessions. Plain field, not
    /// `Rc<RefCell<_>>` like `editor_preview`: nothing in the `Navigator`'s
    /// screen stack reads or writes this -- only `ui-ffi`'s
    /// `pl_ui_debug_eq_command` reaches it, directly through `&mut App`.
    eq_import_session: Option<EqApoSession>,
    /// The effects list's one-shot "focus this row" mailbox -- bead
    /// `pico-link-ryw.12.4`, Uma's design (`ryw12-3-ux.md` sec 2): "move
    /// FOCUS to the new/updated row." Same `Rc<RefCell<_>>`-mailbox shape
    /// [`Self::editor_preview`]/[`Self::wizard_phase`] use for the same
    /// "a pushed screen buried in the `Navigator`'s stack has no other
    /// path back from `App`" reason. Set by [`Self::import_preset`] on a
    /// successful import; consumed (and cleared) by
    /// `EffectsListView::sync`'s [`crate::render::FieldList::focus_key`]
    /// call the next time the effects list screen is on top and syncs --
    /// a stale target left behind (the list screen was never open) is
    /// simply never consumed, matching Uma's "anywhere else: nothing on
    /// screen" rule.
    import_focus: Rc<RefCell<Option<ListItemKey>>>,
    /// Whether C's boot-time preset-store high-water mark has arrived yet
    /// (`Event::PresetStoreLoaded`'s `next_id`) -- bead `pico-link-ryw.14`,
    /// Ada's preset-id-allocation contract. `core` now allocates every
    /// preset id itself ([`PresetStore::create`]), so it must not create
    /// ANY preset (New effect, import) before this is `true`, or a fresh id
    /// could alias one C already holds for a deleted-then-reused slot --
    /// with an upsert contract, a collision silently OVERWRITES an existing
    /// stored preset rather than merely refusing, so this gate is not
    /// optional defence-in-depth.
    ///
    /// Starts `false`, on every build alike (no test-only/production-only
    /// split): a real boot genuinely has a window, however narrow, between
    /// [`App::new`] and C's flash read finishing, and a host test that
    /// wants to create/import a preset must push a real
    /// [`Event::PresetStoreLoaded`] first, same as C's own boot sequence
    /// would -- see `test_support::ready_presets`. `Rc<RefCell<_>>`, not a
    /// plain field: the effects list's New-effect row
    /// (`build_effects_list_screen`) reads it from inside a pushed screen,
    /// the same "second reader reaches into `App`'s state via a shared
    /// mailbox" shape [`Self::presets`] itself already uses.
    presets_ready: Rc<RefCell<bool>>,
    /// The debug DSP override a finished `EQ END` session produced, if
    /// any -- [`Self::dsp_program`] returns this AHEAD of the editor
    /// preview and the connected device's assigned preset (bead
    /// description: "returns first"). Non-persisted: cleared by `EQ OFF`
    /// ([`Self::debug_eq_off`]) or a reboot (this field simply doesn't
    /// exist across one). Survives Bluetooth connect/disconnect cycles
    /// untouched -- nothing here folds on any [`Event`].
    debug_dsp_override: Option<EqApoOverride>,
    /// The page-0 Home telemetry snapshot's monotonic `snap_seq` counter
    /// (bead `pico-link-jyhk.3`, "ADA DESIGN" comment on `pico-link-jyhk.1`,
    /// section 3/4). `Cell`, not a plain field behind `&mut self`:
    /// [`Self::telemetry_snapshot`] takes `&self` deliberately (the
    /// design's Rust contract -- "the borrow is read-only... never touches
    /// dirty, damage or idle state" -- reads as *no caller-visible
    /// mutation*, and a private, monotonically-increasing wire counter
    /// with no read-back API is the one piece of state that needs to
    /// change on every poll regardless). Starts at `0`; the wire's own
    /// `0 == not ready` convention is instead realised by C never having
    /// called this function yet (its static reply buffer is zero-
    /// initialised), so the first successful call here already returns
    /// `1`, matching the design's "before the first generation... snap_seq
    /// `0`" note.
    telemetry_snap_seq: core::cell::Cell<u32>,
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
        let cushion_policy = Rc::new(RefCell::new(CushionPolicyState::default()));
        let abr_floor = Rc::new(RefCell::new(AbrFloorState::default()));
        let model: ModelHandle = Rc::new(RefCell::new(BtModel::default()));
        let presets = Rc::new(RefCell::new(PresetStore::new()));
        let editor_preset_id = Rc::new(RefCell::new(None));
        let editor_preview = Rc::new(RefCell::new(None));
        let import_focus = Rc::new(RefCell::new(None));
        // Starts `false` -- see this field's own doc comment for why.
        let presets_ready = Rc::new(RefCell::new(false));
        let navigator = Navigator::new(build_home_screen(
            &model,
            &home_face,
            &commands,
            &wizard_phase,
            Instant::from_micros(0),
            &display_settings,
            &cushion_policy,
            &abr_floor,
            &presets,
            &editor_preset_id,
            &editor_preview,
            &import_focus,
            &presets_ready,
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
            home_face,
            display_settings,
            cushion_policy,
            abr_floor,
            presets,
            editor_preview,
            eq_import_session: None,
            debug_dsp_override: None,
            import_focus,
            presets_ready,
            telemetry_snap_seq: core::cell::Cell::new(0),
        }
        // `editor_preset_id` is not stored on `Self` -- see its local
        // binding above. `App` never reads it back after construction
        // (bead `pico-link-ryw.14` removed the one call site that used to,
        // `on_preset_loaded`'s echo-adoption block); the `Rc` stays alive
        // for the app's lifetime purely because `HomeView` (built once,
        // never rebuilt) holds its own clone in the Effects-row activation
        // closure, the same way every other per-screen mailbox here does.
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

    /// The live global congestion-cushion policy -- bead pico-link-8pp1.4
    /// (S3). For a future Settings row/picker (S4) to read.
    #[must_use]
    pub fn cushion_policy(&self) -> CushionPolicy {
        self.cushion_policy.borrow().current
    }

    /// Seeds the live policy (from `Event::CushionPolicyLoaded`, C's
    /// `PL:S:1` boot load) -- sets `current` but deliberately does NOT
    /// mark it pending *save*: seeding is "here is what's already
    /// stored/defaulted," not a user edit, same discipline
    /// [`App::set_display_settings`] uses for its own seed.
    pub fn set_cushion_policy(&mut self, policy: CushionPolicy) {
        self.cushion_policy.borrow_mut().current = policy;
    }

    /// User-initiated pick (a future Settings picker, S4): sets `current`
    /// AND marks it pending *save* -- unlike
    /// [`App::set_display_settings`]/[`App::take_display_settings_to_apply`]'s
    /// pair, there is no "apply" latch here (see [`CushionPolicyState`]'s
    /// doc comment for why).
    pub fn request_cushion_policy(&mut self, policy: CushionPolicy) {
        let mut state = self.cushion_policy.borrow_mut();
        state.current = policy;
        state.save_pending = true;
    }

    /// Drains the "persist" latch -- `Some` at most once per user pick,
    /// consumed by `ui-ffi`'s `pl_ui_poll_command` (design D9, same shape
    /// [`App::take_display_settings_to_save`] uses).
    pub fn take_cushion_policy_to_save(&mut self) -> Option<CushionPolicy> {
        let mut state = self.cushion_policy.borrow_mut();
        if state.save_pending {
            state.save_pending = false;
            Some(state.current)
        } else {
            None
        }
    }

    /// The live global LDAC Adaptive floor -- bead pico-link-d42g.3 (F3).
    /// For a future Settings row/picker (F4) to read.
    #[must_use]
    pub fn abr_floor(&self) -> AbrFloor {
        self.abr_floor.borrow().current
    }

    /// Seeds the live floor (from `Event::AbrFloorLoaded`, C's `PL:S:2`
    /// boot load) -- sets `current` but deliberately does NOT mark it
    /// pending *save*: seeding is "here is what's already
    /// stored/defaulted," not a user edit, same discipline
    /// [`App::set_cushion_policy`] uses for its own seed.
    pub fn set_abr_floor(&mut self, floor: AbrFloor) {
        self.abr_floor.borrow_mut().current = floor;
    }

    /// User-initiated pick (a future Settings picker, F4): sets `current`
    /// AND marks it pending *save* -- same "no separate apply latch" shape
    /// as [`App::request_cushion_policy`], since `core` has nothing of its
    /// own to apply this to (C applies it live via
    /// `pl_codec_ldac_set_floor`).
    pub fn request_abr_floor(&mut self, floor: AbrFloor) {
        let mut state = self.abr_floor.borrow_mut();
        state.current = floor;
        state.save_pending = true;
    }

    /// Drains the "persist" latch -- `Some` at most once per user pick,
    /// consumed by `ui-ffi`'s `pl_ui_poll_command` (design D9, same shape
    /// [`App::take_cushion_policy_to_save`] uses).
    pub fn take_abr_floor_to_save(&mut self) -> Option<AbrFloor> {
        let mut state = self.abr_floor.borrow_mut();
        if state.save_pending {
            state.save_pending = false;
            Some(state.current)
        } else {
            None
        }
    }

    /// The DSP program the connected device's assigned preset compiles to
    /// at `fs_hz` -- bead `pico-link-ryw.5`, design sec 3.1's "active-preset
    /// resolution (connected device's `preset_id`, else Off)". `ui-ffi`'s
    /// pull API (`pl_ui_take_dsp_program`) calls this once per superloop
    /// iteration and hands the result to C over the FFI boundary; `core`
    /// never submits it itself (no Bluetooth/DSP engine of its own to
    /// submit to).
    ///
    /// Resolution order: no connected device, OR a connected device whose
    /// `preset_id` is [`crate::dsp::store::NO_PRESET_ID`]/dangling, both
    /// produce [`Program::off`] (Andreas's ruling: new/unassigned devices
    /// get Off) -- `core` never distinguishes "never assigned" from
    /// "assigned to a since-deleted preset," same as
    /// [`crate::dsp::PresetStore::resolve`]'s own doc comment.
    ///
    /// Bead `pico-link-ryw.11` adds a debug override AHEAD of everything
    /// below: if `EQ END` has produced an [`EqApoOverride`] (and it
    /// hasn't since been cleared by `EQ OFF`/reboot), it wins
    /// unconditionally -- even over an open DSP effects editor's live
    /// preview. It is checked first and returned early, same "checked
    /// first, returned early" shape the editor-preview paragraph below
    /// already documents for its own precedence over the connected
    /// device's stored assignment.
    ///
    /// design sec 5.1 overrides this whole resolution while a DSP effects
    /// editor is open (bead `pico-link-ryw.7`, review fix): rule 1 (not
    /// bypassed) previews [`App::editor_preview`]'s draft; rule 2
    /// (bypassed) previews Off; only rule 3 (no editor open) falls through
    /// to the connected-device resolution below. Checked first and
    /// returned early -- the editor's preview always wins over the
    /// connected device's stored assignment while open, independent of
    /// whether the draft has been saved yet.
    #[must_use]
    pub fn dsp_program(&self, fs_hz: u32) -> Program {
        if let Some(over) = self.debug_dsp_override.as_ref() {
            return over.to_program(fs_hz);
        }
        if let Some((draft, bypassed)) = self.editor_preview.borrow().as_ref() {
            return if *bypassed { Program::off(fs_hz) } else { Program::from_preset(draft, fs_hz) };
        }
        let model = self.model.borrow();
        let presets = self.presets.borrow();
        let preset = model
            .connected_addr
            .and_then(|addr| model.paired.iter().find(|d| d.addr == addr))
            .and_then(|device| presets.resolve(device.preset_id));
        match preset {
            Some(preset) => Program::from_preset(preset, fs_hz),
            None => Program::off(fs_hz),
        }
    }

    /// Encodes the page-0 Home telemetry snapshot for the web companion's
    /// `GET_TELEMETRY` poll (bead `pico-link-jyhk.3`, "ADA DESIGN" comment
    /// on `pico-link-jyhk.1`, sections 3-4; wire layout owned by
    /// [`telemetry::encode_home_snapshot`]). `page` selects which payload
    /// to build; only [`telemetry::TELEMETRY_PAGE_HOME`] (`0`) exists
    /// today (the design's "F3 diagnostics becomes page 1" note is a
    /// future payload, not yet built).
    ///
    /// Writes into `buf` and returns the number of bytes written -- always
    /// [`telemetry::HOME_SNAPSHOT_LEN`] on a successful page-0 encode.
    /// Returns `0` (and never partially writes `buf`) if `page` is
    /// unsupported or `buf` is too small to hold the whole snapshot; the
    /// caller (`ui-ffi`'s `pl_ui_telemetry`) treats `0` as "don't publish
    /// this poll," matching the design's C-side "not ready" handling.
    ///
    /// Takes `&self`, not `&mut self`: the design's Rust contract requires
    /// "the borrow is read-only... never touches dirty, damage or idle
    /// state." [`Self::telemetry_snap_seq`] is the one piece of state this
    /// call advances regardless, via a `Cell` -- see that field's doc
    /// comment for why an internal wire counter with no read-back API
    /// doesn't count as caller-visible mutation.
    #[must_use]
    pub fn telemetry_snapshot(&self, page: u8, buf: &mut [u8]) -> usize {
        if page != telemetry::TELEMETRY_PAGE_HOME {
            return 0;
        }
        if buf.len() < telemetry::HOME_SNAPSHOT_LEN {
            return 0;
        }

        // `0` is reserved for "not ready" (this field's doc comment); skip
        // it on the vanishingly unlikely `u32` wraparound rather than let
        // one poll in 2^32 read back as not-ready.
        let mut seq = self.telemetry_snap_seq.get().wrapping_add(1);
        if seq == 0 {
            seq = 1;
        }
        self.telemetry_snap_seq.set(seq);

        let now = Instant::from_micros(self.now_us);
        let model = self.model.borrow();
        let presets = self.presets.borrow();
        let bytes = telemetry::encode_home_snapshot(&model, &presets, now, seq);
        buf[..bytes.len()].copy_from_slice(&bytes);
        bytes.len()
    }

    /// `EQ BEGIN`: starts a fresh [`EqApoSession`], discarding any
    /// in-progress (never-`END`ed) one. Does NOT touch
    /// [`Self::debug_dsp_override`] -- the previously loaded override (if
    /// any) keeps playing until this new session successfully reaches
    /// `EQ END` (or `EQ OFF` clears it explicitly).
    pub fn debug_eq_begin(&mut self) {
        self.eq_import_session = Some(EqApoSession::new());
    }

    /// `EQ <line>`: feeds one Equalizer APO text line into the
    /// in-progress session. [`DebugEqCommandError::NotInSession`] if `EQ
    /// BEGIN` hasn't been called (or was already consumed by `EQ END`).
    pub fn debug_eq_line(&mut self, line: &str) -> Result<(), DebugEqCommandError> {
        let session = self.eq_import_session.as_mut().ok_or(DebugEqCommandError::NotInSession)?;
        session.feed_line(line).map_err(DebugEqCommandError::Line)
    }

    /// `EQ END`: finishes the in-progress session and, on success,
    /// installs its [`EqApoOverride`] as [`Self::debug_dsp_override`] --
    /// the next [`Self::dsp_program`] call picks it up immediately.
    /// [`DebugEqCommandError::NotInSession`] if `EQ BEGIN` hasn't been
    /// called. Leaves the previous override in place on failure (a
    /// malformed `EQ END` must not silently kill whatever was already
    /// playing).
    pub fn debug_eq_end(&mut self) -> Result<(), DebugEqCommandError> {
        let session = self.eq_import_session.take().ok_or(DebugEqCommandError::NotInSession)?;
        let doc = session.finish().map_err(DebugEqCommandError::Finish)?;
        self.debug_dsp_override = Some(EqApoOverride::from_parsed(doc));
        Ok(())
    }

    /// `EQ OFF`: drops any in-progress session and clears
    /// [`Self::debug_dsp_override`] -- [`Self::dsp_program`] falls back to
    /// whatever it would have resolved to otherwise (the open editor's
    /// preview, or the connected device's assigned preset). The only
    /// other way the override clears is a reboot.
    pub fn debug_eq_off(&mut self) {
        self.eq_import_session = None;
        self.debug_dsp_override = None;
    }

    /// The active override's band count and explicit preamp, for a debug
    /// status line (`EQ STATUS`/the post-`EQ END` log line) -- `None` if
    /// no override is currently active.
    #[must_use]
    pub fn debug_eq_override_info(&self) -> Option<(u8, f32)> {
        self.debug_dsp_override.as_ref().map(|over| (over.band_count(), over.preamp_db()))
    }

    /// Imports a whole Equalizer APO / `AutoEQ` document over the ITF-6
    /// config transport (bead `pico-link-ryw.12.4`, `crate::dsp::import`)
    /// -- distinct from [`Self::debug_eq_begin`]/[`Self::debug_eq_line`]/
    /// [`Self::debug_eq_end`]'s line-at-a-time debug console session: this
    /// is one whole-document call. `host_name` is the host-resolved
    /// fallback name (used only when `text` has no `Name:` line) -- see
    /// [`crate::dsp::import`]'s module doc for the full replace/suffix/
    /// reject duplicate-name policy this defers to.
    ///
    /// On success, queues exactly one [`Command::SavePreset`] IMMEDIATELY
    /// -- Andreas's ruling (`pico-link-ryw.12.2`'s review comment,
    /// restated on this bead): there is no on-device confirm, so
    /// `crate::dsp::import::import`'s in-memory-only mutation must be
    /// followed by a save in the SAME call, not deferred to a later user
    /// press. Also arms [`Self::import_focus`] with the resulting row's
    /// key, but ONLY while the effects list screen is currently on top of
    /// the navigator's stack (Uma's design, `ryw12-3-ux.md` sec 2:
    /// "Effects list on top: ... move FOCUS to the new/updated row.
    /// Anywhere else: nothing on screen") -- `EffectsListView::sync`
    /// consumes it the next time that screen syncs.
    ///
    /// # Errors
    ///
    /// Returns the first [`ImportError`] encountered, without touching
    /// [`Self::presets`] or queuing any [`Command`] -- see
    /// [`crate::dsp::import::import`]'s own doc comment for the exact
    /// "never partially mutates on an error path" guarantee this forwards.
    pub fn import_preset(&mut self, text: &str, host_name: &str) -> Result<(u16, ImportOutcome), ImportError> {
        // Bead `pico-link-ryw.14`, Ada's preset-id-allocation contract:
        // refuse to allocate before C's boot-time high-water mark has
        // arrived -- see `App::presets_ready`'s doc comment.
        if !*self.presets_ready.borrow() {
            return Err(ImportError::NotReady);
        }
        let outcome = {
            let mut presets = self.presets.borrow_mut();
            import_preset(&mut presets, text, host_name)
        }?;
        let (id, _) = outcome;

        let blob = self.presets.borrow().get(id).map(Preset::to_wire);
        if let Some(blob) = blob {
            self.commands.borrow_mut().push_back(Command::SavePreset { preset_id: id, blob: blob.to_vec() });
        }

        let effects_list_on_top = self.navigator.depth() > 0
            && self.navigator.id_at(self.navigator.depth() - 1) == Some(ScreenId::EffectsList);
        if effects_list_on_top {
            *self.import_focus.borrow_mut() = Some(screens::effects::effect_row_key(id));
        }

        self.mark_model_changed();
        Ok(outcome)
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
                Some(
                    ScreenId::DevicePage(addr)
                    | ScreenId::Picker(PickerKind::LdacQuality | PickerKind::Effect, addr),
                ) => !self.model.borrow().paired.iter().any(|d| d.addr == addr),
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
