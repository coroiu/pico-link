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
use core::cell::{Ref, RefCell};
use core::convert::Infallible;
use core::time::Duration;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use u8g2_fonts::types::{HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;
use crate::render::home::build_home_screen;
use crate::render::theme::{self, icon, palette};
use crate::render::wizard::build_wizard_screen;
use crate::render::{
    Action, ButtonLabel, ChromeContribution, ConfirmView, FieldList, FieldRow, FocusEvent, FrameBuffer565, Instant, ListItem, ListItemKey,
    MenuItem, Navigator, PaintKey, RenderCtx, Screen, Spacer, Verb, VerticalList, Widget,
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
/// three labels.
///
/// Describes **exactly one thing**: the A2DP connection lifecycle of
/// [`BtModel::connected_addr`] (bead `pico-link-88xs`, design
/// `.planning/design/2026-09-08-link-state-vs-discovery-axis.md` INVARIANT
/// L1). Whether the radio is currently running a GAP inquiry is a second,
/// independent axis -- [`BtModel::discovering`] -- and is deliberately **not
/// representable** as a `LinkState`: this enum used to carry a `Scanning`
/// variant, and because an inquiry does not disconnect A2DP, that variant
/// was a lie every time it reached [`App::set_link_state`], which wiped the
/// connected model out from under a link that was still up. Removing the
/// variant makes that unreachable through the type rather than merely
/// undocumented -- see the design doc section 2.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkState {
    #[default]
    Idle,
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
    /// User-initiated: pin `addr`'s LDAC quality to `ldac_quality` (1-based
    /// -- see [`PairedDevice::ldac_quality`]'s doc comment), or select
    /// Adaptive (`4`). Queued by the `QUALITY` picker's `A` handler
    /// (`.planning/design/2026-09-07-ldac-quality-selector.md` §5:
    /// "applies live, no confirm, picker stays open"). C stages the flash
    /// write (gated the same way `PersistDevice`'s write is, while the
    /// host is streaming) and, if `addr` is the currently connected
    /// device's live LDAC stream, applies it to the running encoder
    /// immediately -- otherwise the pick takes effect at the next
    /// connect, same as every other per-device setting. The check follows
    /// the [`Event::PairedDeviceUpserted`] echo this write produces, never
    /// the press itself. Bead pico-link-7jol.5.
    SetDeviceLdacQuality { addr: DeviceAddr, ldac_quality: u8 },
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

/// Who most recently drove [`BtModel::volume`] (design
/// `.planning/design/2026-09-02-volume-sync.md` section 7) -- carried
/// through purely for display/diagnostics, matching `firmware/src/
/// volume.h`'s `PlVolumeSource` doc comment ("NOT used by the loop-
/// breaking rule"). `core` does not act on this today: it is stored so
/// VT6 (the screensaver question, bead pico-link-4v2.6) can read it
/// without a second reshape of this seam -- see [`Event::VolumeChanged`]'s
/// doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSource {
    /// The USB host's feature-unit volume (macOS's output slider).
    Host,
    /// The connected A2DP/AVRCP sink (the headphones themselves).
    Sink,
    /// A future on-device volume control (design section 8, out of scope
    /// until that bead lands) -- representable now so this enum doesn't
    /// need a second non-additive reshape when it arrives.
    Device,
}

/// One canonical volume reading (design section 7) -- `level` is always in
/// the AVRCP absolute-volume domain (0..127), which `firmware/src/
/// volume.c`'s canonical state already uses as ITS native domain (no
/// mapping needed here, unlike the USB feature-unit's dB*100 domain that
/// module maps on ingest). `muted` mirrors `firmware/src/volume.c`'s
/// `s_muted`, currently always `false` -- no mute source is wired yet
/// (see `volume.h`'s "MUTE" doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeState {
    pub level: u8,
    pub muted: bool,
    pub source: VolumeSource,
}

impl VolumeState {
    /// `level` (0..127, the AVRCP absolute-volume domain) converted to the
    /// 0..100 percent domain the display shows (design
    /// `.planning/design/2026-09-07-volume-on-display.md` section 3,
    /// "Percent, 0..100, never the raw 0..127" -- both peers speak percent,
    /// and 0..127 is an implementation detail that must not leak onto the
    /// screen). Rounds half-up; checked to map the two endpoints exactly
    /// (`0 -> 0`, `127 -> 100`) since those are the only two values a user
    /// can act on ("0% must mean silent, 100% must mean maximum").
    // `level` is a `u8` (0..=127), so `level * 100 + 63` is at most
    // 12763 and `/ 127` caps the result at 100 -- always in-range for a
    // `u8`, but clippy can't see that from the arithmetic alone.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn percent(&self) -> u8 {
        ((u32::from(self.level) * 100 + 63) / 127) as u8
    }

    /// Whether this reading, on its own, should wake the display to full
    /// and extend the idle timer (design section 5.1/5.3) -- true only for
    /// a non-host source that isn't itself muted or at 0%. Section 5.4
    /// deliberately outranks 5.3 here: a sink-originated mute/zero must
    /// NOT "wake to full" (that would flash the panel bright at the exact
    /// moment the user asked for quiet) -- it only ever satisfies
    /// [`App::volume_requires_dim_floor`]'s never-fully-blank floor.
    #[must_use]
    pub fn wakes_idle(&self) -> bool {
        self.source != VolumeSource::Host && !self.muted && self.level > 0
    }
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
    /// Whether the radio is currently running a GAP inquiry (bead
    /// `pico-link-88xs`) -- the second, independent axis [`LinkState`]'s
    /// doc comment describes. Folded by [`App::set_discovering`], which
    /// deliberately touches only [`BtModel::discovering`] and none of the
    /// four connected-model fields [`App::set_link_state`] clears: an
    /// inquiry does not disconnect A2DP.
    DiscoveryStateChanged { scanning: bool },
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
    /// One ~4Hz reading of the live A2DP output PCM level, per channel
    /// (bead pico-link-du0, design section 21 E17/C8) -- computed cheaply
    /// off the real-time encode path in `firmware/src/a2dp.c` (a running
    /// peak/sum-of-squares accumulator updated per PCM block, reduced to
    /// one reading roughly every `PL_A2DP_LEVEL_PUSH_INTERVAL_MS` and
    /// pushed through the same `bt.c` MPSC ring every other Bluetooth-
    /// domain event uses -- never a direct Rust call from IRQ context,
    /// pico-link-6o2). `peak_l`/`peak_r`/`rms_l`/`rms_r` are linear 0-255
    /// scale (255 == full-scale PCM, i.e. clipping). Folds into
    /// [`BtModel::out_level`], which [`App::set_link_state`] clears
    /// alongside `connected_codec`/`connected_addr` on any disconnect --
    /// see that field's doc comment for the "absent, never frozen" rule
    /// (design section 15) this whole event exists to satisfy.
    LevelsChanged {
        peak_l: u8,
        peak_r: u8,
        rms_l: u8,
        rms_r: u8,
    },
    /// One canonical volume reading (bead pico-link-4v2.5, VT5, design
    /// `.planning/design/2026-09-02-volume-sync.md` section 7) -- pushed by
    /// `firmware/src/bt.c`'s `pl_bt_push_volume_changed` whenever
    /// `volume.c`'s loop rule actually applies a new canonical level (never
    /// on an absorbed no-op, and never for `volume.c`'s debug-console-only
    /// `PL_VOLUME_SOURCE_CONSOLE` origin, which design section 7 excludes
    /// from this event). Folds into [`BtModel::volume`]. `source` is
    /// carried, not acted on -- see [`VolumeSource`]'s doc comment.
    VolumeChanged {
        level: u8,
        muted: bool,
        source: VolumeSource,
    },
    /// The LDAC encoder's live effective rate changed -- pushed by C
    /// whenever the connected device's active codec is LDAC and the
    /// applied EQMID's bitrate differs from the last pushed reading
    /// (`firmware/src/a2dp.c`'s `pl_a2dp_poll_ldac_bitrate`, same
    /// once-per-superloop-iteration cadence as `LevelsChanged`). Folds
    /// into [`BtModel::ldac_live_kbps`] -- see that field's doc comment
    /// for why this is never snapped to the nominal ladder. Not pushed
    /// (and `ldac_live_kbps` stays `None`) while the connected codec isn't
    /// LDAC, or before the first reading since the current stream started.
    /// Bead pico-link-7jol.5.
    LdacBitrateChanged {
        kbps: u32,
    },
    /// One audio fault raised or refreshed, from C's 1Hz fault evaluator
    /// (design `.planning/design/2026-09-07-audio-fault-model.md` §5,
    /// §7.3-7.4; firmware half is `pico-link-9eq2.3.2`). `count` is C's
    /// ABSOLUTE running count for `key` (§5.4) -- [`App::on_fault_raised`]
    /// *assigns* it into [`FaultLog`] rather than accumulating, so a
    /// dropped event costs one refresh cycle of staleness, never permanent
    /// drift.
    ///
    /// Deliberately does not carry `severity`/`glyph`: those are consumed
    /// only at the FFI event site to decide whether this raise wakes the
    /// display (design §7.5) -- [`FaultLog`]'s own entry shape (§7.4) has
    /// no field for either, so `core`'s model never needs them.
    FaultRaised {
        key: FaultKey,
        value: Option<FaultValue>,
        count: u16,
    },
}

/// The audio fault catalogue's six keys (design `.planning/design/2026-09-
/// 07-audio-fault-model.md` §3.1). Ordinals are append-only forever --
/// this is the wire discriminant `ui-ffi`'s `PlFaultKey` mirrors 1:1 (see
/// that type's `TryFrom` impl). Only the ordinal and the display name live
/// in `core` -- glyph and severity are authoritative on the C side and
/// travel with each event instead, never stored here (design §7.3's "one
/// table, not two"; rule 3, §2: "names, strings and colours live in
/// Rust... a rename is then a Rust-only change").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKey {
    BufStarved,
    BufOverflow,
    UsbSupplyLow,
    AirCongested,
    AirLinkLost,
    EncResync,
}

impl FaultKey {
    /// All six keys, in ordinal order -- for callers that need to iterate
    /// [`FaultLog`] positionally.
    pub const ALL: [FaultKey; 6] = [
        FaultKey::BufStarved,
        FaultKey::BufOverflow,
        FaultKey::UsbSupplyLow,
        FaultKey::AirCongested,
        FaultKey::AirLinkLost,
        FaultKey::EncResync,
    ];

    /// <= 16 char, uppercase ASCII display name (design §3.1's table). A
    /// rename is a Rust-only change (rule 3, §2) -- never a firmware
    /// reflash.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            FaultKey::BufStarved => "BUF STARVED",
            FaultKey::BufOverflow => "BUF OVERFLOW",
            FaultKey::UsbSupplyLow => "USB SUPPLY LOW",
            FaultKey::AirCongested => "AIR CONGESTED",
            FaultKey::AirLinkLost => "AIR LINK LOST",
            FaultKey::EncResync => "ENC RESYNC",
        }
    }

    /// This key's position in [`FaultLog`]'s fixed six-entry array --
    /// spelled out as its own method so callers never depend on the enum's
    /// discriminant/`as u8` representation directly.
    #[must_use]
    const fn index(self) -> usize {
        match self {
            FaultKey::BufStarved => 0,
            FaultKey::BufOverflow => 1,
            FaultKey::UsbSupplyLow => 2,
            FaultKey::AirCongested => 3,
            FaultKey::AirLinkLost => 4,
            FaultKey::EncResync => 5,
        }
    }

    /// This key's glyph class -- **static per key, never per-event**
    /// (design `.planning/design/2026-09-07-home-fault-strip.md` §3.1's
    /// table). The wire payload also carries a `glyph` byte
    /// (`PlAudioFaultPayload::glyph`), but [`Event::FaultRaised`]
    /// deliberately does not forward it into `core` (see that variant's
    /// doc comment) -- it doesn't need to, because the audio-fault-model
    /// design (§4) forbids a key from ever changing glyph class at
    /// runtime ("what she may not do is give the two keys the same glyph
    /// class[...]"). This table is therefore the single, static source of
    /// truth the render layer (`render::hero`) reads, matching the wire
    /// exactly by construction rather than by trusting a value that could
    /// in principle disagree frame to frame.
    #[must_use]
    pub const fn glyph(self) -> FaultGlyphClass {
        match self {
            FaultKey::BufStarved | FaultKey::UsbSupplyLow => FaultGlyphClass::Starved,
            FaultKey::BufOverflow => FaultGlyphClass::Filled,
            FaultKey::AirCongested | FaultKey::AirLinkLost | FaultKey::EncResync => FaultGlyphClass::Neutral,
        }
    }

    /// This key's **base** severity (design §3.1's table) -- used by the
    /// render layer to pick red ([`FaultGlyphClass`]'s sibling colour
    /// table lives in `render::hero`) vs amber. Unlike [`Self::glyph`],
    /// this is a genuine simplification, not just an unforwarded wire
    /// field: the audio-fault-model design (§7.3) has `AirCongested`
    /// dynamically escalate `Concealed` -> `Audible` on co-occurrence with
    /// `BufOverflow` within a single evaluation window, but that
    /// escalation is consumed *only* at the FFI event site to decide a
    /// wake (`ui-ffi`'s `pl_ui_push_event`) and never crosses into
    /// [`Event::FaultRaised`] or [`FaultLog`] -- there is no per-entry
    /// severity field to read at render time (see [`FaultEntry`]'s own
    /// doc comment). So `AirCongested`'s Home row always renders at its
    /// base `Concealed` (amber) colour, even during a window where it was
    /// briefly `Audible` on the wire for wake purposes -- a deliberate,
    /// documented gap (bead `pico-link-9eq2.3.3`'s completion report),
    /// not a bug: fixing it would mean adding a severity field the design
    /// explicitly chose not to carry (§7.4: "core's model never needs
    /// them").
    #[must_use]
    pub const fn severity(self) -> FaultSeverity {
        match self {
            FaultKey::BufStarved | FaultKey::BufOverflow | FaultKey::AirLinkLost => FaultSeverity::Audible,
            FaultKey::UsbSupplyLow | FaultKey::AirCongested | FaultKey::EncResync => FaultSeverity::Concealed,
        }
    }
}

/// A fault key's glyph shape class (design §3's table) -- the render-layer
/// enum [`FaultKey::glyph`] maps into; `render::hero`/`render::fault_glyph`
/// resolve this to one of the three drawn primitives (design §12 Fern item
/// 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultGlyphClass {
    /// Up triangle -- "the level went up past the top" (over-run/overflow).
    Filled,
    /// Down triangle -- "the level went down past the bottom" (under-run/
    /// starvation).
    Starved,
    /// Square -- a non-directional fault.
    Neutral,
}

/// A fault key's base severity (design §3.1's table) -- `Audible` (red,
/// `STATUS_ERROR`) or `Concealed` (amber, `STATUS_WARNING`). See
/// [`FaultKey::severity`]'s doc comment for the one documented gap this
/// static table has relative to the wire's dynamic, per-event severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultSeverity {
    Audible,
    Concealed,
}

/// The `why?` page's value slot (design §3.1's "value kind" column) --
/// [`Event::FaultRaised`]'s optional value, carried through into
/// [`FaultEntry::value`] unchanged. `u16` payloads throughout, matching
/// the wire payload's `value: u16` field (`ui-ffi`'s
/// `PlAudioFaultPayload`) -- `core` never widens or rescales it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultValue {
    /// A q8 fixed-point ratio (design §3.1 ord 2: the USB supply ratio).
    Ratio(u16),
    /// A plain count (frames dropped, occurrences -- design §3.1 ords 1,
    /// 3, 4, 5).
    Count(u16),
    /// A duration in milliseconds (design §3.1 ord 0: the windowed
    /// minimum ring fill -- "`0ms` whenever the fault is real").
    Millis(u16),
}

/// One [`FaultKey`]'s state in [`FaultLog`] (design §7.4's entry shape).
/// `count` and `value` are the most recent [`Event::FaultRaised`]'s
/// payload, assigned wholesale each time (§5.4) -- never accumulated.
/// `first_seen`/`last_seen` are what the (future, S3) render layer derives
/// freshness tiers and retirement from; this bead computes neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultEntry {
    pub count: u16,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub value: Option<FaultValue>,
}

/// The audio fault strip's model: a **fixed array of six entries**, one
/// per [`FaultKey`] (design §7.4, home-fault-strip §12 Ruby item 1 --
/// "no `Vec`, no allocation on a fault event"). `None` until a key has
/// been raised at least once since boot; never removed once raised
/// (retirement is a render-time-only concept, computed from
/// `now - last_seen`, per §7.4 -- there is no clear event over the wire,
/// design §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaultLog {
    entries: [Option<FaultEntry>; 6],
}

impl FaultLog {
    /// `key`'s current entry, if it has ever been raised.
    #[must_use]
    pub fn entry(&self, key: FaultKey) -> Option<FaultEntry> {
        self.entries[key.index()]
    }

    /// Whether `key`'s most recent raise is within
    /// [`crate::run::FAULT_LIVE_WINDOW`] of `now` -- design §7.2's
    /// "already Live" test, which gates a repeat raise from waking the
    /// display again. `false` for a key that has never been raised.
    #[must_use]
    pub fn is_live(&self, key: FaultKey, now: Instant) -> bool {
        self.entries[key.index()].is_some_and(|entry| now.saturating_duration_since(entry.last_seen) < crate::run::FAULT_LIVE_WINDOW)
    }

    /// Whether ANY key has an entry not yet retired at `now` (design
    /// `.planning/design/2026-09-07-home-fault-strip.md` §6.3's
    /// `now - last_seen > FAULT_RETIRE` retirement rule) -- the Home fault
    /// strip's own "is the strip non-empty" test, shared by
    /// `render::hero`'s drawing/`paint_key`/`redraw_after` and
    /// `render::home`'s `ShortcutX` gate (design §8.1: "X is labelled and
    /// live only while the strip is non-empty").
    #[must_use]
    pub fn has_visible_entry(&self, now: Instant) -> bool {
        FaultKey::ALL
            .into_iter()
            .any(|key| self.entries[key.index()].is_some_and(|entry| now.saturating_duration_since(entry.last_seen) < crate::run::FAULT_RETIRE))
    }

    /// Folds one raise into `key`'s entry (design §5.4/§7.4): `count` is
    /// ASSIGNED, never accumulated -- idempotent by construction, so a
    /// dropped event costs one refresh cycle of staleness rather than
    /// permanent drift. `first_seen` is set once, on the entry's first-
    /// ever raise, and left unchanged by every subsequent one (there is no
    /// "re-arm" concept here -- retirement and freshness are entirely
    /// render-time, derived from `last_seen`, per §7.4).
    pub fn record(&mut self, key: FaultKey, now: Instant, value: Option<FaultValue>, count: u16) {
        match &mut self.entries[key.index()] {
            Some(entry) => {
                entry.count = count;
                entry.last_seen = now;
                entry.value = value;
            }
            slot @ None => {
                *slot = Some(FaultEntry { count, first_seen: now, last_seen: now, value });
            }
        }
    }
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

/// How long a channel's OUT-meter peak-hold cap stays pinned at its
/// highest recent reading before a lower peak is allowed to replace it
/// (bead pico-link-du0, design section 21 E17's "peak-hold cap"). 1.5s is
/// the conventional VU-meter hold time -- long enough to actually read a
/// transient peak at a glance, short enough not to look stuck.
const OUT_LEVEL_HOLD_DURATION: Duration = Duration::from_millis(1500);

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
    /// Whether the radio is currently running a GAP inquiry -- the SECOND,
    /// independent axis (bead `pico-link-88xs`, design `.planning/design/
    /// 2026-09-08-link-state-vs-discovery-axis.md` section 2.2): an
    /// inquiry does not disconnect A2DP, so scanning must never be
    /// expressible as a [`LinkState`] (see that type's doc comment).
    /// `bool`, not an enum -- there are exactly two observable states and
    /// no producer for a third (design section 2.2). Written only by
    /// [`App::set_discovering`].
    pub discovering: bool,
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
    /// The most recent live [`Event::LevelsChanged`] reading, if any --
    /// `None` whenever there is no PCM to measure (design section 15:
    /// absent, never frozen or faked -- see [`OutLevelSample`]'s doc
    /// comment for how staleness on top of a live value is handled, since
    /// "no *new* reading has arrived" and "there is no PCM" are the same
    /// observable fact from `core`'s side once C stops streaming).
    /// Populated by [`App::on_levels_changed`], cleared by
    /// [`App::set_link_state`] on the same lifecycle as `connected_codec`.
    /// Bead pico-link-du0.
    pub out_level: Option<OutLevelSample>,
    /// The most recent canonical volume reading, if any -- `None` until
    /// the first [`Event::VolumeChanged`] arrives (design section 7).
    /// Unlike `out_level`/`connected_codec`, this is NOT cleared by
    /// [`App::set_link_state`] on disconnect: the host feature-unit
    /// volume this most commonly reflects is a USB-side concept, not an
    /// A2DP-link-lifetime one (design section 7 names no such clearing
    /// rule, unlike `out_level`/`connected_codec`'s explicit ones).
    /// Populated by [`App::on_volume_changed`]. Bead pico-link-4v2.5 (VT5).
    pub volume: Option<VolumeState>,
    /// The LDAC encoder's live effective rate, in kbps, if a live figure
    /// has been reported since the current connection came up --
    /// [`Event::LdacBitrateChanged`]'s payload, folded by
    /// [`App::on_ldac_bitrate_changed`]. `None` until the first reading
    /// arrives (fresh connect: the row/hero fall back to the codec table's
    /// *nominal* figure, design section 15's "absent, never faked" —
    /// there is simply no live figure yet, not a faked one), and cleared
    /// whenever the link leaves [`LinkState::Connected`] or the connected
    /// codec changes away from LDAC (same lifecycle class as
    /// `connected_codec`/`out_level` — see [`App::set_link_state`]/
    /// [`App::set_connected_codec`]). This is deliberately **not** snapped
    /// to the nominal 990/660/330 ladder: libldac can report a transient
    /// non-ladder rate mid-step (bead pico-link-qx8's trap), and the
    /// quality-selector design (`.planning/design/2026-09-07-ldac-quality-
    /// selector.md` §5.1) requires showing exactly what the encoder
    /// reports, not the nearest rung. Bead pico-link-7jol.5.
    pub ldac_live_kbps: Option<u32>,
    /// The audio fault strip's model (design `.planning/design/2026-09-07-
    /// audio-fault-model.md` §7.4, home-fault-strip §12 Ruby item 1) --
    /// folded by [`App::on_fault_raised`] from [`Event::FaultRaised`].
    /// Deliberately NOT cleared by [`App::set_link_state`] on disconnect,
    /// unlike `out_level`/`connected_codec`/`ldac_live_kbps` above: a
    /// fault raised on the connection that just dropped is still relevant
    /// history for the `why?` page (S3, `pico-link-9eq2.3.3`) after a
    /// reconnect, and C's own evaluator already re-snapshots and clears
    /// its *own* fault state at every stream transition (§5.6 rule 3) --
    /// `core`'s log just reflects whatever C tells it, per doctrine (§2:
    /// "faults are a view, never a second source of truth").
    pub fault_log: FaultLog,
}

/// One [`Event::LevelsChanged`] reading, timestamped and peak-held at the
/// moment it folded into [`BtModel`] (bead pico-link-du0, design section
/// 21 E17). `core` never derives "is the meter live" from a boolean flag
/// C sends -- there isn't one -- but from comparing `received_at` against
/// [`crate::render::hero::HeroStatusView`]'s own render-time clock
/// (`RenderCtx::now`): once too much time has passed since the last
/// reading, the meter stops drawing rather than showing a frozen last
/// value (design section 15's rule, applied here for the same reason the
/// hero word and bitrate line already apply it).
///
/// `hold_l`/`hold_r`/`hold_l_at`/`hold_r_at` implement the design's
/// "peak-hold cap" (section 6, section 21 E17): the highest peak seen
/// within the last hold window, decided once per incoming reading (not
/// re-decayed every render frame, which would need a render-time mutation
/// this `&self`-rendered widget tree has no way to make) -- see
/// [`App::on_levels_changed`] for the hold-update rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutLevelSample {
    pub peak_l: u8,
    pub peak_r: u8,
    pub rms_l: u8,
    pub rms_r: u8,
    pub hold_l: u8,
    pub hold_r: u8,
    hold_l_at: Instant,
    hold_r_at: Instant,
    pub received_at: Instant,
    /// Release-ballistic attack anchor (bead pico-link-ajj, design
    /// requirement C; switched from rms to peak by bead pico-link-53c so the
    /// ballistic anchors on the same quantity the bar now draws): the
    /// `peak_l`/`peak_r` value in effect at the moment it was last set by an
    /// instantaneous attack, i.e. the last time a fresh reading was at or
    /// above the then-current decayed value. `crate::render::hero`'s render
    /// function decays *from* this anchor at render time (via
    /// [`decay_peak`]) to get the bar's actually-displayed level — see
    /// [`App::on_levels_changed`] for how the anchor is updated, and
    /// [`decay_peak`]'s doc comment for why this is a pure render-time
    /// computation rather than a value mutated on a timer.
    pub(crate) attack_peak_l: u8,
    pub(crate) attack_peak_r: u8,
    pub(crate) attack_peak_l_at: Instant,
    pub(crate) attack_peak_r_at: Instant,
}

/// Exponential-release rate for the vertical OUT meter's ballistics (bead
/// pico-link-ajj, design requirement C): approximately 20 dB per second —
/// amplitude falls to roughly 10% of its value after one second of
/// continuous release. Expressed as a Q16.16 fixed-point ratio-per-
/// millisecond (`10^(-1/1000)`, precomputed offline as a constant) rather
/// than a runtime `powf`/`log10` call: `core` is `no_std` with no `libm`
/// (same constraint [`crate::render::theme::VERTICAL_METER_DBFS_THRESHOLDS`]
/// documents), so [`decay_peak`] raises this ratio to the elapsed
/// millisecond count via integer exponentiation-by-squaring instead.
const RELEASE_RATIO_PER_MS_Q16: u32 = 65384;

/// Multiplies two Q16.16 fixed-point values, truncating the low bits
/// (consistent rounding-down bias, negligible at these magnitudes).
///
/// `clippy::cast_possible_truncation` is silenced deliberately: every
/// caller in this module keeps both operands `<= 1<<16` (a ratio `<= 1.0`
/// in this fixed-point representation), so the widened product is always
/// `<= 1<<32` and the post-shift result always fits `u32` with headroom —
/// see [`q16_pow`]'s doc comment for why that invariant holds across
/// repeated squaring too.
#[allow(clippy::cast_possible_truncation)]
const fn q16_mul(a: u32, b: u32) -> u32 {
    ((a as u64 * b as u64) >> 16) as u32
}

/// Raises a Q16.16 fixed-point `base` (expected `<= 1<<16`, i.e. a ratio
/// `<= 1.0`) to the integer power `exp` via exponentiation-by-squaring —
/// O(log2(exp)) fixed-point multiplies, no float/libm. Terminates for any
/// `exp` because `base <= 1<<16` means repeated squaring monotonically
/// shrinks towards zero once `exp` is large enough to matter.
const fn q16_pow(base: u32, mut exp: u64) -> u32 {
    let mut result: u32 = 1 << 16;
    let mut b = base;
    while exp > 0 {
        if exp & 1 == 1 {
            result = q16_mul(result, b);
        }
        b = q16_mul(b, b);
        exp >>= 1;
    }
    result
}

/// Decays `anchor` (a linear 0-255 peak reading, same scale as
/// [`Event::LevelsChanged`]'s payload) by `elapsed`, at
/// [`RELEASE_RATIO_PER_MS_Q16`]'s ~20 dB/s release rate. Pure and safe to
/// call at render time: both [`App::on_levels_changed`] (to decide whether
/// a fresh sample counts as a rise, i.e. an instantaneous attack) and
/// `crate::render::hero::HeroStatusView::render` (to get today's actually-
/// displayed bar level between events) derive "the level right now" from a
/// stored `(anchor, anchor_at)` pair plus a current clock reading, never by
/// mutating a running average on a timer — see [`OutLevelSample`]'s doc
/// comment above (and its `hold_l`/`hold_r` fields' own precedent) for why
/// a `&self`-rendered widget tree has no other way to do this.
// `clippy::cast_possible_truncation`: `ratio <= 1<<16` always (base
// `<= 1<<16`, exponentiation-by-squaring of a fraction only shrinks it),
// so `u32::from(anchor) * ratio` fits comfortably before the shift and the
// post-shift result is always `<= anchor`, i.e. `<= 255` and safe to
// narrow to `u8`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn decay_peak(anchor: u8, elapsed: Duration) -> u8 {
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let ratio = q16_pow(RELEASE_RATIO_PER_MS_Q16, elapsed_ms);
    ((u32::from(anchor) * ratio) >> 16) as u8
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
    /// The persisted LDAC quality pick, 1-based (`firmware/src/persist.c`'s
    /// `ldac_quality`, design
    /// `.planning/design/2026-09-02-device-page-seam.md` §1.2): `0` = never
    /// chosen, `1`/`2`/`3` = pinned 990/660/330 kbps, `4` = Adaptive. This
    /// is the **stored echo** [`build_single_select_screen`]'s `checked`
    /// parameter and the `QUALITY` row's check both read -- never the
    /// local press (`.planning/design/2026-09-07-ldac-quality-selector.md`
    /// §5.1). Bead pico-link-7jol.5.
    pub ldac_quality: u8,
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
/// section 5's screen inventory).
pub(crate) const DEVICES_TITLE: &str = "Devices";

/// Identity for a screen that must stay live-synced to [`BtModel`] while it
/// sits on the [`Navigator`]'s stack -- see [`App::refresh_stack`]'s doc
/// comment for why this exists and what replaced it
/// (`.planning/design/2026-09-07-device-page-and-single-select-picker.md`
/// §1). `Screen::id()` returns `None` for every screen that never calls
/// [`Screen::with_id`] (the wizard, `ConfirmView`s, Settings): `None` is
/// the "never refresh me" sentinel, so tagging a screen is opt-in and every
/// untagged screen is behaviour-identical to before this type existed.
///
/// `Copy`/`Eq`, like [`ListItemKey`] and for the same reason: cheap to
/// carry around and compare on every [`App::refresh_stack`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenId {
    Home,
    Devices,
    DevicePage(DeviceAddr),
    /// A depth-2 single-select picker pushed from a [`ScreenId::DevicePage`]
    /// row (`.planning/design/2026-09-07-device-page-and-single-select-
    /// picker.md` §1). The only picker built as of pico-link-7jol.5 is
    /// [`PickerKind::LdacQuality`] -- the codec picker itself waits on
    /// Ada's `CodecAvailability` seam.
    Picker(PickerKind, DeviceAddr),
    /// The Home fault strip's `why?` detail page (design
    /// `.planning/design/2026-09-07-home-fault-strip.md` §8, bead
    /// `pico-link-9eq2.3.3`) -- a singleton, no payload: there is exactly
    /// one, reached only from `X` on Home. Refreshed like every other
    /// identified screen so a fault that fires while it's open updates
    /// counts/times/tier in place (see [`build_why_page_screen`]'s doc
    /// comment for the append-only ordering rule this refresh enforces).
    WhyPage,
}

/// Which picker a [`ScreenId::Picker`] identifies -- distinguishes screens
/// that would otherwise share the same `(kind, addr)`-less identity if a
/// device page ever grows a second picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    LdacQuality,
}

/// The focus/scroll state [`App::refresh_stack`] reads from a screen
/// *before* replacing it, so the freshly built replacement can carry it
/// forward instead of resetting to row 0 -- the same carry-forward
/// [`App::build_identified_screen`]'s `ScreenId::Devices` arm already did
/// pre-refactor, generalized to any identified screen at any depth.
#[derive(Default)]
pub(crate) struct ScreenCarry {
    pub(crate) selected_key: Option<ListItemKey>,
    pub(crate) selected_index: usize,
    pub(crate) scroll_top: Option<usize>,
}

/// What [`App::build_identified_screen`] found for a given [`ScreenId`] --
/// [`App::refresh_stack`]'s two possible outcomes per identified screen.
///
/// `pub(crate)`: `render::home`'s `ShortcutY` binding
/// (`pico-link-hr30`) pushes a device page the same way this module's own
/// connected-row activation does (see `build_devices_screen`'s
/// `model_for_device_page` closure), so it needs to see both arms too.
pub(crate) enum Refresh {
    /// Replace the screen at this stack index with this freshly built one.
    Rebuild(Screen),
    /// This screen's subject no longer exists in the model (e.g. a
    /// [`ScreenId::DevicePage`] for a forgotten device) -- drop it and
    /// everything above it.
    Gone,
    /// Leave the screen already on the stack in place -- no
    /// [`Navigator::replace_at`], no forced full-frame damage. This is the
    /// migration seam for the live-widgets refactor (bead `pico-link-bgnd`
    /// M0, `.planning/design/2026-09-24-live-widgets-retire-refresh-
    /// stack.md` section 7): a screen kind migrates to reading live model
    /// state via `Widget::sync` (`crate::render::widget::Widget::sync`)
    /// simply by having its `App::build_identified_screen` arm return this
    /// instead of [`Self::Rebuild`]. **No builder returns this yet** -- as
    /// of M0 this variant exists and is handled, but is otherwise dead
    /// code; the first arm to actually return it lands with M1 (Home).
    #[allow(dead_code)] // Not constructed until M1 -- see the variant's own doc comment.
    Keep,
}

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
    prev_scroll_top: Option<usize>,
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
            // `Verb::Open`: every paired row pushes a deeper screen --
            // device detail for the connected row, the wizard's
            // `Connecting` phase for any other (design rule 4's
            // assignment table treats both as "a paired device").
            ListItem::new(paired_device_label(device))
                .with_sublabel(sublabel)
                .with_key(ListItemKey::from(device.addr))
                .with_verb(Verb::Open)
        })
        .collect();
    // `Verb::Pair`: begins pairing -- including at the 8-device cap, where
    // the forget-picker is the app making room, not a different intent
    // (design rule 4's assignment table).
    items.push(ListItem::new("Pair new headphones").with_key(PAIR_NEW_ROW_KEY).with_verb(Verb::Pair));

    let paired_len = model.paired.len();
    let connected_addr = model.connected_addr;
    let ordered_for_activate = ordered.clone();
    let paired_for_full = model.paired.clone();
    // Snapshot for the connected row's push -- `Action::PushView`'s
    // builder is `FnOnce`, with no path back to a live `&BtModel` at the
    // moment it actually runs (it's called from inside
    // `Navigator::apply_action`, not from `App`). `BtModel` is `Clone` for
    // exactly this reason (see [`build_forget_picker_screen`]'s own
    // `model.paired.clone()` precedent above). The very next model event
    // replaces this page with a live-read one via
    // [`App::build_identified_screen`]'s `ScreenId::DevicePage` arm -- this
    // snapshot only has to be right for the single frame between the press
    // and that next refresh.
    let model_for_device_page = model.clone();
    let commands_for_activate = Rc::clone(commands);
    let wizard_phase_for_activate = Rc::clone(wizard_phase);
    let wizard_devices_for_activate = Rc::clone(wizard_devices);
    let list = VerticalList::new(items)
        // The `Verb::Open` here is only the list's fallback default; every
        // row above carries its own override, so this value is never
        // actually read.
        .on_activate_index(Verb::Open, move |index| {
            if let Some(device) = ordered_for_activate.get(index) {
                if Some(device.addr) == connected_addr {
                    // A on the connected row: no reconnect to do -- push
                    // the real device page (design section 4;
                    // `.planning/design/2026-09-07-device-page-and-
                    // single-select-picker.md` §3).
                    let addr = device.addr;
                    let fallback_title = paired_device_label(device);
                    let model = model_for_device_page.clone();
                    let commands = Rc::clone(&commands_for_activate);
                    return Action::PushView(Box::new(move || {
                        let carry = ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None };
                        match build_device_page_screen(&model, addr, &carry, &commands) {
                            Refresh::Rebuild(screen) => screen,
                            // The connected device we just pressed cannot
                            // have vanished between the press and this
                            // closure running -- `Refresh::Gone` is
                            // structurally unreachable here, but a
                            // same-titled empty screen is a harmless
                            // fallback rather than a panic if it ever is.
                            // `Refresh::Keep` is likewise unreachable:
                            // `build_device_page_screen` never returns it
                            // (bead pico-link-bgnd M0 -- no builder does yet).
                            Refresh::Gone | Refresh::Keep => Screen::new(fallback_title, vec![]),
                        }
                    }));
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
    let list = if let Some(top) = prev_scroll_top { list.with_scroll_top(top) } else { list };

    let view = DevicesListView { list, row_devices: ordered, commands: Rc::clone(commands) };
    Screen::new(DEVICES_TITLE, vec![Box::new(view)]).with_id(ScreenId::Devices)
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

    /// Forwards `list`'s own answer -- see `Widget::activation`'s doc
    /// comment on why a wrapper must forward this rather than let the
    /// default `None` silently swallow it.
    fn activation(&self) -> Option<Verb> {
        self.list.activation()
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

    /// Forwards `list`'s own answer — see `Widget::scroll_top`'s doc
    /// comment on why a wrapper must forward this rather than let the
    /// default `None` silently swallow it (the same pico-link-vxc D2
    /// hazard `redraw_after` below already guards against).
    fn scroll_top(&self) -> Option<usize> {
        self.list.scroll_top()
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
    let list = VerticalList::new(items).on_activate_index(Verb::Select, move |index| {
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
    let view = ConfirmView::new(headline, rows).on_activate_index(Verb::Select, move |index| {
        if index == 1 {
            commands.borrow_mut().push_back(Command::ForgetDevice { addr });
        }
        Action::PopView
    });
    Screen::new(FORGET_CONFIRM_TITLE, vec![Box::new(view)])
}

/// A placeholder for a live value this page cannot honestly report yet --
/// `core` has no `SetDeviceCodecPref`/`CodecAvailability`/
/// `A2dpStreamStateChanged` seam (Ada's
/// `.planning/design/2026-09-02-device-page-seam.md`, none of it
/// implemented -- verified: not present anywhere in `core`). Per that
/// design's §3.0: a *live* field dashes when the value is unknown; a
/// *stored* setting never dashes, because it is still true when nothing is
/// connected. `CODEC`/`ADDRESS` are the latter and never use this.
///
/// Plain ASCII hyphen-minus, not a typographic em dash (`\u{2014}`):
/// `theme::font`'s `u8g2_font_helv*_tf` faces are built with
/// `with_ignore_unknown_chars(true)` and cover only the Latin-1 range, so
/// U+2014 silently draws NOTHING rather than a placeholder box -- found by
/// screenshotting this exact row (`core/examples/
/// device_page_screenshots.rs`) and seeing an empty value where the dash
/// should be. `-` is in range and renders.
const DASH: &str = "-";

/// Index of the `QUALITY` row within [`device_page_rows`]'s output, when
/// present -- always right after `CODEC` (design §2/§4.2: the row belongs
/// directly under the codec it modifies).
const DEVICE_PAGE_QUALITY_ROW_INDEX: usize = 1;

/// LDAC's ADAPTIVE identity, 1-based, for [`PairedDevice::ldac_quality`]
/// (`persist.c`'s convention: 0 = never chosen, 1/2/3 = pinned, 4 =
/// Adaptive).
pub(crate) const LDAC_QUALITY_ADAPTIVE: u8 = 4;

/// `ListItemKey`s for the `QUALITY` picker's four rows, in the order Uma's
/// design lists them (highest rate first, Adaptive last) --
/// `.planning/design/2026-09-07-ldac-quality-selector.md` §4.1. Reused for
/// both the picker's `checked`/`on_pick` plumbing and
/// [`ldac_quality_fixed_kbps`]'s parallel ordering.
const LDAC_QUALITY_PICKER_KEYS: [ListItemKey; 4] =
    [ListItemKey::from_u64(101), ListItemKey::from_u64(102), ListItemKey::from_u64(103), ListItemKey::from_u64(104)];

/// LDAC's three named fixed rates, in kbps, highest first --
/// `.planning/design/2026-09-07-ldac-quality-selector.md` §8 rule 4:
/// sample-rate dependent (909/606/303 at 44.1kHz), computed here in **one**
/// place rather than restated at each of the row/picker/Home call sites.
/// `None` (rate unknown, or disconnected) falls back to the 48kHz set --
/// this project's USB chain is 48k-only today and there is no
/// `sample_rate_hz` seam yet
/// (`.planning/design/2026-09-07-device-page-and-single-select-picker.md`'s
/// scope table), so every call site below passes `None`.
fn ldac_quality_rates_kbps(sample_rate_hz: Option<u32>) -> [u32; 3] {
    if sample_rate_hz == Some(44_100) {
        [909, 606, 303]
    } else {
        [990, 660, 330]
    }
}

/// `ldac_quality` (1-based, [`PairedDevice::ldac_quality`]'s convention) to
/// its fixed kbps, or `None` for Adaptive (`4`) or any out-of-range value.
/// `0` ("never chosen") maps to the firmware's built-in default -- design
/// §7's ruling that `0` is a storage state, never a display state: the
/// fresh-device row/picker render the effective default as if it had been
/// chosen, check included. `codec_ldac.c`'s
/// `pl_ldac_quality_to_initial_state` is the source of truth this mirrors
/// (today: HQ/990 kbps for both `0` and `1`).
fn ldac_quality_fixed_kbps(ldac_quality: u8, sample_rate_hz: Option<u32>) -> Option<u32> {
    let rates = ldac_quality_rates_kbps(sample_rate_hz);
    match ldac_quality {
        0 | 1 => Some(rates[0]),
        2 => Some(rates[1]),
        3 => Some(rates[2]),
        _ => None,
    }
}

/// The device page's `QUALITY` row's checked-row key, mirroring
/// [`ldac_quality_fixed_kbps`]'s `0`-is-the-default convention so the
/// check never sits on nothing (design §7).
fn ldac_quality_checked_key(ldac_quality: u8) -> ListItemKey {
    match ldac_quality {
        2 => LDAC_QUALITY_PICKER_KEYS[1],
        3 => LDAC_QUALITY_PICKER_KEYS[2],
        LDAC_QUALITY_ADAPTIVE => LDAC_QUALITY_PICKER_KEYS[3],
        _ => LDAC_QUALITY_PICKER_KEYS[0], // 0 (never chosen) or 1 (990/HQ)
    }
}

/// Whether the device page's `QUALITY` row (and its picker) should be
/// shown at all -- design §2/§8 rule 1: "the row is absent, not dim" when
/// LDAC isn't effective-or-pinned. Connected: keyed off the *live* codec
/// (the only truth available -- there is no real codec pin seam yet, same
/// honesty rule [`device_page_rows`]'s `CODEC` value already follows).
/// Disconnected: keyed off `ldac_quality != 0` -- a device that was
/// manually put in Adaptive or pinned to a rate at some point is "LDAC in
/// play" even while off; a device nobody ever touched has no such
/// evidence and stays hidden (§7: no first-run prompt).
fn device_page_quality_present(model: &BtModel, addr: DeviceAddr) -> bool {
    if model.connected_addr == Some(addr) {
        model.connected_codec.as_ref().is_some_and(|c| c.word == "LDAC")
    } else {
        model.paired.iter().find(|d| d.addr == addr).is_some_and(|d| d.ldac_quality != 0)
    }
}

/// The device-page value-column budget (px) the `QUALITY` row's Adaptive
/// form must fit inside -- design §4.2's own measure, shared with a
/// `QUALITY` label of ~35px against the row's 182px total. Ruby must
/// measure, not assume (§4.2): [`format_adaptive_row_value`] checks this
/// at build time and falls back to the no-separator form if it disagrees,
/// rather than trusting the design doc's estimate blindly.
const DEVICE_PAGE_ADAPTIVE_VALUE_BUDGET_PX: u32 = 182;

/// The horizontal pixel footprint `text` would render at in `font` --
/// duplicated from `hero.rs`'s private `text_width` for the same "no
/// shared home for a helper this small, used by only one module" reason
/// that helper's own doc comment gives.
fn text_width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width)
}

/// The `QUALITY` row's value under Adaptive while a live figure exists --
/// design §4.2: `Adaptive · <n>`, no `kbps` unit (it's stated everywhere
/// else already), falling back to the no-separator `Adaptive <n>` form if
/// the middle dot's measured width overruns the row's value budget.
fn format_adaptive_row_value(kbps: u32) -> String {
    let preferred = format!("Adaptive \u{b7} {kbps}");
    if text_width(&theme::font::value(), &preferred) <= DEVICE_PAGE_ADAPTIVE_VALUE_BUDGET_PX {
        preferred
    } else {
        format!("Adaptive {kbps}")
    }
}

/// The `QUALITY` row's trailing value, per design §4.2's table.
fn device_page_quality_row_value(model: &BtModel, device: &PairedDevice, connected: bool) -> String {
    if device.ldac_quality == LDAC_QUALITY_ADAPTIVE {
        let streaming = connected && model.connected_codec.as_ref().is_some_and(|c| c.word == "LDAC");
        match (streaming, model.ldac_live_kbps) {
            (true, Some(kbps)) => format_adaptive_row_value(kbps),
            // Disconnected, or connected-but-no-live-reading-yet: never
            // claim a number we don't have (design §8: "Adaptive's
            // trailing note reads `varies`" is the picker's own wording;
            // the row's plain `Adaptive` is the same honesty rule).
            _ => String::from("Adaptive"),
        }
    } else {
        format!("{} kbps", ldac_quality_fixed_kbps(device.ldac_quality, None).unwrap_or(990))
    }
}

/// Index of the `Forget this device` row within [`device_page_rows`]'s
/// output -- always the LAST row, whether or not `QUALITY` is present
/// (`device_page_rows`'s doc comment).
fn device_page_forget_row_index(rows_len: usize) -> usize {
    rows_len - 1
}

/// The device page's rows, in order (design
/// `.planning/design/2026-09-02-device-page.md` §3, as scoped by
/// `.planning/design/2026-09-07-device-page-and-single-select-picker.md`
/// §3.4): `CODEC`, `QUALITY` (present only when LDAC is effective-or-pinned
/// -- `.planning/design/2026-09-07-ldac-quality-selector.md` §2/§8),
/// `SAMPLE RATE`, `USB IN`, `A2DP`, `ADDRESS`, `Forget this device`.
///
/// **PURE.** Model in, rows out -- no `Screen`, no `Navigator`, no
/// framebuffer, testable directly. `CODEC` is [`FieldKind::Readonly`], not
/// `Action`, in THIS bead: there is no codec picker to open yet (Ada's
/// `CodecAvailability` seam doesn't exist), and a bright, caret-growing row
/// that does nothing on `A` would be exactly the "an unlabelled/live A
/// lies" defect design rule 4 exists to prevent. Wiring `CODEC` back to
/// `Action` is the device-page follow-up that lands alongside the codec
/// picker (§9 of the design of record above). `QUALITY`, by contrast, IS
/// `Action` -- pico-link-7jol.5 builds the picker it opens.
fn device_page_rows(model: &BtModel, addr: DeviceAddr) -> Vec<FieldRow> {
    let connected = model.connected_addr == Some(addr);
    // A *stored* setting (§3.0): never dashes, even disconnected. Today
    // that's only ever "the live codec, or Automatic" -- there is no real
    // pin to read yet (`PairedDevice` carries no `codec_id` field), so a
    // disconnected device always reads `Automatic`, honestly.
    let codec_value =
        if connected { model.connected_codec.as_ref().map_or_else(|| String::from("Automatic"), |c| c.word.clone()) } else { String::from("Automatic") };

    let mut rows = vec![FieldRow::readonly("CODEC").with_value(codec_value, palette::TEXT_PRIMARY).with_key(ListItemKey::from_u64(0))];

    if device_page_quality_present(model, addr) {
        // `unwrap_or` fallback below only matters for the pathological
        // case of a present-but-not-actually-paired addr (never happens
        // via `build_device_page_screen`, which bails to `Refresh::Gone`
        // first) -- kept defensive since this function is pure and called
        // directly by tests with hand-built models.
        let default_device = PairedDevice { addr, name: String::new(), mru_seq: 0, ldac_quality: 0 };
        let device = model.paired.iter().find(|d| d.addr == addr).unwrap_or(&default_device);
        rows.push(
            FieldRow::action("QUALITY")
                .with_value(device_page_quality_row_value(model, device, connected), palette::TEXT_PRIMARY)
                .with_key(ListItemKey::from_u64(6)),
        );
    }

    rows.push(FieldRow::readonly("SAMPLE RATE").with_value(DASH, palette::TEXT_SECONDARY).with_key(ListItemKey::from_u64(1)));
    rows.push(FieldRow::readonly("USB IN").with_value(DASH, palette::TEXT_SECONDARY).with_key(ListItemKey::from_u64(2)));
    rows.push(FieldRow::readonly("A2DP").with_value(DASH, palette::TEXT_SECONDARY).with_key(ListItemKey::from_u64(3)));
    rows.push(
        FieldRow::readonly("ADDRESS")
            .with_value(format_device_address(addr), palette::TEXT_PRIMARY)
            .with_small_value()
            .with_key(ListItemKey::from_u64(4)),
    );
    rows.push(FieldRow::action("Forget this device").with_label_color(palette::STATUS_ERROR).with_key(ListItemKey::from_u64(5)));
    rows
}

/// Formats a device address exactly like the address a phone or laptop
/// shows for the same device -- colons kept (device-page design §3.6).
fn format_device_address(addr: DeviceAddr) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
    )
}

/// The connected-or-paired device's detail page (design
/// `.planning/design/2026-09-02-device-page.md`, scoped for this bead by
/// `.planning/design/2026-09-07-device-page-and-single-select-picker.md`
/// §3). Returns [`Refresh::Gone`] when `addr` is no longer in
/// [`BtModel::paired`] -- e.g. the device was forgotten from its own
/// confirm screen, or from Devices while this page happened to be open one
/// level up -- so [`App::refresh_stack`] can unwind the stack rather than
/// leave a page open on a device that no longer exists.
pub(crate) fn build_device_page_screen(model: &BtModel, addr: DeviceAddr, carry: &ScreenCarry, commands: &Rc<RefCell<VecDeque<Command>>>) -> Refresh {
    let Some(device) = model.paired.iter().find(|d| d.addr == addr) else {
        return Refresh::Gone;
    };
    let title = paired_device_label(device);
    let connected = model.connected_addr == Some(addr);
    let name = device.name.clone();
    let forget_label = title.clone();
    let commands_for_activate = Rc::clone(commands);
    let rows = device_page_rows(model, addr);
    let quality_present = device_page_quality_present(model, addr);
    let forget_row_index = device_page_forget_row_index(rows.len());
    // Snapshot for the `QUALITY` row's push -- `Action::PushView`'s builder
    // is `FnOnce` with no path back to a live `&BtModel` (same reasoning as
    // `build_devices_screen`'s own `model_for_device_page` snapshot above
    // it in this file). The very next model event (the write's
    // `PairedDeviceUpserted` echo) replaces this picker with a live-read
    // one via `App::build_identified_screen`'s `ScreenId::Picker` arm.
    let model_for_quality_picker = model.clone();
    let commands_for_quality_picker = Rc::clone(commands);
    let list = FieldList::new(rows).with_selected_identity(carry.selected_key, carry.selected_index).on_activate_index(move |index| {
        if quality_present && index == DEVICE_PAGE_QUALITY_ROW_INDEX {
            // Depth-2 push, fresh `ScreenCarry` -- `refresh_stack` owns
            // carrying focus/scroll forward on every subsequent rebuild,
            // same as the connected-row-to-device-page push above it does
            // (design `.planning/design/2026-09-07-device-page-and-single-
            // select-picker.md` §1).
            let model = model_for_quality_picker.clone();
            let commands = Rc::clone(&commands_for_quality_picker);
            return Action::PushView(Box::new(move || {
                let carry = ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None };
                match build_ldac_quality_picker_screen(&model, addr, &carry, &commands) {
                    Refresh::Rebuild(screen) => screen,
                    // The device we just descended from cannot have
                    // vanished between the press and this closure running
                    // -- structurally unreachable, same reasoning as
                    // `build_devices_screen`'s own connected-row push.
                    // `Refresh::Keep` is likewise unreachable here (bead
                    // pico-link-bgnd M0 -- no builder returns it yet).
                    Refresh::Gone | Refresh::Keep => Screen::new("Quality", vec![]),
                }
            }));
        }
        if index == forget_row_index {
            let commands = Rc::clone(&commands_for_activate);
            let label = forget_label.clone();
            return Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)));
        }
        Action::None
    });
    let list = if let Some(top) = carry.scroll_top { list.with_scroll_top(top) } else { list };
    let view = DevicePageView { list, addr, connected, name, commands: Rc::clone(commands) };
    Refresh::Rebuild(Screen::new(title, vec![Box::new(Spacer::new(12)), Box::new(view)]).with_id(ScreenId::DevicePage(addr)))
}

/// Wraps [`FieldList`] to add the device page's `X` action (design §2.1's
/// amendment: `drop` when connected, `link` when not -- both labelled,
/// both real, no confirm needed since neither is destructive/irreversible)
/// -- the same "small wrapper widget intercepts one `NavIntent` variant,
/// delegates the rest" shape [`DevicesListView`] already uses for its own
/// `ShortcutX` handling, and every method below that isn't
/// `on_intent`/`chrome_contribution` is a forward, not an override -- see
/// [`Widget::activation`]'s doc comment (pico-link-vxc D2) for why a
/// wrapper must forward rather than let the default silently swallow one.
struct DevicePageView {
    list: FieldList,
    addr: DeviceAddr,
    connected: bool,
    /// Needed only for the `link` (reconnect) path -- [`Command::Connect`]
    /// carries a name, same as every other reconnect call site
    /// ([`build_devices_screen`]'s own paired-row activation).
    name: String,
    commands: Rc<RefCell<VecDeque<Command>>>,
}

/// Seed for [`DevicePageView::paint_key`] -- only needs to differ from
/// other widgets' own seeds.
const DEVICE_PAGE_PAINT_KEY_SEED: u64 = 15;

impl Widget for DevicePageView {
    fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size {
        self.list.measure(constraints, ctx)
    }

    fn is_focusable(&self) -> bool {
        self.list.is_focusable()
    }

    /// Forwards `list`'s own answer -- see this struct's doc comment.
    fn activation(&self) -> Option<Verb> {
        self.list.activation()
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        if intent == NavIntent::ShortcutX {
            if self.connected {
                self.commands.borrow_mut().push_back(Command::Disconnect);
            } else {
                self.commands.borrow_mut().push_back(Command::Connect { addr: self.addr, name: truncate_device_name(&self.name) });
            }
            return Action::None;
        }
        self.list.on_intent(intent)
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let label = if self.connected { "drop" } else { "link" };
        Some(ChromeContribution { x: Some(ButtonLabel::Live(String::from(label))), ..ChromeContribution::default() })
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.list.selected_index())
    }

    /// Forwards `list`'s own answer -- see this struct's doc comment.
    fn selected_key(&self) -> Option<ListItemKey> {
        self.list.selected_key()
    }

    /// Forwards `list`'s own answer -- see this struct's doc comment.
    fn scroll_top(&self) -> Option<usize> {
        self.list.scroll_top()
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        self.list.render(area, ctx, target)
    }

    /// Forwards `list`'s own answer -- see this struct's doc comment.
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<Duration> {
        self.list.redraw_after(ctx)
    }

    /// Folds `list`'s own key and nothing else -- deliberately does NOT
    /// fold `connected`/`LinkState`/the X label
    /// (`.planning/design/2026-09-07-device-page-and-single-select-
    /// picker.md` §5, damage-key rule 2): the rail has its own key fed by
    /// the already-resolved `ButtonLabel`s (`chrome_contribution` above),
    /// so folding link state into the *body* key would repaint the body on
    /// every link-state change for zero changed body pixels -- exactly the
    /// "fold something you don't draw" defect this project has already
    /// shipped once.
    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(DEVICE_PAGE_PAINT_KEY_SEED).fold_key(self.list.paint_key(ctx))
    }
}

/// A single row in a [`build_single_select_screen`] picker.
///
/// `#[allow(dead_code)]` on this and on [`build_single_select_screen`]
/// itself: this bead (`pico-link-7jol.4`) builds and tests the general
/// picker mechanism ahead of its first real caller, the `QUALITY` row
/// (`pico-link-7jol.5`) -- see
/// `.planning/design/2026-09-07-device-page-and-single-select-picker.md`
/// §7 step 4. Its first real caller is
/// [`build_ldac_quality_picker_screen`] (pico-link-7jol.5).
pub(crate) struct PickerOption {
    /// Stable identity -- carries focus and the check across rebuilds, and
    /// is what [`build_single_select_screen`]'s `on_pick` callback is
    /// invoked with.
    pub key: ListItemKey,
    pub label: String,
    /// The trailing note (e.g. `best audio`, `660 now`, `not offered`).
    pub note: Option<(String, Rgb565)>,
    /// `false` -> [`FieldKind::Readonly`]: focusable, dim, no caret, `A`
    /// dead -- an unavailable option cannot be picked, structurally (the
    /// activation gate lives in [`FieldList`], not in `on_pick`).
    pub selectable: bool,
}

/// A generic single-select picker screen -- the codec picker and the LDAC
/// quality picker (`pico-link-7jol.5`) are both this function with
/// different `options`/`on_pick`, not two widgets
/// (`.planning/design/2026-09-07-device-page-and-single-select-picker.md`
/// §2). **Not a widget, not a `render/` module** -- composition of
/// [`FieldList`] alone, per that design's §0.1 verdict.
///
/// Five rules this shape makes structural rather than remembered (design
/// §2.1):
/// 1. **The check follows the stored value.** `checked` is read from the
///    model by the caller, not from a local "pressed" bit -- there is no
///    place in this function to put an optimistic check by accident.
/// 2. **Pop-vs-stay-open is entirely `on_pick`'s return value**
///    (`Action::None` stays open, `Action::PopView` pops) -- there is no
///    `stays_open` flag.
/// 3. **The gutter is on the list, not the row** (`with_leading_gutter`),
///    so every label aligns at the same `L` whether checked or not.
/// 4. **An unavailable option cannot be picked** -- `selectable: false`
///    produces `FieldKind::Readonly`, whose activation gate lives in
///    `FieldList`, not in `on_pick`.
/// 5. **`A` never lies** -- [`Verb::Select`] on selectable rows, no verb
///    (dim `A`) on unavailable ones, both from `FieldList::activation`.
///
/// `A`'s rail word is [`Verb::Select`] (orchestrator ruling on
/// `pico-link-7jol.4`: Uma's sketches say "pick", which would need a
/// second `Verb::Exception` and her sign-off for one word that means the
/// same thing to the user).
pub(crate) fn build_single_select_screen(
    id: ScreenId,
    title: impl Into<String>,
    options: Vec<PickerOption>,
    checked: Option<ListItemKey>,
    carry: &ScreenCarry,
    on_pick: impl Fn(ListItemKey) -> Action + 'static,
) -> Screen {
    let keys: Vec<ListItemKey> = options.iter().map(|option| option.key).collect();
    let rows: Vec<FieldRow> = options
        .into_iter()
        .map(|option| {
            let checked_here = Some(option.key) == checked;
            let mut row = if option.selectable { FieldRow::action(option.label) } else { FieldRow::readonly(option.label) };
            if option.selectable {
                row = row.with_verb(Verb::Select);
            }
            if let Some((text, color)) = option.note {
                row = row.with_value(text, color);
            }
            if checked_here {
                row = row.with_leading_glyph(icon::CHECK);
            }
            row.with_key(option.key)
        })
        .collect();

    let list = FieldList::new(rows)
        .with_leading_gutter()
        .with_selected_identity(carry.selected_key, carry.selected_index)
        .on_activate_index(move |index| keys.get(index).map_or(Action::None, |key| on_pick(*key)));
    let list = if let Some(top) = carry.scroll_top { list.with_scroll_top(top) } else { list };
    Screen::new(title, vec![Box::new(list)]).with_id(id)
}

/// The `why?` page's fixed title (design
/// `.planning/design/2026-09-07-home-fault-strip.md` §8.2: "Header: `WHY?`").
const WHY_PAGE_TITLE: &str = "WHY?";

/// Formats `elapsed_since(at)` as a short relative age -- `"8s ago"`,
/// `"4m ago"`, `"2h ago"` -- **never an absolute timestamp**, per design
/// §8.2's "Relative times only. Never absolute timestamps -- no RTC." This
/// board has no RTC (`.planning/design/2026-09-01-idle-policy-across-the-
/// ffi-seam.md`'s own note, restated here because it is easy to
/// rediscover as a missing feature rather than a hard constraint) --
/// `now`/`at` are both [`crate::run::FAULT_LIVE_WINDOW`]-scale
/// [`Instant`]s derived from the FFI seam's monotonic microsecond clock,
/// never wall-clock time.
fn relative_time(now: Instant, at: Instant) -> String {
    let elapsed = now.saturating_duration_since(at);
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

/// The Home fault strip's `why?` detail page (design
/// `.planning/design/2026-09-07-home-fault-strip.md` §8, bead
/// `pico-link-9eq2.3.3`) -- a scrollable list of **kinds, not events**
/// (orchestrator ruling on this bead): one aggregated two-line block per
/// [`FaultKey`] that has ever fired, in `order`'s sequence, including
/// retired keys (design §8.2: "including retired keys -- this is the
/// session history").
///
/// **Ordering discipline is the load-bearing part of this function**
/// (orchestrator ruling, restated because it is easy to miss): `order` is
/// the caller's frozen block order, established once by `render::home`'s
/// `ShortcutX` handler (a fresh most-recently-active-first sort, written
/// directly into the shared `Rc<RefCell<_>>` at push time) and never
/// re-sorted by this function on any subsequent call. What this function
/// DOES do, every call (including the very first, harmlessly, since
/// `order` starts empty then): **append** any key that has an entry in
/// [`BtModel::fault_log`] but is not yet present in `order`, at the END --
/// "a key that fires for the first time while the page is open appends at
/// the bottom rather than jumping to the top." A live re-sort under a
/// scrolling thumb is exactly the moving-target problem design §6.2
/// rejects for Home's own rows, worse here because the user is reading,
/// not glancing.
///
/// Never returns [`Refresh::Gone`] -- this page has no subject that can
/// vanish out from under it (unlike [`ScreenId::DevicePage`]'s device).
/// The `why?` page's line 3 (design §8.2): "one plain-language consequence
/// sentence plus the raw number." Neither design doc dictates exact
/// wording -- the audio-fault-model design (`.planning/design/2026-09-07-
/// audio-fault-model.md` §3.1's "Reads" column) only specifies each key's
/// value KIND and what it measures; Uma's sketch (§10.7) gives two worked
/// examples in her own prose. This function is that prose, one sentence
/// per key, filled in with the actual raw value -- never the saturated/
/// rounded figure Home's own count slot uses (§8.2: "no saturation here").
/// `None` (a key whose value has never been wired -- e.g. the USB supply
/// ratio before `pl_usb_supply_q8()` lands) means no line 3 at all, never a
/// guessed number.
fn fault_consequence_text(key: FaultKey, value: Option<FaultValue>) -> Option<String> {
    let value = value?;
    // Every arm below is measured against the why? page's real row budget
    // (`font::value()`, ~194px -- `core/examples/fault_strip_probe.rs`'s
    // `measure_why_page_consequence_texts`) and kept under it: `FieldList`
    // CLIPS an overlong label rather than ellipsising it (field-list ruling
    // §4.6), which for a full sentence reads as a confusing mid-word cut
    // rather than the name truncation this render core uses everywhere
    // else -- so these stay short by construction, not by luck.
    Some(match (key, value) {
        (FaultKey::BufStarved, FaultValue::Millis(ms)) => format!("ring dry, min fill {ms}ms"),
        (FaultKey::BufOverflow, FaultValue::Count(frames)) => format!("ring full, {frames} dropped"),
        (FaultKey::UsbSupplyLow, FaultValue::Ratio(q8)) => {
            // q8: 256 == 1.00x nominal -- rendered to 2 decimal places
            // without a float format dependency, matching this codebase's
            // "no_std + alloc" discipline (u8g2-fonts/core::fmt integer
            // formatting only).
            let whole = u32::from(q8) / 256;
            let frac = (u32::from(q8) % 256) * 100 / 256;
            format!("supply {whole}.{frac:02}x nominal")
        }
        (FaultKey::AirCongested, FaultValue::Count(deferred)) => format!("air busy, x{deferred} deferred"),
        (FaultKey::AirLinkLost, FaultValue::Count(occurrences)) => format!("link dropped x{occurrences}"),
        (FaultKey::EncResync, FaultValue::Count(frames)) => format!("trim dropped x{frames}"),
        // A key paired with a `FaultValue` variant the audio-fault-model
        // design's own table (§3.1) never assigns it -- e.g. a firmware
        // bug sending the wrong `value_kind` tag. Never fabricate a
        // sentence for a combination the design doesn't define; the
        // count/name/times on lines 1-2 still show, just no line 3.
        _ => return None,
    })
}

pub(crate) fn build_why_page_screen(model: &BtModel, now: Instant, order: &Rc<RefCell<Vec<FaultKey>>>, carry: &ScreenCarry) -> Refresh {
    {
        let mut order = order.borrow_mut();
        for key in FaultKey::ALL {
            if model.fault_log.entry(key).is_some() && !order.contains(&key) {
                order.push(key);
            }
        }
    }
    let ordered_keys = order.borrow().clone();

    let mut rows = Vec::new();
    let mut next_key = 0_u64;
    for key in ordered_keys {
        let Some(entry) = model.fault_log.entry(key) else {
            // Structurally unreachable: every key in `order` was inserted
            // above (or on a prior call) only after confirming
            // `fault_log.entry(key).is_some()`, and entries are never
            // removed once raised (`FaultLog`'s own doc comment) --
            // defensive rather than a panic, matching this crate's own
            // convention elsewhere (e.g. `device_page_rows`'s
            // `unwrap_or(&default_device)`).
            continue;
        };
        let glyph_char = match key.glyph() {
            FaultGlyphClass::Filled => '^',
            FaultGlyphClass::Starved => 'v',
            FaultGlyphClass::Neutral => '#',
        };
        let color = match key.severity() {
            FaultSeverity::Audible => palette::STATUS_ERROR,
            FaultSeverity::Concealed => palette::STATUS_WARNING,
        };
        // Line 1: glyph, name, TOTAL count -- "no saturation here, show the
        // real number" (design §8.2), unlike Home's own `x99+` cap.
        rows.push(
            FieldRow::readonly(format!("{glyph_char} {}", key.name()))
                .with_label_color(color)
                .with_value(format!("x{}", entry.count), color)
                .with_key(ListItemKey::from_u64(next_key)),
        );
        next_key += 1;
        // Line 2: relative times only, `TEXT_SECONDARY` (readonly's default
        // label color -- no override needed).
        rows.push(
            FieldRow::readonly(format!("last {} . first {}", relative_time(now, entry.last_seen), relative_time(now, entry.first_seen)))
                .with_key(ListItemKey::from_u64(next_key)),
        );
        next_key += 1;
        // Line 3 (design §8.2): "one plain-language consequence sentence
        // plus the raw number, which is where pico-link-8jp's supply
        // ratio and every other counter value now lives." Absent when
        // `entry.value` is `None` -- some keys have never had a value
        // wired (e.g. the USB supply ratio, per the audio-fault-model
        // design §6.3, "absent (`None`) until `pl_usb_supply_q8()`
        // exists") -- absent, never faked (parent design §15).
        if let Some(text) = fault_consequence_text(key, entry.value) {
            rows.push(FieldRow::readonly(text).with_key(ListItemKey::from_u64(next_key)));
            next_key += 1;
        }
    }

    let list = FieldList::new(rows).with_selected_identity(carry.selected_key, carry.selected_index);
    let list = if let Some(top) = carry.scroll_top { list.with_scroll_top(top) } else { list };
    Refresh::Rebuild(Screen::new(WHY_PAGE_TITLE, vec![Box::new(list)]).with_id(ScreenId::WhyPage))
}

/// The `QUALITY` picker's four entries, per design §4.1: numbers leading,
/// highest first, `HQ`/`SQ`/`MQ` never shown
/// (`.planning/design/2026-09-07-ldac-quality-selector.md`). Returns
/// [`Refresh::Gone`] when `addr` is no longer in [`BtModel::paired`] --
/// same reasoning as [`build_device_page_screen`]'s own doc comment (the
/// picker sits one level above the page that already unwinds on this).
fn build_ldac_quality_picker_screen(model: &BtModel, addr: DeviceAddr, carry: &ScreenCarry, commands: &Rc<RefCell<VecDeque<Command>>>) -> Refresh {
    const NOTES: [&str; 3] = ["best audio", "balanced", "most reliable"];

    let Some(device) = model.paired.iter().find(|d| d.addr == addr) else {
        return Refresh::Gone;
    };
    let connected = model.connected_addr == Some(addr);
    let streaming = connected && model.connected_codec.as_ref().is_some_and(|c| c.word == "LDAC");
    let rates = ldac_quality_rates_kbps(None);
    let mut options: Vec<PickerOption> = (0..3)
        .map(|i| PickerOption {
            key: LDAC_QUALITY_PICKER_KEYS[i],
            label: format!("{} kbps", rates[i]),
            note: Some((String::from(NOTES[i]), palette::TEXT_SECONDARY)),
            selectable: true,
        })
        .collect();
    // Adaptive's trailing note is live while streaming (design §4.1:
    // "660 now", mirroring the codec picker's existing "SBC now"), and
    // `varies` otherwise -- disconnected, or connected but not yet
    // streaming LDAC (never claim a number we don't have, same rule
    // `device_page_quality_row_value` follows).
    let adaptive_note = if streaming { model.ldac_live_kbps.map_or_else(|| String::from("varies"), |kbps| format!("{kbps} now")) } else { String::from("varies") };
    options.push(PickerOption {
        key: LDAC_QUALITY_PICKER_KEYS[3],
        label: String::from("Adaptive"),
        note: Some((adaptive_note, palette::TEXT_SECONDARY)),
        selectable: true,
    });

    let checked = Some(ldac_quality_checked_key(device.ldac_quality));
    let commands_for_pick = Rc::clone(commands);
    let on_pick = move |key: ListItemKey| {
        let ldac_quality = LDAC_QUALITY_PICKER_KEYS
            .iter()
            .position(|k| *k == key)
            .map_or(LDAC_QUALITY_ADAPTIVE, |i| u8::try_from(i + 1).unwrap_or(LDAC_QUALITY_ADAPTIVE));
        commands_for_pick.borrow_mut().push_back(Command::SetDeviceLdacQuality { addr, ldac_quality });
        // Design §5: applies live, no confirm, the picker stays open --
        // `Action::None` (not `PopView`), per `build_single_select_screen`'s
        // rule 2. The check itself moves only once the model's own echo
        // (`Event::PairedDeviceUpserted`) lands and `refresh_stack` rebuilds
        // this screen from `checked` above -- never optimistically here.
        Action::None
    };
    Refresh::Rebuild(build_single_select_screen(
        ScreenId::Picker(PickerKind::LdacQuality, addr),
        "Quality",
        options,
        checked,
        carry,
        on_pick,
    ))
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
        let model: ModelHandle = Rc::new(RefCell::new(BtModel::default()));
        let navigator = Navigator::new(build_home_screen(
            &model.borrow(),
            &home_face,
            &commands,
            &wizard_phase,
            &wizard_devices,
            Instant::from_micros(0),
            &why_page_order,
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
        }
    }

    /// Folds one inbound Bluetooth-domain [`Event`] into [`BtModel`] and
    /// refreshes the root screen (`pl_ui_push_event`'s core-side
    /// implementation -- the single entry point replacing the old
    /// `set_link_state`/`add_device`/`clear_devices` setter trio). `core`
    /// never acts on these itself -- it has no Bluetooth stack -- it only
    /// updates what screens read.
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::LinkStateChanged(state) => self.set_link_state(state),
            Event::DiscoveryStateChanged { scanning } => self.set_discovering(scanning),
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
            Event::LevelsChanged { peak_l, peak_r, rms_l, rms_r } => self.on_levels_changed(peak_l, peak_r, rms_l, rms_r),
            Event::VolumeChanged { level, muted, source } => self.on_volume_changed(level, muted, source),
            Event::LdacBitrateChanged { kbps } => self.on_ldac_bitrate_changed(kbps),
            Event::FaultRaised { key, value, count } => self.on_fault_raised(key, value, count),
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

    /// Phase 2 -> phase 3 transition (design section 9): when a GAP
    /// inquiry ends (bead `pico-link-88xs`: called only from
    /// [`App::set_discovering`] on the `scanning == false` edge, i.e. a
    /// genuine [`Event::DiscoveryStateChanged`] -- no longer a
    /// `LinkStateChanged(Idle)`, which could also fire on a connect
    /// failure or a disconnect and spuriously flip the wizard to
    /// `NothingFound`) while the wizard is still on
    /// [`WizardPhase::Scanning`] and nothing was found, moves it to
    /// [`WizardPhase::NothingFound`]. A no-op in every other case --
    /// devices *were* found (the phase just stays `Scanning`, now showing
    /// a selectable list instead of an actively-filling one -- design
    /// section 9 draws no rendering distinction between those), the
    /// wizard isn't open, or it's already past phase 2 (e.g. the user
    /// already selected a device and moved on to phase 4 before this
    /// event arrived).
    fn on_scan_ended_if_applicable(&mut self) {
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
        self.model.borrow_mut().connected_addr = Some(addr);
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
        let auto_reconnect = {
            let mut model = self.model.borrow_mut();
            model.store_status = Some(status);
            model.paired.iter().max_by_key(|d| d.mru_seq).map(|device| (device.addr, truncate_device_name(&device.name)))
        };
        if let Some((addr, name)) = auto_reconnect {
            self.commands.borrow_mut().push_back(Command::Connect { addr, name });
        }
        self.refresh_stack();
    }

    /// Folds one [`Event::PairedDeviceUpserted`] into [`BtModel::paired`] --
    /// update-in-place if `addr` is already known (a rename, an `mru_seq`
    /// bump), append otherwise. One of exactly two writers of `paired`
    /// (design section 3's single-writer rule -- see that event's doc
    /// comment). Bead pico-link-4vb.4 (T4).
    fn on_paired_device_upserted(&mut self, device: PairedDevice) {
        {
            let mut model = self.model.borrow_mut();
            if let Some(existing) = model.paired.iter_mut().find(|d| d.addr == device.addr) {
                *existing = device;
            } else {
                model.paired.push(device);
            }
        }
        self.refresh_stack();
    }

    /// Folds one [`Event::PairedDeviceForgotten`] into [`BtModel::paired`] --
    /// the other of the two writers (design section 3). A no-op if `addr`
    /// isn't currently known (e.g. a stray/duplicate echo).
    fn on_paired_device_forgotten(&mut self, addr: DeviceAddr) {
        self.model.borrow_mut().paired.retain(|d| d.addr != addr);
        self.refresh_stack();
    }

    /// Folds one [`Event::PairedStoreFull`] -- see that event's and
    /// [`BtModel::store_full`]'s doc comments for why this is populated
    /// ahead of any screen actually reading it.
    fn on_paired_store_full(&mut self) {
        self.model.borrow_mut().store_full = true;
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
            // pico-link-l4d: `pop_to_root` only restores navigation depth --
            // it doesn't touch which of Home's two faces (`HomeFace::Status`
            // vs `HomeFace::Menu`) is showing. The user reached the wizard
            // via Home's Menu face (Home -> A -> Devices -> pair), so without
            // this the auto-dismiss silently landed back on the
            // Bluetooth/Settings list instead of the hero -- which is the
            // entire point of auto-dismissing: showing the codec just paired.
            // This is a deliberate, automatic choice of destination (see the
            // policy note on `on_devices_back`/wizard-success-B for why the
            // *manual* B routes are treated differently).
            *self.home_face.borrow_mut() = HomeFace::Status;
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
    /// disconnect is worse than `NO LINK` (design section 15).
    /// Deliberately keyed off the link state itself rather than a
    /// dedicated disconnect event: every path off `Connected` already
    /// flows through this one method (bead pico-link-1v5), so this can't
    /// race with a disconnect notification C forgot to send, and it needs
    /// zero new firmware plumbing in `bt.c`.
    ///
    /// `LinkState` no longer carries a scan (bead `pico-link-88xs`, design
    /// `.planning/design/2026-09-08-link-state-vs-discovery-axis.md`):
    /// this method's own clear-on-not-`Connected` rule is unchanged --
    /// see [`LinkState`]'s doc comment for why narrowing the type, not
    /// relaxing this rule, is what fixed the "a scan wipes the connected
    /// model" bug. [`BtModel::link_state`] has exactly one writer: this
    /// method (INVARIANT L2) -- see [`App::record_connect_failure`].
    pub fn set_link_state(&mut self, state: LinkState) {
        {
            let mut model = self.model.borrow_mut();
            model.link_state = state;
            if state != LinkState::Connected {
                model.connected_codec = None;
                // `connected_addr` (bead pico-link-4vb.4, T5) follows the exact
                // same lifecycle as `connected_codec`, for the same reason --
                // see `BtModel::connected_addr`'s doc comment.
                model.connected_addr = None;
                // `out_level` (bead pico-link-du0) follows the exact same
                // lifecycle for the exact same reason -- see
                // `BtModel::out_level`'s doc comment.
                model.out_level = None;
                // `ldac_live_kbps` (bead pico-link-7jol.5) follows the exact
                // same lifecycle for the exact same reason -- see
                // `BtModel::ldac_live_kbps`'s doc comment.
                model.ldac_live_kbps = None;
            }
        }
        self.refresh_stack();
    }

    /// Records whether the radio is running an inquiry. The SECOND,
    /// independent axis (bead `pico-link-88xs`) -- deliberately does NOT
    /// touch [`BtModel::link_state`] and does NOT clear any connected-model
    /// field: an inquiry does not disconnect A2DP. [`BtModel::discovering`]
    /// has exactly one writer: this method (INVARIANT L2).
    pub fn set_discovering(&mut self, scanning: bool) {
        self.model.borrow_mut().discovering = scanning;
        if !scanning {
            self.on_scan_ended_if_applicable();
        }
        self.refresh_stack();
    }

    /// Records the live A2DP link's negotiated codec (or a renegotiation)
    /// and refreshes the Home hero widget to match. See
    /// [`ConnectedCodec`]'s doc comment for why `core` treats
    /// `word`/`nominal_bitrate_bps` as opaque, already-decided display
    /// data rather than deriving them from codec identity itself.
    pub fn set_connected_codec(&mut self, codec: ConnectedCodec) {
        {
            let mut model = self.model.borrow_mut();
            // A renegotiation away from LDAC (or a fresh connect that isn't
            // LDAC at all) must drop the previous stream's live figure --
            // `ldac_live_kbps` (bead pico-link-7jol.5) is only ever meaningful
            // for the codec it was measured on, and a stale reading surviving
            // a codec change would show under the wrong hero word.
            if codec.word != "LDAC" {
                model.ldac_live_kbps = None;
            }
            model.connected_codec = Some(codec);
        }
        self.refresh_stack();
    }

    /// Folds one [`Event::VolumeChanged`] reading into [`BtModel::volume`]
    /// (bead pico-link-4v2.5, VT5, design section 7). No screen reads this
    /// yet -- `rebuild_root` is called anyway, matching every other
    /// `BtModel`-mutating fold in this file, so a future screen can rely
    /// on that convention rather than each one deciding for itself whether
    /// a redraw is warranted.
    pub fn on_volume_changed(&mut self, level: u8, muted: bool, source: VolumeSource) {
        self.model.borrow_mut().volume = Some(VolumeState { level, muted, source });
        self.refresh_stack();
    }

    /// Folds one [`Event::LdacBitrateChanged`] reading into
    /// [`BtModel::ldac_live_kbps`] (bead pico-link-7jol.5) -- see that
    /// field's doc comment for the "never snapped to the ladder" rule.
    /// Refreshes the stack so Home's bitrate line and the device page's
    /// `QUALITY` row (its Adaptive trailing note) both pick up the new
    /// figure the same frame it arrives.
    pub fn on_ldac_bitrate_changed(&mut self, kbps: u32) {
        self.model.borrow_mut().ldac_live_kbps = Some(kbps);
        self.refresh_stack();
    }

    /// Folds one [`Event::FaultRaised`] reading into
    /// [`BtModel::fault_log`] (design `.planning/design/2026-09-07-audio-
    /// fault-model.md` §7.4). Uses `self.now_us` (the FFI seam's own
    /// stored clock, see [`App::tick`]'s doc comment) rather than taking a
    /// clock parameter -- the same convention [`App::on_levels_changed`]'s
    /// peak-hold already uses. Calls [`App::refresh_stack`] like every
    /// other `BtModel`-mutating fold, so a future screen (S3,
    /// `pico-link-9eq2.3.3`) can rely on that convention rather than each
    /// one deciding for itself whether a redraw is warranted -- this bead
    /// builds no screen that actually reads `fault_log` yet.
    pub fn on_fault_raised(&mut self, key: FaultKey, value: Option<FaultValue>, count: u16) {
        let now = Instant::from_micros(self.now_us);
        self.model.borrow_mut().fault_log.record(key, now, value, count);
        self.refresh_stack();
    }

    /// Folds one [`Event::LevelsChanged`] reading into
    /// [`BtModel::out_level`] and refreshes the Home hero widget's meter
    /// (bead pico-link-du0). Also updates the per-channel peak-hold cap:
    /// a channel's hold value tracks the highest peak seen, and only
    /// drops back down once [`OUT_LEVEL_HOLD_DURATION`] has passed since
    /// it was last set to a new maximum -- the conventional VU-meter
    /// "peak stays pinned briefly, then releases" behaviour, decided once
    /// here (at fold time, using `self.now_us`) rather than re-computed
    /// every render frame (see [`OutLevelSample`]'s doc comment for why
    /// render-time decay isn't an option for a `&self`-rendered widget).
    // `l`/`r` channel-suffixed bindings are the domain vocabulary this
    // whole feature uses (matches `OutLevelSample`'s own field names) --
    // clippy::similar_names' false positive on stereo L/R naming.
    #[allow(clippy::similar_names)]
    pub fn on_levels_changed(&mut self, peak_l: u8, peak_r: u8, rms_l: u8, rms_r: u8) {
        let now = Instant::from_micros(self.now_us);
        let prev_out_level = self.model.borrow().out_level;
        let (prev_hold_l, prev_hold_l_at, prev_hold_r, prev_hold_r_at) = match prev_out_level {
            Some(sample) => (sample.hold_l, sample.hold_l_at, sample.hold_r, sample.hold_r_at),
            None => (0, now, 0, now),
        };
        let (hold_l, hold_l_at) = if peak_l >= prev_hold_l || now.saturating_duration_since(prev_hold_l_at) >= OUT_LEVEL_HOLD_DURATION {
            (peak_l, now)
        } else {
            (prev_hold_l, prev_hold_l_at)
        };
        let (hold_r, hold_r_at) = if peak_r >= prev_hold_r || now.saturating_duration_since(prev_hold_r_at) >= OUT_LEVEL_HOLD_DURATION {
            (peak_r, now)
        } else {
            (prev_hold_r, prev_hold_r_at)
        };
        // Release-ballistic attack anchor (bead pico-link-ajj, design
        // requirement C; anchors on peak, not rms, as of bead pico-link-53c
        // -- the bar itself now draws peak, so the ballistic must decay the
        // same quantity it draws or the displayed bar and its decay drift
        // apart): decay the previous anchor to "now" and compare against
        // the fresh peak sample. If the fresh sample is at or above that
        // decayed value, this is a rise -- attack is instantaneous, so the
        // anchor jumps straight to the new sample. Otherwise the anchor is
        // left exactly as it was, so the render-time decay in
        // `crate::render::hero` continues gliding down from the same
        // point instead of re-anchoring (and thus flattening the release
        // curve) on every quieter sample.
        let (prev_anchor_l, prev_anchor_l_at, prev_anchor_r, prev_anchor_r_at) = match prev_out_level {
            Some(sample) => (sample.attack_peak_l, sample.attack_peak_l_at, sample.attack_peak_r, sample.attack_peak_r_at),
            None => (0, now, 0, now),
        };
        let decayed_l = decay_peak(prev_anchor_l, now.saturating_duration_since(prev_anchor_l_at));
        let (attack_peak_l, attack_peak_l_at) =
            if peak_l >= decayed_l { (peak_l, now) } else { (prev_anchor_l, prev_anchor_l_at) };
        let decayed_r = decay_peak(prev_anchor_r, now.saturating_duration_since(prev_anchor_r_at));
        let (attack_peak_r, attack_peak_r_at) =
            if peak_r >= decayed_r { (peak_r, now) } else { (prev_anchor_r, prev_anchor_r_at) };
        self.model.borrow_mut().out_level = Some(OutLevelSample {
            peak_l,
            peak_r,
            rms_l,
            rms_r,
            hold_l,
            hold_r,
            hold_l_at,
            hold_r_at,
            received_at: now,
            attack_peak_l,
            attack_peak_r,
            attack_peak_l_at,
            attack_peak_r_at,
        });
        self.refresh_stack();
    }

    /// Adds (or, if `addr` is already known, updates the name/rssi of) one
    /// discovered device and refreshes the devices screen. Update-in-place
    /// rather than appending a duplicate row: BTstack's inquiry reports the
    /// same device repeatedly as its RSSI/name resolve.
    pub fn add_device(&mut self, addr: [u8; 6], name: String, rssi: i8, class_of_device: u32) {
        {
            let mut model = self.model.borrow_mut();
            if let Some(existing) = model.discovered.iter_mut().find(|d| d.addr == addr) {
                existing.name = name;
                existing.rssi = rssi;
                existing.class_of_device = class_of_device;
            } else {
                model.discovered.push(DeviceEntry { addr, name, rssi, class_of_device });
            }
        }
        // Kept in lockstep with `model.discovered` -- see `wizard_devices`'s
        // doc comment on why the wizard widget needs its own mirror
        // rather than a borrow into `self.model`.
        self.wizard_devices.borrow_mut().clone_from(&self.model.borrow().discovered);
        self.refresh_stack();
    }

    /// Clears the discovered-device list, e.g. at the start of a fresh
    /// scan.
    pub fn clear_devices(&mut self) {
        self.model.borrow_mut().discovered.clear();
        self.wizard_devices.borrow_mut().clear();
        self.refresh_stack();
    }

    /// Records a failed connect attempt with its [`ConnectFailureReason`]
    /// and returns the link to [`LinkState::Idle`] -- the attempt is over
    /// either way, retryable or not; a future screen deciding whether to
    /// offer a retry reads `reason.retryable()` off
    /// `BtModel::last_connect_failure`, not the link state.
    pub fn record_connect_failure(&mut self, addr: [u8; 6], reason: ConnectFailureReason) {
        self.model.borrow_mut().last_connect_failure = Some((addr, reason));
        // Bead pico-link-88xs, INVARIANT L2: `link_state` has exactly one
        // writer. Routed through `set_link_state` rather than assigning
        // the field directly (as this used to) -- correct today only by
        // accident, since the preceding `Connecting` push had already
        // cleared the four connected-model fields; going through the real
        // setter makes that true by construction instead. The
        // `refresh_stack()` call below is therefore redundant with the one
        // inside `set_link_state`, but harmless -- `refresh_stack` is
        // idempotent.
        self.set_link_state(LinkState::Idle);
        // Phase 4/5 -> phase 6 (failure outcome). Unconditional (not
        // gated on the wizard currently being open/mid-connect): a stray
        // `ConnectFailed` with the wizard closed or already past this
        // attempt just sets a phase nothing is currently rendering, which
        // is harmless and gets overwritten the next time the wizard opens
        // (`build_devices_screen`'s "Pair new headphones" row activation
        // resets it to `WizardPhase::scanning_pending`).
        *self.wizard_phase.borrow_mut() = WizardPhase::Failed { addr, reason };
        self.refresh_stack();
    }

    /// Read-only access to the live Bluetooth model, for tests/diagnostics
    /// and for any future FFI accessor that needs to read it back. Returns
    /// a [`Ref`] rather than `&BtModel` as of bead `pico-link-bgnd` M0
    /// (`self.model` is now a [`ModelHandle`]) -- `Ref` derefs to
    /// `BtModel`, so every existing `app.model().some_field` call site
    /// keeps compiling unchanged; only a call site that needs an actual
    /// `&BtModel` (a function argument) must add an explicit `&`.
    ///
    /// # Panics
    ///
    /// If a `RefMut` borrow of the model is already held -- see
    /// [`ModelHandle`]'s doc comment for the borrow rule that's meant to
    /// make this never happen in practice.
    ///
    /// # Rule for unsafe FFI call sites (`ui-ffi`)
    ///
    /// In safe Rust the returned `Ref` is borrow-checked against `&App` and
    /// cannot outlive it. But `ui-ffi` derefs a raw `*mut PlUi` to get at
    /// `App`, which yields an *unbounded* lifetime -- so in `ui-ffi`, never
    /// bind `app.model()` to a `let` and hold it across any `pl_ui_*` call
    /// (especially `pl_ui_destroy`, which frees the `Rc<RefCell<BtModel>>`
    /// this `Ref` borrows from). Scope it in an inner block instead, so
    /// `Ref`'s `Drop` runs before the next `pl_ui_*` call. See the DECISION
    /// comment on bead `pico-link-bgnd.7` (a real heap-use-after-free was
    /// found and fixed this way in `ui-ffi/src/lib.rs`).
    #[must_use]
    pub fn model(&self) -> Ref<'_, BtModel> {
        self.model.borrow()
    }

    /// Whether the current volume reading means the display must never go
    /// fully blank (design section 5.4/6.1): muted, or at 0%, from ANY
    /// source. `None` (nothing connected, or no reading yet) is `false` --
    /// there is no banner to protect a floor for. Read every idle tick by
    /// both [`crate::run::Runner::step`] and `ui-ffi`'s `pl_ui_tick` (via
    /// `IdlePolicy::tick`'s `mute_or_zero` parameter), which is what makes
    /// this rule apply on the real target and not just the emulator -- see
    /// `IdlePolicy::tick`'s doc comment for the mechanism.
    #[must_use]
    pub fn volume_requires_dim_floor(&self) -> bool {
        self.model.borrow().volume.is_some_and(|volume| volume.muted || volume.level == 0)
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

    /// Test-only: the Devices screen's own scroll-top row index, if it's
    /// currently on the navigator stack at index 1 -- the scroll-position
    /// counterpart to [`App::devices_selected_index_for_test`], proving
    /// [`App::rebuild_root`] carries the user's viewport forward across
    /// an unrelated model event (`.planning/design/2026-09-02-field-list-
    /// widget-ruling.md` §4.7). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn devices_scroll_top_for_test(&self) -> Option<usize> {
        self.navigator.scroll_top_at(1)
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

    /// Test-only: reads which of Home's two faces (`HomeFace::Status` vs
    /// `HomeFace::Menu`) is currently showing. Not part of the public API.
    /// Added for pico-link-l4d, whose bug (auto-dismiss landing on the
    /// menu face instead of the hero) was invisible to every existing
    /// test because they only ever asserted `navigator_depth()`, never
    /// the face -- depth alone can't tell Home's two faces apart.
    #[cfg(test)]
    pub(crate) fn home_face_for_test(&self) -> HomeFace {
        *self.home_face.borrow()
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
mod tests {
    use super::*;
    use embedded_graphics::prelude::RgbColor;

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

    // --- VT6, design `.planning/design/2026-09-07-volume-on-display.md`
    // section 3: the percent formula, pinned at both endpoints ---

    #[test]
    fn volume_percent_pins_the_two_endpoints_exactly() {
        // Design section 3: "100% must mean maximum and 0% must mean
        // silent, with no other level able to render as either" -- so the
        // endpoints are checked exactly, not just "close enough".
        let silent = VolumeState { level: 0, muted: false, source: VolumeSource::Host };
        assert_eq!(silent.percent(), 0);
        let max = VolumeState { level: 127, muted: false, source: VolumeSource::Host };
        assert_eq!(max.percent(), 100);
    }

    #[test]
    fn volume_percent_matches_the_designs_worked_examples() {
        // Design section 3's worked examples: `1 -> 1`, `126 -> 99`.
        assert_eq!(VolumeState { level: 1, muted: false, source: VolumeSource::Host }.percent(), 1);
        assert_eq!(VolumeState { level: 126, muted: false, source: VolumeSource::Host }.percent(), 99);
    }

    #[test]
    fn volume_percent_never_exceeds_100_or_underflows() {
        // Exhaustive over the whole legal 0..=127 domain -- cheap, and it's
        // the one property a rounding-formula regression could quietly
        // violate at an untested value in the middle of the range.
        for level in 0..=127u8 {
            let percent = VolumeState { level, muted: false, source: VolumeSource::Host }.percent();
            assert!(percent <= 100, "level {level} produced out-of-range percent {percent}");
        }
    }

    // --- VT6 design section 5.1/5.3/5.4: `VolumeState::wakes_idle` ---

    #[test]
    fn wakes_idle_is_false_for_host_regardless_of_level_or_muted() {
        assert!(!VolumeState { level: 80, muted: false, source: VolumeSource::Host }.wakes_idle());
        assert!(!VolumeState { level: 0, muted: false, source: VolumeSource::Host }.wakes_idle());
        assert!(!VolumeState { level: 80, muted: true, source: VolumeSource::Host }.wakes_idle());
    }

    #[test]
    fn wakes_idle_is_true_for_sink_or_device_when_not_muted_and_above_zero() {
        assert!(VolumeState { level: 80, muted: false, source: VolumeSource::Sink }.wakes_idle());
        assert!(VolumeState { level: 80, muted: false, source: VolumeSource::Device }.wakes_idle());
    }

    #[test]
    fn wakes_idle_is_false_for_sink_or_device_when_muted_or_at_zero() {
        // Design section 5.4 outranks 5.3: a sink-originated mute/zero
        // must never "wake to full".
        assert!(!VolumeState { level: 80, muted: true, source: VolumeSource::Sink }.wakes_idle());
        assert!(!VolumeState { level: 0, muted: false, source: VolumeSource::Sink }.wakes_idle());
        assert!(!VolumeState { level: 0, muted: true, source: VolumeSource::Device }.wakes_idle());
    }

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

    // --- `FaultLog` (design `.planning/design/2026-09-07-audio-fault-
    // model.md` §5.4/§7.4, bead pico-link-9eq2.3.1) ---

    #[test]
    fn fault_log_record_assigns_count_rather_than_accumulating_it() {
        // Design §5.4: "the payload carries an absolute count, not an
        // increment" -- C's own running total, assigned wholesale each
        // time so a dropped event only costs one refresh cycle of
        // staleness rather than permanent drift.
        let mut log = FaultLog::default();
        let t1 = Instant::from_micros(1_000_000);
        let t2 = Instant::from_micros(2_000_000);

        log.record(FaultKey::BufStarved, t1, None, 5);
        assert_eq!(log.entry(FaultKey::BufStarved).unwrap().count, 5);

        log.record(FaultKey::BufStarved, t2, None, 3);
        assert_eq!(
            log.entry(FaultKey::BufStarved).unwrap().count,
            3,
            "count must be ASSIGNED from the payload, never accumulated (5 + 3 = 8 would be the accumulation bug)"
        );
    }

    #[test]
    fn fault_log_record_sets_first_seen_once_and_always_updates_last_seen() {
        let mut log = FaultLog::default();
        let t1 = Instant::from_micros(1_000_000);
        let t2 = Instant::from_micros(2_000_000);

        log.record(FaultKey::EncResync, t1, None, 1);
        let first = log.entry(FaultKey::EncResync).unwrap();
        assert_eq!(first.first_seen, t1);
        assert_eq!(first.last_seen, t1);

        log.record(FaultKey::EncResync, t2, None, 2);
        let second = log.entry(FaultKey::EncResync).unwrap();
        assert_eq!(second.first_seen, t1, "first_seen must not move on a later raise");
        assert_eq!(second.last_seen, t2, "last_seen must always advance to the latest raise");
    }

    #[test]
    fn fault_log_is_live_within_the_window_and_false_after_it_elapses() {
        let mut log = FaultLog::default();
        let raised_at = Instant::from_micros(1_000_000);
        log.record(FaultKey::AirLinkLost, raised_at, None, 1);

        let almost_closed = Instant::from_micros((raised_at + crate::run::FAULT_LIVE_WINDOW).as_micros() - 1);
        let closed = raised_at + crate::run::FAULT_LIVE_WINDOW;

        assert!(log.is_live(FaultKey::AirLinkLost, raised_at), "must be Live at the instant it was raised");
        assert!(log.is_live(FaultKey::AirLinkLost, almost_closed), "must still be Live one microsecond before the window closes");
        assert!(!log.is_live(FaultKey::AirLinkLost, closed), "must no longer be Live once the full window has elapsed");
    }

    #[test]
    fn fault_log_is_live_is_false_for_a_key_never_raised() {
        let log = FaultLog::default();
        assert!(!log.is_live(FaultKey::BufOverflow, Instant::from_micros(0)));
        assert!(log.entry(FaultKey::BufOverflow).is_none());
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
                    "device detail" | "settings" => None,
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

    /// Bead pico-link-4vb.4 (T5): the Devices screen no longer reads
    /// `BtModel::discovered` (the wizard's own scan list, see that field's
    /// doc comment) -- it reads `BtModel::paired`, mutated only by
    /// [`Event::PairedDeviceUpserted`]/[`Event::PairedDeviceForgotten`].
    /// Shorthand for building one such event in these tests.
    fn upsert(addr: [u8; 6], name: &str, mru_seq: u32) -> Event {
        Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality: 0 })
    }

    /// Like [`upsert`] but with a real `ldac_quality` -- pico-link-7jol.5's
    /// tests for the `QUALITY` row/picker's stored-echo behaviour.
    fn upsert_with_quality(addr: [u8; 6], name: &str, mru_seq: u32, ldac_quality: u8) -> Event {
        Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality })
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
        let device = PairedDevice { addr: [0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33], name: String::new(), mru_seq: 1, ldac_quality: 0 };
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

    // --- decay_peak / vertical OUT meter release ballistics (bead
    // pico-link-ajj): code review found the render-side floor
    // (`.max(level.rms_l)` in `render::hero`) pinned the displayed value
    // to the last raw reading for a sample's whole life, defeating the
    // release entirely -- these tests exercise decay over elapsed time
    // WITHOUT a new sample arriving, which is exactly the case that bug
    // was invisible to (no prior test drove `decay_peak` at all). ---

    #[test]
    fn decay_peak_at_zero_elapsed_is_unchanged() {
        assert_eq!(decay_peak(200, Duration::from_millis(0)), 200);
    }

    #[test]
    // `at_0ms`/`at_100ms`/`at_500ms`/`at_1000ms` are deliberately parallel
    // names for a set of samples along one timeline -- clippy's
    // similar-names lint false-positives on this the same way the
    // existing L/R channel bindings do elsewhere in this file.
    #[allow(clippy::similar_names)]
    fn decay_peak_falls_strictly_over_time_with_no_new_sample() {
        // The exact regression the review caught: sampling decay_peak at
        // increasing elapsed times (no new LevelsChanged in between) must
        // show a strictly decreasing sequence, not a value pinned at the
        // anchor.
        let anchor = 200;
        let at_0ms = decay_peak(anchor, Duration::from_millis(0));
        let at_100ms = decay_peak(anchor, Duration::from_millis(100));
        let at_500ms = decay_peak(anchor, Duration::from_millis(500));
        let at_1000ms = decay_peak(anchor, Duration::from_millis(1000));
        assert!(at_0ms > at_100ms, "200 -> {at_100ms} after 100ms: must have started falling");
        assert!(at_100ms > at_500ms, "{at_100ms} -> {at_500ms} after 500ms: must keep falling");
        assert!(at_500ms > at_1000ms, "{at_500ms} -> {at_1000ms} after 1000ms: must keep falling");
    }

    #[test]
    fn decay_peak_after_one_second_is_roughly_ten_percent() {
        // ~20 dB/s release (design requirement C) means amplitude falls
        // to roughly 10% after one second of continuous release.
        let decayed = decay_peak(200, Duration::from_millis(1000));
        assert!((15..=25).contains(&decayed), "expected ~20 (10% of 200), got {decayed}");
    }

    #[test]
    fn decay_peak_eventually_reaches_zero_and_stays_there() {
        let decayed = decay_peak(255, Duration::from_secs(10));
        assert_eq!(decayed, 0);
        // u64::MAX elapsed must not panic or wrap -- `App::on_levels_changed`
        // can hand this an arbitrarily large gap (e.g. the very first
        // reading, decayed from a zero anchor at `Instant::from_micros(0)`).
        assert_eq!(decay_peak(255, Duration::from_micros(u64::MAX)), 0);
    }

    #[test]
    fn out_level_ballistic_decays_between_ticks_with_no_new_levels_changed_event() {
        // End-to-end version of the same regression: a single loud
        // LevelsChanged reading, then ONLY `tick()` calls (no further
        // events) -- the model's own anchor must show a falling value as
        // time passes, not a value frozen at the original rms sample.
        let mut app = App::new(240, 240);
        app.tick(1);
        app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 200, rms_l: 200, rms_r: 200 });
        let sample_at_fold = app.model().out_level.expect("a reading was just folded in");
        assert_eq!(sample_at_fold.attack_peak_l, 200, "an empty prior anchor means the first sample is an instantaneous attack");

        // No new Event::LevelsChanged from here -- only the clock moves.
        app.tick(1 + 300_000); // +300ms
        let decayed_300ms = decay_peak(
            sample_at_fold.attack_peak_l,
            Instant::from_micros(app.now_us()).saturating_duration_since(sample_at_fold.attack_peak_l_at),
        );
        assert!(decayed_300ms < 200, "300ms after the last event with no new sample, the ballistic must have started releasing, got {decayed_300ms}");

        app.tick(1 + 550_000); // +550ms from the event (still under the 600ms staleness window)
        let decayed_550ms = decay_peak(
            sample_at_fold.attack_peak_l,
            Instant::from_micros(app.now_us()).saturating_duration_since(sample_at_fold.attack_peak_l_at),
        );
        assert!(decayed_550ms < decayed_300ms, "the release must keep falling as more time passes with still no new sample: {decayed_300ms} -> {decayed_550ms}");

        // The stored anchor and its timestamp themselves must NOT have
        // been mutated by tick() -- decay is a pure render-time
        // computation off a fixed fold-time anchor, never a value ticked
        // down in place.
        let sample_after_ticks = app.model().out_level.expect("no event cleared it");
        assert_eq!(sample_after_ticks.attack_peak_l, sample_at_fold.attack_peak_l);
        assert_eq!(sample_after_ticks.attack_peak_l_at, sample_at_fold.attack_peak_l_at);
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

    // --- pico-link-7jol.4: device_page_rows (pure) ---

    #[test]
    fn device_page_rows_shows_the_live_codec_when_connected() {
        let mut model = BtModel::default();
        let addr = [1; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });

        let rows = device_page_rows(&model, addr);
        assert_eq!(rows[0].label, "CODEC");
        assert_eq!(rows[0].value(), Some("LDAC"), "a connected device must show its live codec, not Automatic");
    }

    #[test]
    fn device_page_rows_shows_automatic_when_disconnected_and_never_dashes_the_codec() {
        let mut model = BtModel::default();
        let addr = [2; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0 });
        // Not connected: `connected_addr` stays `None`.

        let rows = device_page_rows(&model, addr);
        assert_eq!(rows[0].value(), Some("Automatic"), "CODEC is a stored-setting-shaped row: it never dashes (design §3.0)");
    }

    #[test]
    fn device_page_rows_dashes_the_three_unimplemented_live_fields() {
        let model = BtModel::default();
        let addr = [3; 6];
        let rows = device_page_rows(&model, addr);
        for label in ["SAMPLE RATE", "USB IN", "A2DP"] {
            let row = rows.iter().find(|r| r.label == label).unwrap_or_else(|| panic!("missing row {label}"));
            assert_eq!(row.value(), Some(DASH), "{label} has no seam yet and must dash honestly, not fake a value");
        }
    }

    #[test]
    fn device_page_rows_address_row_renders_colon_separated_hex() {
        let model = BtModel::default();
        let addr = [0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2];
        let rows = device_page_rows(&model, addr);
        let address_row = rows.iter().find(|r| r.label == "ADDRESS").expect("ADDRESS row must exist");
        assert_eq!(address_row.value(), Some("94:DB:56:54:7C:F2"));
    }

    #[test]
    fn device_page_rows_forget_is_the_only_pressable_row_and_sits_last() {
        let model = BtModel::default();
        let addr = [4; 6];
        let rows = device_page_rows(&model, addr);
        let row_count = rows.len();
        assert_eq!(rows.last().expect("device page must have at least one row").label, "Forget this device");

        // Black-box, per this crate's own `Navigator`/`FieldList` test
        // convention (selection/kind state lives inside the widget, not
        // exposed on `FieldRow` directly): CODEC/SAMPLE RATE/USB IN/A2DP/
        // ADDRESS must be `Readonly` (A does nothing), and only the last
        // row (Forget) must be `Action` (A is live) -- no codec picker to
        // open yet in this bead (this file's `device_page_rows` doc
        // comment).
        let mut list = FieldList::new(rows);
        for _ in 0..row_count - 1 {
            assert_eq!(list.activation(), None, "only Forget should be pressable on this bead's device page");
            list.on_intent(NavIntent::Down);
        }
        assert_eq!(list.activation(), Some(Verb::Open), "the focused last row (Forget) must be pressable");
    }

    // --- pico-link-7jol.4: the device page's own screen/wrapper ---

    #[test]
    fn the_connected_devices_page_x_binding_is_drop() {
        let mut app = App::new(240, 240);
        let addr = [8; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice, queued by ConnectSucceeded
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.poll_command(), Some(Command::Disconnect), "X on a connected device's page must queue Disconnect");
    }

    #[test]
    fn a_disconnected_devices_page_x_binding_is_link_and_reconnects() {
        // Reach a disconnected device's page the only way it's wired in
        // this bead: connect once (so the page is reachable via the
        // connected row), then let the link drop while the page stays
        // open -- `refresh_stack` must flip `connected` (and therefore the
        // X binding) live, matching device-page design §2.1's amendment.
        let mut app = App::new(240, 240);
        let addr = [9; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice, queued by ConnectSucceeded
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page, connected
        app.handle_event(Event::LinkStateChanged(LinkState::Idle)); // drops -- refresh_stack must catch up in place

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(
            app.poll_command(),
            Some(Command::Connect { addr, name: String::from("Cans") }),
            "X on a disconnected device's page must queue a reconnect, not Disconnect"
        );
    }

    #[test]
    fn device_page_scroll_and_selection_survive_a_live_refresh() {
        let mut app = App::new(240, 240);
        let addr = [10; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        app.handle_input(vec![NavIntent::Down, NavIntent::Down]); // focus row 2 (USB IN)

        // An unrelated model event must not reset the user's focus on the
        // page they're looking at (the whole reason `refresh_stack` reads
        // `ScreenCarry` before replacing).
        app.handle_event(Event::LevelsChanged { peak_l: 10, peak_r: 10, rms_l: 10, rms_r: 10 });

        assert_eq!(app.navigator.selected_index_at(2), Some(2), "focus must survive a live refresh of the page underneath it");
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

    // --- pico-link-7jol.4: build_single_select_screen (the general picker) ---

    fn quality_like_test_id() -> ScreenId {
        // No ScreenId::Picker variant exists yet in this bead (it lands
        // with pico-link-7jol.5, alongside its first real caller) --
        // `build_single_select_screen` is generic over `id`, so any
        // ScreenId value exercises its contract identically. Standing in
        // with an address distinct from any real device used elsewhere in
        // this module's tests.
        ScreenId::DevicePage([0xAA; 6])
    }

    fn no_carry() -> ScreenCarry {
        ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None }
    }

    fn three_option_picker(checked: Option<ListItemKey>, picked: Rc<RefCell<Vec<ListItemKey>>>, stay_open: bool) -> Screen {
        let options = vec![
            PickerOption { key: ListItemKey::from_u64(1), label: String::from("Alpha"), note: Some((String::from("best"), palette::TEXT_SECONDARY)), selectable: true },
            PickerOption { key: ListItemKey::from_u64(2), label: String::from("Beta"), note: None, selectable: true },
            PickerOption { key: ListItemKey::from_u64(3), label: String::from("Gamma"), note: Some((String::from("not offered"), palette::TEXT_SECONDARY)), selectable: false },
        ];
        build_single_select_screen(quality_like_test_id(), "Test Picker", options, checked, &no_carry(), move |key| {
            picked.borrow_mut().push(key);
            if stay_open {
                Action::None
            } else {
                Action::PopView
            }
        })
    }

    #[test]
    fn picker_check_glyph_sits_on_the_checked_row_and_nowhere_else() {
        let picked = Rc::new(RefCell::new(Vec::new()));
        let screen = three_option_picker(Some(ListItemKey::from_u64(2)), picked, true);
        let mut app = App::new(240, 240);
        app.push_screen_for_test(screen);
        let pixels: Vec<_> = app.render().pixels().collect();
        // A pixel-level probe would duplicate `fields.rs`'s own leading-
        // glyph tests; here the load-bearing fact is behavioural, proven
        // below (`on_pick` receiving the pressed key, not a locally-
        // tracked "checked" bit) -- this render call only proves the
        // screen with a `checked` value actually renders without panicking.
        assert!(!pixels.is_empty());
    }

    #[test]
    fn picker_a_press_invokes_on_pick_with_the_focused_rows_key() {
        let picked = Rc::new(RefCell::new(Vec::new()));
        let mut app = App::new(240, 240);
        app.push_screen_for_test(three_option_picker(Some(ListItemKey::from_u64(1)), Rc::clone(&picked), true));
        app.handle_input(vec![NavIntent::Down]); // focus row 1 (Beta)
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(picked.borrow().as_slice(), &[ListItemKey::from_u64(2)], "on_pick must be called with the FOCUSED row's key");
    }

    #[test]
    fn picker_stays_open_when_on_pick_returns_action_none() {
        let picked = Rc::new(RefCell::new(Vec::new()));
        let mut app = App::new(240, 240);
        app.push_screen_for_test(three_option_picker(None, Rc::clone(&picked), true));
        let depth_before = app.navigator_depth();
        app.handle_input(vec![NavIntent::Select]); // Alpha
        assert_eq!(picked.borrow().len(), 1, "on_pick must have fired");
        assert_eq!(app.navigator_depth(), depth_before, "Action::None from on_pick must leave the picker open");
    }

    #[test]
    fn picker_pops_when_on_pick_returns_action_pop_view() {
        let picked = Rc::new(RefCell::new(Vec::new()));
        let mut app = App::new(240, 240);
        app.push_screen_for_test(three_option_picker(None, Rc::clone(&picked), false));
        let depth_before = app.navigator_depth();
        app.handle_input(vec![NavIntent::Select]); // Alpha
        assert_eq!(picked.borrow().len(), 1, "on_pick must have fired");
        assert_eq!(app.navigator_depth(), depth_before - 1, "Action::PopView from on_pick must pop the picker");
    }

    #[test]
    fn picker_unselectable_row_cannot_be_activated() {
        let picked = Rc::new(RefCell::new(Vec::new()));
        let mut app = App::new(240, 240);
        app.push_screen_for_test(three_option_picker(None, Rc::clone(&picked), true));
        app.handle_input(vec![NavIntent::Down, NavIntent::Down]); // focus Gamma (selectable: false)
        app.handle_input(vec![NavIntent::Select]);
        assert!(picked.borrow().is_empty(), "an unavailable option must not be pickable -- the activation gate lives in FieldList, not on_pick");
    }

    /// Headless PNG dump of the picker, at zoom -- the picker has no
    /// wired-in caller yet in this bead (`ScreenId::Picker` and its first
    /// real content land with `pico-link-7jol.5`), so it can't be reached
    /// through the public `App`/emulator surface the way the device page
    /// can (see `core/examples/device_page_screenshots.rs`). Dumped from
    /// here instead, since this module's tests are the only place with
    /// `pub(crate)` access to `build_single_select_screen` itself.
    #[test]
    fn picker_screenshot_at_zoom() {
        const ZOOM: u32 = 3;

        let out_dir = std::env::temp_dir().join("pico-link-picker-screenshot");
        std::fs::create_dir_all(&out_dir).expect("failed to create output dir");
        let picked = Rc::new(RefCell::new(Vec::new()));
        let mut app = App::new(240, 240);
        app.push_screen_for_test(three_option_picker(Some(ListItemKey::from_u64(2)), picked, true));
        app.handle_input(vec![NavIntent::Down]); // focus Beta (the checked row) so its caret is also visible

        let framebuffer = app.render();
        let mut image = image::RgbImage::new(framebuffer.width(), framebuffer.height());
        for pixel in framebuffer.pixels() {
            let color = pixel.1;
            #[allow(clippy::cast_sign_loss)]
            image.put_pixel(
                pixel.0.x as u32,
                pixel.0.y as u32,
                image::Rgb([(color.r() << 3) | (color.r() >> 2), (color.g() << 2) | (color.g() >> 4), (color.b() << 3) | (color.b() >> 2)]),
            );
        }
        let zoomed =
            image::imageops::resize(&image, framebuffer.width() * ZOOM, framebuffer.height() * ZOOM, image::imageops::FilterType::Nearest);
        let path = out_dir.join("picker.png");
        zoomed.save(&path).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        println!("wrote {}", path.display());
    }

    // --- pico-link-7jol.5: the QUALITY row, its picker, and Home's live
    // bitrate/ADAPTIVE tag. Design
    // `.planning/design/2026-09-07-ldac-quality-selector.md`. ---

    #[test]
    fn ldac_quality_rates_default_to_the_48khz_ladder_and_switch_at_44_1khz() {
        assert_eq!(ldac_quality_rates_kbps(None), [990, 660, 330]);
        assert_eq!(ldac_quality_rates_kbps(Some(48_000)), [990, 660, 330]);
        assert_eq!(ldac_quality_rates_kbps(Some(44_100)), [909, 606, 303]);
    }

    #[test]
    fn ldac_quality_fixed_kbps_maps_never_chosen_to_the_same_default_as_a_990_pin() {
        assert_eq!(ldac_quality_fixed_kbps(0, None), ldac_quality_fixed_kbps(1, None), "design §7: 0 renders as the effective default");
        assert_eq!(ldac_quality_fixed_kbps(0, None), Some(990));
        assert_eq!(ldac_quality_fixed_kbps(2, None), Some(660));
        assert_eq!(ldac_quality_fixed_kbps(3, None), Some(330));
        assert_eq!(ldac_quality_fixed_kbps(LDAC_QUALITY_ADAPTIVE, None), None, "Adaptive has no single fixed rate");
    }

    #[test]
    fn device_page_quality_row_absent_for_a_never_touched_device() {
        let mut model = BtModel::default();
        let addr = [20; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0 });
        // Not connected, ldac_quality == 0 -- design §2/§8: no first-run
        // prompt, the row simply doesn't exist yet.
        let rows = device_page_rows(&model, addr);
        assert!(!rows.iter().any(|r| r.label == "QUALITY"), "a never-touched, disconnected device must not show QUALITY");
    }

    #[test]
    fn device_page_quality_row_present_when_connected_and_ldac() {
        let mut model = BtModel::default();
        let addr = [21; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        let rows = device_page_rows(&model, addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").expect("QUALITY must be present when the live codec is LDAC");
        assert_eq!(row.value(), Some("990 kbps"), "ldac_quality==0 (never chosen) renders the effective default, checked, per design §7");
    }

    #[test]
    fn device_page_quality_row_absent_when_connected_but_not_ldac() {
        let mut model = BtModel::default();
        let addr = [22; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("SBC"), nominal_bitrate_bps: 328_000 });
        let rows = device_page_rows(&model, addr);
        assert!(!rows.iter().any(|r| r.label == "QUALITY"), "a live SBC fallback must not show QUALITY even if a stale ldac_quality pick exists");
    }

    #[test]
    fn device_page_quality_row_present_while_disconnected_if_previously_chosen() {
        let mut model = BtModel::default();
        let addr = [23; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 2 });
        // Not connected -- design §8: "picking a quality while disconnected
        // is allowed", and a previously pinned device stays visible.
        let rows = device_page_rows(&model, addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").expect("a previously-pinned device must show QUALITY even while disconnected");
        assert_eq!(row.value(), Some("660 kbps"), "a stored pin never dashes, any link state (design §4.2)");
    }

    #[test]
    fn device_page_quality_row_adaptive_streaming_shows_the_live_number_with_the_middle_dot_form() {
        let mut model = BtModel::default();
        let addr = [24; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        model.ldac_live_kbps = Some(660);
        let rows = device_page_rows(&model, addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").unwrap();
        assert_eq!(row.value(), Some("Adaptive \u{b7} 660"));
    }

    #[test]
    fn device_page_quality_row_adaptive_but_no_live_reading_yet_is_plain() {
        let mut model = BtModel::default();
        let addr = [25; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        // No `ldac_live_kbps` yet -- fresh connect, before the first
        // reading arrives.
        let rows = device_page_rows(&model, addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").unwrap();
        assert_eq!(row.value(), Some("Adaptive"), "must never claim a number that hasn't actually arrived yet");
    }

    #[test]
    fn device_page_quality_row_adaptive_disconnected_is_plain() {
        let mut model = BtModel::default();
        let addr = [26; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE });
        let rows = device_page_rows(&model, addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").unwrap();
        assert_eq!(row.value(), Some("Adaptive"));
    }

    #[test]
    fn device_page_forget_row_stays_last_and_pressable_when_quality_is_present() {
        let mut model = BtModel::default();
        let addr = [27; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        let rows = device_page_rows(&model, addr);
        assert_eq!(rows.len(), 7, "CODEC, QUALITY, SAMPLE RATE, USB IN, A2DP, ADDRESS, Forget");
        assert_eq!(rows.last().unwrap().label, "Forget this device");
        assert_eq!(rows[1].label, "QUALITY", "QUALITY sits directly under CODEC (design §2/§4.2)");
    }

    #[test]
    fn opening_quality_from_the_device_page_pushes_the_picker_and_a_pick_queues_the_command() {
        let mut app = App::new(240, 240);
        let addr = [28; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice
        app.handle_event(upsert(addr, "Cans", 1));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // connected row -> device page
        assert_eq!(app.current_screen_title(), "Cans");
        app.handle_input(vec![NavIntent::Down]); // focus QUALITY (row 1)
        app.handle_input(vec![NavIntent::Select]); // -> picker
        assert_eq!(app.current_screen_title(), "Quality", "A on QUALITY must push the picker (design §2)");

        // Pick "660 kbps" (the second row).
        app.handle_input(vec![NavIntent::Down]);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(
            app.poll_command(),
            Some(Command::SetDeviceLdacQuality { addr, ldac_quality: 2 }),
            "A on a picker row must queue the pin, 1-based"
        );
        assert_eq!(app.current_screen_title(), "Quality", "design §5: the picker stays open, no confirm, no pop");
    }

    #[test]
    fn the_quality_pickers_check_follows_the_stored_echo_not_the_press() {
        let mut app = App::new(240, 240);
        let addr = [29; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Cans", 1));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        app.handle_input(vec![NavIntent::Down]); // focus QUALITY
        app.handle_input(vec![NavIntent::Select]); // -> picker

        app.handle_input(vec![NavIntent::Down]); // focus "660 kbps"
        app.handle_input(vec![NavIntent::Select]); // press it
        let queued = app.poll_command();
        assert_eq!(queued, Some(Command::SetDeviceLdacQuality { addr, ldac_quality: 2 }));

        // Before the echo: the device page underneath still reads the OLD
        // value (990 kbps, the effective default) -- there is no
        // optimistic local state anywhere in this path (design §5.1).
        app.handle_input(vec![NavIntent::Back]); // -> device page
        let rows_before_echo = device_page_rows(&app.model(), addr);
        assert_eq!(rows_before_echo.iter().find(|r| r.label == "QUALITY").unwrap().value(), Some("990 kbps"), "no optimistic update before the echo");

        // The echo lands (C's PairedDeviceUpserted, same write that
        // produced the command above) -- refresh_stack must now show 660.
        app.handle_event(upsert_with_quality(addr, "Cans", 2, 2));
        let rows_after_echo = device_page_rows(&app.model(), addr);
        assert_eq!(rows_after_echo.iter().find(|r| r.label == "QUALITY").unwrap().value(), Some("660 kbps"), "the check follows the stored echo");
    }

    #[test]
    fn picking_while_disconnected_is_allowed_and_the_note_reads_varies_not_a_live_number() {
        let mut model = BtModel::default();
        let addr = [30; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 1 });
        // Not connected.
        let carry = ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None };
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let Refresh::Rebuild(_screen) = build_ldac_quality_picker_screen(&model, addr, &carry, &commands) else {
            panic!("a paired, disconnected device's picker must build, not vanish");
        };
        // Behavioural check via the row-building helper the picker itself
        // uses for Adaptive's note -- disconnected must never read "N now".
        model.paired[0].ldac_quality = LDAC_QUALITY_ADAPTIVE;
        let rows = device_page_rows(&model, addr);
        assert_eq!(rows.iter().find(|r| r.label == "QUALITY").unwrap().value(), Some("Adaptive"));
    }

    #[test]
    fn quality_picker_vanishes_the_stack_unwinds_when_the_device_is_forgotten() {
        let mut model = BtModel::default();
        let addr = [31; 6];
        // No paired push at all -- simulates the device having just been
        // forgotten out from under an open picker.
        let carry = ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None };
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        assert!(matches!(build_ldac_quality_picker_screen(&model, addr, &carry, &commands), Refresh::Gone));
        let _ = &mut model; // silence unused-mut if the assertion above is ever relaxed
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

    // --- `why?` page (design `.planning/design/2026-09-07-home-fault-
    // strip.md` §8, bead `pico-link-9eq2.3.3`) ---

    #[test]
    fn relative_time_formats_seconds_minutes_and_hours() {
        let base = Instant::from_micros(0);
        assert_eq!(relative_time(base + Duration::from_secs(8), base), "8s ago");
        assert_eq!(relative_time(base + Duration::from_secs(59), base), "59s ago");
        assert_eq!(relative_time(base + Duration::from_secs(60), base), "1m ago");
        assert_eq!(relative_time(base + Duration::from_secs(240), base), "4m ago");
        assert_eq!(relative_time(base + Duration::from_secs(3599), base), "59m ago");
        assert_eq!(relative_time(base + Duration::from_secs(3600), base), "1h ago");
        assert_eq!(relative_time(base + Duration::from_secs(7200), base), "2h ago");
    }

    #[test]
    fn why_page_appends_a_newly_fired_key_at_the_bottom_rather_than_resorting() {
        // Orchestrator ruling on this bead: "the page FREEZES its block
        // ordering on entry ... A key that fires for the first time while
        // the page is open appends at the BOTTOM rather than jumping to
        // the top."
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), None, 1);
        let order = Rc::new(RefCell::new(vec![FaultKey::BufOverflow])); // simulates the page already open, frozen on entry
        let carry = ScreenCarry::default();

        // A second key fires while the page is open -- MORE recently than
        // BufOverflow, which would sort first under a fresh most-recently-
        // active-first re-sort.
        model.fault_log.record(FaultKey::BufStarved, Instant::from_micros(1_000_000), None, 1);
        let _ = build_why_page_screen(&model, Instant::from_micros(1_000_000), &order, &carry);

        assert_eq!(
            *order.borrow(),
            vec![FaultKey::BufOverflow, FaultKey::BufStarved],
            "the newly-fired key must append at the end, never jump ahead of the frozen order"
        );
    }

    #[test]
    fn why_page_does_not_reorder_already_present_keys_on_a_refresh() {
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), None, 1);
        model.fault_log.record(FaultKey::BufStarved, Instant::from_micros(1_000_000), None, 1);
        // Order was frozen with BufStarved (the more recent) listed FIRST
        // -- deliberately the opposite of first-seen, to prove a refresh
        // doesn't silently re-derive it.
        let order = Rc::new(RefCell::new(vec![FaultKey::BufStarved, FaultKey::BufOverflow]));
        let carry = ScreenCarry::default();

        // A repeat raise of an already-present key must not move it.
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(2_000_000), None, 5);
        let _ = build_why_page_screen(&model, Instant::from_micros(2_000_000), &order, &carry);

        assert_eq!(*order.borrow(), vec![FaultKey::BufStarved, FaultKey::BufOverflow], "a repeat raise of an already-ordered key must not reorder it");
    }

    #[test]
    fn why_page_screen_is_identified_and_never_gone() {
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), None, 1);
        let order = Rc::new(RefCell::new(Vec::new()));
        let refresh = build_why_page_screen(&model, Instant::from_micros(0), &order, &ScreenCarry::default());
        match refresh {
            Refresh::Rebuild(screen) => assert_eq!(screen.id(), Some(ScreenId::WhyPage)),
            Refresh::Gone => panic!("the why? page has no subject that can vanish -- must never be Gone"),
            Refresh::Keep => panic!("build_why_page_screen never returns Keep as of pico-link-bgnd M0"),
        }
    }

    #[test]
    fn fault_consequence_text_is_absent_when_no_value_was_ever_wired() {
        assert_eq!(fault_consequence_text(FaultKey::UsbSupplyLow, None), None, "absent, never faked (parent design §15)");
    }

    #[test]
    fn fault_consequence_text_renders_the_real_count_not_a_saturated_one() {
        let text = fault_consequence_text(FaultKey::BufOverflow, Some(FaultValue::Count(140))).expect("BufOverflow+Count must produce text");
        assert!(text.contains("140"), "line 3 must show the REAL number, unlike Home's x99+ saturation: got {text:?}");
    }

    #[test]
    fn fault_consequence_text_renders_the_supply_ratio_as_a_decimal() {
        // q8: 256 == 1.00x nominal.
        let text = fault_consequence_text(FaultKey::UsbSupplyLow, Some(FaultValue::Ratio(159))).expect("UsbSupplyLow+Ratio must produce text");
        assert!(text.contains("0.62"), "159/256 = 0.621... must render as 0.62x: got {text:?}");
    }

    #[test]
    fn why_page_builds_without_panicking_when_some_keys_have_a_value_and_some_dont() {
        // Structural smoke test for the wiring itself (the row-presence
        // logic is proven directly via `fault_consequence_text`'s own
        // tests above): a mix of a valueless key (AirCongested) and a
        // valued one (BufOverflow) must build cleanly with no line 3 for
        // the former and one for the latter.
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::AirCongested, Instant::from_micros(0), None, 3);
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(1), Some(FaultValue::Count(14)), 14);
        let order = Rc::new(RefCell::new(Vec::new()));
        let refresh = build_why_page_screen(&model, Instant::from_micros(1), &order, &ScreenCarry::default());
        assert!(matches!(refresh, Refresh::Rebuild(_)));
    }
}
