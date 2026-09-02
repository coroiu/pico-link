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
use core::convert::Infallible;
use core::time::Duration;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::input::NavIntent;
use crate::render::home::build_home_screen;
use crate::render::theme::palette;
use crate::render::wizard::build_wizard_screen;
use crate::render::{
    Action, ButtonLabel, ChromeContribution, ConfirmView, FocusEvent, FrameBuffer565, Instant, ListItem, ListItemKey, MenuItem,
    Navigator, RenderCtx, Screen, VerticalList, Widget,
};

/// The devices screen's "Pair new headphones" row's identity key (bead
/// pico-link-4vb.4, design `.planning/design/2026-09-01-remembered-devices.md`
/// section 4). Replaces the old `SCAN_ROW_KEY` -- the row it names no longer
/// starts a scan directly (it opens the wizard, or the pick-one-to-forget
/// flow when the store is full), so the old name was already stale. Not
/// backed by a `DeviceAddr` (it isn't a device), so it's a fixed sentinel
/// instead — see [`ListItemKey::from`]'s doc comment for why this can
/// never collide with a real device's key (a device key's top two bytes
/// are always `0`; this sentinel's are always `0xFE` -- distinct from the
/// old `0xFF` sentinel in case any stale carried-forward selection key from
/// a pre-upgrade build is ever compared against it).
const PAIR_NEW_ROW_KEY: ListItemKey = ListItemKey::from_bytes([0xFE; 8]);

/// How many devices the flash store can remember (design section 6: slots
/// `PL:D:0`..`PL:D:7`). The Devices screen gates opening the wizard on this
/// *before* any radio work (design section 4) -- fullness must be
/// discoverable without `BTstack` ever attempting a pairing that a full store
/// would then refuse to persist.
const MAX_PAIRED_DEVICES: usize = 8;

/// The remembered-device name's on-flash/on-wire cap -- matches
/// `firmware/src/persist.c`'s `pl_persist_device_record_t::name[32]` and
/// [`PlConnectPayload::name`]/[`PlPairedDeviceUpsertedPayload::name`]'s wire
/// buffers exactly (design section 5.1/5.3).
const MAX_DEVICE_NAME_BYTES: usize = 32;

/// Truncates `name` to at most [`MAX_DEVICE_NAME_BYTES`], respecting a
/// UTF-8 **character** boundary -- design section 5.3, "Rust owns text; C
/// owns bytes": `core` is the only side of the FFI seam that can safely
/// find a char boundary (C only ever sees bytes), so this must happen
/// before a name is ever placed on a [`Command::Connect`], not after it
/// crosses into `ui-ffi`'s fixed-size wire buffer.
pub(crate) fn truncate_device_name(name: &str) -> String {
    if name.len() <= MAX_DEVICE_NAME_BYTES {
        return String::from(name);
    }
    let mut end = MAX_DEVICE_NAME_BYTES;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    String::from(&name[..end])
}

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
    /// BTstack's raw 24-bit Class-of-Device from the inquiry result
    /// (`gap_event_inquiry_result_get_class_of_device`), carried
    /// uninterpreted -- same "core never interprets BTstack internals
    /// blindly" rule as `addr` above. `0` means BTstack reported nothing
    /// (there is no separate "available" flag for this field, unlike
    /// `name`/`rssi`) and must be treated as *unknown*, never *non-audio*
    /// -- see [`is_audio_sink`]'s doc comment. Added by bead
    /// pico-link-znb.11 (E9, design section 21 Tier 1).
    pub class_of_device: u32,
}

/// Decodes BTstack's raw Class-of-Device into "should this show up in the
/// pairing wizard's scan list" (design section 9 phase 2 rule 3: "Filter by
/// Class-of-Device to audio sinks" -- every phone and laptop in the room is
/// noise the user cannot disambiguate, and design section 2's hard ~12-item
/// list cap makes an unfiltered inquiry a real usability failure, not a
/// cosmetic one).
///
/// Decodes the major device class (bits 8-12 of the 24-bit CoD, i.e.
/// `(cod >> 8) & 0x1F` -- see the Bluetooth Assigned Numbers "Baseband"
/// class-of-device format) and accepts the Audio/Video major class
/// (`0x04`), which covers headphones/headsets/speakers alongside a handful
/// of things that also plausibly want audio.
///
/// **Judgement call (this bead): filter, not rank, but with an explicit
/// unknown-is-included escape hatch.** Design section 9 already settled on
/// a hard filter over a ranked/demoted list. The risk that filter alone
/// creates: a device whose CoD is `0` (not reported, no "available" flag
/// exists for this field) or one that reports its major class oddly would
/// otherwise be a device the user can see in the room but can never
/// select, with nothing on screen explaining why -- silently unpairable.
/// So `0` (unknown) is treated as *included*, not excluded; only a CoD that
/// *positively* reports a known non-audio major class (phone, computer,
/// etc.) is filtered out. This can never make the list too permissive in a
/// crowded room, because [`MAX_SCAN_LIST_ITEMS`] backstops that regardless.
#[must_use]
pub(crate) fn is_audio_sink(class_of_device: u32) -> bool {
    if class_of_device == 0 {
        return true;
    }
    const MAJOR_DEVICE_CLASS_MASK: u32 = 0x1F;
    const MAJOR_AUDIO_VIDEO: u32 = 0x04;
    ((class_of_device >> 8) & MAJOR_DEVICE_CLASS_MASK) == MAJOR_AUDIO_VIDEO
}

/// Backstop cap on the pairing wizard's scan list (design section 21 Tier 1
/// row E9 / section 13's Class-of-Device row): "cap the scan list at 12
/// with a 'showing 12 of N' readout" -- built regardless of whether
/// [`is_audio_sink`] filtering is working, since Class-of-Device is only
/// marked *Expected*, not *Confirmed*, in the design doc, and a crowded
/// room can in principle still exceed 12 audio-classed devices. Matches
/// design section 2's hard ~12-item list rule, which exists because
/// press-edge-only input (no key repeat) makes a longer list a genuine
/// navigation failure, not a scrolling inconvenience.
pub(crate) const MAX_SCAN_LIST_ITEMS: usize = 12;

/// A user-initiated action queued by the devices screen for C to poll via
/// [`App::poll_command`] (`pl_ui_poll_command` in the FFI surface). `core`
/// never acts on these itself -- it has no Bluetooth stack to act with --
/// it only records "the user asked for this" and hands it back out.
///
/// No longer `Copy` as of bead pico-link-4vb.4 (T4) -- [`Command::Connect`]
/// gained a `String` field (see its own doc comment), which isn't `Copy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    StartScan,
    /// `name` added by bead pico-link-4vb.4 (T4), design section 5.3
    /// (alternative A: "name rides on Connect", chosen over a separate
    /// `SetDeviceName` command sent first -- see that section's cost
    /// analysis). Already truncated to [`MAX_DEVICE_NAME_BYTES`] on a
    /// UTF-8 character boundary by [`truncate_device_name`] before this
    /// variant is ever constructed -- `core` owns text, C only ever sees
    /// bytes. An empty `name` is a legal, deliberate value for a retry
    /// reissued from [`WizardPhase::NotResponding`]/[`WizardPhase::Failed`]
    /// (which carry no name of their own): C's read-modify-write persist
    /// path (design section 6) treats an empty name as "keep whatever name
    /// the record already has", so this never regresses an already-known
    /// name.
    Connect { addr: [u8; 6], name: String },
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
    /// `core`'s auto-reconnect/remember-this-device POLICY output (bead
    /// pico-link-cz0.6, M5 persistence design point 7): "please persist
    /// `addr` as the last-used device." Queued by
    /// [`App::on_connect_succeeded`] once a connect attempt actually
    /// succeeds -- `core` decides *when* a device is worth remembering;
    /// C only stages the write, gates it against the streaming/timing
    /// constraints flash access has on this hardware, and eventually
    /// flushes it. `core` has no flash of its own and never writes
    /// anything itself.
    PersistDevice { addr: [u8; 6] },
    /// User-initiated "forget this remembered device" (bead pico-link-4vb.4,
    /// T5, design section 5.3) -- the Devices screen's X action on a paired
    /// row, or the pick-one-to-forget flow's own confirm when the store is
    /// full. `core` does not remove `addr` from [`BtModel::paired`] itself
    /// on queuing this -- the single-writer rule (design section 3) means
    /// `paired` only changes on a real [`Event::PairedDeviceForgotten`]
    /// echoed back once C's delete (record + link key, design section 6)
    /// actually lands.
    ForgetDevice { addr: [u8; 6] },
    /// User-initiated "drop the current Bluetooth link" (bead
    /// pico-link-44w, FFI surface only -- no screen wires this yet; the
    /// design-of-record's manage-connected-device screen is the eventual
    /// caller, design section 21 Tier constraint rule 2 forbidding a
    /// labelled-but-dead affordance is why this stays unreachable from any
    /// screen for now). Carries no address: `firmware/src/a2dp.c` tracks at
    /// most one active connection at a time (`s_ctx.a2dp_cid`), and the C
    /// side's existing debug-only disconnect path
    /// (`pl_bt_debug_disconnect`, bead pico-link-nb6) already queues
    /// `PL_BT_PENDING_DISCONNECT` with a null address for the same reason
    /// -- this reuses that assumption rather than inventing a
    /// currently-meaningless target parameter.
    Disconnect,
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

/// What C's flash-backed store looked like at boot, as reported by
/// [`Event::StoreLoaded`] (bead pico-link-cz0.6, M5 persistence design
/// point 5). `core` has no flash of its own -- this is purely what C found,
/// so a fresh/reset store is never rendered identically to a healthy one
/// that simply has no device saved yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreStatus {
    /// No marker tag at all -- genuinely first boot, an empty store.
    FirstBoot,
    /// A device record was found and its CRC checked out (or the marker
    /// was present with no device record yet, which is also a valid,
    /// healthy store -- just nothing to reconnect to).
    Loaded,
    /// The marker was fine but the device record's CRC16 failed -- that
    /// one record was dropped; the store is otherwise intact.
    RecordCorrupt,
    /// The marker's schema version didn't match this firmware's -- this
    /// project's own records were dropped, but BTstack's link-key tags
    /// (a separate namespace) were left untouched.
    VersionMismatch,
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
    /// `addr` added by bead pico-link-cz0.6 (M5 persistence): which device
    /// this success is for, so [`App::on_connect_succeeded`] can queue
    /// [`Command::PersistDevice`] correctly REGARDLESS of whether the
    /// connection was driven by the wizard (which separately tracks `addr`
    /// on [`WizardPhase`]) or the `PL_DEBUG_REMOTE` bypass path (which does
    /// not touch the wizard at all) -- see that method's doc comment.
    ConnectSucceeded { addr: [u8; 6], degraded: bool },
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
    /// C's flash-backed store finished loading at boot (bead pico-link-cz0.6,
    /// M5 persistence) -- fired exactly once, after the radio is confirmed
    /// up (see `firmware/src/bt.c`'s `BTSTACK_EVENT_STATE`/
    /// `HCI_STATE_WORKING` case; pushed there rather than at `pl_bt_init`
    /// itself so a queued auto-reconnect can't race `hci_power_control`'s
    /// own async power-up), and always the *terminator* of C's boot push
    /// sequence: `count` x [`Event::PairedDeviceUpserted`], then this event
    /// (design section 5.2).
    ///
    /// Reshaped by bead pico-link-4vb.4 (T4), design section 5.2 -- the old
    /// `device_addr: Option<[u8; 6]>` field is REMOVED. That field was C
    /// DECIDING which device to auto-reconnect to; with eight slots that's
    /// a real policy, not "the one slot", and design point 7 puts that
    /// policy on `core`'s side of the seam. `core`'s auto-reconnect policy
    /// ([`App::on_store_loaded`]) now computes the target itself, once
    /// every preceding `PairedDeviceUpserted` has folded into
    /// [`BtModel::paired`]: `paired.iter().max_by_key(|d| d.mru_seq)`,
    /// queuing [`Command::Connect`] for it -- reusing the exact same
    /// command the Devices screen's own paired-row activation uses; `core`
    /// never opens a connection itself.
    StoreLoaded { status: StoreStatus },
    /// One remembered (paired) device the flash store holds -- pushed
    /// either at boot (C's `count` x this event ahead of
    /// [`Event::StoreLoaded`], design section 5.2) or after a device is
    /// newly persisted/updated. The single-writer rule (design section 3):
    /// this is one of exactly two events that mutate [`BtModel::paired`]
    /// (the other is [`Event::PairedDeviceForgotten`]) -- `core` never
    /// optimistically appends a row on [`Event::ConnectSucceeded`], because
    /// then `core`'s list and flash could disagree (a full store, a CRC
    /// failure, a deferred write) and a row for a device that was never
    /// actually persisted is exactly the "silently forgot your pairing"
    /// defect this whole line of work exists to kill. Bead pico-link-4vb.4
    /// (T4).
    PairedDeviceUpserted(PairedDevice),
    /// A remembered device was removed from the flash store (a real
    /// deletion C echoed back, not merely requested -- see
    /// [`Command::ForgetDevice`]'s doc comment). The other of the two
    /// events allowed to mutate [`BtModel::paired`] (design section 3).
    /// Bead pico-link-4vb.4 (T4).
    PairedDeviceForgotten { addr: DeviceAddr },
    /// The flash store refused a save because every slot already held a
    /// *different* address (design section 3: "must never silently
    /// evict"). Carries no payload -- there is nothing more specific to
    /// report than "full". The Devices screen already gates opening the
    /// wizard on `paired.len() < MAX_PAIRED_DEVICES` before any radio work
    /// (design section 4), so this is the defensive fallback for a race
    /// that gate can't fully close (e.g. two connect attempts racing each
    /// other), not the primary way fullness is discovered. Bead
    /// pico-link-4vb.4 (T4).
    PairedStoreFull,
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
    /// Inquiry-scan results. **Wizard-only reader** -- renamed from
    /// `devices` by bead pico-link-4vb.4 (T4), design section 3: the rename
    /// is the point, not cosmetics, because it turns "the Devices screen
    /// reads scan results" from a habit into a compile error. Mutated only
    /// by [`App::add_device`]/[`App::clear_devices`] (via
    /// [`Event::DeviceDiscovered`]/[`Event::DevicesCleared`]).
    pub discovered: Vec<DeviceEntry>,
    /// Remembered (paired) devices, restored from C's flash store --
    /// **Devices-screen-only reader** (design section 4). The single
    /// source of truth is C's flash: `core` never invents a row here, and
    /// mutates this list only by folding [`Event::PairedDeviceUpserted`]/
    /// [`Event::PairedDeviceForgotten`] -- see those variants' doc comments
    /// for the single-writer rule (design section 3). Deliberately carries
    /// no codec/volume/preset fields -- those are per-device *settings*
    /// with no screen yet (Tier 2, design section 9's task table), and
    /// their flash bytes are already reserved; adding model fields nobody
    /// reads would be gold-plating. Bead pico-link-4vb.4 (T4).
    pub paired: Vec<PairedDevice>,
    /// Which [`PairedDevice::addr`] (if any) the live A2DP link is
    /// currently connected to -- lets the Devices screen pin that device
    /// at the top (design section 4). Set by [`App::on_connect_succeeded`]
    /// and cleared by [`App::set_link_state`] whenever the link leaves
    /// [`LinkState::Connected`], the exact same lifecycle
    /// [`BtModel::connected_codec`] already follows and for the same
    /// reason (see that field's doc comment) -- every path off `Connected`
    /// already flows through `set_link_state`, so this can't race a
    /// disconnect C forgot to send. Bead pico-link-4vb.4 (T4/T5).
    pub connected_addr: Option<DeviceAddr>,
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
    /// What C's flash-backed store looked like at boot (bead pico-link-cz0.6,
    /// M5 persistence) -- `None` until [`Event::StoreLoaded`] has arrived
    /// (i.e. before the radio has finished powering on). Not yet rendered
    /// by any screen in this bead's scope -- populated so a reset/corrupt
    /// store is representable ahead of the screen that will surface it
    /// (design point 5's "never renders identically to a new one"),
    /// mirroring `last_connect_failure`'s own "populated ahead of its
    /// screen" precedent above.
    pub store_status: Option<StoreStatus>,
    /// Whether the flash store most recently refused a save because every
    /// slot held a different address (see [`Event::PairedStoreFull`]'s doc
    /// comment). Not yet rendered by any screen -- the Devices screen
    /// already gates pairing on `paired.len() < MAX_PAIRED_DEVICES` before
    /// any radio work, so this is populated ahead of the screen that will
    /// eventually surface the race this can't fully close, mirroring
    /// `last_connect_failure`/`store_status`'s own precedent above. Bead
    /// pico-link-4vb.4 (T4).
    pub store_full: bool,
}

/// A Bluetooth device address, aliased for readability at call sites that
/// pair it with a [`ConnectFailureReason`].
pub type DeviceAddr = [u8; 6];

/// One remembered (paired) device, as reported by C over
/// [`Event::PairedDeviceUpserted`] -- restored from the flash store at boot
/// or freshly persisted after a successful pairing. Bead pico-link-4vb.4
/// (T4), design `.planning/design/2026-09-01-remembered-devices.md`
/// section 3.
///
/// Deliberately carries no codec/volume/flags/preset fields -- see
/// [`BtModel::paired`]'s doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub addr: DeviceAddr,
    /// Possibly empty -- rendered `(unknown device)` plus the address's
    /// last three bytes as the discriminator (design section 4/13).
    pub name: String,
    /// Monotonic use-sequence C assigns (never a wall clock -- this board
    /// has no RTC). The Devices screen's ordering key (design section 4:
    /// MRU-descending), and what [`App::on_store_loaded`]'s auto-reconnect
    /// policy maximizes over.
    pub mru_seq: u32,
}

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

/// A paired device's display label -- its name, or, if C never reported one
/// (or it hasn't resolved yet), `(unknown device)` plus the address's last
/// three bytes as the discriminator (design section 4/13's rule that a
/// nameless row must still be distinguishable from every other nameless
/// row).
fn paired_device_label(device: &PairedDevice) -> String {
    if device.name.is_empty() {
        format!("(unknown device) {:02X}:{:02X}:{:02X}", device.addr[3], device.addr[4], device.addr[5])
    } else {
        device.name.clone()
    }
}

/// Builds the devices screen (bead pico-link-4vb.4, T5, design section 4):
/// the connected device (if any) pinned first, sublabelled `Connected`;
/// then every other paired device, MRU-descending, sublabelled `Paired`;
/// then `Pair new headphones` last. **No RSSI, no address, no availability
/// dot** -- design section 4/18: never claim availability that hasn't been
/// verified, and the recurring "switch device" job belongs under the
/// cursor while the rare "pair a new one" job belongs at the end.
///
/// Replaces the old scan-result rendering entirely -- see
/// [`BtModel::discovered`]/[`BtModel::paired`]'s doc comments for the
/// "wizard-only" / "Devices-screen-only" reader split this enforces.
///
/// Every row carries a [`ListItemKey`] (`device.addr` via `From<[u8; 6]>`,
/// or [`PAIR_NEW_ROW_KEY`] for the fixed last row) so `prev_key`/
/// `prev_index` can carry the user's selection forward **by identity**
/// through [`VerticalList::with_selected_identity`] -- a device arriving,
/// being renamed in place, reordering by a fresh `mru_seq`, or dropping out
/// entirely no longer moves the selection just because the *index* it used
/// to occupy now means something else. `prev_key` of `None`/not-found
/// falls back to clamping `prev_index` -- see that method's doc comment.
pub(crate) fn build_devices_screen(
    model: &BtModel,
    prev_key: Option<ListItemKey>,
    prev_index: usize,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    wizard_phase: &Rc<RefCell<WizardPhase>>,
    wizard_devices: &Rc<RefCell<Vec<DeviceEntry>>>,
) -> Screen {
    let connected = model.connected_addr.and_then(|addr| model.paired.iter().find(|d| d.addr == addr));
    let mut others: Vec<&PairedDevice> =
        model.paired.iter().filter(|d| Some(d.addr) != model.connected_addr).collect();
    others.sort_by_key(|d| core::cmp::Reverse(d.mru_seq));

    let mut ordered: Vec<PairedDevice> = Vec::with_capacity(model.paired.len());
    if let Some(device) = connected {
        ordered.push(device.clone());
    }
    ordered.extend(others.into_iter().cloned());

    let mut items: Vec<ListItem> = ordered
        .iter()
        .map(|device| {
            let sublabel = if Some(device.addr) == model.connected_addr { "Connected" } else { "Paired" };
            ListItem::new(paired_device_label(device)).with_sublabel(sublabel).with_key(ListItemKey::from(device.addr))
        })
        .collect();
    items.push(ListItem::new("Pair new headphones").with_key(PAIR_NEW_ROW_KEY));

    let paired_len = model.paired.len();
    let connected_addr = model.connected_addr;
    let ordered_for_activate = ordered.clone();
    let paired_for_full = model.paired.clone();
    let commands_for_activate = Rc::clone(commands);
    let wizard_phase_for_activate = Rc::clone(wizard_phase);
    let wizard_devices_for_activate = Rc::clone(wizard_devices);
    let list = VerticalList::new(items)
        .on_activate_index(move |index| {
            if let Some(device) = ordered_for_activate.get(index) {
                if Some(device.addr) == connected_addr {
                    // A on the connected row: no reconnect to do -- push
                    // the stub device-detail screen, the same
                    // labelled-row-needs-a-destination precedent
                    // `build_settings_screen` set (design section 4).
                    let title = paired_device_label(device);
                    return Action::PushView(Box::new(move || build_device_detail_screen(title)));
                }
                // A on any other paired row: switch to it, reusing the
                // wizard (design section 4/S8) -- `Command::Connect` +
                // pushing straight into `Connecting`, no new phase.
                commands_for_activate
                    .borrow_mut()
                    .push_back(Command::Connect { addr: device.addr, name: truncate_device_name(&device.name) });
                *wizard_phase_for_activate.borrow_mut() = WizardPhase::connecting_pending(device.addr, ConnectStep::Connecting);
                let phase = Rc::clone(&wizard_phase_for_activate);
                let devices = Rc::clone(&wizard_devices_for_activate);
                let commands = Rc::clone(&commands_for_activate);
                return Action::PushView(Box::new(move || build_wizard_screen(phase, devices, commands)));
            }
            // "Pair new headphones", the fixed last row. Gated on capacity
            // *before* any radio work (design section 4): under the cap,
            // open the wizard exactly as before; at the cap, open the
            // pick-one-to-forget flow instead.
            if paired_len < MAX_PAIRED_DEVICES {
                *wizard_phase_for_activate.borrow_mut() = WizardPhase::scanning_pending();
                wizard_devices_for_activate.borrow_mut().clear();
                commands_for_activate.borrow_mut().push_back(Command::StartScan);
                let phase = Rc::clone(&wizard_phase_for_activate);
                let devices = Rc::clone(&wizard_devices_for_activate);
                let commands = Rc::clone(&commands_for_activate);
                Action::PushView(Box::new(move || build_wizard_screen(phase, devices, commands)))
            } else {
                let paired = paired_for_full.clone();
                let commands = Rc::clone(&commands_for_activate);
                Action::PushView(Box::new(move || build_forget_picker_screen(paired, commands)))
            }
        })
        .with_selected_identity(prev_key, prev_index);

    let view = DevicesListView { list, row_devices: ordered, commands: Rc::clone(commands) };
    Screen::new(DEVICES_TITLE, vec![Box::new(view)])
}

/// Wraps [`VerticalList`] to add the Devices screen's X action (design
/// section 4: "X opens a forget confirm") on top of it -- `VerticalList`
/// itself has no opinion about `ShortcutX` (see `crate::input::NavIntent::
/// ShortcutX`'s doc comment), so this is the same "small wrapper widget
/// intercepts one `NavIntent` variant, delegates the rest" shape
/// `crate::render::wizard::PairingWizardView` already uses for its own
/// phase-specific `ShortcutX` handling.
struct DevicesListView {
    list: VerticalList,
    /// Parallel to `list`'s rows *up to* the fixed "Pair new headphones"
    /// row (which carries no entry here) -- lets `on_intent` map the
    /// currently selected index back to a real device without needing a
    /// `ListItemKey` -> address lookup.
    row_devices: Vec<PairedDevice>,
    commands: Rc<RefCell<VecDeque<Command>>>,
}

impl Widget for DevicesListView {
    fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size {
        self.list.measure(constraints, ctx)
    }

    fn is_focusable(&self) -> bool {
        self.list.is_focusable()
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        if intent == NavIntent::ShortcutX {
            let index = self.list.selected_index();
            return if let Some(device) = self.row_devices.get(index) {
                let addr = device.addr;
                let label = paired_device_label(device);
                let commands = Rc::clone(&self.commands);
                Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)))
            } else {
                // The fixed "Pair new headphones" row has nothing to forget.
                Action::None
            };
        }
        self.list.on_intent(intent)
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let index = self.list.selected_index();
        let x = if index < self.row_devices.len() {
            ButtonLabel::Live(String::from("forget"))
        } else {
            ButtonLabel::Inert
        };
        Some(ChromeContribution { x: Some(x), ..ChromeContribution::default() })
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.list.selected_index())
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        self.list.selected_key()
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        self.list.render(area, ctx, target)
    }

    /// Forwards `list`'s own answer (pico-link-vxc, D2) -- without this the
    /// default (`None`) would swallow it. `VerticalList` has no time-driven
    /// content today, but the wrapper must not be the thing that silently
    /// drops a future one under the dirty gate.
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        self.list.redraw_after(ctx)
    }
}

/// The pick-one-to-forget screen (design section 4): reached only when
/// `Pair new headphones` is activated while `paired.len() ==
/// MAX_PAIRED_DEVICES` -- fullness discovered and resolved entirely
/// before any radio work. Every row pushes the same
/// [`build_forget_confirm_screen`] a device row's X action does.
const FORGET_PICKER_TITLE: &str = "Pick one to forget";

fn build_forget_picker_screen(paired: Vec<PairedDevice>, commands: Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let items: Vec<ListItem> =
        paired.iter().map(|device| ListItem::new(paired_device_label(device)).with_key(ListItemKey::from(device.addr))).collect();
    let paired_for_activate = paired;
    let list = VerticalList::new(items).on_activate_index(move |index| {
        if let Some(device) = paired_for_activate.get(index) {
            let addr = device.addr;
            let label = paired_device_label(device);
            let commands = Rc::clone(&commands);
            return Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)));
        }
        Action::None
    });
    Screen::new(FORGET_PICKER_TITLE, vec![Box::new(list)])
}

/// The forget-confirmation screen -- destructive-action confirm, per
/// [`ConfirmView`]'s own precedent (design section 4's X action, and the
/// pick-one-to-forget flow above). Cancel is row 0 (the safe default);
/// Forget is row 1, styled in [`palette::STATUS_ERROR`], and is the only
/// row that queues [`Command::ForgetDevice`] -- `core` does not remove
/// `addr` from [`BtModel::paired`] itself (see that command's doc comment
/// for the single-writer rule this preserves).
const FORGET_CONFIRM_TITLE: &str = "Forget device?";

fn build_forget_confirm_screen(addr: DeviceAddr, label: &str, commands: Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let headline = format!("Forget {label}?");
    let rows = vec![MenuItem::new("Cancel"), MenuItem::new("Forget").with_label_color(palette::STATUS_ERROR)];
    let view = ConfirmView::new(headline, rows).on_activate_index(move |index| {
        if index == 1 {
            commands.borrow_mut().push_back(Command::ForgetDevice { addr });
        }
        Action::PopView
    });
    Screen::new(FORGET_CONFIRM_TITLE, vec![Box::new(view)])
}

/// The connected device's detail screen -- a stub, per the exact precedent
/// [`build_settings_screen`] set for the Settings row (design section 4: a
/// labelled, reachable row must have *somewhere* to go; its real content is
/// design section 10's job, not this bead's). `title` is the device's own
/// display label (its name, or the `(unknown device)` fallback), matching
/// [`ChromeContribution::title`]'s override precedent used elsewhere for a
/// detail view showing its own item's identity instead of a screen-level
/// static title.
fn build_device_detail_screen(title: String) -> Screen {
    Screen::new(title, vec![])
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
            Event::DeviceDiscovered(device) => {
                self.add_device(device.addr, device.name, device.rssi, device.class_of_device);
            }
            Event::DevicesCleared => self.clear_devices(),
            Event::ConnectFailed { addr, reason } => self.record_connect_failure(addr, reason),
            Event::ConnectStepChanged(step) => self.on_connect_step_changed(step),
            Event::ConnectRetrying { attempt } => self.on_connect_retrying(attempt),
            Event::ConnectSucceeded { addr, degraded } => self.on_connect_succeeded(addr, degraded),
            Event::WizardAutoDismiss => self.on_wizard_auto_dismiss(),
            Event::CodecChanged(codec) => self.set_connected_codec(codec),
            Event::StoreLoaded { status } => self.on_store_loaded(status),
            Event::PairedDeviceUpserted(device) => self.on_paired_device_upserted(device),
            Event::PairedDeviceForgotten { addr } => self.on_paired_device_forgotten(addr),
            Event::PairedStoreFull => self.on_paired_store_full(),
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
    ///
    /// Also queues [`Command::PersistDevice { addr }`] (bead pico-link-cz0.6,
    /// M5 persistence design point 7 -- `core`'s auto-reconnect/remember-
    /// this-device POLICY: a connect that actually succeeded is worth
    /// remembering), unconditionally -- `addr` comes straight off the event
    /// itself (see [`Event::ConnectSucceeded`]'s doc comment for why that,
    /// not `WizardPhase`, is the source of truth: it works identically for
    /// a wizard-driven connect and the `PL_DEBUG_REMOTE` bypass, which
    /// never touches `WizardPhase` at all).
    fn on_connect_succeeded(&mut self, addr: [u8; 6], degraded: bool) {
        self.commands.borrow_mut().push_back(Command::PersistDevice { addr });
        self.model.connected_addr = Some(addr);
        *self.wizard_phase.borrow_mut() = WizardPhase::Succeeded { degraded };
        self.dirty = true;
    }

    /// Folds one [`Event::StoreLoaded`] -- records `status` in [`BtModel`]
    /// and runs the auto-reconnect policy: `core` decides *whether* and
    /// *which* device to reconnect to, C only loads/stages/flushes (design
    /// point 7). Reshaped by bead pico-link-4vb.4 (T4), design section 5.2:
    /// this event no longer carries an address -- by the time it arrives,
    /// every `Event::PairedDeviceUpserted` C pushed ahead of it (its own
    /// boot sequence's `count` records) has already folded into
    /// [`BtModel::paired`] (see [`App::on_paired_device_upserted`]), so the
    /// target is simply the highest `mru_seq` in that list. Queues the
    /// exact same [`Command::Connect`] a manual paired-row activation uses.
    /// Does not touch the wizard or rebuild the root screen -- this fires
    /// once at boot, before the user has done anything, and the queued
    /// `Connect` drives the same `LinkStateChanged`/`ConnectStepChanged`/
    /// `ConnectSucceeded` event flow a manual connect would, which is what
    /// actually updates the UI as the auto-reconnect proceeds.
    fn on_store_loaded(&mut self, status: StoreStatus) {
        self.model.store_status = Some(status);
        if let Some(device) = self.model.paired.iter().max_by_key(|d| d.mru_seq) {
            let addr = device.addr;
            let name = truncate_device_name(&device.name);
            self.commands.borrow_mut().push_back(Command::Connect { addr, name });
        }
        self.rebuild_root();
    }

    /// Folds one [`Event::PairedDeviceUpserted`] into [`BtModel::paired`] --
    /// update-in-place if `addr` is already known (a rename, an `mru_seq`
    /// bump), append otherwise. One of exactly two writers of `paired`
    /// (design section 3's single-writer rule -- see that event's doc
    /// comment). Bead pico-link-4vb.4 (T4).
    fn on_paired_device_upserted(&mut self, device: PairedDevice) {
        if let Some(existing) = self.model.paired.iter_mut().find(|d| d.addr == device.addr) {
            *existing = device;
        } else {
            self.model.paired.push(device);
        }
        self.rebuild_root();
    }

    /// Folds one [`Event::PairedDeviceForgotten`] into [`BtModel::paired`] --
    /// the other of the two writers (design section 3). A no-op if `addr`
    /// isn't currently known (e.g. a stray/duplicate echo).
    fn on_paired_device_forgotten(&mut self, addr: DeviceAddr) {
        self.model.paired.retain(|d| d.addr != addr);
        self.rebuild_root();
    }

    /// Folds one [`Event::PairedStoreFull`] -- see that event's and
    /// [`BtModel::store_full`]'s doc comments for why this is populated
    /// ahead of any screen actually reading it.
    fn on_paired_store_full(&mut self) {
        self.model.store_full = true;
        self.dirty = true;
    }

    /// Folds one [`Event::WizardAutoDismiss`] -- pops all the way back to
    /// Home, but **only** if it's currently showing a plain (non-degraded)
    /// success; see that event's doc comment for why this guard exists.
    /// pico-link-4vb.2 (Andreas's ruling): landing on Devices left him
    /// pressing Back repeatedly to get back to Home, so this now calls
    /// [`crate::render::Navigator::pop_to_root`] instead of
    /// [`crate::render::Navigator::pop`] -- a no-op if the wizard isn't
    /// actually the top of the stack any more (e.g. this event arrived
    /// after the user already backed out via B), same as `pop` was.
    fn on_wizard_auto_dismiss(&mut self) {
        let should_pop = matches!(*self.wizard_phase.borrow(), WizardPhase::Succeeded { degraded: false });
        if should_pop {
            self.navigator.pop_to_root();
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
            // `connected_addr` (bead pico-link-4vb.4, T5) follows the exact
            // same lifecycle as `connected_codec`, for the same reason --
            // see `BtModel::connected_addr`'s doc comment.
            self.model.connected_addr = None;
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
    pub fn add_device(&mut self, addr: [u8; 6], name: String, rssi: i8, class_of_device: u32) {
        if let Some(existing) = self.model.discovered.iter_mut().find(|d| d.addr == addr) {
            existing.name = name;
            existing.rssi = rssi;
            existing.class_of_device = class_of_device;
        } else {
            self.model.discovered.push(DeviceEntry { addr, name, rssi, class_of_device });
        }
        // Kept in lockstep with `model.discovered` -- see `wizard_devices`'s
        // doc comment on why the wizard widget needs its own mirror
        // rather than a borrow into `self.model`.
        self.wizard_devices.borrow_mut().clone_from(&self.model.discovered);
        self.rebuild_root();
    }

    /// Clears the discovered-device list, e.g. at the start of a fresh
    /// scan.
    pub fn clear_devices(&mut self) {
        self.model.discovered.clear();
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
        // (`build_devices_screen`'s "Pair new headphones" row activation
        // resets it to `WizardPhase::scanning_pending`).
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

    /// Whether the navigator is currently showing Home at the root of its
    /// stack (depth 1) -- either face (design section 4/7's `HomeFace`).
    /// `false` for any pushed screen, including Devices (depth 2),
    /// Settings (depth 2), and the pairing wizard (depth 3, see
    /// `crate::render::wizard`'s own navigator-depth tests) -- the pairing
    /// wizard in particular must never be mistaken for "at Home root": a
    /// blanked screen mid-pairing reads as a crash (bead pico-link-4vb.3).
    /// `crate::run::Runner::step` uses this to gate the idle-screensaver
    /// tier so it arms only here.
    #[must_use]
    pub fn is_at_home_root(&self) -> bool {
        self.navigator_depth() == 1
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

    /// Test-only: pops the top screen off the navigator stack, the
    /// counterpart to [`App::push_screen_for_test`] -- lets a test
    /// simulate "the user backed out to a previous screen" (e.g. back to
    /// Home root) without going through real screen-specific `B` handling.
    /// Not part of the public API.
    #[cfg(test)]
    pub(crate) fn pop_screen_for_test(&mut self) {
        self.navigator.pop();
    }

    /// Test-only: replaces the navigator's *root* screen with an arbitrary
    /// one, staying at depth 1 (Home root) -- unlike
    /// [`App::push_screen_for_test`], which adds a screen on top. Lets a
    /// test exercise generic run-loop behavior (e.g. focus/selection) that
    /// needs some focusable content, while still satisfying
    /// [`App::is_at_home_root`] (bead pico-link-4vb.3's screensaver gate)
    /// the way real Home content does. Not part of the public API.
    #[cfg(test)]
    pub(crate) fn replace_root_for_test(&mut self, screen: Screen) {
        self.navigator.replace_root(screen);
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
    pub fn render(&mut self) -> &FrameBuffer565 {
        let ctx = RenderCtx::at(Instant::from_micros(self.now_us));
        self.navigator
            .render(&ctx, &mut self.framebuffer)
            .expect("core DrawTarget is Infallible");
        self.next_redraw_at = self.navigator.redraw_after(&ctx).map(|duration| ctx.now() + duration.max(MIN_REDRAW_DELAY));
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

    /// Home(1) -> Devices(2) -> Wizard(3) -- see `open_devices` above; one
    /// further `Select` activates the fixed "Pair new headphones" row
    /// (there being no other paired devices at this point), pushing the
    /// wizard straight into `WizardPhase::Scanning`. Local copy of
    /// `render::wizard`'s own test-only helper of the same name -- that
    /// one is private to its module's test mod, and this crate has no
    /// shared test-support module to hoist it into.
    fn open_wizard(app: &mut App) {
        open_devices(app);
        app.handle_input(vec![NavIntent::Select]); // "Pair new headphones" row -> pushes the wizard
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
            app.handle_event(Event::LinkStateChanged(LinkState::Idle));
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
            app.handle_input(vec![NavIntent::ShortcutY]); // Home status face -> Settings
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

    #[test]
    fn is_audio_sink_accepts_the_audio_video_major_device_class() {
        // 0x24_04_04: real headphones-shaped CoD (major device class 0x04,
        // Audio/Video; minor device class 0x04, wearable headset).
        assert!(is_audio_sink(0x24_04_04));
        // The major-class bits alone (no service-class bits set) must
        // still decode correctly.
        assert!(is_audio_sink(0x04 << 8));
    }

    #[test]
    fn is_audio_sink_rejects_known_non_audio_major_device_classes() {
        assert!(!is_audio_sink(0x20_02_0C), "major device class 0x02 (Phone) must be excluded");
        assert!(!is_audio_sink(0x01 << 8), "major device class 0x01 (Computer) must be excluded");
    }

    #[test]
    fn is_audio_sink_treats_an_unreported_class_of_device_as_unknown_not_excluded() {
        // `0` means BTstack reported nothing (no separate "available" flag
        // exists for this inquiry-result field) -- must never be treated
        // as "known non-audio", or a device with odd/missing CoD reporting
        // becomes silently unpairable with no way for the user to tell why.
        assert!(is_audio_sink(0));
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
        app.handle_event(Event::LinkStateChanged(LinkState::Scanning));
        assert_eq!(app.navigator_depth(), 2, "LinkStateChanged must not pop the pushed screen");
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

    /// Bead pico-link-4vb.4 (T5): the Devices screen no longer reads
    /// `BtModel::discovered` (the wizard's own scan list, see that field's
    /// doc comment) -- it reads `BtModel::paired`, mutated only by
    /// [`Event::PairedDeviceUpserted`]/[`Event::PairedDeviceForgotten`].
    /// Shorthand for building one such event in these tests.
    fn upsert(addr: [u8; 6], name: &str, mru_seq: u32) -> Event {
        Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq })
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

    // --- pico-link-znb.4 / pico-link-4vb.4: selection carried by identity, not index ---
    //
    // These prove the selection resolves to the *same device address* (not
    // just the same index), across exactly the cases the design calls out:
    // reordering by a fresh `mru_seq` (which the old append-only scan list
    // could never produce, since MRU sorting is new to this bead), an
    // update-in-place rename, and a forgotten device vanishing.

    #[test]
    fn selecting_a_paired_device_survives_reordering_identified_by_address_not_just_index() {
        let mut app = App::new(240, 240);
        let device_a = [1, 1, 1, 1, 1, 1];
        app.handle_event(upsert(device_a, "Device A", 1));
        open_devices(&mut app);

        // Devices rows: 0 = Device A, 1 = "Pair new headphones" -- default
        // selection starts at row 0, already Device A.
        assert_eq!(app.devices_selected_index_for_test(), Some(0));
        assert_eq!(app.model().paired[0].addr, device_a);

        // Two more devices, with HIGHER mru_seq, push Device A down the list.
        app.handle_event(upsert([2, 2, 2, 2, 2, 2], "Device B", 5));
        app.handle_event(upsert([3, 3, 3, 3, 3, 3], "Device C", 6));

        // Rows now: 0 = C (6), 1 = B (5), 2 = A (1), 3 = Pair new.
        let selected = app.devices_selected_index_for_test().expect("a device must still be selected");
        assert_eq!(selected, 2, "the selection must follow Device A's address even though it moved rows");
    }

    #[test]
    fn a_paired_device_upserted_for_a_known_addr_updates_its_row_in_place() {
        let mut app = App::new(240, 240);
        let addr = [7, 7, 7, 7, 7, 7];
        app.handle_event(upsert(addr, "", 1)); // nameless first report
        open_devices(&mut app);

        assert_eq!(app.devices_selected_index_for_test(), Some(0));

        // The name resolves later, same address, higher mru_seq.
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 2));

        assert_eq!(app.model().paired.len(), 1, "a re-upsert for a known addr must update the existing row, not append a second one");
        assert_eq!(app.model().paired[0].name, "Sony WH-1000XM5");
        assert_eq!(app.devices_selected_index_for_test(), Some(0), "the update must not disturb the selection");
    }

    #[test]
    fn forgetting_the_selected_paired_device_clamps_selection_instead_of_resetting_to_row_zero() {
        let mut app = App::new(240, 240);
        let addr_a = [1, 1, 1, 1, 1, 1];
        let addr_b = [2, 2, 2, 2, 2, 2];
        app.handle_event(upsert(addr_a, "Device A", 2));
        app.handle_event(upsert(addr_b, "Device B", 1));
        open_devices(&mut app);

        // Rows: 0 = A, 1 = B, 2 = Pair new. Select B.
        app.handle_input(vec![NavIntent::Down]);
        assert_eq!(app.devices_selected_index_for_test(), Some(1));

        app.handle_event(Event::PairedDeviceForgotten { addr: addr_b });

        // Only Device A and "Pair new headphones" remain (rows 0 and 1);
        // B's key is gone, so `with_selected_identity` falls back to
        // clamping the previous index (1) into the new list's bounds --
        // landing on row 1, not snapping back past it to row 0.
        assert_eq!(
            app.devices_selected_index_for_test(),
            Some(1),
            "losing the selected row must clamp to the nearest surviving row, not reset to row 0"
        );
        assert!(app.model().paired.iter().all(|d| d.addr != addr_b), "the forgotten device must be gone from the model");
    }

    // --- pico-link-4vb.4 (T5): the new Devices screen's own behaviors ---

    #[test]
    fn the_connected_row_pins_first_and_a_pushes_its_device_detail_screen() {
        let mut app = App::new(240, 240);
        let addr = [4, 4, 4, 4, 4, 4];
        app.handle_event(upsert(addr, "Connected Cans", 1));
        open_devices(&mut app);

        app.handle_event(Event::LinkStateChanged(LinkState::Connected));
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice
        // C echoes the upsert once the persist write actually lands --
        // this is what actually refreshes the Devices screen's pinned row
        // (design section 7's hazard 4).
        app.handle_event(upsert(addr, "Connected Cans", 2));

        // The only paired device, now connected, pins at row 0; "Pair new
        // headphones" is row 1.
        assert_eq!(app.devices_selected_index_for_test(), Some(0));
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.current_screen_title(), "Connected Cans", "A on the connected row must push its device-detail screen");
    }

    #[test]
    fn selecting_a_paired_non_connected_row_queues_connect_and_pushes_the_wizard_at_connecting() {
        let mut app = App::new(240, 240);
        let addr = [5, 5, 5, 5, 5, 5];
        app.handle_event(upsert(addr, "Headphones", 1));
        open_devices(&mut app);

        app.handle_input(vec![NavIntent::Select]); // row 0: the sole paired (not-connected) device
        assert_eq!(app.navigator_depth(), 3, "selecting a paired row must push the wizard");
        assert!(
            matches!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr: a, step: ConnectStep::Connecting, .. } if a == addr),
            "the wizard must enter straight into Connecting for this device"
        );
        assert_eq!(app.poll_command(), Some(Command::Connect { addr, name: String::from("Headphones") }));
    }

    #[test]
    fn pair_new_headphones_opens_the_wizard_when_under_capacity() {
        let mut app = App::new(240, 240);
        open_devices(&mut app); // no paired devices -- the sole row is "Pair new headphones"

        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.navigator_depth(), 3, "under capacity, Pair new headphones must open the wizard");
        assert_eq!(app.poll_command(), Some(Command::StartScan));
    }

    #[test]
    fn pair_new_headphones_opens_the_forget_picker_when_at_capacity() {
        let mut app = App::new(240, 240);
        for i in 0..8u8 {
            app.handle_event(upsert([i; 6], "Device", u32::from(i)));
        }
        open_devices(&mut app);

        // Rows: 8 paired devices (MRU-descending) then "Pair new
        // headphones" at index 8 -- gated on capacity *before* any radio
        // work (design section 4).
        app.handle_input(vec![
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
        ]);
        assert_eq!(app.devices_selected_index_for_test(), Some(8));

        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.navigator_depth(), 3, "at capacity, Pair new headphones must open a picker screen, not the wizard");
        assert_eq!(app.poll_command(), None, "no StartScan should be queued when gated by capacity");
    }

    #[test]
    fn x_on_a_paired_row_opens_forget_confirm_and_confirming_queues_forget_device() {
        let mut app = App::new(240, 240);
        let addr = [6, 6, 6, 6, 6, 6];
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.navigator_depth(), 3, "X on a paired row must push the forget-confirm screen");

        // Cancel is the default selection (row 0, the safe default per
        // `ConfirmView`'s own precedent) -- Forget is row 1.
        app.handle_input(vec![NavIntent::Down]);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::ForgetDevice { addr }));
        assert_eq!(app.navigator_depth(), 2, "confirming must pop back to Devices");
    }

    #[test]
    fn x_on_the_pair_new_row_does_nothing() {
        let mut app = App::new(240, 240);
        open_devices(&mut app); // no paired devices -- the sole row is "Pair new headphones"

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.navigator_depth(), 2, "X on the fixed Pair-new row must be a no-op");
    }

    #[test]
    fn a_nameless_paired_device_renders_the_unknown_device_fallback_label() {
        let device = PairedDevice { addr: [0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33], name: String::new(), mru_seq: 1 };
        assert_eq!(paired_device_label(&device), "(unknown device) 11:22:33");
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
        app.handle_event(Event::LinkStateChanged(LinkState::Scanning));
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
    fn truncate_device_name_backs_off_to_a_utf8_character_boundary_instead_of_panicking() {
        // Code review on pico-link-4vb.4: every prior test used a pure-ASCII
        // name, so the exact bug class the design called out ("a
        // byte-boundary truncation would panic or corrupt on any non-ASCII
        // device name") had zero coverage. U+65E5 ("日") is 3 bytes; 11 of
        // them is 33 bytes, one over MAX_DEVICE_NAME_BYTES (32), and byte 32
        // lands one byte into the 11th character -- exactly the mid-character
        // cut that a naive `&name[..32]` would panic on.
        let name: String = "日".repeat(11);
        assert_eq!(name.len(), 33, "fixture must actually exceed MAX_DEVICE_NAME_BYTES for this test to be meaningful");
        assert!(!name.is_char_boundary(MAX_DEVICE_NAME_BYTES), "fixture must land mid-character at the cut point, or this test proves nothing");

        let truncated = truncate_device_name(&name);

        // A `String` can never hold invalid UTF-8, so the fact this line
        // returned at all (rather than panicking inside the slice) is the
        // real assertion; `chars().count()` re-parsing cleanly is belt and
        // braces confirmation there's no corruption hiding in a `String`
        // built some other way in the future.
        assert_eq!(truncated.chars().count(), 10, "must back off a full character rather than keep a partial one");
        assert_eq!(truncated.len(), 30, "the boundary one character back from byte 32 is byte 30");
        assert!(truncated.len() <= MAX_DEVICE_NAME_BYTES);
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
}
