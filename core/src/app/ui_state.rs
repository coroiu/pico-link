use crate::power::DisplaySettings;
use crate::render::Instant;

use super::events::{ConnectFailureReason, ConnectStep};

/// The pairing wizard's own phase state (design section 9 / bead
/// pico-link-znb.7 / E5), independent of [`BtModel`]: `BtModel` is "what
/// core knows about the Bluetooth link and discovered devices", while this
/// is "which of the wizard's six phases is currently on screen and that
/// phase's own local data" -- e.g. the scan list itself lives in
/// `BtModel::discovered` (read live, not duplicated here), but "the user is
/// on the not-responding screen, this is attempt 3" has no other home.
///
/// Deliberately carries its own `addr`/`reason` on the phases that need
/// them (`Connecting`/`NotResponding`/`Failed`) rather than re-reading
/// `BtModel::last_connect_failure` at render time: both are populated from
/// the exact same [`Event`] data, so there is no fidelity loss, and
/// keeping the wizard screen's rendering self-contained here avoids
/// needing to hand it a live reference to the whole `BtModel` just to read
/// one field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum WizardPhase {
    /// Phase 1 (formerly "instructions", now removed -- pico-link-4vb.2:
    /// Andreas wanted the wizard to open straight into scanning rather
    /// than requiring an A press first). Scanning (10.24s, C-timed)
    /// and/or showing whatever's accumulated in `BtModel::discovered` so far
    /// -- this phase covers both "still actively scanning" and "scan
    /// finished, results on screen, user is choosing one", since nothing
    /// about the rendered content differs between them. `started` is when
    /// the scan began -- see the frame-scoped clock ADR's "event
    /// timestamps" note: a timestamp is domain state and belongs in the
    /// model, current time is not. Widget code (`render::wizard`) has no
    /// clock of its own, so a freshly entered `Scanning` phase carries
    /// [`PENDING_TIMESTAMP`] until [`App`]'s own `now_us` backfills it --
    /// see [`WizardPhase::scanning_pending`].
    Scanning { started: Instant },
    /// Phase 3 (default): the scan ended (a C `LinkStateChanged(Idle)`
    /// event while this phase was `Scanning`) with zero devices found.
    /// Also the wizard's closed/not-yet-opened placeholder value -- see
    /// this variant's use as `#[default]`: opening the wizard always sets
    /// [`WizardPhase::scanning_pending`] explicitly (`build_devices_
    /// screen`'s `on_activate_index`), so this default is never actually
    /// rendered as "nothing found" for a fresh wizard.
    #[default]
    NothingFound,
    /// Phase 4: connecting, at the named sub-`step` currently in
    /// progress. `addr` is carried so a subsequent retry (phase 5's "keep
    /// trying") knows which device to reissue [`Command::Connect`] for.
    /// `started` is when this connect attempt began -- same
    /// pending-then-backfilled shape as `Scanning`'s own `started`, via
    /// [`WizardPhase::connecting_pending`].
    Connecting { addr: [u8; 6], step: ConnectStep, started: Instant },
    /// Phase 5: surfaced once a connect attempt has gone unanswered for
    /// ~6s (a C-timed threshold -- core never decides this itself, it
    /// only renders whatever [`Event::ConnectRetrying`] reports).
    /// `attempt` is the free liveness counter design section 9 calls for.
    NotResponding { addr: [u8; 6], attempt: u16 },
    /// Phase 6, success outcome. `degraded` selects between the two
    /// outcomes design section 9 draws a hard line between: plain success
    /// (auto-dismisses via [`Event::WizardAutoDismiss`]) and degraded
    /// success (requires acknowledgement, never auto-dismisses).
    Succeeded { degraded: bool },
    /// Phase 6, failure outcome. `reason` selects which of the five named
    /// messages/remedies (design section 9's table) to show, and whether
    /// a retry is offered at all (`reason.retryable()`).
    Failed { addr: [u8; 6], reason: ConnectFailureReason },
}

/// Sentinel `started` value a widget-driven `WizardPhase` transition uses
/// when it has no clock to stamp itself with. `render::wizard`'s
/// `PairingWizardView` enters `Scanning`/`Connecting` directly from
/// `on_focus`/`on_intent` (a d-pad press, not a Bluetooth [`Event`]), and
/// neither of those gets a [`crate::render::RenderCtx`] -- deliberately:
/// `Widget::render` is the only place time enters a widget, precisely so
/// `render` stays a pure function of state/area/time (see the frame-scoped
/// clock ADR) and mutating a timestamp from inside `render` would violate
/// that. So the widget stamps `PENDING_TIMESTAMP` instead, and [`App`]
/// (which does have `now_us`) backfills the real value immediately after
/// dispatch -- see [`App::stamp_pending_wizard_timestamp`], called from
/// both [`App::handle_input`] and [`App::handle_event`] since either path
/// can produce a freshly entered phase.
///
/// `u64::MAX` rather than `0`: a genuinely-zero real timestamp (`now_us`
/// before the very first tick, which every test in this module starts
/// from) must never be confused with "not yet stamped".
pub(in crate::app) const PENDING_TIMESTAMP: Instant = Instant::from_micros(u64::MAX);

impl WizardPhase {
    /// Constructs a fresh `Scanning` phase with a not-yet-stamped
    /// `started` -- see [`PENDING_TIMESTAMP`]'s doc comment.
    pub(crate) fn scanning_pending() -> Self {
        Self::Scanning { started: PENDING_TIMESTAMP }
    }

    /// Constructs a fresh `Connecting` phase with a not-yet-stamped
    /// `started` -- see [`PENDING_TIMESTAMP`]'s doc comment.
    pub(crate) fn connecting_pending(addr: [u8; 6], step: ConnectStep) -> Self {
        Self::Connecting { addr, step, started: PENDING_TIMESTAMP }
    }
}

/// Home's two faces (design section 4's Home exception, section 7 --
/// bead `pico-link-znb.8`/E7). A **face**, not a pushed screen: Home is
/// [`crate::render::Navigator`] depth 1 (design's "depth 0") on both
/// faces, never depth 2, which is what keeps `B, B` a reliable escape
/// from anywhere in the app (see [`build_home_screen`]'s module-level
/// doc comment for the full argument). Lives in an `Rc<RefCell<_>>`
/// shared with the `HomeView` widget instance the same way
/// [`WizardPhase`] does -- see [`App::home_face`]'s doc comment for why
/// that indirection is required (a fresh `HomeView` is constructed on
/// every [`App::rebuild_root`], and the face must survive that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HomeFace {
    /// The hero/status display -- design section 6.
    #[default]
    Status,
    /// The two-row Bluetooth/Settings menu -- design section 7.
    Menu,
}

/// The mailbox `App` shares with the Settings screen and its two pickers --
/// the same `Rc<RefCell<_>>` shape `home_face`/`wizard_phase` use, since
/// `Action::PushView`'s builder closures are `FnOnce` with no path back to
/// a live `&mut App`.
///
/// Three independent flags, deliberately NOT folded into one "dirty" bit:
/// `apply_pending` (core's own `IdlePolicy` needs the new value applied
/// next `Runner::step`/`pl_ui_tick`) and `save_pending` (the value needs
/// persisting) fire together on a user pick but NOT on
/// [`App::set_display_settings`]'s initial seed from a loaded/default
/// value (D9: seeding must never re-save what was just loaded).
/// `refresh_pending` is a third, purely presentational latch: the Settings
/// screen's rows and the open picker's checkmark must reflect the pick on
/// the very same frame (design S6), which needs `App::refresh_stack`, not
/// anything `Runner`/`pl_ui_tick` does.
#[derive(Default)]
pub struct DisplaySettingsState {
    pub(in crate::app) current: DisplaySettings,
    pub(in crate::app) apply_pending: bool,
    pub(in crate::app) save_pending: bool,
    pub(in crate::app) refresh_pending: bool,
}
