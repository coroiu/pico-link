use crate::render::Instant;

/// The audio fault catalogue's six keys. Ordinals are append-only forever
/// -- this is the wire discriminant `ui-ffi`'s `PlFaultKey` mirrors 1:1
/// (see that type's `TryFrom` impl). Only the ordinal and the display
/// name live in `core` -- glyph and severity are authoritative on the C
/// side and travel with each event instead ("names, strings and colours
/// live in Rust... a rename is then a Rust-only change").
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

    /// <= 16 char, uppercase ASCII display name. A rename is a Rust-only
    /// change -- never a firmware reflash.
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

    /// This key's glyph class -- **static per key, never per-event**. The
    /// wire payload also carries a `glyph` byte
    /// (`PlAudioFaultPayload::glyph`), but [`Event::FaultRaised`]
    /// deliberately does not forward it into `core` (see that variant's
    /// doc comment) -- it doesn't need to, because a key can never change
    /// glyph class at runtime. This table is therefore the single, static
    /// source of truth the render layer (`render::hero`) reads, matching
    /// the wire exactly by construction rather than by trusting a value
    /// that could in principle disagree frame to frame.
    #[must_use]
    pub const fn glyph(self) -> FaultGlyphClass {
        match self {
            FaultKey::BufStarved | FaultKey::UsbSupplyLow => FaultGlyphClass::Starved,
            FaultKey::BufOverflow => FaultGlyphClass::Filled,
            FaultKey::AirCongested | FaultKey::AirLinkLost | FaultKey::EncResync => FaultGlyphClass::Neutral,
        }
    }

    /// This key's **base** severity -- used by the render layer to pick
    /// red ([`FaultGlyphClass`]'s sibling colour table lives in
    /// `render::hero`) vs amber. Unlike [`Self::glyph`], this is a genuine
    /// simplification, not just an unforwarded wire field: `AirCongested`
    /// dynamically escalates `Concealed` -> `Audible` on co-occurrence with
    /// `BufOverflow` within a single evaluation window, but that
    /// escalation is consumed *only* at the FFI event site to decide a
    /// wake (`ui-ffi`'s `pl_ui_push_event`) and never crosses into
    /// [`Event::FaultRaised`] or [`FaultLog`] -- there is no per-entry
    /// severity field to read at render time (see [`FaultEntry`]'s own
    /// doc comment). So `AirCongested`'s Home row always renders at its
    /// base `Concealed` (amber) colour, even during a window where it was
    /// briefly `Audible` on the wire for wake purposes -- a deliberate,
    /// documented gap, not a bug: fixing it would mean adding a severity
    /// field the model explicitly chose not to carry.
    #[must_use]
    pub const fn severity(self) -> FaultSeverity {
        match self {
            FaultKey::BufStarved | FaultKey::BufOverflow | FaultKey::AirLinkLost => FaultSeverity::Audible,
            FaultKey::UsbSupplyLow | FaultKey::AirCongested | FaultKey::EncResync => FaultSeverity::Concealed,
        }
    }
}

/// A fault key's glyph shape class -- the render-layer enum
/// [`FaultKey::glyph`] maps into; `render::hero`/`render::fault_glyph`
/// resolve this to one of the three drawn primitives.
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

/// A fault key's base severity -- `Audible` (red, `STATUS_ERROR`) or
/// `Concealed` (amber, `STATUS_WARNING`). See
/// [`FaultKey::severity`]'s doc comment for the one documented gap this
/// static table has relative to the wire's dynamic, per-event severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultSeverity {
    Audible,
    Concealed,
}

/// The `why?` page's value slot -- [`Event::FaultRaised`]'s optional
/// value, carried through into [`FaultEntry::value`] unchanged. `u16`
/// payloads throughout, matching the wire payload's `value: u16` field
/// (`ui-ffi`'s `PlAudioFaultPayload`) -- `core` never widens or rescales
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultValue {
    /// A q8 fixed-point ratio (the USB supply ratio).
    Ratio(u16),
    /// A plain count (frames dropped, occurrences).
    Count(u16),
    /// A duration in milliseconds (the windowed minimum ring fill --
    /// "`0ms` whenever the fault is real").
    Millis(u16),
}

/// One [`FaultKey`]'s state in [`FaultLog`]. `count` and `value` are the
/// most recent [`Event::FaultRaised`]'s payload, assigned wholesale each
/// time -- never accumulated. `first_seen`/`last_seen` are what the render
/// layer derives freshness tiers and retirement from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultEntry {
    pub count: u16,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub value: Option<FaultValue>,
}

/// The audio fault strip's model: a **fixed array of six entries**, one
/// per [`FaultKey`] -- no `Vec`, no allocation on a fault event. `None`
/// until a key has been raised at least once since boot; never removed
/// once raised (retirement is a render-time-only concept, computed from
/// `now - last_seen` -- there is no clear event over the wire).
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
    /// [`crate::run::FAULT_LIVE_WINDOW`] of `now` -- the "already Live"
    /// test, which gates a repeat raise from waking the display again.
    /// `false` for a key that has never been raised.
    #[must_use]
    pub fn is_live(&self, key: FaultKey, now: Instant) -> bool {
        self.entries[key.index()].is_some_and(|entry| now.saturating_duration_since(entry.last_seen) < crate::run::FAULT_LIVE_WINDOW)
    }

    /// Whether ANY key has an entry not yet retired at `now`
    /// (`now - last_seen > FAULT_RETIRE`) -- the Home fault strip's own
    /// "is the strip non-empty" test, shared by `render::hero`'s
    /// drawing/`paint_key`/`redraw_after` and `render::home`'s
    /// `ShortcutX` gate ("X is labelled and live only while the strip is
    /// non-empty").
    #[must_use]
    pub fn has_visible_entry(&self, now: Instant) -> bool {
        FaultKey::ALL
            .into_iter()
            .any(|key| self.entries[key.index()].is_some_and(|entry| now.saturating_duration_since(entry.last_seen) < crate::run::FAULT_RETIRE))
    }

    /// Folds one raise into `key`'s entry: `count` is ASSIGNED, never
    /// accumulated -- idempotent by construction, so a dropped event costs
    /// one refresh cycle of staleness rather than permanent drift.
    /// `first_seen` is set once, on the entry's first-ever raise, and left
    /// unchanged by every subsequent one (there is no "re-arm" concept
    /// here -- retirement and freshness are entirely render-time, derived
    /// from `last_seen`).
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- `FaultLog` ---

    #[test]
    fn fault_log_record_assigns_count_rather_than_accumulating_it() {
        // "The payload carries an absolute count, not an increment" --
        // C's own running total, assigned wholesale each time so a
        // dropped event only costs one refresh cycle of staleness rather
        // than permanent drift.
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
}
