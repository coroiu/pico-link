use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::input::NavIntent;
use crate::render::theme::palette;
use crate::render::{Action, FieldList, FieldRow, FocusEvent, FrameBuffer565, Instant, ListItemKey, PaintKey, RenderCtx, Screen, Verb, Widget};

use super::super::{BtModel, FaultGlyphClass, FaultKey, FaultSeverity, FaultValue, ModelHandle, ScreenId};

/// The `why?` page's fixed title.
const WHY_PAGE_TITLE: &str = "WHY?";

/// Seed for [`WhyPageView`]'s own projection key -- see
/// [`WhyPageView::sync`]'s doc comment. Unrelated to any widget's
/// [`PaintKey`]-the-paint-key -- this is a private "did the fields I read
/// from the model (plus `now`) change" hash, not anything compared against
/// a previous frame's drawn pixels.
const WHY_PAGE_PROJECTION_SEED: u64 = 61;

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

/// The `why?` page's line 3: "one plain-language consequence sentence plus
/// the raw number." `None` (a key whose value has never been wired --
/// e.g. the USB supply ratio before `pl_usb_supply_q8()` lands) means no
/// line 3 at all, never a guessed number.
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

/// Builds the `why?` page's rows from a live `&BtModel` read plus the
/// caller's `order` -- shared between [`WhyPageView::new`]'s initial
/// construction and [`WhyPageView::sync`]'s in-place update, so the two can
/// never drift into building rows two different ways. See
/// [`WhyPageView`]'s own doc comment for what `order` means and who owns
/// appending to it.
fn build_rows(model: &BtModel, now: Instant, order: &[FaultKey]) -> Vec<FieldRow> {
    let mut rows = Vec::new();
    let mut next_key = 0_u64;
    for &key in order {
        let Some(entry) = model.fault_log.entry(key) else {
            // Structurally unreachable: every key in `order` was inserted
            // only after confirming `fault_log.entry(key).is_some()`
            // (`WhyPageView::sync`), and entries are never removed once
            // raised (`FaultLog`'s own doc comment) -- defensive rather
            // than a panic, matching this crate's own convention
            // elsewhere (e.g. `device_page_rows`'s
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
    rows
}

/// A cheap, total summary of every field [`build_rows`] reads -- **not**
/// built from `FieldRow`'s own fields (most are private to
/// `crate::render::fields`), so this walks `order` against `model`/`now`
/// directly, computing the same rendered text `build_rows` would, and
/// folds that. [`WhyPageView::sync`]'s allocation-saving skip check, the
/// same projection-key shape [`super::picker::PickerView`]'s own
/// `projection_key_of` uses (design
/// `.planning/design/2026-09-24-live-widgets-retire-refresh-stack.md` §5's
/// R2 vocabulary: "everything the view READS", never folded into a
/// [`Widget::paint_key`]). Deliberately folds `now`: the relative-time
/// text on line 2 changes purely with the passage of time, with no
/// `BtModel` field changing at all, so `now` genuinely is part of what
/// this view reads.
fn projection_key_of(model: &BtModel, now: Instant, order: &[FaultKey]) -> PaintKey {
    let mut key = PaintKey::of(WHY_PAGE_PROJECTION_SEED).fold(order.len() as u64);
    for &fault_key in order {
        let Some(entry) = model.fault_log.entry(fault_key) else { continue };
        key = key.fold(fault_key as u64);
        key = key.fold(u64::from(entry.count));
        key = key.fold_str(&relative_time(now, entry.last_seen));
        key = key.fold_str(&relative_time(now, entry.first_seen));
        key = key.fold_opt_str(fault_consequence_text(fault_key, entry.value).as_deref());
    }
    key
}

/// The Home fault strip's `why?` detail page -- a scrollable list of
/// **kinds, not events**: one aggregated two-line block per [`FaultKey`]
/// that has ever fired, in `order`'s sequence, including retired keys --
/// this is the session history.
///
/// As of bead `pico-link-bgnd` M4, long-lived for as long as the `why?`
/// page stays on the navigator's stack -- [`Widget::sync`] re-reads the
/// live model itself every frame this screen is on top, instead of
/// [`build_why_page_screen`] being re-invoked on every fault event.
///
/// **Ordering discipline is the load-bearing part of this widget.**
/// `order` is seeded once by `render::home`'s `ShortcutX` handler (a fresh
/// most-recently-active-first sort, computed at push time and handed to
/// [`build_why_page_screen`]) and never re-sorted by this widget on any
/// subsequent [`Self::sync`] call. What [`Self::sync`] DOES do, every call
/// (including the very first, harmlessly, since `order` starts as whatever
/// was seeded): **append** any key that has an entry in
/// [`BtModel::fault_log`] but is not yet present in `order`, at the END --
/// a key that fires for the first time while the page is open appends at
/// the bottom rather than jumping to the top. A live re-sort under a
/// scrolling thumb is exactly the moving-target problem Home's own rows
/// reject, worse here because the user is reading, not glancing.
struct WhyPageView {
    list: FieldList,
    model: ModelHandle,
    /// The frozen block order -- see this struct's own doc comment. Owned
    /// directly by this widget (no more `App::why_page_order` mailbox,
    /// bead `pico-link-bgnd` M4): the page is pushed once and lives for as
    /// long as it stays on the navigator's stack, so there is no rebuild
    /// for a plain field to fail to survive.
    order: Vec<FaultKey>,
    /// The render-time clock, updated by every [`Self::sync`] call from
    /// `ctx.now()` -- the relative-time text on line 2 is computed against
    /// this.
    now: Instant,
    /// The last [`projection_key_of`]-shaped hash of [`build_rows`]'s
    /// output -- see that function's own doc comment. Recomputed every
    /// frame by [`Self::sync`]; [`FieldList::set_rows`] is only called
    /// when it changed -- an allocation-saving skip, not a correctness
    /// dependency, same as every other M2/M3 projection key in this crate.
    projection_key: PaintKey,
}

impl WhyPageView {
    fn new(model: &ModelHandle, now: Instant, mut order: Vec<FaultKey>) -> Self {
        let model_ref = model.borrow();
        for key in FaultKey::ALL {
            if model_ref.fault_log.entry(key).is_some() && !order.contains(&key) {
                order.push(key);
            }
        }
        let rows = build_rows(&model_ref, now, &order);
        let projection_key = projection_key_of(&model_ref, now, &order);
        drop(model_ref);
        let list = FieldList::new(rows);
        Self { list, model: Rc::clone(model), order, now, projection_key }
    }
}

impl Widget for WhyPageView {
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

    /// Appends any newly-fired key to `order` (never re-sorts, never
    /// removes -- see this struct's own doc comment), then re-reads
    /// `model` and updates `list`'s rows **in place** via
    /// [`FieldList::set_rows`] whenever [`projection_key_of`] changed
    /// since the last call.
    fn sync(&mut self, ctx: &RenderCtx) {
        self.now = ctx.now();
        let model = self.model.borrow();
        for key in FaultKey::ALL {
            if model.fault_log.entry(key).is_some() && !self.order.contains(&key) {
                self.order.push(key);
            }
        }
        let key = projection_key_of(&model, self.now, &self.order);
        if key != self.projection_key {
            let rows = build_rows(&model, self.now, &self.order);
            drop(model);
            self.list.set_rows(rows);
            self.projection_key = key;
        }
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        self.list.on_intent(intent)
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
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        self.list.redraw_after(ctx)
    }

    /// Folds `list`'s own key and nothing else -- `order`/`projection_key`
    /// are a projection, never a pixel this widget draws (same rule
    /// `PickerView::paint_key`'s doc comment states).
    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(WHY_PAGE_PROJECTION_SEED).fold_key(self.list.paint_key(ctx))
    }
}

/// Builds the `why?` page screen -- the one place it is constructed,
/// called once per push from `render::home`'s `ShortcutX` handler, which
/// computes the fresh most-recently-active-first `order` right before
/// calling this. Once pushed, this function is never re-invoked on every
/// fault event the way it used to be (bead `pico-link-bgnd` M4) --
/// [`WhyPageView::sync`] re-reads the live model itself every frame
/// instead.
pub(crate) fn build_why_page_screen(model: &ModelHandle, order: Vec<FaultKey>) -> Screen {
    let view = WhyPageView::new(model, Instant::from_micros(0), order);
    Screen::new(WHY_PAGE_TITLE, vec![Box::new(view)]).with_id(ScreenId::WhyPage)
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use core::cell::RefCell;

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
        let model = Rc::new(RefCell::new(model));
        // Seeded order simulates the page already open, frozen on entry.
        let mut view = WhyPageView::new(&model, Instant::from_micros(0), vec![FaultKey::BufOverflow]);

        // A second key fires while the page is open -- MORE recently than
        // BufOverflow, which would sort first under a fresh most-recently-
        // active-first re-sort.
        model.borrow_mut().fault_log.record(FaultKey::BufStarved, Instant::from_micros(1_000_000), None, 1);
        view.sync(&RenderCtx::at(Instant::from_micros(1_000_000)));

        assert_eq!(
            view.order,
            vec![FaultKey::BufOverflow, FaultKey::BufStarved],
            "the newly-fired key must append at the end, never jump ahead of the frozen order"
        );
    }

    #[test]
    fn why_page_does_not_reorder_already_present_keys_on_a_refresh() {
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), None, 1);
        model.fault_log.record(FaultKey::BufStarved, Instant::from_micros(1_000_000), None, 1);
        let model = Rc::new(RefCell::new(model));
        // Order was frozen with BufStarved (the more recent) listed FIRST
        // -- deliberately the opposite of first-seen, to prove a refresh
        // doesn't silently re-derive it.
        let mut view = WhyPageView::new(&model, Instant::from_micros(1_000_000), vec![FaultKey::BufStarved, FaultKey::BufOverflow]);

        // A repeat raise of an already-present key must not move it.
        model.borrow_mut().fault_log.record(FaultKey::BufOverflow, Instant::from_micros(2_000_000), None, 5);
        view.sync(&RenderCtx::at(Instant::from_micros(2_000_000)));

        assert_eq!(view.order, vec![FaultKey::BufStarved, FaultKey::BufOverflow], "a repeat raise of an already-ordered key must not reorder it");
    }

    #[test]
    fn why_page_screen_is_identified() {
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), None, 1);
        let model = Rc::new(RefCell::new(model));
        let screen = build_why_page_screen(&model, Vec::new());
        assert_eq!(screen.id(), Some(ScreenId::WhyPage));
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
        let model = Rc::new(RefCell::new(model));
        let screen = build_why_page_screen(&model, Vec::new());
        assert_eq!(screen.id(), Some(ScreenId::WhyPage));
    }

    /// Bead `pico-link-bgnd` M4 regression net: proves the page reads the
    /// LIVE model rather than a snapshot frozen at push time -- a fault's
    /// count bumping (without a pop/re-push of the screen) must actually
    /// repaint.
    #[test]
    fn a_live_fault_count_bump_is_reflected_with_no_pop_or_repush() {
        let mut model = BtModel::default();
        model.fault_log.record(FaultKey::BufOverflow, Instant::from_micros(0), Some(FaultValue::Count(1)), 1);
        let model = Rc::new(RefCell::new(model));
        let mut app = crate::app::App::new(240, 240);
        app.push_screen_for_test(build_why_page_screen(&model, Vec::new()));
        app.render(); // establish a clean baseline
        let depth_before = app.navigator_depth();

        model.borrow_mut().fault_log.record(FaultKey::BufOverflow, Instant::from_micros(1_000_000), Some(FaultValue::Count(140)), 140);
        let output = app.render();
        assert!(output.pixels().any(|p| p.1 != palette::BACKGROUND), "the live model change must actually repaint something");
        assert_eq!(app.navigator_depth(), depth_before, "the row must update in place, with no push/pop of the screen itself");
    }
}
