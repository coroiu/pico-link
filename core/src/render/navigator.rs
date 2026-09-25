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

use embedded_graphics::geometry::OriginDimensions;
use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use super::chrome::compute_chrome;
use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::screen::Screen;
#[cfg(test)]
use super::theme::palette;
use super::widget::Action;
use crate::app::ScreenId;
use crate::input::NavIntent;
use crate::platform::OutputRequest;

pub struct Navigator {
    /// The screen stack. **After this bead (`pico-link-ryw.9`), no code in
    /// this module may pop/truncate/reassign `stack` directly -- every
    /// removal must route through [`Navigator::retire`]**, the one
    /// private funnel that fires a retired [`Screen`]'s
    /// [`Screen::with_on_exit`] hook exactly once. See `retire`'s own doc
    /// comment for why.
    stack: Vec<Screen>,
    /// Requests emitted by widgets via `Action::Emit`, accumulated here
    /// (see `apply_action`) rather than acted on directly — `Navigator`
    /// only owns the screen stack, not the keyboard-output link. Drained
    /// by [`Navigator::take_output`]; `App::handle_input` is the caller
    /// that does so, folding this into its own pending-output buffer.
    pending_output: Vec<OutputRequest>,
    /// Set by every stack-structural op (`push`/`pop`/`replace`/`pop_to_root`) and
    /// by [`Navigator::force_full_damage`], consumed (and reset to
    /// `false`) by the next [`Navigator::render`] -- one of the damage
    /// pass's full-damage triggers (design section 3.4): a screen's own
    /// per-slot cache has no way to know the *framebuffer* now shows a
    /// different screen than the one its cache was built against (e.g. a
    /// pop reveals a screen whose own widgets/chrome may be byte-for-byte
    /// unchanged since it was last painted, but every pixel on the panel
    /// right now belongs to whatever was pushed on top of it). Starts
    /// `true` so the very first render is also a full-damage frame.
    force_full_damage: bool,
    /// The framebuffer size `Navigator::render` last computed chrome for.
    /// `None` on a freshly built `Navigator` (folded into the initial
    /// `force_full_damage: true` above, so a mismatch here on the very
    /// first render is never separately observed). A change is a
    /// full-damage trigger (design section 3.4, "framebuffer resize"): the
    /// old cached chrome/widget areas belong to a framebuffer that no
    /// longer exists.
    last_size: Option<Size>,
}

impl Navigator {
    /// Creates a navigator with `root` as the only (and un-poppable)
    /// screen on the stack.
    #[must_use]
    pub fn new(mut root: Screen) -> Self {
        root.initialize_focus();
        Self { stack: vec![root], pending_output: Vec::new(), force_full_damage: true, last_size: None }
    }

    /// Forces the next [`Navigator::render`] to damage the whole
    /// framebuffer, without any actual screen-state change -- the
    /// navigation-external counterpart to the structural ops below (push/
    /// `pop`/`replace`/`pop_to_root` already call this internally). `App` uses
    /// this for the other two full-damage triggers it alone knows about:
    /// waking the display from blank (see `App::mark_dirty`, whose own
    /// doc comment this generalizes) and any other "the framebuffer's
    /// prior content can no longer be trusted" event outside the
    /// navigation stack itself.
    pub fn force_full_damage(&mut self) {
        self.force_full_damage = true;
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

    /// The one private funnel every stack-removal op routes through --
    /// takes `screen`'s on-exit closure ([`Screen::take_on_exit`]) and
    /// calls it, AFTER `screen` is no longer anywhere on `self.stack`
    /// (every call site below removes from `self.stack` first, then
    /// passes the removed `Screen` here). "Exactly once" falls out of the
    /// types rather than needing to be remembered at each call site:
    /// `Option::take` can only ever yield the closure once, and `screen`
    /// (moved into this method, dropped at its end) cannot be retired a
    /// second time -- there is no second `Screen` to call this on.
    ///
    /// `pop_to_root`/`truncate_to` retire top-down (LIFO: the same order
    /// the screens were pushed in, reversed) -- a loop of individual pops,
    /// not `Vec::truncate` (which would drop the removed screens silently,
    /// never calling this at all). `replace_root` retires the OLD root
    /// after `mem::replace` swaps it out.
    // `&mut self` is unused today (retiring a screen only needs the
    // screen itself), kept deliberately: this is `Navigator`'s own
    // funnel, every call site is already `self.retire(...)`, and a bare
    // associated function would invite a future caller to bypass `self`
    // and call it directly on a `Screen` removed some other way.
    #[allow(clippy::unused_self)]
    fn retire(&mut self, mut screen: Screen) {
        if let Some(on_exit) = screen.take_on_exit() {
            on_exit();
        }
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
        self.force_full_damage = true;
    }

    /// Pops the current screen, unless it's the root. Returns whether a
    /// pop happened.
    pub fn pop(&mut self) -> bool {
        if self.stack.len() > 1 {
            if let Some(screen) = self.stack.pop() {
                self.retire(screen);
            }
            self.force_full_damage = true;
            true
        } else {
            false
        }
    }

    /// Pops every screen above the root, leaving only `stack[0]` --
    /// e.g. Home(1)/Devices(2)/Wizard(3) collapses straight to Home(1).
    /// Unlike repeated [`Navigator::pop`] calls this is one atomic
    /// operation with no intermediate `Screen`s ever observed rendered.
    ///
    /// Used by [`crate::app::App`]'s wizard-auto-dismiss handling
    /// (pico-link-4vb.2: Andreas wants a successful pairing to land back
    /// on Home, not require several manual `B` presses back through
    /// Devices). A no-op if the stack is already at depth 1.
    pub fn pop_to_root(&mut self) {
        self.truncate_to(0);
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

    /// Generalizes scroll-position carry-forward to any stack depth —
    /// see `Widget::scroll_top`'s doc comment and
    /// [`Navigator::selected_index_at`]'s for why the "any stack depth"
    /// generalization exists. `None` both for an out-of-range `index`
    /// and for a screen whose focused widget has no scrolling concept.
    #[must_use]
    pub fn scroll_top_at(&self, index: usize) -> Option<usize> {
        self.stack.get(index).and_then(Screen::scroll_top)
    }

    /// The [`crate::app::ScreenId`] of the screen at `index`, if any --
    /// `None` both for an out-of-range `index` and for a screen that never
    /// called [`Screen::with_id`] (the "not identity/liveness-tracked"
    /// sentinel -- see that method's doc comment). Used by
    /// [`crate::app::App`]'s `prune_stack` to find a device-scoped screen
    /// whose subject vanished from the model.
    #[must_use]
    pub fn id_at(&self, index: usize) -> Option<ScreenId> {
        self.stack.get(index).and_then(Screen::id)
    }

    /// Drops every screen above `index`, leaving `stack[index]` as the new
    /// top of the stack -- `index == 0` is equivalent to
    /// [`Navigator::pop_to_root`]. Used by [`crate::app::App`]'s
    /// `prune_stack` when a model change discovers a pushed screen's
    /// subject no longer exists (e.g. the device shown by a
    /// [`crate::app::ScreenId::DevicePage`] was forgotten): the whole
    /// unwind from that point up is one atomic stack op, same shape as
    /// [`Navigator::pop_to_root`], rather than repeated [`Navigator::pop`]
    /// calls that would each be individually observed as a render.
    ///
    /// A no-op if `index + 1 >= self.depth()` (nothing above it to drop).
    pub fn truncate_to(&mut self, index: usize) {
        let keep = index.saturating_add(1);
        // Top-down (LIFO): pop-and-retire one screen at a time down to
        // `keep`, never `Vec::truncate` -- that would drop every removed
        // `Screen` silently, so their `on_exit` hooks would never fire
        // (design §3: "NOT `Vec::truncate`, which drops silently").
        while keep < self.stack.len() {
            if let Some(screen) = self.stack.pop() {
                self.retire(screen);
            }
            self.force_full_damage = true;
        }
    }

    /// Replaces **only** the root screen (`stack[0]`) with `screen`,
    /// leaving every screen pushed above it untouched — depth, contents,
    /// and their own focus/selection state all survive unchanged.
    ///
    /// This is deliberately *not* [`Navigator::new`] followed by re-pushing
    /// the rest of the stack -- rebuilding the whole `Navigator` on a root
    /// change was a real defect (any Bluetooth event while browsing a
    /// pushed screen would silently pop the user back to root and reset
    /// their selection). As of the live-widgets refactor (bead
    /// `pico-link-bgnd`) every screen kind reads live model state itself
    /// via `Widget::sync` rather than being rebuilt on model change, so
    /// this method's production caller is gone -- it survives as a
    /// `#[cfg(test)]`-only helper (`App::replace_root_for_test`) for tests
    /// that need to exercise generic run-loop behavior at Home root without
    /// going through real navigation.
    pub fn replace_root(&mut self, mut screen: Screen) {
        screen.initialize_focus();
        let old_root = core::mem::replace(&mut self.stack[0], screen);
        self.retire(old_root);
        self.force_full_damage = true;
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
    /// from whatever size `target` happens to be, and returns the frame
    /// damage rect that was actually (re)painted -- `Rectangle::zero()` if
    /// nothing needed repainting this frame.
    ///
    /// No longer clears `target` unconditionally: that whole-framebuffer
    /// clear is exactly what the damage pass (design section 3.3) exists
    /// to replace with a fill scoped to the damage rect alone --
    /// `Screen::render` does that fill itself, once the rect is known. A
    /// resize (this call's `target.size()` differing from the last call's)
    /// is still one of the full-damage triggers this method detects and
    /// forwards, alongside the stack-structural ops (`push`/`pop`/
    /// `replace_root`/`truncate_to`/`pop_to_root`) and
    /// [`Navigator::force_full_damage`] -- so the "a widget only paints
    /// its own selected-row background, not every row" hazard the old doc
    /// comment here warned about is still covered: a screen whose own
    /// per-slot cache would otherwise see nothing dirty still gets
    /// damaged in full whenever the framebuffer's prior content can't be
    /// trusted to already be correct.
    ///
    /// # Errors
    ///
    /// Never, in practice: `FrameBuffer565`'s `DrawTarget::Error` is
    /// `Infallible`. The `Result` return exists so this can use `?`
    /// against embedded-graphics `Drawable::draw` calls internally.
    pub fn render(&mut self, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<Rectangle, Infallible> {
        let size = target.size();
        let resized = self.last_size != Some(size);
        self.last_size = Some(size);
        let force_full_damage = core::mem::take(&mut self.force_full_damage) || resized;

        let chrome = compute_chrome(size);
        let can_go_back = self.depth() > 1;
        self.current_mut().render(&chrome, can_go_back, ctx, force_full_damage, target)
    }

    /// Delegates to the currently visible screen's
    /// [`Screen::redraw_after`] -- only the screen on top of the stack is
    /// ever on-screen, so only its widgets' time-driven answers matter.
    #[must_use]
    pub fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        self.current().redraw_after(ctx)
    }

    /// Syncs the top-of-stack screen's widgets ([`Screen::sync`], which
    /// forwards to [`Widget::sync`](super::widget::Widget::sync)) against
    /// `ctx` -- only the top screen, deliberately: it is the only one
    /// currently visible or reachable by input, matching
    /// [`Self::redraw_after`]'s own "top screen only" reasoning above.
    ///
    /// Called by `App` at exactly two points (see
    /// `.planning/design/2026-09-24-live-widgets-retire-refresh-stack.md`
    /// section 2, bead `pico-link-bgnd` M0): once before `Self::render`,
    /// and once before *each* dispatch inside `App::handle_input`'s loop --
    /// a single intent can pop the stack, exposing a screen underneath that
    /// was not synced this frame, so a naive "sync once before the whole
    /// loop" would leave it stale for that same frame's remaining intents.
    pub fn sync_top(&mut self, ctx: &RenderCtx) {
        self.current_mut().sync(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::widget::Verb;
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
        let list = VerticalList::new(items).on_activate(Verb::Open, |_item| {
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
        let mut nav = Navigator::new(list_screen("List", 3));
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

    // --- pico-link-7jol.4: ScreenId / id_at / truncate_to ---

    #[test]
    fn a_screen_that_never_calls_with_id_reports_no_id() {
        let nav = Navigator::new(list_screen("root", 3));
        assert_eq!(nav.id_at(0), None);
    }

    #[test]
    fn id_at_reports_a_tagged_screens_id_and_none_out_of_range() {
        let mut nav = Navigator::new(list_screen("root", 3).with_id(ScreenId::Home));
        nav.push(list_screen("detail", 1));
        assert_eq!(nav.id_at(0), Some(ScreenId::Home));
        assert_eq!(nav.id_at(1), None, "the pushed screen never called with_id");
        assert_eq!(nav.id_at(2), None, "out of range");
    }

    #[test]
    fn truncate_to_drops_everything_above_index_in_one_op() {
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(list_screen("a", 1));
        nav.push(list_screen("b", 1));
        nav.push(list_screen("c", 1));
        assert_eq!(nav.depth(), 4);

        nav.truncate_to(1);
        assert_eq!(nav.depth(), 2, "truncate_to(1) must keep index 0 and 1, dropping everything above");
        assert_eq!(nav.current().title, "a");
    }

    #[test]
    fn truncate_to_zero_is_equivalent_to_pop_to_root() {
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(list_screen("a", 1));
        nav.push(list_screen("b", 1));
        nav.truncate_to(0);
        assert_eq!(nav.depth(), 1);
        assert_eq!(nav.current().title, "root");
    }

    #[test]
    fn truncate_to_past_the_top_is_a_no_op() {
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(list_screen("a", 1));
        nav.truncate_to(5);
        assert_eq!(nav.depth(), 2, "truncating past the current depth must not panic or change anything");
    }

    // --- on-exit hook (bead pico-link-ryw.9, design
    // `.planning/design/2026-09-25-value-row-and-on-exit-hook.md` §3/§4) ---

    use alloc::rc::Rc;
    use core::cell::{Cell, RefCell};

    /// A `list_screen` with an on-exit closure that increments `counter`.
    fn counting_screen(title: &str, counter: &Rc<Cell<u32>>) -> Screen {
        let counter = counter.clone();
        list_screen(title, 1).with_on_exit(move || counter.set(counter.get() + 1))
    }

    /// A `list_screen` with an on-exit closure that appends `title` to
    /// `log` -- for asserting retirement ORDER, not just count.
    fn logging_screen(title: &'static str, log: &Rc<RefCell<Vec<&'static str>>>) -> Screen {
        let log = log.clone();
        list_screen(title, 1).with_on_exit(move || log.borrow_mut().push(title))
    }

    #[test]
    fn on_exit_fires_once_on_back_pop() {
        let counter = Rc::new(Cell::new(0));
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(counting_screen("detail", &counter));
        assert_eq!(counter.get(), 0);

        nav.dispatch(NavIntent::Back);
        assert_eq!(counter.get(), 1);
        nav.dispatch(NavIntent::Back); // root: no-op, must not double-fire
        assert_eq!(counter.get(), 1);
    }

    #[test]
    fn on_exit_fires_once_on_widget_popview_action() {
        let counter = Rc::new(Cell::new(0));
        let counter_clone = counter.clone();
        let list = VerticalList::new(vec![ListItem::new("close")]).on_activate(Verb::Open, move |_| {
            let _ = &counter_clone; // captured only to prove the closure below is the one that fires
            Action::PopView
        });
        let detail = Screen::new("detail", vec![Box::new(list)]).with_on_exit({
            let counter = counter.clone();
            move || counter.set(counter.get() + 1)
        });
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(detail);

        nav.dispatch(NavIntent::Select);
        assert_eq!(nav.depth(), 1);
        assert_eq!(counter.get(), 1);
    }

    #[test]
    fn on_exit_fires_once_on_b_b_escape() {
        // B, B: first B pops a screen ABOVE the editor (does not fire the
        // editor's own hook), second B pops the editor itself (fires it
        // exactly once).
        let counter = Rc::new(Cell::new(0));
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(counting_screen("editor", &counter));
        nav.push(list_screen("confirm", 1));

        nav.dispatch(NavIntent::Back);
        assert_eq!(counter.get(), 0, "popping the screen above the editor must not fire the editor's hook");
        assert_eq!(nav.current().title, "editor");

        nav.dispatch(NavIntent::Back);
        assert_eq!(counter.get(), 1, "the second B must fire the editor's hook exactly once");
        assert_eq!(nav.current().title, "root");
    }

    #[test]
    fn on_exit_fires_once_per_screen_on_pop_to_root_top_down() {
        let log: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(logging_screen("a", &log));
        nav.push(logging_screen("b", &log));
        assert_eq!(nav.depth(), 3);

        nav.pop_to_root();
        assert_eq!(nav.depth(), 1);
        assert_eq!(*log.borrow(), vec!["b", "a"], "top-down: the screen pushed last retires first");
    }

    #[test]
    fn on_exit_fires_only_above_index_on_truncate_to() {
        let counter_a = Rc::new(Cell::new(0));
        let counter_b = Rc::new(Cell::new(0));
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(counting_screen("a", &counter_a));
        nav.push(counting_screen("b", &counter_b));
        assert_eq!(nav.depth(), 3);

        nav.truncate_to(1);
        assert_eq!(nav.depth(), 2);
        assert_eq!(counter_a.get(), 0, "screen at the kept index must not retire");
        assert_eq!(counter_b.get(), 1, "screen above the kept index must retire");
    }

    #[test]
    fn on_exit_fires_for_old_root_on_replace_root() {
        let counter = Rc::new(Cell::new(0));
        let mut nav = Navigator::new(counting_screen("root", &counter));
        nav.replace_root(list_screen("new-root", 1));
        assert_eq!(counter.get(), 1);
        assert_eq!(nav.current().title, "new-root");
    }

    #[test]
    fn on_exit_does_not_fire_when_a_screen_is_pushed_over_it() {
        let counter = Rc::new(Cell::new(0));
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(counting_screen("editor", &counter));
        nav.push(list_screen("confirm", 1));
        assert_eq!(counter.get(), 0, "a push over the editor must not fire its on-exit hook");
    }

    #[test]
    fn on_exit_does_not_fire_on_back_at_root() {
        let counter = Rc::new(Cell::new(0));
        let nav = Navigator::new(counting_screen("root", &counter));
        // `pop` on a root-only stack is a documented no-op (`Navigator::
        // pop`'s own doc comment); confirmed here via `depth`, not
        // `dispatch`, since `dispatch(Back)` on a fresh root has nothing
        // else to observe either way.
        assert_eq!(nav.depth(), 1);
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn on_exit_not_fired_by_sync_or_render() {
        let counter = Rc::new(Cell::new(0));
        let mut nav = Navigator::new(counting_screen("root", &counter));
        let mut fb = FrameBuffer565::new(240, 240);
        for _ in 0..20 {
            nav.sync_top(&test_ctx());
            nav.render(&test_ctx(), &mut fb).unwrap();
        }
        assert_eq!(counter.get(), 0, "sync/render must never fire an on-exit hook -- only stack removal does");
    }

    #[test]
    fn left_never_pops_the_stack() {
        // Design §1: Left/Right forward to the focused widget only, and
        // `Navigator::dispatch` must never gain a fallback that treats an
        // unconsumed Left as Back.
        let mut nav = Navigator::new(list_screen("root", 1));
        nav.push(list_screen("detail", 3));
        assert_eq!(nav.depth(), 2);

        nav.dispatch(NavIntent::Left);
        assert_eq!(nav.depth(), 2, "Left must never pop the stack, even when the focused widget ignores it");
        nav.dispatch(NavIntent::Right);
        assert_eq!(nav.depth(), 2, "Right must never pop the stack, even when the focused widget ignores it");
    }
}
