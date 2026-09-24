use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use crate::render::Instant;

use super::events::{ConnectFailureReason, StoreStatus, VolumeState};
use super::fault::FaultLog;

/// How many devices the flash store can remember (design section 6: slots
/// `PL:D:0`..`PL:D:7`). The Devices screen gates opening the wizard on this
/// *before* any radio work (design section 4) -- fullness must be
/// discoverable without `BTstack` ever attempting a pairing that a full store
/// would then refuse to persist.
pub(in crate::app) const MAX_PAIRED_DEVICES: usize = 8;

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

/// How long a channel's OUT-meter peak-hold cap stays pinned at its
/// highest recent reading before a lower peak is allowed to replace it
/// (bead pico-link-du0, design section 21 E17's "peak-hold cap"). 1.5s is
/// the conventional VU-meter hold time -- long enough to actually read a
/// transient peak at a glance, short enough not to look stuck.
pub(in crate::app) const OUT_LEVEL_HOLD_DURATION: Duration = Duration::from_millis(1500);

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
    pub(in crate::app) hold_l_at: Instant,
    pub(in crate::app) hold_r_at: Instant,
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

#[cfg(test)]
mod tests {
    use super::super::{App, Event};
    use super::*;

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
}
