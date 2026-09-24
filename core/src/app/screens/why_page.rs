use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::render::theme::palette;
use crate::render::{FieldList, FieldRow, Instant, ListItemKey, Screen};

use super::super::{BtModel, FaultGlyphClass, FaultKey, FaultSeverity, FaultValue, Refresh, ScreenCarry, ScreenId};

/// The `why?` page's fixed title.
const WHY_PAGE_TITLE: &str = "WHY?";

/// Formats `elapsed_since(at)` as a short relative age -- `"8s ago"`,
/// `"4m ago"`, `"2h ago"` -- **never an absolute timestamp**: this board
/// has no RTC, so `now`/`at` are both
/// [`crate::run::FAULT_LIVE_WINDOW`]-scale [`Instant`]s derived from the
/// FFI seam's monotonic microsecond clock, never wall-clock time.
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

/// The Home fault strip's `why?` detail page -- a scrollable list of
/// **kinds, not events**: one aggregated two-line block per [`FaultKey`]
/// that has ever fired, in `order`'s sequence, including retired keys --
/// this is the session history.
///
/// **Ordering discipline is the load-bearing part of this function.**
/// `order` is the caller's frozen block order, established once by
/// `render::home`'s `ShortcutX` handler (a fresh
/// most-recently-active-first sort, written directly into the shared
/// `Rc<RefCell<_>>` at push time) and never re-sorted by this function on
/// any subsequent call. What this function DOES do, every call (including
/// the very first, harmlessly, since `order` starts empty then):
/// **append** any key that has an entry in [`BtModel::fault_log`] but is
/// not yet present in `order`, at the END -- a key that fires for the
/// first time while the page is open appends at the bottom rather than
/// jumping to the top. A live re-sort under a scrolling thumb is exactly
/// the moving-target problem Home's own rows reject, worse here because
/// the user is reading, not glancing.
///
/// Never returns [`Refresh::Gone`] -- this page has no subject that can
/// vanish out from under it (unlike [`ScreenId::DevicePage`]'s device).
/// The `why?` page's line 3: "one plain-language consequence sentence plus
/// the raw number." This function fills that in, one sentence per key,
/// with the actual raw value -- never the saturated/rounded figure Home's
/// own count slot uses ("no saturation here"). `None` (a key whose value
/// has never been wired -- e.g. the USB supply ratio before
/// `pl_usb_supply_q8()` lands) means no line 3 at all, never a guessed
/// number.
fn fault_consequence_text(key: FaultKey, value: Option<FaultValue>) -> Option<String> {
    let value = value?;
    // Every arm below is measured against the why? page's real row budget
    // (`font::value()`, ~194px -- `core/examples/fault_strip_probe.rs`'s
    // `measure_why_page_consequence_texts`) and kept under it: `FieldList`
    // CLIPS an overlong label rather than ellipsising it, which for a full
    // sentence reads as a confusing mid-word cut rather than the name
    // truncation this render core uses everywhere else -- so these stay
    // short by construction, not by luck.
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
        // A key paired with a `FaultValue` variant it's never assigned --
        // e.g. a firmware bug sending the wrong `value_kind` tag. Never
        // fabricate a sentence for an undefined combination; the
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
        // Line 1: glyph, name, TOTAL count -- "no saturation here, show
        // the real number", unlike Home's own `x99+` cap.
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
        // Line 3: "one plain-language consequence sentence plus the raw
        // number." Absent when `entry.value` is `None` -- some keys have
        // never had a value wired (e.g. the USB supply ratio, "absent
        // (`None`) until `pl_usb_supply_q8()` exists") -- absent, never
        // faked.
        if let Some(text) = fault_consequence_text(key, entry.value) {
            rows.push(FieldRow::readonly(text).with_key(ListItemKey::from_u64(next_key)));
            next_key += 1;
        }
    }

    let list = FieldList::new(rows).with_selected_identity(carry.selected_key, carry.selected_index);
    let list = if let Some(top) = carry.scroll_top { list.with_scroll_top(top) } else { list };
    Refresh::Rebuild(Screen::new(WHY_PAGE_TITLE, vec![Box::new(list)]).with_id(ScreenId::WhyPage))
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;

    // --- `why?` page ---

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
        // The page FREEZES its block ordering on entry -- a key that
        // fires for the first time while the page is open appends at the
        // BOTTOM rather than jumping to the top.
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
