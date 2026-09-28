use alloc::string::String;

use super::fault::{FaultKey, FaultValue};
use super::{ConnectedCodec, DeviceAddr, DeviceEntry, LinkState, PairedDevice};

/// A user-initiated action queued by the devices screen for C to poll via
/// [`App::poll_command`] (`pl_ui_poll_command` in the FFI surface). `core`
/// never acts on these itself -- it has no Bluetooth stack to act with --
/// it only records "the user asked for this" and hands it back out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    StartScan,
    /// `name` is already truncated to [`MAX_DEVICE_NAME_BYTES`] on a UTF-8
    /// character boundary by [`truncate_device_name`] before this variant
    /// is ever constructed -- `core` owns text, C only ever sees bytes. An
    /// empty `name` is a legal, deliberate value for a retry reissued from
    /// [`WizardPhase::NotResponding`]/[`WizardPhase::Failed`] (which carry
    /// no name of their own): C's read-modify-write persist path treats an
    /// empty name as "keep whatever name the record already has", so this
    /// never regresses an already-known name.
    ///
    /// `seq` is [`super::model::ConnectAttempt::seq`] -- ADA DESIGN v2
    /// (bead `pico-link-chc3`): `core` allocates it at
    /// [`super::radio_actions::connect`] and C echoes it on every
    /// connect-lifecycle event ([`Event::ConnectStepChanged`]/
    /// [`Event::ConnectRetrying`]/[`Event::ConnectSucceeded`]/
    /// [`Event::ConnectFailed`]) so a late echo of a cancelled or
    /// superseded attempt can be told apart from the attempt this `seq`
    /// actually belongs to. Never `0` (see [`super::model::BtModel::
    /// bump_attempt_seq`]'s doc comment).
    Connect { addr: [u8; 6], name: String, seq: u16 },
    /// User-initiated: stop an in-flight inquiry scan. Needed because the
    /// inquiry scan runs a fixed 10.24s with nothing else to interrupt it:
    /// without this, B does nothing during that window and, in the
    /// zero-results case, the screen is empty for the whole 10.24s --
    /// exactly when a user reaches for a button, gets nothing, and
    /// concludes the device is frozen.
    CancelScan,
    /// User-initiated: abort an in-flight connect attempt (phase 4
    /// `Connecting` or phase 5 `NotResponding`). Whether C actually
    /// implements the abort (real ACL/AVDTP teardown) or this stays
    /// plumbed-but-unhandled is a separate, C-side decision.
    ///
    /// `seq` is the [`super::model::ConnectAttempt::seq`] being cancelled
    /// (`0` if there was no attempt in flight to cancel -- see
    /// [`super::radio_actions::cancel_connect`]'s doc comment). ADA DESIGN
    /// v2: C scopes the abort to the cid owned by this `seq`, never
    /// globally -- a debug-only `PL_SEQ_ANY` ("cancel whatever is in
    /// flight") exists on the C side only and is never produced here.
    CancelConnect { addr: [u8; 6], seq: u16 },
    /// `core`'s auto-reconnect/remember-this-device policy output: "please
    /// persist `addr` as the last-used device." Queued by
    /// [`App::on_connect_succeeded`] once a connect attempt actually
    /// succeeds -- `core` decides *when* a device is worth remembering;
    /// C only stages the write, gates it against the streaming/timing
    /// constraints flash access has on this hardware, and eventually
    /// flushes it. `core` has no flash of its own and never writes
    /// anything itself.
    PersistDevice { addr: [u8; 6] },
    /// User-initiated "forget this remembered device" -- the Devices
    /// screen's X action on a paired row, or the pick-one-to-forget flow's
    /// own confirm when the store is full. `core` does not remove `addr`
    /// from [`BtModel::paired`] itself on queuing this -- the
    /// single-writer rule means `paired` only changes on a real
    /// [`Event::PairedDeviceForgotten`] echoed back once C's delete
    /// (record + link key) actually lands.
    ForgetDevice { addr: [u8; 6] },
    /// User-initiated "drop the current Bluetooth link". Carries no
    /// address: `firmware/src/a2dp.c` tracks at most one active connection
    /// at a time (`s_ctx.a2dp_cid`), and the C side's existing debug-only
    /// disconnect path (`pl_bt_debug_disconnect`) already queues
    /// `PL_BT_PENDING_DISCONNECT` with a null address for the same reason
    /// -- this reuses that assumption rather than inventing a
    /// currently-meaningless target parameter.
    Disconnect,
    /// User-initiated: pin `addr`'s LDAC quality to `ldac_quality` (1-based
    /// -- see [`PairedDevice::ldac_quality`]'s doc comment), or select
    /// Adaptive (`4`). Queued by the `QUALITY` picker's `A` handler --
    /// applies live, no confirm, picker stays open. C stages the flash
    /// write (gated the same way `PersistDevice`'s write is, while the
    /// host is streaming) and, if `addr` is the currently connected
    /// device's live LDAC stream, applies it to the running encoder
    /// immediately -- otherwise the pick takes effect at the next
    /// connect, same as every other per-device setting. The check follows
    /// the [`Event::PairedDeviceUpserted`] echo this write produces, never
    /// the press itself.
    SetDeviceLdacQuality { addr: DeviceAddr, ldac_quality: u8 },
    /// User-initiated: create (`preset_id == 0`) or overwrite (`preset_id`
    /// nonzero) a DSP effects preset with `blob`'s contents -- bead
    /// `pico-link-ryw.5`, design sec 3.2. Andreas's ruling: "every value
    /// change in the editor saves immediately," so a future preset editor
    /// (`pico-link-ryw.7`) queues this on EVERY field edit, not just on
    /// exit -- `core` never stages/coalesces these the way
    /// `SetDisplaySettings`'s latch does (design D9), because there is no
    /// "last write wins before C polls" race to close here: C's flash
    /// write is the source of truth for the allocated id, and every save
    /// (including a rapid burst) must reach it so
    /// [`Event::PresetLoaded`]'s echo can confirm each one in turn. `blob`
    /// is [`crate::dsp::preset::Preset::to_wire`]'s exact output -- `core`
    /// owns the wire format, C stores it opaquely (design sec 2.2: "C
    /// NEVER parses the blob").
    SavePreset { preset_id: u16, blob: alloc::vec::Vec<u8> },
    /// User-initiated: delete a DSP effects preset (the preset list's
    /// delete-confirm flow, `pico-link-ryw.7`). Per design sec 2.4, C does
    /// NOT rewrite any device record that referenced this id -- a dangling
    /// reference resolves to Off by construction
    /// ([`crate::dsp::PresetStore::resolve`]'s doc comment), so no
    /// up-to-8-device rewrite is needed here.
    DeletePreset { preset_id: u16 },
    /// User-initiated: assign (or clear, with [`crate::dsp::store::NO_PRESET_ID`])
    /// `addr`'s DSP effects preset -- the device page's preset-assignment
    /// row (`pico-link-ryw.7`). Applies live immediately (the pull API in
    /// `ui-ffi` recomputes the active program from whatever `core` now
    /// resolves for the connected device, design sec 3.2's "core computes
    /// the program from the active preset"); C additionally stages the
    /// flash write, same discipline as [`Self::SetDeviceLdacQuality`]. The
    /// check follows the [`Event::PairedDeviceUpserted`] echo this write
    /// produces, never the local press.
    AssignPreset { addr: DeviceAddr, preset_id: u16 },
}

/// Why a connect attempt failed, as reported by C over
/// [`Event::ConnectFailed`]. `core` has no Bluetooth stack of its own -- it
/// only records the *category* of failure BTstack/the radio reported, for a
/// (future) screen to render with an appropriate remedy.
///
/// Distinguishing these five (rather than a single generic "failed") is
/// deliberate: each has a different user-facing remedy, and
/// [`ConnectFailureReason::retryable`] draws the one distinction that
/// matters most -- some failures are worth an automatic or user-initiated
/// retry, and two structurally are not (see that method's doc comment).
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
    /// entry. Retrying without a way to supply the PIN can never succeed
    /// either, so this is also non-retryable until text entry exists.
    NeedsPin,
    /// The radio/HCI layer itself reported an error (not a per-device
    /// remote-side rejection) -- typically transient, worth retrying.
    RadioError,
}

/// What C's flash-backed store looked like at boot, as reported by
/// [`Event::StoreLoaded`]. `core` has no flash of its own -- this is purely
/// what C found, so a fresh/reset store is never rendered identically to a
/// healthy one that simply has no device saved yet.
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

/// Who most recently drove [`BtModel::volume`] -- carried through purely
/// for display/diagnostics, matching `firmware/src/volume.h`'s
/// `PlVolumeSource` doc comment ("NOT used by the loop-breaking rule").
/// `core` does not act on this today: it is stored so a future screen can
/// read it without a second reshape of this seam -- see
/// [`Event::VolumeChanged`]'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSource {
    /// The USB host's feature-unit volume (macOS's output slider).
    Host,
    /// The connected A2DP/AVRCP sink (the headphones themselves).
    Sink,
    /// A future on-device volume control, out of scope until that lands --
    /// representable now so this enum doesn't need a second non-additive
    /// reshape when it arrives.
    Device,
}

/// One canonical volume reading -- `level` is always in the AVRCP
/// absolute-volume domain (0..127), which `firmware/src/volume.c`'s
/// canonical state already uses as ITS native domain (no mapping needed
/// here, unlike the USB feature-unit's dB*100 domain that module maps on
/// ingest). `muted` mirrors `firmware/src/volume.c`'s `s_muted`, currently
/// always `false` -- no mute source is wired yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeState {
    pub level: u8,
    pub muted: bool,
    pub source: VolumeSource,
}

impl VolumeState {
    /// `level` (0..127, the AVRCP absolute-volume domain) converted to the
    /// 0..100 percent domain the display shows -- both peers speak percent,
    /// and 0..127 is an implementation detail that must not leak onto the
    /// screen. Rounds half-up; checked to map the two endpoints exactly
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
    /// and extend the idle timer -- true only for a non-host source that
    /// isn't itself muted or at 0%. A sink-originated mute/zero must NOT
    /// "wake to full" (that would flash the panel bright at the exact
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
/// tagged union in). `core` never originates these; it only folds them
/// into [`BtModel`] and marks the app dirty -- see [`App::handle_event`].
/// Adding a variant is purely additive (every existing tag/payload is
/// unchanged) and does not need a version bump -- see `ui-ffi`'s module
/// doc for the version-guard rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    LinkStateChanged(LinkState),
    /// Whether the radio is currently running a GAP inquiry -- the second,
    /// independent axis [`LinkState`]'s doc comment describes. Folded by
    /// [`App::set_discovering`], which deliberately touches only
    /// [`BtModel::discovering`] and none of the four connected-model fields
    /// [`App::set_link_state`] clears: an inquiry does not disconnect A2DP.
    DiscoveryStateChanged { scanning: bool },
    /// A connect attempt has begun -- the THIRD, independent axis
    /// [`LinkState`]'s doc comment describes (bead `pico-link-0cq2`). On
    /// the wire this is the former `PL_LINK_STATE_CONNECTING` (value `2`),
    /// decoded by `ui-ffi` into this variant instead of
    /// `LinkStateChanged` -- no C ABI change, no wire byte change. Folded
    /// by [`App::set_connecting`], which deliberately touches only
    /// [`BtModel::connecting`] and none of the connected-model fields
    /// [`App::set_link_state`] clears: an attempt targeting a second
    /// device must not disturb an already-established link to a first.
    ConnectAttemptStarted,
    DeviceDiscovered(DeviceEntry),
    DevicesCleared,
    /// `seq` is ADA DESIGN v2's (bead `pico-link-chc3`) attempt identity --
    /// see [`Command::Connect::seq`]'s doc comment. `0` means "not core's
    /// attempt" (a remote-initiated session, or the `PL_DEBUG_REMOTE`
    /// bypass): [`App::record_connect_failure`] logs it but touches
    /// neither [`BtModel::attempt`] nor the wizard for a `0` `seq`.
    /// Nonzero: applied iff it matches [`BtModel::attempt`]'s `seq`,
    /// dropped otherwise (a stale echo of a cancelled or superseded
    /// attempt) -- see that method's doc comment.
    ConnectFailed { addr: [u8; 6], reason: ConnectFailureReason, seq: u16 },
    /// One of phase 4's four named sub-steps has begun (ACL connect,
    /// SSP/link-key pairing, AVDTP stream setup, then codec negotiation --
    /// see [`ConnectStep`]). Only meaningful while the wizard's phase is
    /// [`WizardPhase::Connecting`] or [`WizardPhase::NotResponding`] (a
    /// stray event outside that window -- e.g. arriving after the user
    /// backed out -- is silently ignored; see
    /// [`App::on_connect_step_changed`]).
    ///
    /// `seq`: same identity/gating rule as [`Event::ConnectFailed::seq`],
    /// except a `0` `seq` here is simply ignored (ADA DESIGN v2) -- a
    /// remote session has no wizard step sequence to report against.
    ConnectStepChanged(ConnectStep, u16),
    /// C is about to retry the in-flight connect attempt after the ~6s
    /// "not responding" surfacing point -- carries the attempt number so
    /// the wizard's "Still trying (N)" counter has something to increment.
    /// Like [`Event::ConnectStepChanged`], a no-op outside the
    /// `Connecting`/`NotResponding` phases, and outside its own matching
    /// `seq` (same rule as [`Event::ConnectStepChanged::seq`]).
    ConnectRetrying { attempt: u16, seq: u16 },
    /// The in-flight connect attempt succeeded. `degraded` distinguishes
    /// plain success (auto-dismisses) from degraded success (does not --
    /// see [`Event::WizardAutoDismiss`]'s doc comment for why that
    /// distinction matters enough to be its own event rather than inferred
    /// from `LinkStateChanged(Connected)` alone, which carries no fallback
    /// information). `addr`: which device this success is for, so
    /// [`App::on_connect_succeeded`] can queue [`Command::PersistDevice`]
    /// correctly REGARDLESS of whether the connection was driven by the
    /// wizard (which separately tracks `addr` on [`WizardPhase`]) or the
    /// `PL_DEBUG_REMOTE` bypass path (which does not touch the wizard at
    /// all).
    ///
    /// `seq`: ADA DESIGN v2's attempt identity (same rule as
    /// [`Event::ConnectFailed::seq`]) -- EXCEPT `0` here means a session
    /// genuinely *appeared* (a headset-initiated reconnect, or the
    /// `PL_DEBUG_REMOTE` bypass): [`App::on_connect_succeeded`] still sets
    /// [`BtModel::connected_addr`] and queues [`Command::PersistDevice`]
    /// for it, but leaves [`BtModel::attempt`]/`last_outcome`/the wizard
    /// untouched, so it can never resurrect or conclude an attempt it
    /// didn't start. A nonzero `seq` not matching the live attempt is
    /// dropped entirely (F7: a late success for an attempt the user
    /// already cancelled is a stray echo of *that* attempt, not a new
    /// session).
    ConnectSucceeded { addr: [u8; 6], degraded: bool, seq: u16 },
    /// C's own ~2s timer firing -- explicitly *not* a core-owned clock
    /// feature, see [`crate::app`]'s `now_us`/`tick` doc comments for why
    /// timers stay on the C side of this seam. Pops the wizard back to
    /// Devices **only** if the wizard is currently showing a plain
    /// (non-degraded) success -- degraded success requires acknowledgement
    /// and must never auto-dismiss -- and this event arriving at any other
    /// time (a stray/late timer, the user having already backed out) is a
    /// defensive no-op.
    WizardAutoDismiss,
    /// The live A2DP link's codec finished negotiating (or renegotiated),
    /// as reported by C's signaling codec-configuration handler
    /// (`firmware/src/a2dp.c`) -- never the media timer path. Folds into
    /// [`BtModel::connected_codec`]; see [`ConnectedCodec`]'s doc comment
    /// for why `core` never derives the name/bitrate itself.
    CodecChanged(ConnectedCodec),
    /// C's flash-backed store finished loading at boot -- fired exactly
    /// once, after the radio is confirmed up (see `firmware/src/bt.c`'s
    /// `BTSTACK_EVENT_STATE`/`HCI_STATE_WORKING` case; pushed there rather
    /// than at `pl_bt_init` itself so a queued auto-reconnect can't race
    /// `hci_power_control`'s own async power-up), and always the
    /// *terminator* of C's boot push sequence: `count` x
    /// [`Event::PairedDeviceUpserted`], then this event.
    ///
    /// Carries no target address: with eight slots, which device to
    /// auto-reconnect to is a real policy decision, not "the one slot", and
    /// that policy lives on `core`'s side of the seam.
    /// [`App::on_store_loaded`] computes the target itself, once every
    /// preceding `PairedDeviceUpserted` has folded into
    /// [`BtModel::paired`]: `paired.iter().max_by_key(|d| d.mru_seq)`,
    /// queuing [`Command::Connect`] for it -- reusing the exact same
    /// command the Devices screen's own paired-row activation uses; `core`
    /// never opens a connection itself.
    StoreLoaded { status: StoreStatus },
    /// One remembered (paired) device the flash store holds -- pushed
    /// either at boot (C's `count` x this event ahead of
    /// [`Event::StoreLoaded`]) or after a device is newly
    /// persisted/updated. The single-writer rule: this is one of exactly
    /// two events that mutate [`BtModel::paired`] (the other is
    /// [`Event::PairedDeviceForgotten`]) -- `core` never optimistically
    /// appends a row on [`Event::ConnectSucceeded`], because then `core`'s
    /// list and flash could disagree (a full store, a CRC failure, a
    /// deferred write) and a row for a device that was never actually
    /// persisted is exactly the "silently forgot your pairing" defect this
    /// whole line of work exists to kill.
    PairedDeviceUpserted(PairedDevice),
    /// A remembered device was removed from the flash store (a real
    /// deletion C echoed back, not merely requested -- see
    /// [`Command::ForgetDevice`]'s doc comment). The other of the two
    /// events allowed to mutate [`BtModel::paired`].
    PairedDeviceForgotten { addr: DeviceAddr },
    /// The flash store refused a save because every slot already held a
    /// *different* address ("must never silently evict"). Carries no
    /// payload -- there is nothing more specific to report than "full".
    /// The Devices screen already gates opening the wizard on
    /// `paired.len() < MAX_PAIRED_DEVICES` before any radio work, so this
    /// is the defensive fallback for a race that gate can't fully close
    /// (e.g. two connect attempts racing each other), not the primary way
    /// fullness is discovered.
    PairedStoreFull,
    /// One ~4Hz reading of the live A2DP output PCM level, per channel --
    /// computed cheaply off the real-time encode path in
    /// `firmware/src/a2dp.c` (a running peak/sum-of-squares accumulator
    /// updated per PCM block, reduced to one reading roughly every
    /// `PL_A2DP_LEVEL_PUSH_INTERVAL_MS` and pushed through the same `bt.c`
    /// MPSC ring every other Bluetooth-domain event uses -- never a direct
    /// Rust call from IRQ context). `peak_l`/`peak_r`/`rms_l`/`rms_r` are
    /// linear 0-255 scale (255 == full-scale PCM, i.e. clipping). Folds
    /// into [`BtModel::out_level`], which [`App::set_link_state`] clears
    /// alongside `connected_codec`/`connected_addr` on any disconnect --
    /// see that field's doc comment for the "absent, never frozen" rule.
    LevelsChanged {
        peak_l: u8,
        peak_r: u8,
        rms_l: u8,
        rms_r: u8,
    },
    /// One canonical volume reading -- pushed by `firmware/src/bt.c`'s
    /// `pl_bt_push_volume_changed` whenever `volume.c`'s loop rule actually
    /// applies a new canonical level (never on an absorbed no-op, and
    /// never for `volume.c`'s debug-console-only `PL_VOLUME_SOURCE_CONSOLE`
    /// origin). Folds into [`BtModel::volume`]. `source` is carried, not
    /// acted on -- see [`VolumeSource`]'s doc comment.
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
    LdacBitrateChanged {
        kbps: u32,
    },
    /// One audio fault raised or refreshed, from C's 1Hz fault evaluator.
    /// `count` is C's ABSOLUTE running count for `key` --
    /// [`App::on_fault_raised`] *assigns* it into [`FaultLog`] rather than
    /// accumulating, so a dropped event costs one refresh cycle of
    /// staleness, never permanent drift.
    ///
    /// Deliberately does not carry `severity`/`glyph`: those are consumed
    /// only at the FFI event site to decide whether this raise wakes the
    /// display -- [`FaultLog`]'s own entry shape has no field for either,
    /// so `core`'s model never needs them.
    FaultRaised {
        key: FaultKey,
        value: Option<FaultValue>,
        count: u16,
    },
    /// C's flash-backed store finished loading the screensaver dim/off +
    /// timeout setting at boot, or -- in the emulator, which has no
    /// separate boot-load event of its own -- pushed synthetically right
    /// after `App::new` from `DisplaySettings::load`. Wire values, not
    /// [`DisplaySettings`] itself: `core` decodes with
    /// [`DisplaySettings::from_wire`] (`core` owns the live value, unlike
    /// the LDAC-quality picker, where C owns the device record).
    DisplaySettingsLoaded { mode: u8, timeout_s: u16 },
    /// C's flash-backed store finished loading the global congestion-
    /// cushion policy (`PL:S:1`) at boot -- bead pico-link-8pp1.4 (S3),
    /// design `.planning/design/2026-09-24-congestion-cushion.md` sec 4.
    /// Wire value, not [`crate::audio::CushionPolicy`] itself: `core`
    /// decodes with [`crate::audio::CushionPolicy::from_wire`], same
    /// discipline as [`Self::DisplaySettingsLoaded`] above.
    CushionPolicyLoaded { policy: u8 },
    /// C's flash-backed store finished loading the global LDAC Adaptive
    /// floor (`PL:S:2`) at boot -- bead pico-link-d42g.3 (F3), design
    /// `.planning/design/2026-09-25-adaptive-floor.md` sec 2. Wire value,
    /// not [`crate::audio::AbrFloor`] itself: `core` decodes with
    /// [`crate::audio::AbrFloor::from_wire`], same discipline as
    /// [`Self::CushionPolicyLoaded`] above.
    AbrFloorLoaded { floor: u8 },
    /// One DSP effects preset the flash store holds -- bead
    /// `pico-link-ryw.5`, design sec 2.2/3.2. Pushed either at boot (C's
    /// `count` x this event ahead of [`Event::PresetStoreLoaded`], same
    /// boot-sequence shape [`Event::PairedDeviceUpserted`]/
    /// [`Event::StoreLoaded`] use) OR as the save echo for
    /// [`Command::SavePreset`] -- `id` is C's allocated id either way
    /// (`preset_id == 0` on the command meant "allocate", and this event's
    /// `id` is never `0`: [`crate::dsp::store::NO_PRESET_ID`] is reserved
    /// and never assigned to a real preset). `blob` is opaque to C
    /// (design sec 2.2: "C NEVER parses the blob") -- `core` decodes it
    /// with [`crate::dsp::preset::Preset::from_wire`], same
    /// per-field-fallback discipline every other `*Loaded` wire decode in
    /// this module uses.
    PresetLoaded { id: u16, blob: alloc::vec::Vec<u8> },
    /// A DSP effects preset was deleted -- the [`Command::DeletePreset`]
    /// echo, a REAL deletion C's flash store performed, not merely
    /// requested (same "echo, not the request" discipline
    /// [`Event::PairedDeviceForgotten`]'s doc comment describes).
    PresetDeleted { id: u16 },
    /// C's flash-backed DSP preset store (`PL:P:<slot>`) finished loading
    /// at boot -- the terminator of C's `count` x [`Event::PresetLoaded`]
    /// boot push sequence, same shape [`Event::StoreLoaded`] is for
    /// [`Event::PairedDeviceUpserted`]. `status` reuses [`StoreStatus`]'s
    /// four variants (the device store's own boot-status vocabulary
    /// already covers "first boot / loaded / one record's CRC failed /
    /// schema version mismatch" and nothing about it is device-specific).
    ///
    /// `next_id` (bead `pico-link-ryw.14`, Ada's preset-id-allocation
    /// contract): C's own preset-id high-water mark -- the id C's flash
    /// allocator would hand out next. `core` now owns id allocation
    /// (`PresetStore::create`), so it must be seeded with this value
    /// (`PresetStore::raise_next_id`) before allocating anything itself, or
    /// a fresh id could alias one C already holds for a deleted-then-
    /// reused slot.
    PresetStoreLoaded { count: u16, status: StoreStatus, next_id: u16 },
}

/// Phase 4's four named connect sub-steps: naming the current one tells
/// the user *and us* where a stalled connect attempt actually got stuck,
/// which a single generic "Connecting..." spinner cannot.
///
/// `Disconnecting` (bead `pico-link-sfw6`, design `.planning/design/2026-
/// 09-25-device-switch-break-before-make.md` sec 3) is a fifth, additive
/// variant: A's teardown at the start of a break-before-make device
/// switch, before B's own four named sub-steps begin. Deliberately kept
/// out of [`ConnectStep::all`] -- that fixed four-element array is still
/// "the four named sub-steps of *a connect attempt*", and `Disconnecting`
/// is a distinct phase that precedes one, not a fifth step within it (see
/// `crate::render::wizard`'s `render_connecting_steps`, which renders it as
/// its own line rather than inserting it into that array).
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
    /// Tearing down the previous device's ACL before paging this one.
    Disconnecting,
}

impl ConnectStep {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ConnectStep::Connecting => "Connecting",
            ConnectStep::Pairing => "Pairing",
            ConnectStep::SettingUpAudio => "Setting up audio",
            ConnectStep::NegotiatingCodec => "Negotiating codec",
            ConnectStep::Disconnecting => "Disconnecting",
        }
    }

    /// The fixed display order phase 4 always shows the four steps in:
    /// "1 Connecting ... 2 Pairing ... 3 Setting up audio ... 4 Negotiating
    /// codec". Does NOT include [`ConnectStep::Disconnecting`] -- see this
    /// enum's doc comment.
    #[must_use]
    pub fn all() -> [ConnectStep; 4] {
        [ConnectStep::Connecting, ConnectStep::Pairing, ConnectStep::SettingUpAudio, ConnectStep::NegotiatingCodec]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- The percent formula, pinned at both endpoints ---

    #[test]
    fn volume_percent_pins_the_two_endpoints_exactly() {
        // "100% must mean maximum and 0% must mean silent, with no other
        // level able to render as either" -- so the endpoints are checked
        // exactly, not just "close enough".
        let silent = VolumeState { level: 0, muted: false, source: VolumeSource::Host };
        assert_eq!(silent.percent(), 0);
        let max = VolumeState { level: 127, muted: false, source: VolumeSource::Host };
        assert_eq!(max.percent(), 100);
    }

    #[test]
    fn volume_percent_matches_the_designs_worked_examples() {
        // Worked examples: `1 -> 1`, `126 -> 99`.
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

    // --- `VolumeState::wakes_idle` ---

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
        // A sink-originated mute/zero must never "wake to full".
        assert!(!VolumeState { level: 80, muted: true, source: VolumeSource::Sink }.wakes_idle());
        assert!(!VolumeState { level: 0, muted: false, source: VolumeSource::Sink }.wakes_idle());
        assert!(!VolumeState { level: 0, muted: true, source: VolumeSource::Device }.wakes_idle());
    }

    #[test]
    fn connect_failure_reasons_representable_and_the_two_impossible_ones_are_marked_non_retryable() {
        // Two causes (no A2DP sink, needs a PIN) must offer no retry
        // because retrying is structurally impossible -- see
        // `ConnectFailureReason::retryable`'s doc comment.
        assert!(ConnectFailureReason::Timeout.retryable());
        assert!(ConnectFailureReason::Rejected.retryable());
        assert!(ConnectFailureReason::RadioError.retryable());
        assert!(!ConnectFailureReason::NoA2dpSink.retryable());
        assert!(!ConnectFailureReason::NeedsPin.retryable());
    }
}
