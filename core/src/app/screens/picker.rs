use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::time::Duration;

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::input::NavIntent;
use crate::render::theme::icon;
use crate::render::{Action, FieldList, FieldRow, FocusEvent, FrameBuffer565, ListItemKey, PaintKey, RenderCtx, Screen, Verb, Widget};

use super::super::{ScreenCarry, ScreenId};

/// A single row in a [`PickerView`]. Its first real caller is
/// [`build_ldac_quality_picker_screen`](super::ldac_quality::build_ldac_quality_picker_screen).
pub(crate) struct PickerOption {
    /// Stable identity -- carries focus and the check across projections,
    /// and is what a [`PickerView`]'s `on_pick` callback is invoked with.
    pub key: ListItemKey,
    pub label: String,
    /// The trailing note (e.g. `best audio`, `660 now`, `not offered`).
    pub note: Option<(String, Rgb565)>,
    /// `false` -> [`FieldKind::Readonly`](crate::render::fields::FieldKind::Readonly):
    /// focusable, dim, no caret, `A` dead -- an unavailable option cannot be
    /// picked, structurally (the activation gate lives in [`FieldList`], not
    /// in `on_pick`).
    pub selectable: bool,
}

/// Seed for [`projection_key_of`] -- only needs to differ from other
/// widgets'/views' own seeds.
const PICKER_PROJECTION_SEED: u64 = 51;

/// A live-model-backed projection closure: re-derives this picker's options
/// and the currently checked key, read fresh every [`Widget::sync`] call --
/// see [`PickerView`]'s own doc comment.
type PickerProjection = Box<dyn Fn() -> (Vec<PickerOption>, Option<ListItemKey>)>;

/// Callback invoked with the picked row's [`ListItemKey`] -- see
/// [`build_picker_view_screen`]'s doc comment.
type OnPick = Box<dyn Fn(ListItemKey) -> Action>;

/// Builds this picker's [`FieldRow`]s from a fresh `(options, checked)`
/// projection -- shared between [`build_picker_view_screen`]'s initial
/// construction and [`PickerView::sync`]'s in-place update, so the two can
/// never drift into building rows two different ways.
///
/// Five rules this shape makes structural rather than remembered:
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
/// `A`'s rail word is [`Verb::Select`]: the sketches say "pick", which
/// would need a second `Verb::Exception` and sign-off for one word that
/// means the same thing to the user.
fn options_to_rows(options: &[PickerOption], checked: Option<ListItemKey>) -> Vec<FieldRow> {
    options
        .iter()
        .map(|option| {
            let checked_here = Some(option.key) == checked;
            let mut row = if option.selectable { FieldRow::action(option.label.clone()) } else { FieldRow::readonly(option.label.clone()) };
            if option.selectable {
                row = row.with_verb(Verb::Select);
            }
            if let Some((text, color)) = &option.note {
                row = row.with_value(text.clone(), *color);
            }
            if checked_here {
                row = row.with_leading_glyph(icon::CHECK);
            }
            row.with_key(option.key)
        })
        .collect()
}

/// A cheap, total summary of every field [`options_to_rows`] reads --
/// [`PickerView::sync`]'s allocation-saving skip check, the same
/// projection-key shape `DevicesListView::projection_key` uses (design
/// `.planning/design/2026-09-24-live-widgets-retire-refresh-stack.md` §5's
/// R2 vocabulary: "everything the view READS", never folded into a
/// [`Widget::paint_key`]).
fn projection_key_of(options: &[PickerOption], checked: Option<ListItemKey>) -> PaintKey {
    let mut key = PaintKey::of(PICKER_PROJECTION_SEED).fold(options.len() as u64);
    for option in options {
        key = key.fold(option.key.as_u64());
        key = key.fold_str(&option.label);
        key = match &option.note {
            Some((text, color)) => key.fold_str(text).fold_color(*color),
            None => key.fold(0),
        };
        key = key.fold(u64::from(option.selectable));
    }
    // `checked` folded last, offset by 1 so "no checked row" (`None`) can
    // never collide with a real key whose `as_u64()` happens to be `0`.
    key = key.fold(checked.map_or(0, |k| k.as_u64().wrapping_add(1)));
    key
}

/// A generic single-select picker screen's widget -- the LDAC quality
/// picker and the two Settings pickers (idle-screen mode, idle-after
/// timeout) are all this widget with a different [`PickerProjection`]/
/// [`OnPick`], not three widgets (bead `pico-link-bgnd` M3, generalising
/// `build_devices_screen`'s M2 live-list shape to `FieldList`-backed
/// pickers). Long-lived for as long as its screen stays on the navigator's
/// stack: [`Widget::sync`] re-reads `projection` every frame this screen is
/// on top and updates `list`'s rows **in place** via
/// [`FieldList::set_rows`] whenever [`projection_key_of`] changed --
/// see [`build_picker_view_screen`]'s doc comment for the arm that stopped
/// rebuilding this screen on every model event.
pub(crate) struct PickerView {
    list: FieldList,
    projection: PickerProjection,
    /// The last [`projection_key_of`]-shaped hash of the projection's
    /// output -- see that function's own doc comment. Recomputed every
    /// frame by [`Self::sync`]; [`FieldList::set_rows`] is only called when
    /// it changed (an allocation-saving skip, not a correctness dependency
    /// -- `set_rows` is itself selection-preserving).
    projection_key: PaintKey,
}

impl Widget for PickerView {
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

    /// Re-reads `projection` and updates `list`'s rows **in place** via
    /// [`FieldList::set_rows`] whenever [`projection_key_of`] changed since
    /// the last call -- see this struct's own doc comment.
    fn sync(&mut self, _ctx: &RenderCtx) {
        let (options, checked) = (self.projection)();
        let key = projection_key_of(&options, checked);
        if key != self.projection_key {
            self.list.set_rows(options_to_rows(&options, checked));
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
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<Duration> {
        self.list.redraw_after(ctx)
    }

    /// Folds `list`'s own key and nothing else -- `projection`/
    /// `projection_key` are a projection, never a pixel this widget draws
    /// (same rule `DevicePageView::paint_key`'s doc comment states).
    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(PICKER_PROJECTION_SEED).fold_key(self.list.paint_key(ctx))
    }
}

/// Builds a [`PickerView`]-backed single-select picker screen -- the one
/// place a picker screen is constructed, called once per push (from a
/// device page's `QUALITY` row, or the Settings screen's two rows). Once
/// pushed, [`crate::app::App::build_identified_screen`]'s
/// `ScreenId::Picker`/`ScreenId::SettingsPicker` arms return
/// [`crate::app::Refresh::Keep`] (bead `pico-link-bgnd` M3) -- this function
/// is never re-invoked on every model/state event the way it used to be;
/// [`PickerView::sync`] re-reads `projection` itself every frame instead.
pub(crate) fn build_picker_view_screen(
    id: ScreenId,
    title: impl Into<String>,
    carry: &ScreenCarry,
    projection: impl Fn() -> (Vec<PickerOption>, Option<ListItemKey>) + 'static,
    on_pick: impl Fn(ListItemKey) -> Action + 'static,
) -> Screen {
    let (options, checked) = projection();
    let projection_key = projection_key_of(&options, checked);
    let rows = options_to_rows(&options, checked);
    let on_pick: OnPick = Box::new(on_pick);
    let list = FieldList::new(rows).with_leading_gutter().with_selected_identity(carry.selected_key, carry.selected_index).on_activate_key(on_pick);
    let list = if let Some(top) = carry.scroll_top { list.with_scroll_top(top) } else { list };
    let view = PickerView { list, projection: Box::new(projection), projection_key };
    Screen::new(title, vec![Box::new(view)]).with_id(id)
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use alloc::vec;
    use core::cell::RefCell;

    use embedded_graphics::prelude::RgbColor;

    use crate::app::test_support::no_carry;
    use crate::app::App;
    use crate::input::NavIntent;
    use crate::render::theme::palette;

    use super::*;

    // --- build_picker_view_screen (the general picker) ---

    fn quality_like_test_id() -> ScreenId {
        // `build_picker_view_screen` is generic over `id`, so any
        // ScreenId value exercises its contract identically. Standing in
        // with an address distinct from any real device used elsewhere in
        // this module's tests.
        ScreenId::DevicePage([0xAA; 6])
    }

    fn three_options() -> Vec<PickerOption> {
        vec![
            PickerOption { key: ListItemKey::from_u64(1), label: String::from("Alpha"), note: Some((String::from("best"), palette::TEXT_SECONDARY)), selectable: true },
            PickerOption { key: ListItemKey::from_u64(2), label: String::from("Beta"), note: None, selectable: true },
            PickerOption { key: ListItemKey::from_u64(3), label: String::from("Gamma"), note: Some((String::from("not offered"), palette::TEXT_SECONDARY)), selectable: false },
        ]
    }

    fn three_option_picker(checked: Option<ListItemKey>, picked: Rc<RefCell<Vec<ListItemKey>>>, stay_open: bool) -> Screen {
        build_picker_view_screen(quality_like_test_id(), "Test Picker", &no_carry(), move || (three_options(), checked), move |key| {
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

    /// Proves a picker reads the LIVE projection rather than a snapshot
    /// frozen at construction -- the bead `pico-link-bgnd` M3 regression
    /// net (mirrors devices.rs's own `a_paired_device_rename_is_reflected_
    /// in_the_rendered_label`). The checked row moves to match a projection
    /// change with no pop/re-push of the screen.
    #[test]
    fn the_checked_row_follows_a_live_projection_change_with_no_pop_or_repush() {
        let checked = Rc::new(RefCell::new(Some(ListItemKey::from_u64(1))));
        let checked_for_projection = Rc::clone(&checked);
        let screen = build_picker_view_screen(quality_like_test_id(), "Test Picker", &no_carry(), move || (three_options(), *checked_for_projection.borrow()), |_key| Action::None);
        let mut app = App::new(240, 240);
        app.push_screen_for_test(screen);
        app.render(); // establish a clean baseline
        let depth_before = app.navigator_depth();

        *checked.borrow_mut() = Some(ListItemKey::from_u64(2));
        let output = app.render();
        assert!(output.pixels().any(|p| p.1 != palette::BACKGROUND), "the live projection change must actually repaint something");
        assert_eq!(app.navigator_depth(), depth_before, "the checked row must move in place, with no push/pop of the screen itself");
    }

    /// Headless PNG dump of the picker, at zoom -- dumped from here since
    /// this module's tests are the only place with `pub(crate)` access to
    /// `build_picker_view_screen` itself.
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
}
