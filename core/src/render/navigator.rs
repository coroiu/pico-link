//! `Navigator`: owns the screen stack. Salvaged from
//! `simple_gui::document::Document`, reimplemented against `NavIntent`
//! (Tier 2 semantic input, see `crate::input`) instead of raw button
//! `KeyCode`s.
//!
//! Per-screen focus memory falls out of the data structure for free: a
//! popped screen isn't rebuilt, it's kept on the stack (in `Screen`,
//! `focused_index` lives on the struct itself), so pushing a new screen
//! and later popping back restores exactly the focus state that was there
//! before the push — no explicit save/restore step needed.

use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::OriginDimensions;

use super::chrome::compute_chrome;
use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::screen::Screen;
use super::theme::palette;
use super::widget::Action;
use crate::input::NavIntent;
use crate::platform::OutputRequest;

pub struct Navigator {
    stack: Vec<Screen>,
    /// Requests emitted by widgets via `Action::Emit`, accumulated here
    /// (see `apply_action`) rather than acted on directly — `Navigator`
    /// only owns the screen stack, not the keyboard-output link. Drained
    /// by [`Navigator::take_output`]; `App::handle_input` is the caller
    /// that does so, folding this into its own pending-output buffer.
    pending_output: Vec<OutputRequest>,
}

impl Navigator {
    /// Creates a navigator with `root` as the only (and un-poppable)
    /// screen on the stack.
    #[must_use]
    pub fn new(mut root: Screen) -> Self {
        root.initialize_focus();
        Self { stack: vec![root], pending_output: Vec::new() }
    }

    /// The currently visible screen.
    ///
    /// # Panics
    ///
    /// Never, in practice: the stack always has at least the root screen
    /// (`pop` refuses to remove it). The `expect` exists only because
    /// `Vec::last` returns `Option`.
    #[must_use]
    pub fn current(&self) -> &Screen {
        self.stack.last().expect("navigator stack must never be empty")
    }

    fn current_mut(&mut self) -> &mut Screen {
        self.stack.last_mut().expect("navigator stack must never be empty")
    }

    /// How many screens are on the stack (>= 1; the root screen is never
    /// popped).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Pushes a new screen, establishing its initial focus.
    pub fn push(&mut self, mut screen: Screen) {
        screen.initialize_focus();
        self.stack.push(screen);
    }

    /// Pops the current screen, unless it's the root. Returns whether a
    /// pop happened.
    pub fn pop(&mut self) -> bool {
        if self.stack.len() > 1 {
            self.stack.pop();
            true
        } else {
            false
        }
    }

    /// The root screen's (`stack[0]`'s) own focused widget's selection
    /// index, if any — see `Screen::selected_index`. Read by a caller
    /// about to call [`Navigator::replace_root`], so the freshly built
    /// replacement screen can be constructed with the same selection
    /// carried forward.
    #[must_use]
    pub fn root_selected_index(&self) -> Option<usize> {
        self.stack[0].selected_index()
    }

    /// The root screen's own focused widget's selection **key**, if any —
    /// see `Screen::selected_key`/`Widget::selected_key`. Read alongside
    /// [`Navigator::root_selected_index`] by a caller about to call
    /// [`Navigator::replace_root`]: the key is the primary carry-forward
    /// signal (survives the underlying list reordering/growing/shrinking),
    /// the index is only the fallback for when no key resolves — see
    /// `pico_link_core::render::list::VerticalList::with_selected_identity`'s
    /// doc comment for the exact rule.
    #[must_use]
    pub fn root_selected_key(&self) -> Option<super::list::ListItemKey> {
        self.stack[0].selected_key()
    }

    /// Generalizes [`Navigator::root_selected_index`] to any stack depth --
    /// e.g. `index == 1` for the Devices screen once Home (`index == 0`)
    /// is the root (`pico-link-znb.8`/E7). `None` both for an
    /// out-of-range `index` and for a screen with no focused widget /
    /// selection concept, same as [`Screen::selected_index`].
    #[must_use]
    pub fn selected_index_at(&self, index: usize) -> Option<usize> {
        self.stack.get(index).and_then(Screen::selected_index)
    }

    /// Generalizes [`Navigator::root_selected_key`] to any stack depth --
    /// see [`Navigator::selected_index_at`]'s doc comment for why this is
    /// needed once Devices is no longer the root screen.
    #[must_use]
    pub fn selected_key_at(&self, index: usize) -> Option<super::list::ListItemKey> {
        self.stack.get(index).and_then(Screen::selected_key)
    }

    /// The screen title at `index`, if any -- used by a caller (e.g.
    /// [`crate::app::App::rebuild_root`]) to check whether a specific
    /// live-data-backed screen (e.g. Devices) is currently sitting at a
    /// known stack position before refreshing it via
    /// [`Navigator::replace_at`].
    #[must_use]
    pub fn title_at(&self, index: usize) -> Option<&str> {
        self.stack.get(index).map(|screen| screen.title.as_str())
    }

    /// Replaces **only** the root screen (`stack[0]`) with `screen`,
    /// leaving every screen pushed above it untouched — depth, contents,
    /// and their own focus/selection state all survive unchanged.
    ///
    /// This is deliberately *not* [`Navigator::new`] followed by re-pushing
    /// the rest of the stack: it exists specifically so a live-data-backed
    /// root screen (the devices list, driven by Bluetooth events — see
    /// `pico_link_core::app::App::rebuild_root`) can be refreshed on every
    /// model change without evicting the user from whatever screen they've
    /// navigated to. Rebuilding the whole `Navigator` here was a real
    /// defect: any Bluetooth event while browsing a pushed screen would
    /// silently pop the user back to root and reset their selection.
    pub fn replace_root(&mut self, mut screen: Screen) {
        screen.initialize_focus();
        self.stack[0] = screen;
    }

    /// Generalizes [`Navigator::replace_root`] to any stack depth --
    /// refreshes the screen at `index` in place, leaving every other
    /// stack entry (above or below it) untouched, same non-negotiable
    /// property `replace_root` has for `index == 0`. A no-op if `index`
    /// is out of range (the caller -- [`crate::app::App::rebuild_root`] --
    /// is expected to have checked [`Navigator::title_at`] first, but this
    /// stays defensive rather than panicking on a stale index).
    ///
    /// Exists because Devices is no longer always the root
    /// (`pico-link-znb.8`/E7 makes Home the root and pushes Devices onto
    /// it): a live Bluetooth event must still be able to refresh the
    /// Devices screen while it's sitting one level down, exactly the way
    /// `replace_root` already refreshes whatever *is* the root, without
    /// disturbing anything pushed above it (e.g. the wizard, at index 2).
    pub fn replace_at(&mut self, index: usize, mut screen: Screen) {
        if index >= self.stack.len() {
            return;
        }
        screen.initialize_focus();
        self.stack[index] = screen;
    }

    fn apply_action(&mut self, action: Action) {
        match action {
            Action::PushView(builder) => self.push(builder()),
            Action::PopView | Action::Back => {
                self.pop();
            }
            // Not a stack op: forwarded into `pending_output` rather than
            // consumed as navigation. See `Navigator::take_output`.
            Action::Emit(request) => self.pending_output.push(request),
            Action::None => {}
        }
    }

    /// Drains and returns any `OutputRequest`s emitted by widgets (via
    /// `Action::Emit`) since the last call, in emission order. Uses
    /// `mem::take` so a call with nothing new emitted returns an empty
    /// `Vec` rather than replaying old requests — the same one-shot-drain
    /// shape as `App::take_output`, which is this method's only caller
    /// (from `App::handle_input`, right after dispatching each batch of
    /// intents).
    pub fn take_output(&mut self) -> Vec<OutputRequest> {
        core::mem::take(&mut self.pending_output)
    }

    /// Dispatches a semantic navigation intent to the current screen.
    ///
    /// # Known simplification
    ///
    /// For `Up`/`Down`/`JumpBy`, the intent is forwarded to the focused
    /// widget first (so e.g. a list can move its own internal selection),
    /// and then *also* drives the screen's own top-level focus cycling
    /// (salvaged from `Document::focus_next`/`focus_previous`). On a
    /// screen with exactly one focusable widget — every screen this bead
    /// builds — the top-level cycle is a no-op (there's nothing else to
    /// focus). On a hypothetical future multi-widget screen, this would
    /// mean every `Up`/`Down` both moves the focused widget's internal
    /// selection *and* tries to move top-level focus, with no way for a
    /// widget to say "I consumed that, don't also refocus." Fixing that
    /// needs a "consumed" signal `Widget::on_intent` doesn't have today.
    /// Deferred — flagged here rather than silently shipped as correct.
    ///
    /// `Left`/`Right`/`ShortcutX`/`ShortcutY` have no default screen-level
    /// behavior yet — no widget in this crate today uses the horizontal
    /// axis or the two unbound buttons — so they're forwarded to the
    /// focused widget only, with no top-level focus-cycling side effect.
    pub fn dispatch(&mut self, intent: NavIntent) {
        match intent {
            NavIntent::Down => {
                let action = self.current_mut().forward_to_focused(intent);
                self.apply_action(action);
                self.current_mut().focus_next();
            }
            NavIntent::Up => {
                let action = self.current_mut().forward_to_focused(intent);
                self.apply_action(action);
                self.current_mut().focus_previous();
            }
            NavIntent::JumpBy(n) => {
                let action = self.current_mut().forward_to_focused(intent);
                self.apply_action(action);
                if n >= 0 {
                    self.current_mut().focus_next();
                } else {
                    self.current_mut().focus_previous();
                }
            }
            NavIntent::Select => {
                let action = self.current_mut().activate_focused();
                self.apply_action(action);
            }
            NavIntent::Back => {
                // Forward to the focused widget first, purely so it gets a
                // chance to react as a **side effect** -- e.g. the pairing
                // wizard's scanning phase queuing `Command::CancelScan`
                // (design section 9: "B cancels the scan", pico-link-znb.7)
                // before the screen it's shown on disappears. The
                // *navigation* decision is unconditional and stays
                // `Navigator`'s alone: B always pops (or no-ops at the
                // root), regardless of what the widget returns, so its
                // `Action` is deliberately discarded rather than run
                // through `apply_action` -- a widget cannot use `Back` to
                // push, and cannot prevent the pop. This is safe to add
                // for every existing widget: every `on_intent` impl in
                // this crate already matches `NavIntent::Back` as an
                // explicit no-op returning `Action::None` (see
                // `list::VerticalList`/`menu::MenuList`), so this forward
                // changes nothing for them.
                let _ = self.current_mut().forward_to_focused(intent);
                self.pop();
            }
            NavIntent::Left | NavIntent::Right | NavIntent::ShortcutX | NavIntent::ShortcutY => {
                let action = self.current_mut().forward_to_focused(intent);
                self.apply_action(action);
            }
        }
    }

    /// Renders the current screen into `target`, computing chrome regions
    /// from whatever size `target` happens to be.
    ///
    /// Clears `target` to [`palette::BACKGROUND`] first. This matters because, per the
    /// presentation-surface ADR, the app core owns a single long-lived
    /// framebuffer that gets re-rendered into every frame rather than
    /// reallocated — without an explicit clear, a widget that doesn't
    /// unconditionally repaint every pixel of its area (e.g.
    /// `VerticalList` only fills a *selected* row's background, leaving
    /// unselected rows' backgrounds untouched) would leave stale pixels
    /// from a previous frame's selection highlight visible after the
    /// selection moves away. Widgets and tests that always render into a
    /// fresh `FrameBuffer565` (already black) are unaffected by this.
    ///
    /// # Errors
    ///
    /// Never, in practice: `FrameBuffer565`'s `DrawTarget::Error` is
    /// `Infallible`. The `Result` return exists so this can use `?`
    /// against embedded-graphics `Drawable::draw` calls internally.
    pub fn render(&self, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        target.clear(palette::BACKGROUND)?;
        let chrome = compute_chrome(target.size());
        self.current().render(&chrome, self.depth() > 1, ctx, target)
    }

    /// Delegates to the currently visible screen's
    /// [`Screen::redraw_after`] -- only the screen on top of the stack is
    /// ever on-screen, so only its widgets' time-driven answers matter.
    #[must_use]
    pub fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        self.current().redraw_after(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Instant;
    use crate::render::list::{ListItem, VerticalList};
    use embedded_graphics::prelude::{OriginDimensions, Point, Size};

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

    fn list_screen(title: &str, n: usize) -> Screen {
        let items = (0..n).map(|i| ListItem::new(format!("{title}-item-{i}"))).collect();
        Screen::new(title, vec![Box::new(VerticalList::new(items))])
    }

    #[test]
    fn root_screen_cannot_be_popped() {
        let mut nav = Navigator::new(list_screen("root", 3));
        assert_eq!(nav.depth(), 1);
        assert!(!nav.pop());
        assert_eq!(nav.depth(), 1);
    }

    #[test]
    fn push_then_pop_returns_to_the_previous_screen() {
        let mut nav = Navigator::new(list_screen("root", 3));
        nav.push(list_screen("detail", 1));
        assert_eq!(nav.depth(), 2);
        assert_eq!(nav.current().title, "detail");

        assert!(nav.pop());
        assert_eq!(nav.depth(), 1);
        assert_eq!(nav.current().title, "root");
    }

    #[test]
    fn back_intent_pops_a_pushed_screen_but_not_the_root() {
        let mut nav = Navigator::new(list_screen("root", 3));
        nav.push(list_screen("detail", 1));
        nav.dispatch(NavIntent::Back);
        assert_eq!(nav.depth(), 1);
        nav.dispatch(NavIntent::Back);
        assert_eq!(nav.depth(), 1);
    }

    #[test]
    fn next_intent_moves_the_focused_lists_selection() {
        let mut nav = Navigator::new(list_screen("root", 5));
        nav.dispatch(NavIntent::Down);
        nav.dispatch(NavIntent::Down);
        assert_eq!(nav.current().focused_index(), Some(0));
        // The selection lives inside the widget, not exposed on Screen
        // directly; render + pixel-sample in the PNG-dump test is the
        // black-box proof. Here we at least prove dispatch doesn't panic
        // and focus stays on the (only) focusable widget.
    }

    #[test]
    fn activate_on_a_list_with_no_callback_does_not_change_the_stack() {
        let mut nav = Navigator::new(list_screen("root", 3));
        nav.dispatch(NavIntent::Select);
        assert_eq!(nav.depth(), 1);
    }

    #[test]
    fn pushing_a_screen_via_action_from_a_widget_callback_works() {
        let items = vec![ListItem::new("open detail")];
        let list = VerticalList::new(items).on_activate(|_item| {
            Action::PushView(Box::new(|| {
                Screen::new("detail", vec![Box::new(VerticalList::new(vec![ListItem::new("x")]))])
            }))
        });
        let root = Screen::new("root", vec![Box::new(list)]);
        let mut nav = Navigator::new(root);

        nav.dispatch(NavIntent::Select);
        assert_eq!(nav.depth(), 2);
        assert_eq!(nav.current().title, "detail");
    }

    #[test]
    fn per_screen_focus_memory_is_preserved_across_push_and_pop() {
        let mut nav = Navigator::new(list_screen("root", 5));
        nav.dispatch(NavIntent::Down); // move selection within the list

        nav.push(list_screen("detail", 2));
        assert_eq!(nav.current().focused_index(), Some(0));

        nav.pop();
        // Same screen instance, never rebuilt: its focused_index (which
        // top-level widget has focus) is exactly what it was before the
        // push. This is the "per-screen focus memory" requirement.
        assert_eq!(nav.current().focused_index(), Some(0));
        assert_eq!(nav.current().title, "root");
    }

    #[test]
    fn rendering_into_the_same_framebuffer_twice_does_not_leave_the_previous_frames_selection_highlight_behind() {
        // Regression test for the "reused framebuffer across frames"
        // ghosting bug: `render()` used to skip clearing, so a row's
        // selection-highlight fill from frame N was still visible in
        // frame N+1 after the selection moved away (`VerticalList` only
        // paints a background fill for the *currently* selected row, not
        // every row). This is exactly the scenario `App`/the unified run
        // loop hit in practice, since they render into one long-lived
        // `FrameBuffer565` rather than allocating a fresh one per frame.
        let mut fb = FrameBuffer565::new(240, 240);
        let mut nav = Navigator::new(list_screen("List", 3));

        nav.render(&test_ctx(), &mut fb).unwrap();
        // x=20: past the 4px selection accent bar, so this samples the
        // row's plain elevated fill rather than the accent stripe.
        let row0_highlighted = fb.pixel(Point::new(20, 18));
        assert_eq!(row0_highlighted, palette::SURFACE_ELEVATED, "row 0 starts selected");

        nav.dispatch(NavIntent::Down);
        nav.render(&test_ctx(), &mut fb).unwrap();
        let row0_after_move = fb.pixel(Point::new(20, 18));
        assert_ne!(
            row0_after_move, palette::SURFACE_ELEVATED,
            "row 0's stale highlight from the first render must not survive into the second"
        );
    }

    #[test]
    fn render_works_end_to_end_on_a_fresh_navigator() {
        let mut fb = FrameBuffer565::new(240, 240);
        let nav = Navigator::new(list_screen("List", 3));
        nav.render(&test_ctx(), &mut fb).unwrap();
        assert_eq!(fb.size(), Size::new(240, 240));
        // Sanity: the title bar's surface fill was drawn somewhere, i.e.
        // rendering actually did something (not just the background clear).
        let any_title_bar_surface = fb.pixels().any(|p| p.1 == palette::SURFACE);
        assert!(any_title_bar_surface);
    }

    // --- B liveness (pico-link-znb.5 / E2): a navigator fact, not a screen fact ---

    fn b_slot_rect(chrome: &crate::render::chrome::ChromeLayout) -> embedded_graphics::primitives::Rectangle {
        use crate::panel::Button;
        let order = chrome.orientation.slot_order();
        let index = order.iter().position(|&b| b == Button::B).expect("B is always in slot_order");
        let slot_height = chrome.rail.size.height / 4;
        embedded_graphics::primitives::Rectangle::new(
            Point::new(chrome.rail.top_left.x, chrome.rail.top_left.y + (index as u32 * slot_height) as i32),
            Size::new(chrome.rail.size.width, slot_height),
        )
    }

    fn any_pixel_of_color_in_rect(
        fb: &FrameBuffer565,
        rect: embedded_graphics::primitives::Rectangle,
        color: embedded_graphics::pixelcolor::Rgb565,
    ) -> bool {
        (rect.top_left.y..rect.top_left.y + rect.size.height as i32)
            .any(|y| (rect.top_left.x..rect.top_left.x + rect.size.width as i32).any(|x| fb.pixel(Point::new(x, y)) == color))
    }

    #[test]
    fn b_slot_is_dim_at_the_root_and_live_once_a_screen_is_pushed() {
        let mut nav = Navigator::new(list_screen("root", 3));
        let mut fb = FrameBuffer565::new(240, 240);
        nav.render(&test_ctx(), &mut fb).unwrap();
        let chrome = compute_chrome(fb.size());
        let b_rect = b_slot_rect(&chrome);
        assert!(
            any_pixel_of_color_in_rect(&fb, b_rect, palette::DIVIDER),
            "at depth 1 (root, un-poppable) B must render dim"
        );
        assert!(
            !any_pixel_of_color_in_rect(&fb, b_rect, palette::TEXT_SECONDARY),
            "at depth 1 B must not render live -- there is nothing behind the root to pop back to"
        );

        nav.push(list_screen("detail", 1));
        let mut fb2 = FrameBuffer565::new(240, 240);
        nav.render(&test_ctx(), &mut fb2).unwrap();
        let chrome2 = compute_chrome(fb2.size());
        let b_rect2 = b_slot_rect(&chrome2);
        assert!(
            any_pixel_of_color_in_rect(&fb2, b_rect2, palette::TEXT_SECONDARY),
            "after a push, depth > 1, B must render live"
        );
    }
}
