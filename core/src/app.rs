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

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::input::NavIntent;
use crate::render::home::build_home_screen;
use crate::render::wizard::build_wizard_screen;
use crate::render::{Action, FrameBuffer565, Instant, ListItem, ListItemKey, Navigator, RenderCtx, Screen, VerticalList};

/// The devices screen's "Scan for headphones" row's identity key. Not
/// backed by a `DeviceAddr` (it isn't a device), so it's a fixed sentinel
/// instead — see [`ListItemKey::from`]'s doc comment for why this can
/// never collide with a real device's key (a device key's top two bytes
/// are always `0`; this sentinel's are always `0xFF`).
const SCAN_ROW_KEY: ListItemKey = ListItemKey::from_bytes([0xFF; 8]);

/// The Bluetooth link's coarse lifecycle state, as reported by C over
/// [`App::set_link_state`] (`pl_ui_set_link_state` in the FFI surface).
/// Platform-free: `core` has no idea BTstack exists, it only knows these
/// four labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkState {
    #[default]
    Idle,
    Scanning,
    Connecting,
    Connected,
}

impl LinkState {
    fn label(self) -> &'static str {
        match self {
            LinkState::Idle => "Idle",
            LinkState::Scanning => "Scanning...",
            LinkState::Connecting => "Connecting...",
            LinkState::Connected => "Connected",
        }
    }
}

/// One discovered Bluetooth device, as reported by C over
/// [`Event::DeviceDiscovered`] (`pl_ui_push_event` in the FFI surface).
/// `addr` is a 6-byte Bluetooth device address, big-endian as BTstack itself
/// reports it -- `core` never interprets the bytes, only round-trips them
/// back out via [`Command::Connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    pub addr: [u8; 6],
    pub name: String,
    pub rssi: i8,
}

/// A user-initiated action queued by the devices screen for C to poll via
/// [`App::poll_command`] (`pl_ui_poll_command` in the FFI surface). `core`
/// never acts on these itself -- it has no Bluetooth stack to act with --
/// it only records "the user asked for this" and hands it back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    StartScan,
    Connect { addr: [u8; 6] },
    /// User-initiated: stop an in-flight inquiry scan. The wizard screen
    /// (pico-link-znb.7) binds this to B once it exists -- this bead only
    /// delivers the command itself, no binding. Needed because the inquiry
    /// scan runs a fixed 10.24s with nothing else to interrupt it: without
    /// this, B does nothing during that window and, in the zero-results
    /// case, the screen is empty for the whole 10.24s -- exactly when a
    /// user reaches for a button, gets nothing, and concludes the device
    /// is frozen. See design section 21 Tier 1 row E1.
    CancelScan,
    /// User-initiated: abort an in-flight connect attempt (phase 4
    /// `Connecting` or phase 5 `NotResponding`, design section 9). Added
    /// by pico-link-znb.7's code-review fix -- the wizard's B previously
    /// only left the *screen*, leaving C's ACL/SSP/AVDTP attempt running
    /// with nothing telling it to stop, still delivering
    /// `ConnectStepChanged`/`ConnectRetrying`/`ConnectFailed`/
    /// `ConnectSucceeded` events for an attempt the user already walked
    /// away from. Whether C actually implements the abort (real
    /// ACL/AVDTP teardown) or this stays plumbed-but-unhandled is a
    /// separate, C-side decision -- see this bead's completion report;
    /// `CancelScan` similarly needed its own follow-up bead
    /// (pico-link-znb.2) for its C-side handling.
    CancelConnect { addr: [u8; 6] },
}

/// Why a connect attempt failed, as reported by C over
/// [`Event::ConnectFailed`]. `core` has no Bluetooth stack of its own -- it
/// only records the *category* of failure BTstack/the radio reported, for a
/// (future) screen to render with an appropriate remedy.
///
/// Distinguishing these five (rather than a single generic "failed") is
/// deliberate and driven by the approved on-device UI design
/// (`.planning/design/2026-08-28-on-device-ui.md`): each has a different
/// user-facing remedy, and [`ConnectFailureReason::retryable`] draws the one
/// distinction that matters most -- some failures are worth an automatic or
/// user-initiated retry, and two structurally are not (see that method's
/// doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFailureReason {
    /// No response within the connection's page-timeout window (up to
    /// 5.12s per BTstack's ACL page timeout) -- transient, worth retrying.
    Timeout,
    /// The remote device actively rejected the connection or pairing
    /// (authentication failure, user declined on the headphone side, ...)
    /// -- may be worth retrying if the user acts differently (e.g.
    /// re-enters pairing mode), so not ruled out here.
    Rejected,
    /// AVDTP stream endpoint discovery found no A2DP sink service on the
    /// device at all. This is a fixed capability of the remote device, not
    /// a transient condition -- retrying the *same* device can never
    /// succeed, hence [`ConnectFailureReason::retryable`] is `false`.
    NoA2dpSink,
    /// Pairing requires a PIN, and this product has no on-screen text
    /// entry (confirmed speculative/out of scope -- see the capability
    /// inventory on bead pico-link-aii.1). Retrying without a way to
    /// supply the PIN can never succeed either, so this is also
    /// non-retryable until text entry exists.
    NeedsPin,
    /// The radio/HCI layer itself reported an error (not a per-device
    /// remote-side rejection) -- typically transient, worth retrying.
    RadioError,
}

impl ConnectFailureReason {
    /// Whether a retry of the *same* device could plausibly succeed.
    /// `false` for [`Self::NoA2dpSink`] (a fixed capability of that
    /// device -- it will never grow an A2DP sink between attempts) and
    /// [`Self::NeedsPin`] (this product cannot supply a PIN today, so
    /// nothing about a retry changes the outcome). The other three are
    /// conditions that can plausibly differ on a second attempt.
    #[must_use]
    pub fn retryable(self) -> bool {
        !matches!(self, Self::NoA2dpSink | Self::NeedsPin)
    }
}

/// One inbound Bluetooth-domain event, as reported by C over
/// [`App::handle_event`] (`pl_ui_push_event` in the FFI surface -- one
/// tagged union in, replacing the old per-field `pl_ui_set_link_state`/
/// `pl_ui_add_device`/`pl_ui_clear_devices` setters). `core` never
/// originates these; it only folds them into [`BtModel`] and marks the app
/// dirty -- see [`App::handle_event`].
///
/// The last four variants were added by pico-link-znb.7 (E5, the pairing
/// wizard) -- the design's own "WHAT IS MISSING AND MUST BE ADDED"
/// paragraph names exactly these three gaps (phase-4 sub-steps, the
/// phase-5 retry/attempt counter, the phase-6 degraded-success outcome)
/// plus the C-side auto-dismiss timer (design section 9 phase 6 / section
/// 14's C9). Purely additive: every existing tag/payload is unchanged, so
/// [`crate::app`]'s `PL_EVENT_ABI_VERSION`-equivalent guard in `ui-ffi`
/// does **not** need a bump -- see that crate's module doc for the
/// version-guard rule this follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    LinkStateChanged(LinkState),
    DeviceDiscovered(DeviceEntry),
    DevicesCleared,
    ConnectFailed { addr: [u8; 6], reason: ConnectFailureReason },
    /// One of phase 4's four named sub-steps has begun (ACL connect,
    /// SSP/link-key pairing, AVDTP stream setup, then codec negotiation --
    /// see [`ConnectStep`]). Only meaningful while the wizard's phase is
    /// [`WizardPhase::Connecting`] or [`WizardPhase::NotResponding`] (a
    /// stray event outside that window -- e.g. arriving after the user
    /// backed out -- is silently ignored; see
    /// [`App::on_connect_step_changed`]).
    ConnectStepChanged(ConnectStep),
    /// C is about to retry the in-flight connect attempt after the ~6s
    /// "not responding" surfacing point (design section 9 phase 5) --
    /// carries the attempt number so the wizard's "Still trying (N)"
    /// counter has something to increment. Like
    /// [`Event::ConnectStepChanged`], a no-op outside the
    /// `Connecting`/`NotResponding` phases.
    ConnectRetrying { attempt: u16 },
    /// The in-flight connect attempt succeeded. `degraded` distinguishes
    /// phase 6's two outcomes: plain success (auto-dismisses) vs degraded
    /// success (does not -- see [`Event::WizardAutoDismiss`]'s doc
    /// comment for why that distinction matters enough to be its own
    /// event rather than inferred from `LinkStateChanged(Connected)`
    /// alone, which carries no fallback information).
    ConnectSucceeded { degraded: bool },
    /// C's own ~2s timer firing (design section 9 phase 6, section 14's
    /// C9 "auto-dismiss timer event" -- explicitly *not* a core-owned
    /// clock feature, see [`crate::app`]'s `now_us`/`tick` doc comments
    /// for why timers stay on the C side of this seam). Pops the wizard
    /// back to Devices **only** if the wizard is currently showing a
    /// plain (non-degraded) success -- degraded success requires
    /// acknowledgement and must never auto-dismiss (design section 9
    /// phase 6), and this event arriving at any other time (a stray/late
    /// timer, the user having already backed out) is a defensive no-op.
    WizardAutoDismiss,
    /// The live A2DP link's codec finished negotiating (or renegotiated),
    /// as reported by C's signaling codec-configuration handler
    /// (`firmware/src/a2dp.c`) -- never the media timer path. Folds into
    /// [`BtModel::connected_codec`]; see [`ConnectedCodec`]'s doc comment
    /// for why `core` never derives the name/bitrate itself. Added by
    /// bead pico-link-1v5, closing the Home hero's hardcoded `NO LINK`.
    CodecChanged(ConnectedCodec),
}

/// Phase 4's four named connect sub-steps (design section 9): naming the
/// current one tells the user *and us* where a stalled connect attempt
/// actually got stuck, which a single generic "Connecting..." spinner
/// cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// ACL connect.
    Connecting,
    /// SSP / link-key pairing.
    Pairing,
    /// AVDTP stream endpoint discovery + configuration.
    SettingUpAudio,
    /// Codec negotiation.
    NegotiatingCodec,
}

impl ConnectStep {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ConnectStep::Connecting => "Connecting",
            ConnectStep::Pairing => "Pairing",
            ConnectStep::SettingUpAudio => "Setting up audio",
            ConnectStep::NegotiatingCodec => "Negotiating codec",
        }
    }

    /// The fixed display order phase 4 always shows the four steps in --
    /// design section 9's "1 Connecting ... 2 Pairing ... 3 Setting up
    /// audio ... 4 Negotiating codec".
    #[must_use]
    pub fn all() -> [ConnectStep; 4] {
        [ConnectStep::Connecting, ConnectStep::Pairing, ConnectStep::SettingUpAudio, ConnectStep::NegotiatingCodec]
    }
}

/// The pairing wizard's own phase state (design section 9 / bead
/// pico-link-znb.7 / E5), independent of [`BtModel`]: `BtModel` is "what
/// core knows about the Bluetooth link and discovered devices", while this
/// is "which of the wizard's six phases is currently on screen and that
/// phase's own local data" -- e.g. the scan list itself lives in
/// `BtModel::devices` (read live, not duplicated here), but "the user is
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
    /// Phase 1: instructions, before any scanning. No timer, nothing
    /// moving -- see the design's "the user is the one doing work" note.
    #[default]
    Instructions,
    /// Phase 2: scanning (10.24s, C-timed) and/or showing whatever's
    /// accumulated in `BtModel::devices` so far -- this phase covers both
    /// "still actively scanning" and "scan finished, results on screen,
    /// user is choosing one", since nothing about the rendered content
    /// differs between them. `started` is when the scan began -- see the
    /// frame-scoped clock ADR's "event timestamps" note: a timestamp is
    /// domain state and belongs in the model, current time is not. Widget
    /// code (`render::wizard`) has no clock of its own, so a freshly
    /// entered `Scanning` phase carries [`PENDING_TIMESTAMP`] until
    /// [`App`]'s own `now_us` backfills it -- see [`WizardPhase::
    /// scanning_pending`].
    Scanning { started: Instant },
    /// Phase 3: the scan ended (a C `LinkStateChanged(Idle)` event while
    /// this phase was `Scanning`) with zero devices found.
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
const PENDING_TIMESTAMP: Instant = Instant::from_micros(u64::MAX);

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

/// The Bluetooth-domain state screens read to render themselves --
/// everything [`App`] knows about the link and the discovered/attempted
/// devices, folded in one place from [`Event`]s. Kept as one struct (rather
/// than loose fields on [`App`]) so it's unambiguous what "the model" means
/// when a screen-building function takes `&BtModel`: this, and only this, is
/// live application data; everything else a screen needs is either passed
/// in explicitly (e.g. a carried-forward selection index) or is the
/// screen's own widget state.
///
/// Deliberately grows by adding fields here, not by adding new `App`
/// methods per field or new FFI setters per field -- see the module doc's
/// "sustainable path" rationale (bead pico-link-a67 / pico-link-aii.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BtModel {
    pub link_state: LinkState,
    pub devices: Vec<DeviceEntry>,
    /// The most recent connect failure, if any (and not yet superseded by
    /// a new attempt). Not yet rendered by any screen in this bead's scope
    /// -- populated so the data exists and is representable ahead of the
    /// screen that will read it, per pico-link-a67's explicit ask.
    pub last_connect_failure: Option<(DeviceAddr, ConnectFailureReason)>,
    /// The currently-negotiated codec on the live A2DP link, if any --
    /// `None` whenever there is no connected codec to show (design section
    /// 15: absent, never frozen or faked). Populated from
    /// [`Event::CodecChanged`] (C's signaling codec-configuration handler,
    /// `firmware/src/a2dp.c`) and cleared by [`App::set_link_state`]
    /// whenever the link leaves [`LinkState::Connected`] -- see that
    /// method's doc comment for why clearing keys off the *link state*
    /// rather than a dedicated disconnect event (bead pico-link-1v5).
    pub connected_codec: Option<ConnectedCodec>,
}

/// A Bluetooth device address, aliased for readability at call sites that
/// pair it with a [`ConnectFailureReason`].
pub type DeviceAddr = [u8; 6];

/// The live A2DP link's negotiated codec, as reported by C over
/// [`Event::CodecChanged`] (`pl_ui_push_event` in the FFI surface, fired
/// from `firmware/src/a2dp.c`'s signaling codec-configuration handler --
/// never from the media timer path, which must never call into `core`).
///
/// `word` and `nominal_bitrate_bps` are exactly the two fields
/// `firmware/src/codec_table.h`'s `pl_codec_t`/`pl_codec_frame_info_t`
/// already carry per row (`display_name`, `nominal_bitrate_bps`) -- `core`
/// never derives a codec's display name or bitrate itself, it only
/// displays whatever the one C-side codec table (Andreas's ruling: a
/// table, never a per-call-site branch on codec identity) already decided.
/// `nominal_bitrate_bps` is deliberately the table's *nominal* figure, not
/// a live/adaptive one -- design section 13 confirms only the nominal
/// number, and section 15's "absent, never faked" rule means a live figure
/// this product cannot honestly measure yet must not be synthesized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedCodec {
    /// Which device this codec applies to. Carried for forward
    /// compatibility (a future per-device correlation); today's clearing
    /// path (see [`App::set_link_state`]) does not key off it.
    pub addr: DeviceAddr,
    /// The codec's display name, exactly as the C-side codec table's
    /// `display_name` reads (e.g. "SBC", "LDAC") -- this *is* the hero
    /// word [`crate::render::CodecStatus::Connected::word`] renders.
    pub word: String,
    /// The codec table row's nominal bitrate, in bits per second (the FFI
    /// payload's native unit -- converted to kbps only at render time, see
    /// `render/home.rs`).
    pub nominal_bitrate_bps: u32,
}

/// Builds the devices screen: a "Scan for headphones" row (its sublabel is
/// the live [`LinkState`] label) followed by one row per discovered
/// [`DeviceEntry`]. Selecting the scan row queues [`Command::StartScan`];
/// selecting a device row queues [`Command::Connect`] with that device's
/// address. Rebuilt from scratch on every state change (see
/// [`App::rebuild_root`]) rather than mutated in place -- simplest correct
/// thing for a list this small, and it keeps the closures below trivially
/// `'static` (each rebuild captures a fresh, owned snapshot).
///
/// Every row carries a [`ListItemKey`] -- [`SCAN_ROW_KEY`] for the fixed
/// scan row, `device.addr` (via `From<[u8; 6]>`) for a device row -- so
/// `prev_key`/`prev_index` (the outgoing screen's
/// [`Navigator::root_selected_key`]/[`Navigator::root_selected_index`])
/// can carry the user's selection forward **by identity** through
/// [`VerticalList::with_selected_identity`]: a device arriving, being
/// renamed in place, or a stale one dropping out of `model.devices` no
/// longer moves the selection just because the *index* it used to occupy
/// now means something else. `prev_key` of `None`/not-found falls back to
/// clamping `prev_index` -- see that method's doc comment for the exact
/// rule. `(None, 0)` (first build) starts at row 0.
/// The Devices screen's fixed title -- previously "Pico Link" from back
/// when this screen was the navigator root (pre-`pico-link-znb.8`/E7);
/// renamed now that it's reached by "A"/the menu face's "Bluetooth" row
/// **from** Home, which owns the "Pico Link" brand title instead (design
/// section 5's screen inventory). Also doubles as
/// [`App::rebuild_root`]'s way of checking "is the screen currently
/// sitting at stack index 1 the Devices screen" before refreshing it via
/// [`crate::render::Navigator::replace_at`] -- the same pragmatic,
/// title-string-as-identity approach [`crate::render::wizard::
/// WIZARD_TITLE`] already uses one level up.
pub(crate) const DEVICES_TITLE: &str = "Devices";

pub(crate) fn build_devices_screen(
    model: &BtModel,
    prev_key: Option<ListItemKey>,
    prev_index: usize,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    wizard_phase: &Rc<RefCell<WizardPhase>>,
    wizard_devices: &Rc<RefCell<Vec<DeviceEntry>>>,
) -> Screen {
    let mut items =
        vec![ListItem::new("Scan for headphones").with_sublabel(model.link_state.label()).with_key(SCAN_ROW_KEY)];
    if model.devices.is_empty() {
        // A single-row list has nowhere for Up/Down to move the selection
        // to, which also reads as a dead screen to a first-time user --
        // an explicit "nothing found yet" row keeps the list navigable
        // and communicates the empty state instead of just looking inert.
        // No key: it's a transient placeholder, not a persistent entity
        // worth carrying a selection onto.
        items.push(ListItem::new("No devices found").with_sublabel("Select Scan to search"));
    }
    for device in &model.devices {
        let label = if device.name.is_empty() { String::from("(unknown device)") } else { device.name.clone() };
        let sublabel = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}  RSSI {}",
            device.addr[0], device.addr[1], device.addr[2], device.addr[3], device.addr[4], device.addr[5], device.rssi
        );
        items.push(ListItem::new(label).with_sublabel(sublabel).with_key(ListItemKey::from(device.addr)));
    }

    let devices_snapshot: Vec<DeviceEntry> = model.devices.clone();
    let commands_for_activate = Rc::clone(commands);
    let wizard_phase_for_activate = Rc::clone(wizard_phase);
    let wizard_devices_for_activate = Rc::clone(wizard_devices);
    let list = VerticalList::new(items)
        .on_activate_index(move |index| {
            if index == 0 {
                // pico-link-znb.7 (E5): selecting "Scan" no longer queues
                // `Command::StartScan` directly -- it opens the pairing
                // wizard at phase 1 (instructions), which is what actually
                // starts the scan once the user presses A there. Resetting
                // `wizard_phase`/`wizard_devices` here (rather than only
                // when the wizard widget itself is constructed) covers
                // re-opening the wizard after a previous session ended in
                // a terminal phase (Succeeded/Failed/NothingFound) --
                // without this the wizard would briefly flash its last
                // outcome before the user does anything.
                *wizard_phase_for_activate.borrow_mut() = WizardPhase::Instructions;
                wizard_devices_for_activate.borrow_mut().clear();
                let phase = Rc::clone(&wizard_phase_for_activate);
                let devices = Rc::clone(&wizard_devices_for_activate);
                let commands = Rc::clone(&commands_for_activate);
                return Action::PushView(Box::new(move || build_wizard_screen(phase, devices, commands)));
            } else if let Some(device) = devices_snapshot.get(index - 1) {
                commands_for_activate.borrow_mut().push_back(Command::Connect { addr: device.addr });
            }
            Action::None
        })
        .with_selected_identity(prev_key, prev_index);
    Screen::new(DEVICES_TITLE, vec![Box::new(list)])
}

/// The Settings screen's fixed title. Placeholder content only (no rows)
/// -- this bead (`pico-link-znb.8`/E7) exists to give Home's "Y"/menu-face
/// "Settings" row a real, reachable destination so that affordance isn't
/// labelled-but-broken (design section 4 rule 2), not to build Settings'
/// actual content, which is separate, not-yet-scheduled work (the design's
/// screen inventory lists it; persistence is Tier 2 E15). An empty
/// [`Screen`] is a fully supported, already-tested shape --
/// `Screen::new(title, vec![])` is exactly what
/// `App::push_screen_for_test` uses today.
pub(crate) const SETTINGS_TITLE: &str = "Settings";

pub(crate) fn build_settings_screen() -> Screen {
    Screen::new(SETTINGS_TITLE, vec![])
}

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
    model: BtModel,
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
    /// A live mirror of `model.devices`, shared with the wizard widget the
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
        let model = BtModel::default();
        let navigator =
            Navigator::new(build_home_screen(&model, &home_face, &commands, &wizard_phase, &wizard_devices));
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
        }
    }

    /// Refreshes the root (Home) screen to reflect the current
    /// [`BtModel`], via [`Navigator::replace_root`] -- **not**
    /// `Navigator::new`. That distinction is the whole point: replacing
    /// only the root leaves any screen the user has navigated *to* (pushed
    /// above root) untouched -- same stack depth, same screen instance,
    /// same focus/selection state on it -- and carries the outgoing root
    /// screen's own selection forward via
    /// [`Navigator::root_selected_index`] rather than resetting it to row
    /// 0. Previously this called `Navigator::new`, discarding the whole
    /// stack on every Bluetooth event -- invisible with the one screen this
    /// crate builds today, fatal for the approved multi-screen design (see
    /// pico-link-a67 / pico-link-aii.1's defect 1).
    ///
    /// Since `pico-link-znb.8` (E7) also refreshes the Devices screen, if
    /// one happens to be sitting at stack index 1 (identified by
    /// [`DEVICES_TITLE`], the same pragmatic title-as-identity approach
    /// the wizard already uses) -- **not** because Devices is scoped to
    /// stay live-synced while browsed (design section 8's real "paired
    /// device list" doesn't exist until `pico-link-a67`'s successor, E14),
    /// but because Devices already showed exactly this
    /// discovered-during-scan data when it *was* the root, and preserving
    /// that (rather than silently regressing it) costs one more
    /// `Navigator::replace_at` call.
    fn rebuild_root(&mut self) {
        // Home's own top-level widget (`HomeView`) carries no
        // `ListItemKey`/index selection concept the way the old
        // devices-as-root screen did -- which face is showing lives in
        // `self.home_face` (read fresh by the freshly built `HomeView`
        // below), and the menu face's two-row selection is minor enough
        // (fixed content, two items) not to need identity-based
        // carry-forward across a live Bluetooth event. So, unlike the
        // pre-E7 version of this method, there is no `root_selected_key`/
        // `root_selected_index` to thread through here for Home itself.
        self.navigator.replace_root(build_home_screen(
            &self.model,
            &self.home_face,
            &self.commands,
            &self.wizard_phase,
            &self.wizard_devices,
        ));

        if self.navigator.title_at(1) == Some(DEVICES_TITLE) {
            let devices_prev_key = self.navigator.selected_key_at(1);
            let devices_prev_index = self.navigator.selected_index_at(1).unwrap_or(0);
            self.navigator.replace_at(
                1,
                build_devices_screen(
                    &self.model,
                    devices_prev_key,
                    devices_prev_index,
                    &self.commands,
                    &self.wizard_phase,
                    &self.wizard_devices,
                ),
            );
        }
        self.dirty = true;
    }

    /// Folds one inbound Bluetooth-domain [`Event`] into [`BtModel`] and
    /// refreshes the root screen (`pl_ui_push_event`'s core-side
    /// implementation -- the single entry point replacing the old
    /// `set_link_state`/`add_device`/`clear_devices` setter trio). `core`
    /// never acts on these itself -- it has no Bluetooth stack -- it only
    /// updates what screens read.
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::LinkStateChanged(state) => {
                self.set_link_state(state);
                self.on_scan_ended_if_applicable(state);
            }
            Event::DeviceDiscovered(device) => self.add_device(device.addr, device.name, device.rssi),
            Event::DevicesCleared => self.clear_devices(),
            Event::ConnectFailed { addr, reason } => self.record_connect_failure(addr, reason),
            Event::ConnectStepChanged(step) => self.on_connect_step_changed(step),
            Event::ConnectRetrying { attempt } => self.on_connect_retrying(attempt),
            Event::ConnectSucceeded { degraded } => self.on_connect_succeeded(degraded),
            Event::WizardAutoDismiss => self.on_wizard_auto_dismiss(),
            Event::CodecChanged(codec) => self.set_connected_codec(codec),
        }
        self.stamp_pending_wizard_timestamp();
    }

    /// Backfills [`WizardPhase::Scanning`]/[`WizardPhase::Connecting`]'s
    /// `started` field with the real current time (`self.now_us`) if it's
    /// still [`PENDING_TIMESTAMP`] -- see that constant's doc comment for
    /// why the widget itself can't do this. Called after every
    /// [`App::handle_input`] and [`App::handle_event`], since either path
    /// can produce a freshly entered phase (`handle_input` for a direct
    /// d-pad-driven transition, `handle_event` for the `NotResponding` ->
    /// `Connecting` retry-succeeding case in [`App::on_connect_step_
    /// changed`]). A no-op whenever the current phase isn't pending (the
    /// overwhelmingly common case), or isn't `Scanning`/`Connecting` at
    /// all.
    fn stamp_pending_wizard_timestamp(&mut self) {
        let mut phase = self.wizard_phase.borrow_mut();
        match &mut *phase {
            WizardPhase::Scanning { started } | WizardPhase::Connecting { started, .. } if *started == PENDING_TIMESTAMP => {
                *started = Instant::from_micros(self.now_us);
            }
            _ => {}
        }
    }

    /// Phase 2 -> phase 3 transition (design section 9): when the scan
    /// ends (`link_state` reporting [`LinkState::Idle`] after having been
    /// [`LinkState::Scanning`]) while the wizard is still on
    /// [`WizardPhase::Scanning`] and nothing was found, moves it to
    /// [`WizardPhase::NothingFound`]. A no-op in every other case --
    /// devices *were* found (the phase just stays `Scanning`, now showing
    /// a selectable list instead of an actively-filling one -- design
    /// section 9 draws no rendering distinction between those), the
    /// wizard isn't open, or it's already past phase 2 (e.g. the user
    /// already selected a device and moved on to phase 4 before this
    /// event arrived).
    fn on_scan_ended_if_applicable(&mut self, state: LinkState) {
        if state != LinkState::Idle {
            return;
        }
        let mut phase = self.wizard_phase.borrow_mut();
        if matches!(*phase, WizardPhase::Scanning { .. }) && self.wizard_devices.borrow().is_empty() {
            *phase = WizardPhase::NothingFound;
            drop(phase);
            self.dirty = true;
        }
    }

    /// Folds one [`Event::ConnectStepChanged`] into [`WizardPhase`] --
    /// only meaningful while the wizard is mid-connect (`Connecting` or
    /// already surfaced as `NotResponding`, e.g. the step name changing
    /// right as a retry succeeds); silently ignored otherwise, per
    /// [`Event::ConnectStepChanged`]'s doc comment.
    fn on_connect_step_changed(&mut self, step: ConnectStep) {
        let mut phase = self.wizard_phase.borrow_mut();
        match &*phase {
            // Already `Connecting`: this is the same connect attempt
            // continuing, so its `started` carries forward unchanged.
            WizardPhase::Connecting { addr, started, .. } => {
                *phase = WizardPhase::Connecting { addr: *addr, step, started: *started };
            }
            // Coming back from `NotResponding` (which carries no
            // `started` of its own): the original attempt's start time is
            // gone, so this re-enters as pending, same as a brand-new
            // connect -- `stamp_pending_wizard_timestamp` (called by
            // `handle_event` right after this) backfills it.
            WizardPhase::NotResponding { addr, .. } => {
                *phase = WizardPhase::connecting_pending(*addr, step);
            }
            _ => {}
        }
        drop(phase);
        self.dirty = true;
    }

    /// Folds one [`Event::ConnectRetrying`] into [`WizardPhase`] -- the
    /// phase 4 -> phase 5 transition (or a further phase-5 retry
    /// incrementing its own counter), per [`Event::ConnectRetrying`]'s
    /// doc comment.
    fn on_connect_retrying(&mut self, attempt: u16) {
        let mut phase = self.wizard_phase.borrow_mut();
        match &*phase {
            WizardPhase::Connecting { addr, .. } | WizardPhase::NotResponding { addr, .. } => {
                *phase = WizardPhase::NotResponding { addr: *addr, attempt };
            }
            _ => {}
        }
        drop(phase);
        self.dirty = true;
    }

    /// Folds one [`Event::ConnectSucceeded`] into [`WizardPhase`] -- the
    /// phase 6 success outcome, from either `Connecting` or
    /// `NotResponding`.
    fn on_connect_succeeded(&mut self, degraded: bool) {
        *self.wizard_phase.borrow_mut() = WizardPhase::Succeeded { degraded };
        self.dirty = true;
    }

    /// Folds one [`Event::WizardAutoDismiss`] -- pops the wizard back to
    /// Devices, but **only** if it's currently showing a plain
    /// (non-degraded) success; see that event's doc comment for why this
    /// guard exists. `Navigator::pop` is itself a no-op if the wizard
    /// isn't actually on the stack (e.g. this event arrived after the
    /// user already backed out via B), so no separate "is the wizard
    /// open" check is needed here.
    fn on_wizard_auto_dismiss(&mut self) {
        let should_pop = matches!(*self.wizard_phase.borrow(), WizardPhase::Succeeded { degraded: false });
        if should_pop {
            self.navigator.pop();
            *self.wizard_phase.borrow_mut() = WizardPhase::default();
            self.wizard_devices.borrow_mut().clear();
            self.dirty = true;
        }
    }

    /// Records the Bluetooth link's coarse lifecycle state and refreshes
    /// the devices screen's scan-row sublabel to match. Also reachable
    /// directly (not just via [`App::handle_event`]) since it's a natural
    /// unit for tests and for [`App::record_connect_failure`] to reuse.
    ///
    /// Also clears [`BtModel::connected_codec`] whenever `state` isn't
    /// [`LinkState::Connected`] -- a stale codec word surviving a
    /// disconnect (or a scan/connect that reuses the link before a fresh
    /// [`Event::CodecChanged`] arrives) is worse than `NO LINK` (design
    /// section 15). Deliberately keyed off the link state itself rather
    /// than a dedicated disconnect event: every path off `Connected`
    /// already flows through this one method (bead pico-link-1v5), so
    /// this can't race with a disconnect notification C forgot to send,
    /// and it needs zero new firmware plumbing in `bt.c`.
    pub fn set_link_state(&mut self, state: LinkState) {
        self.model.link_state = state;
        if state != LinkState::Connected {
            self.model.connected_codec = None;
        }
        self.rebuild_root();
    }

    /// Records the live A2DP link's negotiated codec (or a renegotiation)
    /// and refreshes the Home hero widget to match. See
    /// [`ConnectedCodec`]'s doc comment for why `core` treats
    /// `word`/`nominal_bitrate_bps` as opaque, already-decided display
    /// data rather than deriving them from codec identity itself.
    pub fn set_connected_codec(&mut self, codec: ConnectedCodec) {
        self.model.connected_codec = Some(codec);
        self.rebuild_root();
    }

    /// Adds (or, if `addr` is already known, updates the name/rssi of) one
    /// discovered device and refreshes the devices screen. Update-in-place
    /// rather than appending a duplicate row: BTstack's inquiry reports the
    /// same device repeatedly as its RSSI/name resolve.
    pub fn add_device(&mut self, addr: [u8; 6], name: String, rssi: i8) {
        if let Some(existing) = self.model.devices.iter_mut().find(|d| d.addr == addr) {
            existing.name = name;
            existing.rssi = rssi;
        } else {
            self.model.devices.push(DeviceEntry { addr, name, rssi });
        }
        // Kept in lockstep with `model.devices` -- see `wizard_devices`'s
        // doc comment on why the wizard widget needs its own mirror
        // rather than a borrow into `self.model`.
        self.wizard_devices.borrow_mut().clone_from(&self.model.devices);
        self.rebuild_root();
    }

    /// Clears the discovered-device list, e.g. at the start of a fresh
    /// scan.
    pub fn clear_devices(&mut self) {
        self.model.devices.clear();
        self.wizard_devices.borrow_mut().clear();
        self.rebuild_root();
    }

    /// Records a failed connect attempt with its [`ConnectFailureReason`]
    /// and returns the link to [`LinkState::Idle`] -- the attempt is over
    /// either way, retryable or not; a future screen deciding whether to
    /// offer a retry reads `reason.retryable()` off
    /// `BtModel::last_connect_failure`, not the link state.
    pub fn record_connect_failure(&mut self, addr: [u8; 6], reason: ConnectFailureReason) {
        self.model.last_connect_failure = Some((addr, reason));
        self.model.link_state = LinkState::Idle;
        // Phase 4/5 -> phase 6 (failure outcome). Unconditional (not
        // gated on the wizard currently being open/mid-connect): a stray
        // `ConnectFailed` with the wizard closed or already past this
        // attempt just sets a phase nothing is currently rendering, which
        // is harmless and gets overwritten the next time the wizard opens
        // (`build_devices_screen`'s row-0 activation resets it to
        // `Instructions`).
        *self.wizard_phase.borrow_mut() = WizardPhase::Failed { addr, reason };
        self.rebuild_root();
    }

    /// Read-only access to the live Bluetooth model, for tests/diagnostics
    /// and for any future FFI accessor that needs to read it back.
    #[must_use]
    pub fn model(&self) -> &BtModel {
        &self.model
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

    /// The most recent `now_us` recorded via [`App::tick`]. `0` before the
    /// first tick.
    #[must_use]
    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    /// How many screens are on the navigator's stack (>= 1). Exposed for
    /// tests/diagnostics.
    #[must_use]
    pub fn navigator_depth(&self) -> usize {
        self.navigator.depth()
    }

    /// The currently visible screen's title. Exposed for tests/diagnostics
    /// -- in particular, proving that a Bluetooth [`Event`] mid-navigation
    /// doesn't silently pop the user back to the root screen (see
    /// [`App::rebuild_root`]'s doc comment).
    #[must_use]
    pub fn current_screen_title(&self) -> &str {
        &self.navigator.current().title
    }

    /// The root (devices) screen's own selection index, if it currently has
    /// one. Exposed for tests/diagnostics -- proving an [`Event`] carries
    /// the user's list selection forward instead of resetting it to row 0.
    #[must_use]
    pub fn root_selected_index(&self) -> Option<usize> {
        self.navigator.root_selected_index()
    }

    /// Test-only: the Devices screen's own selection index, if it's
    /// currently on the navigator stack at index 1 -- since
    /// `pico-link-znb.8` (E7) made Home (not Devices) the root, this is
    /// what most of this module's pre-E7 `root_selected_index` tests
    /// actually needed to observe (Home's own top-level widget has no
    /// `ListItemKey`/index selection concept -- see `render::home`'s
    /// module doc). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn devices_selected_index_for_test(&self) -> Option<usize> {
        self.navigator.selected_index_at(1)
    }

    /// Test-only: pushes an arbitrary screen onto the navigator stack, so
    /// tests can simulate "the user navigated away from root" without this
    /// bead building any real second screen (out of its scope -- see
    /// pico-link-a67's scope-discipline note). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn push_screen_for_test(&mut self, screen: Screen) {
        self.navigator.push(screen);
    }

    /// Test-only: enqueues a [`Command`] directly, bypassing the UI
    /// interaction that would normally queue one. `CancelScan` has no
    /// binding built by this bead (the wizard screen in pico-link-znb.7
    /// does that), so this is the only way to exercise its
    /// [`App::poll_command`] round-trip today. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn push_command_for_test(&mut self, command: Command) {
        self.commands.borrow_mut().push_back(command);
    }

    /// Test-only: reads the pairing wizard's current phase. Not part of
    /// the public API.
    #[cfg(test)]
    pub(crate) fn wizard_phase_for_test(&self) -> WizardPhase {
        self.wizard_phase.borrow().clone()
    }

    /// Dispatches every polled `NavIntent` to the navigator, in order.
    /// A no-op (including leaving `dirty` untouched) if `intents` is empty.
    pub fn handle_input(&mut self, intents: Vec<NavIntent>) {
        if intents.is_empty() {
            return;
        }
        for intent in intents {
            self.navigator.dispatch(intent);
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
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Renders the current screen into the app's framebuffer and clears
    /// the dirty flag, returning the freshly rendered framebuffer for the
    /// caller to hand to a `DisplaySurface::flush`.
    ///
    /// Also recomputes [`App::next_redraw_at`] from
    /// [`Navigator::redraw_after`] at this frame's `ctx` -- so a widget's
    /// "I'll look different again in N" answer is always relative to the
    /// instant that was actually just rendered, not a stale one.
    ///
    /// # Panics
    ///
    /// Never, in practice: `Navigator::render`'s `Result` is over
    /// `Infallible`'s uninhabited error type (the core `DrawTarget` can
    /// never fail to draw). The `expect` exists only because
    /// `Result::expect` is how that's asserted at the call site.
    pub fn render(&mut self) -> &FrameBuffer565 {
        let ctx = RenderCtx::at(Instant::from_micros(self.now_us));
        self.navigator
            .render(&ctx, &mut self.framebuffer)
            .expect("core DrawTarget is Infallible");
        self.next_redraw_at = self.navigator.redraw_after(&ctx).map(|duration| ctx.now() + duration);
        self.dirty = false;
        &self.framebuffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Home(1) -> Devices(2): since `pico-link-znb.8` (E7) made Home the
    /// navigator root, reaching the Devices screen (whose list rows this
    /// module's older tests exercise) takes two `Select`s -- centre
    /// toggles Home to its menu face (Bluetooth pre-selected), centre
    /// again activates that row.
    fn open_devices(app: &mut App) {
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
        app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
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
        app.handle_event(Event::LinkStateChanged(LinkState::Scanning));
        assert_eq!(app.navigator_depth(), 2, "LinkStateChanged must not pop the pushed screen");
        assert_eq!(app.current_screen_title(), "detail");

        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 2, 3, 4, 5, 6], name: String::from("Cans"), rssi: -40 }));
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
    fn a_device_arriving_mid_navigation_does_not_reset_the_devices_screens_selection() {
        let mut app = App::new(240, 240);
        open_devices(&mut app);
        // Two devices so there's a non-zero selection to move to and lose.
        app.add_device([1, 1, 1, 1, 1, 1], String::from("Device A"), -50);
        app.add_device([2, 2, 2, 2, 2, 2], String::from("Device B"), -60);

        // Devices list rows: 0 = "Scan for headphones", 1 = Device A, 2 = Device B.
        app.handle_input(vec![NavIntent::Down, NavIntent::Down]);
        assert_eq!(app.devices_selected_index_for_test(), Some(2), "selection should be on Device B's row");

        // A third device arriving must not snap the selection back to row 0
        // -- proving `App::rebuild_root`'s `Navigator::replace_at(1, ...)`
        // path (added alongside `replace_root` by `pico-link-znb.8`/E7,
        // since Devices is no longer the root itself) carries the
        // selection forward exactly the way `replace_root` always has.
        app.add_device([3, 3, 3, 3, 3, 3], String::from("Device C"), -70);
        assert_eq!(app.devices_selected_index_for_test(), Some(2), "a new device must not reset the user's selection");

        // Same for a link-state change while browsing.
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        assert_eq!(app.devices_selected_index_for_test(), Some(2), "a link-state change must not reset the user's selection");
    }

    // --- pico-link-znb.4: selection carried by identity, not index ---
    //
    // `a_device_arriving_mid_navigation_does_not_reset_the_devices_screens_
    // selection` above already proves the *index* doesn't move when the
    // App's own append-only device order (design section 9 rule 1: stable
    // sort, first-seen order, append at bottom, never re-sort by RSSI)
    // happens not to disturb it. These go one step further: they assert
    // the selection resolves to the *same device address* (not just the
    // same index -- proving the identity-key path, not an accident of
    // append-only ordering), and cover the update-in-place and
    // selection-vanishes cases the design also calls out. The "a new row
    // gets inserted *before* the selected one" stress case -- which App's
    // append-only ordering can never itself produce -- is exercised
    // directly against `VerticalList::with_selected_identity` in
    // `render::list::tests` instead, since that's the widget-level
    // mechanism this all rests on and the ordering rule App builds atop it
    // makes it unreachable at this level by design.

    #[test]
    fn selecting_a_device_survives_further_devices_arriving_identified_by_address_not_just_index() {
        let mut app = App::new(240, 240);
        open_devices(&mut app);
        let device_a = [1, 1, 1, 1, 1, 1];
        app.add_device(device_a, String::from("Device A"), -50);

        // Devices rows: 0 = Scan, 1 = Device A. Select Device A.
        app.handle_input(vec![NavIntent::Down]);
        assert_eq!(app.devices_selected_index_for_test(), Some(1));
        assert_eq!(app.model().devices[0].addr, device_a);

        app.add_device([2, 2, 2, 2, 2, 2], String::from("Device B"), -60);
        app.add_device([3, 3, 3, 3, 3, 3], String::from("Device C"), -70);

        let selected = app.devices_selected_index_for_test().expect("a device must still be selected");
        assert_eq!(
            app.model().devices[selected - 1].addr,
            device_a,
            "the selected row must still resolve to Device A's address, not merely the same index"
        );
    }

    #[test]
    fn a_late_name_for_an_already_listed_device_replaces_its_row_in_place() {
        let mut app = App::new(240, 240);
        open_devices(&mut app);
        let addr = [7, 7, 7, 7, 7, 7];
        app.add_device(addr, String::new(), -55); // nameless first report

        app.handle_input(vec![NavIntent::Down]); // select the device row
        assert_eq!(app.devices_selected_index_for_test(), Some(1));

        // The name resolves later, same address.
        app.add_device(addr, String::from("Sony WH-1000XM5"), -55);

        assert_eq!(app.model().devices.len(), 1, "a late name must update the existing row, not append a second one");
        assert_eq!(app.model().devices[0].name, "Sony WH-1000XM5");
        assert_eq!(app.devices_selected_index_for_test(), Some(1), "the late name must not disturb the selection");
    }

    #[test]
    fn the_selected_devices_disappearing_clamps_selection_instead_of_resetting_to_row_zero() {
        let mut app = App::new(240, 240);
        open_devices(&mut app);
        app.add_device([1, 1, 1, 1, 1, 1], String::from("Device A"), -50);
        app.handle_input(vec![NavIntent::Down]);
        assert_eq!(app.devices_selected_index_for_test(), Some(1));

        // The selected device drops out of the model entirely (e.g. a
        // future timeout/removal path -- simulated here via the one
        // removal primitive App has today, a full clear).
        app.clear_devices();

        // Only the Scan row and the "No devices found" placeholder remain
        // (rows 0 and 1); the vanished key isn't found, so
        // `with_selected_identity` falls back to clamping the previous
        // index (1) into the new list's bounds -- landing on row 1, not
        // snapping back past it to row 0.
        assert_eq!(
            app.devices_selected_index_for_test(),
            Some(1),
            "losing the selected row must clamp to the nearest surviving row, not reset to row 0"
        );
    }

    #[test]
    fn connect_failure_reasons_representable_and_the_two_impossible_ones_are_marked_non_retryable() {
        // The approved UI design names five distinct failure causes; two
        // (no A2DP sink, needs a PIN) must offer no retry because retrying
        // is structurally impossible -- see `ConnectFailureReason::retryable`'s
        // doc comment.
        assert!(ConnectFailureReason::Timeout.retryable());
        assert!(ConnectFailureReason::Rejected.retryable());
        assert!(ConnectFailureReason::RadioError.retryable());
        assert!(!ConnectFailureReason::NoA2dpSink.retryable());
        assert!(!ConnectFailureReason::NeedsPin.retryable());
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
        app.handle_event(Event::LinkStateChanged(LinkState::Scanning));
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 2, 3, 4, 5, 6], name: String::from("Cans"), rssi: -40 }));
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

    const DGX_ADDR: DeviceAddr = [9, 8, 7, 6, 5, 4];

    /// Folds the two events that take a fresh `App` from Idle to a fully
    /// connected, codec-reporting link -- exactly what `firmware/src/a2dp.c`
    /// fires in sequence on a real successful pairing.
    fn connect_link(app: &mut App) {
        app.handle_event(Event::LinkStateChanged(LinkState::Connected));
        app.handle_event(Event::CodecChanged(ConnectedCodec {
            addr: DGX_ADDR,
            word: String::from("LDAC"),
            nominal_bitrate_bps: 909_000,
        }));
    }

    fn assert_link_still_connected(app: &App) {
        assert_eq!(app.model().link_state, LinkState::Connected, "link_state must still read Connected");
        assert_eq!(
            app.model().connected_codec.as_ref().map(|c| c.word.as_str()),
            Some("LDAC"),
            "connected_codec must survive the navigation -- this is what pico-link-1v5 keys the hero word off"
        );
    }

    /// Drains the command queue and asserts it was completely empty -- not
    /// just absent of one variant. A test that only checks for a Disconnect
    /// that cannot exist in the FFI (`pico_link_ui.h:51-76` has no such tag)
    /// would prove nothing.
    fn assert_no_commands_queued(app: &mut App) {
        let mut drained = Vec::new();
        while let Some(cmd) = app.poll_command() {
            drained.push(cmd);
        }
        assert!(drained.is_empty(), "navigating to Home must not queue any command, got {drained:?}");
    }

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
        app.handle_input(vec![NavIntent::Select]); // Scan row -> pushes the wizard (Instructions)
        assert_eq!(app.navigator_depth(), 3);
        assert_no_commands_queued(&mut app);

        app.handle_event(Event::ConnectSucceeded { degraded });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded });
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
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 1, 1, 1, 1, 1], name: String::from("Other"), rssi: -55 }));
        assert_no_commands_queued(&mut app);
        assert_link_still_connected(&app);
        assert_home_hero_renders_connected(&mut app);
    }

    /// Renders Home (must be at depth 1, status face) and checks the hero
    /// paints the connected codec word in `TEXT_PRIMARY` (nominal, non-
    /// fallback connection -- see `render::hero`'s own
    /// `nominal_codec_renders_the_hero_word_in_text_primary` for the same
    /// pixel-presence technique) and paints no `STATUS_ERROR` ink anywhere
    /// -- `STATUS_ERROR` is exactly what `CodecStatus::NoLink` uses for the
    /// "NO LINK" word (`render/hero.rs`'s `no_link_renders_the_hero_word_
    /// in_status_error`), so its presence would mean Home rendered
    /// disconnected even though the model says otherwise -- exactly the
    /// failure mode this bead worried about.
    fn assert_home_hero_renders_connected(app: &mut App) {
        use crate::render::theme::palette;

        assert_eq!(app.navigator_depth(), 1, "hero only renders on Home's status face at the root");
        let fb = app.render();
        assert!(
            fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY),
            "a nominal connected codec word should paint TEXT_PRIMARY ink somewhere"
        );
        assert!(
            !fb.pixels().any(|p| p.1 == palette::STATUS_ERROR),
            "STATUS_ERROR ink anywhere means Home rendered NO LINK despite a connected model"
        );
    }
}
