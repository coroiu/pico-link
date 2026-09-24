//! The retained-mode `Widget` trait and the two return-value vocabularies
//! it uses: `Action` (what the navigation stack should do next) and
//! `FocusEvent` (what happened to a widget's focus state).
//!
//! Salvaged concepts, reimplemented cleanly on `embedded-graphics` (see
//! `.planning/decisions/2026-08-11-ui-framework-reuse-vs-rewrite.md`):
//! - `simple_gui::components::Component` -> [`Widget`]
//! - `simple_gui::components::ComponentAction` -> [`Action`]
//! - `simple_gui::components::FocusEvent` -> [`FocusEvent`] (unchanged
//!   shape; already formalized upstream in
//!   `.planning/decisions/2026-01-21-focus-management-system.md`)
//!
//! `Widget::render` takes a concrete `&mut FrameBuffer565` rather than a
//! generic `&mut impl DrawTarget`. This is deliberate, not a simplification
//! we'd like to undo later: `embedded_graphics::draw_target::DrawTarget`
//! has generic methods (`fill_contiguous`, `draw_iter`, ...), so it is not
//! object-safe, and `Box<dyn Widget>` (needed for a heterogeneous screen
//! stack) requires every trait method to be dyn-compatible. A widget that
//! wants real clipping still gets it — it calls
//! `target.clipped(&some_sub_area)` *inside* its own `render`, using the
//! `area` it was handed, per `DrawTargetExt::clipped()`.

use alloc::boxed::Box;
use alloc::string::String;
use core::convert::Infallible;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::input::NavIntent;
use crate::panel::Button;
use crate::platform::OutputRequest;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::list::ListItemKey;
use super::paint_key::PaintKey;
use super::rail::ButtonLabel;
use super::screen::Screen;

/// High-level focus state transitions, decoupled from whatever transport
/// triggered them (joystick, button, keyboard, headless HTTP injection —
/// see `crate::input::NavIntent`). The `Navigator` fires these on a widget
/// when its focus state changes or when it is activated while focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEvent {
    /// The widget gained focus (e.g. the user navigated to it).
    Gained,
    /// The widget lost focus (e.g. the user navigated away).
    Lost,
    /// The widget was activated while focused (joystick press, button A,
    /// Enter, headless `Select` intent).
    Activated,
}

/// What a widget wants the navigation stack to do in response to a
/// `FocusEvent` or `NavIntent`. Returned rather than mutating the stack
/// directly, so widgets never need a reference back to the `Navigator`
/// that owns them.
#[derive(Default)]
pub enum Action {
    /// Push a new screen. Boxed as `FnOnce` (not `Fn`): a push is a single
    /// one-shot construction, not a repeatable template.
    PushView(Box<dyn FnOnce() -> Screen>),
    /// Pop the current screen (e.g. "selection complete, return to the
    /// list that opened me").
    PopView,
    /// Semantic back-navigation, as distinct from `PopView`: a widget may
    /// want to signal "back" for reasons other than "I'm done" (e.g.
    /// cancelling an in-progress action). The `Navigator` currently treats
    /// both identically (pop the stack); kept as a separate variant per
    /// the frozen `Action` shape so the two intents don't have to be
    /// conflated if a future widget needs to distinguish them.
    Back,
    /// Emits an [`OutputRequest`] (e.g. "type this text") for the run
    /// loop to forward to `Platform::keyboard()` (see
    /// `.planning/decisions/2026-08-18-m2-ble-hid-output-seam.md`).
    /// **Not** a navigation-stack operation — `Navigator::apply_action`
    /// forwards it into its own pending-output buffer instead of pushing/
    /// popping anything; see `Navigator::take_output`.
    Emit(OutputRequest),
    /// No navigation-stack action.
    #[default]
    None,
}

/// A status-dot color a [`ChromeContribution`] can ask the chrome to paint
/// in the title bar — semantic (what the status *means*), not a raw
/// `Rgb565`, so the mapping to an actual palette color lives in one place
/// ([`super::screen::Screen::render`]) instead of every widget picking its
/// own shade of green/red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeStatus {
    /// Everything's fine (e.g. a store-backed widget's last background
    /// operation succeeded) — rendered in
    /// [`super::theme::palette::STATUS_SUCCESS`].
    Success,
    /// Something's wrong (e.g. the last sync failed) — rendered in
    /// [`super::theme::palette::STATUS_ERROR`].
    Error,
    /// Neither good nor bad news yet (e.g. no sync has run, or it
    /// succeeded with no data) — rendered in a muted neutral color,
    /// not silently omitted, so "we have no status opinion" still reads as
    /// a deliberate state rather than a missing dot.
    Neutral,
}

/// The A2DP/Bluetooth title-bar glyph's resolved presentation state (bead
/// `pico-link-88xs`, design `.planning/design/2026-09-08-link-state-vs-
/// discovery-axis.md` section 5). The glyph now has TWO independent
/// domain inputs -- [`crate::app::LinkState`] and
/// [`crate::app::BtModel::discovering`] -- so the contributing widget
/// (`home.rs`) resolves them down to this one presentation value, rather
/// than [`ChromeContribution::link`] carrying `LinkState` directly the way
/// it used to. This deliberately amends `link`'s own earlier doc-comment
/// ruling that reusing `LinkState` was enough, on that ruling's own stated
/// principle: independent axes of domain state must be able to read
/// differently on screen at once, and once there are two axes feeding one
/// glyph, the chrome should receive the resolved answer, not both inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkGlyph {
    /// Rendered in [`super::theme::palette::TEXT_SECONDARY`].
    Idle,
    /// Rendered in [`super::theme::palette::STATUS_WARNING`] -- either the
    /// link is mid-connect, or a GAP inquiry is currently running
    /// (regardless of link state).
    Busy,
    /// Rendered in [`super::theme::palette::BRAND_BRIGHT`] -- the A2DP
    /// link is up, whether or not a scan happens to be running
    /// concurrently (design section 1.1: a scan must never demote this).
    Live,
}

/// A title-bar volume reading, in the display's own 0..100 percent domain
/// (see [`crate::app::VolumeState::percent`] -- never the raw 0..127
/// AVRCP level, per design section 3). `muted` is carried separately from
/// `percent` rather than folded into it (e.g. as a sentinel) because it
/// changes the rendered *colour*, not just the text -- see
/// `super::screen::title_paint_key`'s doc comment for why a paint key must
/// fold it independently too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeChrome {
    pub percent: u8,
    pub muted: bool,
}

/// What a focused widget wants the chrome (title bar + button rail) to
/// show on its behalf, for the current frame. Returned fresh from
/// [`Widget::chrome_contribution`] on every render rather than pushed/
/// cached, so a store-backed widget that reads its live state every frame
/// always reports whatever the current frame's true state actually is.
///
/// Each field is independently optional: a widget can override just the
/// button labels and leave the title/readout/status to their screen-level
/// defaults ([`super::screen::Screen::render`] falls back to the screen's
/// static `title`/button labels for a `None`, and simply omits the
/// readout/status dot when those are `None`). This is the seam a
/// detail-view (or any other content) widget uses to supply its own live
/// title and button labels without `Screen`/`chrome.rs` needing to know
/// anything list-specific or detail-specific.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChromeContribution {
    /// Overrides the screen's static title, if set (e.g. a detail view
    /// showing its own item's name instead of the screen's static
    /// title).
    pub title: Option<String>,
    /// A right-aligned position readout (e.g. `"2 / 5"`), if this widget
    /// has a meaningful position/count to report.
    pub readout: Option<String>,
    /// The title-bar volume readout, if this widget has a live volume
    /// reading to show (design
    /// `.planning/design/2026-09-07-volume-on-display.md` section 2/3) --
    /// deliberately a separate field from `readout` rather than sharing
    /// it, per that design's section 9.3: a list screen's position readout
    /// and Home's volume percent are independent axes that must be able to
    /// coexist on any screen that wants both. `None` omits the element
    /// entirely and consumes zero width (section 6, the `None` rule) --
    /// never a `--` placeholder. Text and colour are the chrome's to
    /// derive from `percent`/`muted` (see `Screen::render`), not this
    /// widget's, matching the `status`/`link` fields' own division below.
    pub volume: Option<VolumeChrome>,
    /// Overrides the screen's static B-button rail label, if set. **B's
    /// text is always the constant "back"** — only `Navigator` (via
    /// `Screen::render`'s `can_go_back` parameter) decides B's
    /// *liveness*; nothing here or on `Screen` should author B's text.
    pub b: Option<ButtonLabel>,
    /// Overrides the screen's static X-button rail label, if set.
    pub x: Option<ButtonLabel>,
    /// Overrides the screen's static Y-button rail label, if set.
    pub y: Option<ButtonLabel>,
    /// A status-dot color to paint in the title bar, if this widget has an
    /// app-wide status worth surfacing there.
    pub status: Option<ChromeStatus>,
    /// The A2DP/Bluetooth link's connection state, if this widget has one
    /// worth surfacing — rendered as a Bluetooth glyph immediately left
    /// of the `status` dot (see `super::screen::Screen::render`). A
    /// separate field rather than folding into `status`, per design
    /// review: link connectivity and general app status are independent
    /// axes of "state" that must be able to read differently on screen at
    /// the same time (e.g. synced *and* disconnected). `None` omits the
    /// glyph entirely — see `Screen::render`'s handling. Bead
    /// `pico-link-88xs`: [`LinkGlyph`] (the RESOLVED presentation value),
    /// not [`crate::app::LinkState`] directly — see that type's doc
    /// comment for why this field changed shape.
    pub link: Option<LinkGlyph>,
    /// Whether the focused widget's codec link is currently in the
    /// design's fallback state (`.planning/design/2026-08-28-on-device-ui.md`
    /// section 6.2, link 3 of the five-link fallback chain: the X-rail
    /// label switches from "link" to "why?" under fallback). This widget
    /// carries no button-label text itself — the rail
    /// (`pico-link-znb.5`/E2, which gives `ChromeContribution` its own
    /// a/b/x/y label fields) reads this bit to decide which label to
    /// show. Defaults to `false`.
    pub fallback: bool,
}

impl ChromeContribution {
    /// Looks up this contribution's override for `button`, if any, by
    /// logical identity — the accessor the rail iterates through in
    /// physical [`crate::panel::PanelOrientation::slot_order`] to resolve
    /// each slot.
    ///
    /// **Never called for `Button::A`** — A has no field on this struct at
    /// all (design rule 4, `.planning/design/2026-09-02-a-button-label-
    /// rule.md`): its liveness and label come from [`Widget::activation`]
    /// alone, read directly by `Screen::activate_focused`/
    /// `Screen::resolve_a`, never through a `ChromeContribution`. Always
    /// returns `None` for `Button::A`; kept as a match arm rather than a
    /// panic so a caller that (incorrectly) asks doesn't crash, just gets
    /// "no opinion".
    ///
    /// For B/X/Y: returns `None` when this contribution has no opinion
    /// (defer to the screen's static label) — that is a distinct third
    /// state from `Some(&ButtonLabel::Inert)` (actively dead on this
    /// widget's watch). See the field doc comments above: flattening this
    /// to `Option<String>` would lose that distinction.
    #[must_use]
    pub fn button(&self, button: Button) -> Option<&ButtonLabel> {
        match button {
            Button::A => None,
            Button::B => self.b.as_ref(),
            Button::X => self.x.as_ref(),
            Button::Y => self.y.as_ref(),
        }
    }
}

/// The fixed vocabulary for the A button's rail word — see
/// `.planning/design/2026-09-02-a-button-label-rule.md` §4. Every word is
/// <= 6 chars, the measured budget for a 34px rail slot in
/// `font::hint()`. Returned by [`Widget::activation`], which is also what
/// gates whether A does anything at all — see that method's doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Pushes a deeper screen about the focused row. Nothing changes.
    Open,
    /// Commits the focused row's choice on this screen.
    Select,
    /// Begins pairing with the focused device.
    Pair,
    /// (Re)starts discovery. Used where the screen has no focused row.
    Scan,
    /// The one sanctioned escape hatch, with exactly one use today: Home's
    /// status face shows `devs` (design section 4's rail table, inside
    /// the already-declared Home exception — this adds zero new Home
    /// exceptions). `grep -rn 'Verb::Exception' core/` audits every
    /// exception in the app in one command. **Adding a second is a UX
    /// decision, not an implementation one** — bring it to Uma. Must be
    /// <= 6 chars.
    Exception(&'static str),
}

impl Verb {
    /// The literal rail text this verb renders as — always <= 6
    /// characters (the measured budget; see this type's doc comment).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Verb::Open => "open",
            Verb::Select => "select",
            Verb::Pair => "pair",
            Verb::Scan => "scan",
            Verb::Exception(word) => word,
        }
    }
}

const _: () = assert!(Verb::Open.as_str().len() <= 6);
const _: () = assert!(Verb::Select.as_str().len() <= 6);
const _: () = assert!(Verb::Pair.as_str().len() <= 6);
const _: () = assert!(Verb::Scan.as_str().len() <= 6);

/// A retained-mode UI element. Implementors own their own state (selection
/// index, scroll offset, ...) and are told their assigned screen-space
/// `area` at render/measure time — nothing in a `Widget` impl should assume
/// or hardcode a specific display resolution.
pub trait Widget {
    /// Reports how much space this widget wants, given the space on offer.
    /// Screens use this to stack widgets vertically in the content region
    /// (see `Screen::render`); a widget is free to request less than
    /// `constraints` (e.g. a single-line label) or all of it (e.g. a list
    /// that should fill the remaining content area).
    fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size;

    /// Draws into `target`, constrained to `area`. Implementations that
    /// need to guard against overdraw (text overflow, an oversized row)
    /// should call `target.clipped(&area)` (or a sub-rectangle of it) and
    /// draw into that, per `DrawTargetExt::clipped()` — this is the *real*
    /// clipping mechanism the old character-skip marquee code is retired
    /// in favor of.
    ///
    /// # Errors
    ///
    /// Returns `Infallible`'s uninhabited variant in practice: the core's
    /// `DrawTarget` (`FrameBuffer565`) can never fail to draw. The `Result`
    /// return exists only to match `Drawable`/`DrawTarget`'s signature so
    /// widget impls can use `?` freely when calling into embedded-graphics
    /// primitives.
    fn render(
        &self,
        area: Rectangle,
        ctx: &RenderCtx,
        target: &mut FrameBuffer565,
    ) -> Result<(), Infallible>;

    /// A cheap, total summary of everything that affects this widget's
    /// pixels this frame -- see [`PaintKey`]'s doc comment for the full
    /// contract, the SKIP-not-CLIP framing, and the time trap that must be
    /// respected by any override.
    ///
    /// Defaults to [`PaintKey::ALWAYS`], which compares unequal to
    /// everything (including another `ALWAYS`): this is the entire
    /// migration strategy for the damage-rect render pass (bead
    /// `pico-link-7h5`) -- every widget is repainted every frame, exactly
    /// as today, until it opts in to a real key one widget at a time (bead
    /// `pico-link-7h5.5`). **There is no caller of this method yet**; this
    /// bead (`pico-link-7h5.3`) only adds the vocabulary.
    ///
    /// **Mechanical review rule** (see [`PaintKey`]'s doc comment): every
    /// widget that overrides [`Self::redraw_after`] must fold time into
    /// its `paint_key`, and no widget that does not override
    /// `redraw_after` should fold time into its `paint_key`.
    fn paint_key(&self, _ctx: &RenderCtx) -> PaintKey {
        PaintKey::ALWAYS
    }

    /// For a widget whose `paint_key` has changed, which sub-rectangle of
    /// its own `_area` actually needs repainting -- `None` (the default)
    /// means "all of it". Composite widgets that internally cover only
    /// part of their area on a given change (e.g. only the OUT meter's
    /// two columns, not its whole hero region) can narrow this to shrink
    /// the frame damage rect their change contributes (design section 4).
    ///
    /// `Screen` only trusts this method's `Some(r)` once it has ALSO
    /// established, via [`Self::damage_region_key`] compared against its
    /// own cache, that nothing outside `r` could have changed this frame
    /// -- see that method's doc comment for why that comparison cannot
    /// live here, in the widget itself. When `Screen` has not established
    /// that (the common case for a widget that hasn't opted in via
    /// `damage_region_key`), it damages this widget's whole `area`
    /// regardless of what this method returns -- so implementations that
    /// only narrow via `damage_hint` without also overriding
    /// `damage_region_key` are dead code, not a partial win.
    ///
    /// A widget that returns `Some(r)` here still must itself consult
    /// `ctx.needs(..)` inside its own `render` if it wants to actually
    /// *skip* rasterising the untouched part; returning `Some(r)` only
    /// narrows what the *caller* considers damaged, it does not, by
    /// itself, skip anything -- see [`RenderCtx::needs`] and the
    /// module-level SKIP-not-CLIP note on [`PaintKey`].
    fn damage_hint(&self, _area: Rectangle, _ctx: &RenderCtx) -> Option<Rectangle> {
        None
    }

    /// A cheap, total summary of everything OUTSIDE the sub-rectangle
    /// [`Self::damage_hint`] would report, for widgets that narrow their
    /// own damage to less than their full area (bead `pico-link-7h5.9`).
    ///
    /// **This must be compared across frames by the CALLER (`Screen`,
    /// via its own [`PaintKey`]-caching mechanism -- the same one that
    /// already tracks [`Self::paint_key`]), never by the widget itself.**
    /// A widget instance does not survive frames in this codebase: a
    /// composite screen widget backed by live model data (the motivating
    /// case, `HeroStatusView`) is routinely a brand-new instance every
    /// time its owning screen is rebuilt from fresh data, which happens
    /// far more often than once per widget lifetime. A widget-local
    /// `Cell` attempting to remember "my own key last frame" is comparing
    /// a freshly-constructed instance's key against itself the very first
    /// time it is asked -- structurally always "unchanged", regardless of
    /// what actually differs from the *previous* instance's last-painted
    /// state. `Screen`'s [`PaintSlot`] cache is the only thing that
    /// genuinely spans frames here, exactly the reasoning that already
    /// gave `paint_key` its own external cache rather than a widget-local
    /// one -- see `pico-link-7h5.9`'s postmortem for the concrete bug this
    /// prevents (a stale device name/codec word after `damage_hint`
    /// wrongly narrowed a real body change down to a sub-region).
    ///
    /// Defaults to [`PaintKey::ALWAYS`] (never equal to a previous frame's
    /// value, including another `ALWAYS`) -- the same "opt in one widget
    /// at a time" migration guarantee `paint_key`'s own default gives:
    /// until a widget overrides this, `Screen` always treats its
    /// `damage_hint` as untrustworthy and damages its whole `area`
    /// instead, exactly today's behaviour.
    fn damage_region_key(&self, _ctx: &RenderCtx) -> PaintKey {
        PaintKey::ALWAYS
    }

    /// Whether this widget can receive focus. Defaults to `false` (e.g.
    /// static labels, dividers).
    fn is_focusable(&self) -> bool {
        false
    }

    /// Called by the `Navigator` when this widget's focus state changes,
    /// or when it is activated while focused. Only meaningful if
    /// `is_focusable()` is `true`.
    fn on_focus(&mut self, _event: FocusEvent) -> Action {
        Action::None
    }

    /// Called by the `Navigator` with `Up`/`Down`/`JumpBy` (and, for
    /// widgets that care about the horizontal axis, `Left`/`Right`) while
    /// this widget is focused, giving it a chance to react internally
    /// (e.g. a list moving its selected row) before/alongside the
    /// `Navigator`'s own top-level focus cycling — see
    /// `Navigator::dispatch` for the exact interleaving and its known
    /// limitation for multi-widget screens.
    fn on_intent(&mut self, _intent: NavIntent) -> Action {
        Action::None
    }

    /// This widget's contribution to the chrome (title bar + button rail) for
    /// the current frame, if any. Only ever consulted for the *focused*
    /// widget on a screen (see `Screen::chrome_contribution`) — an
    /// unfocused widget has no business overriding chrome that isn't
    /// "its" right now. Defaults to `None` (no override): static labels,
    /// dividers, and any widget with nothing dynamic to report don't need
    /// to implement this.
    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        None
    }

    /// The word this widget's A button should show, and — identically —
    /// whether A does anything at all. `None` (the default) means BOTH "A
    /// is dim" and "activating me is a guaranteed no-op": the two cannot
    /// disagree, because `Screen` reads this same method for both purposes
    /// (see `Screen::activate_focused` and `Screen::resolve_a`). See
    /// `.planning/design/2026-09-02-a-button-label-rule.md` (rule 4) for
    /// the design of record this closes.
    ///
    /// **A widget that wraps another widget must forward this** — exactly
    /// like `redraw_after`/`scroll_top`/`selected_key` (pico-link-vxc,
    /// D2). Unlike those, forgetting is loud: A visibly stops working
    /// instead of silently drifting stale.
    ///
    /// The verb must not depend on time — only on this widget's own state
    /// and its selected row. Chrome is NOT covered by `Screen::
    /// redraw_after`'s fold (see that method's doc comment, D3), so a
    /// time-varying verb would go stale under the `pl_ui_dirty()` blit
    /// gate.
    fn activation(&self) -> Option<Verb> {
        None
    }

    /// Whether this widget wants B to be treated as live even when the
    /// navigator has nowhere to pop to (stack depth 1) — e.g. Home's menu
    /// face, which uses B to fold back to the status face rather than a
    /// navigator pop. Defaults to `false`: B's ordinary liveness (`stack
    /// depth > 1`) already covers every widget that doesn't have an
    /// internal "back" of its own. See `Screen::resolve_button`'s B arm.
    ///
    /// Unlike `activation`, forgetting this the other way (returning
    /// `true` when there's nothing to fold back to) is caught by design
    /// review, not the compiler — see pico-link-4a2, where Home's status
    /// face rendered a live B that did nothing.
    fn handles_back(&self) -> bool {
        false
    }

    /// How much longer, from `ctx.now()`, this widget's next render call
    /// could produce different pixels purely from the passage of time
    /// (e.g. an elapsed-time readout, a spinner) — **not** from a
    /// model/focus/input change, which already goes through the normal
    /// `mark_dirty` path. `None` (the default) means "nothing about my
    /// appearance depends on time"; static labels, lists, and every widget
    /// with no time-varying content don't need to implement this.
    ///
    /// This is what lets [`App::tick`] (`crate::app::App::tick`) schedule a
    /// redraw for a purely time-driven appearance change without either of
    /// the two hacks this seam exists to retire: marking the app dirty on
    /// every tick (which would defeat `Screen::render`'s flush-skip and
    /// full-frame-blit a static screen every frame — see the frame-scoped
    /// clock ADR's "hacks to retire" section) or leaving such a widget
    /// permanently frozen between input events.
    ///
    /// See [`Screen::redraw_after`] for how a screen combines its widgets'
    /// answers, and `.planning/decisions/2026-08-31-render-ctx-frame-
    /// scoped-clock.md` for the full design.
    ///
    /// A returned `Some(Duration::ZERO)` (or any other sub-frame duration)
    /// is not an error, but it is not honoured literally either: the
    /// caller (`crate::app::App::render`) floors it at a small minimum
    /// before adding it to `ctx.now()`, so it cannot come due on the very
    /// next tick with no time elapsed — which would otherwise re-dirty the
    /// app forever and reinstate the always-dirty behaviour the ADR above
    /// forbids. See `pico_link_core::app::MIN_REDRAW_DELAY`'s doc comment
    /// (pico-link-6wz). No production widget relies on this today; treat
    /// it as a guard, not a feature to depend on for a near-instant redraw.
    ///
    /// **A widget that wraps other widgets must forward this** -- see
    /// `HomeView`, `DevicesListView` and `PairingWizardView` for the `min`-
    /// over-children pattern (pico-link-vxc, D2 fix); the default `None`
    /// here would otherwise silently swallow a child's time-driven request
    /// under the FFI dirty gate (`pl_ui_dirty`), freezing the screen.
    ///
    /// **Not covered by this fold: the chrome/button rail** (pico-link-vxc,
    /// D3, noted not fixed). `Screen::redraw_after` only folds over
    /// `self.widgets`; chrome is rendered separately from the focused
    /// widget's `chrome_contribution` and has no clock of its own today. If
    /// the rail ever grows time-driven content (a blinking or timing-out
    /// label), it will need its own hook into the gate -- do not assume
    /// `Screen::redraw_after` already covers it.
    fn redraw_after(&self, _ctx: &RenderCtx) -> Option<core::time::Duration> {
        None
    }

    /// This widget's own internal selection/cursor index, if it has one
    /// (e.g. `VerticalList`'s selected row). `None` for widgets with no
    /// such concept (static labels, dividers).
    ///
    /// Exists so a caller that rebuilds a screen's widgets from scratch on
    /// every model change (e.g. `App::rebuild_root` over live device/link
    /// data — see `pico_link_core::app`'s doc comments) can read back the
    /// *old* widget's selection before discarding it, and carry it forward
    /// into the freshly built replacement (`VerticalList::with_selected`)
    /// instead of resetting the user's place in the list on every event.
    fn selected_index(&self) -> Option<usize> {
        None
    }

    /// This widget's own currently selected row's identity key, if it has
    /// one — see [`super::list::ListItem::key`] / [`ListItemKey`].
    /// `None` for widgets with no keyed-identity concept (the default),
    /// or when the currently selected row was never tagged with a key.
    ///
    /// Exists for the same reason [`Self::selected_index`] does, one
    /// level more robust: a caller that rebuilds a screen's widgets from
    /// scratch on every model change can read this back before discarding
    /// the old widget, then carry it into the freshly built replacement's
    /// selection-resolution call (`VerticalList::with_selected_identity`)
    /// so a rebuild that reorders, inserts, or removes *other* rows
    /// doesn't move the selection away from the row the user was actually
    /// looking at — the failure mode a plain index-based carry-forward
    /// has.
    fn selected_key(&self) -> Option<ListItemKey> {
        None
    }

    /// Projects live state (a model handle, a shared clock, ...) into this
    /// widget's own plain fields, once per frame, before `render`/
    /// `on_intent`/`measure`/the paint-key methods are consulted -- see
    /// `Navigator::sync_top`'s doc comment and the M0 step of
    /// `.planning/design/2026-09-24-live-widgets-retire-refresh-stack.md`
    /// (bead `pico-link-bgnd`) for the full design this seam is the start
    /// of. Defaults to a no-op: **as of this bead, nothing calls this on
    /// any widget with a live handle to project from, and no widget
    /// overrides it** -- `App` still rebuilds screens from scratch on every
    /// model change (`App::refresh_stack`), so `sync` is currently called
    /// on freshly-built, already-up-to-date widgets. It becomes load-
    /// bearing screen by screen as later beads in the same epic migrate
    /// each view to be long-lived instead of rebuilt.
    ///
    /// **A widget that wraps other widgets must forward this to its
    /// children** -- same rule, same hazard, as `redraw_after`/
    /// `scroll_top`/`activation` above.
    fn sync(&mut self, _ctx: &RenderCtx) {}

    /// This widget's scroll-top row index, if it scrolls. `None` for
    /// widgets with no scrolling concept (the default): static labels,
    /// `MenuList` (deliberately never scrolls — see `menu::MenuList`'s
    /// doc comment), and any widget short enough to fit its viewport.
    ///
    /// Exists for the same reason [`Self::selected_index`]/
    /// [`Self::selected_key`] do: a caller that rebuilds a screen from
    /// live model state on every event must carry the user's *viewport*
    /// forward, not just their cursor — otherwise a scrolled list snaps
    /// back to `top_index == 0` on the next unrelated event and
    /// `list::reconcile_top_index` re-lands the selected row at the
    /// viewport's bottom edge. Latent on today's Devices screen (4 rows
    /// fit, nothing scrolls in practice); not latent on a longer list
    /// rebuilt on every live event — see
    /// `.planning/design/2026-09-02-field-list-widget-ruling.md` §4.7.
    ///
    /// **A widget that wraps another scrolling widget must forward
    /// this** — same failure mode as [`Self::redraw_after`]'s "a wrapper
    /// that doesn't forward silently looks like `None` instead of
    /// forgotten" hazard (pico-link-vxc, D2).
    fn scroll_top(&self) -> Option<usize> {
        None
    }
}
