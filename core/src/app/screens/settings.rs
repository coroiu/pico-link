use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::convert::Infallible;
use core::time::Duration;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::audio::{AbrFloor, CushionPolicy};
use crate::input::NavIntent;
use crate::power::{DisplaySettings, ScreensaverMode, ScreensaverTimeout};
use crate::render::theme::palette;
use crate::render::{Action, FieldList, FieldRow, FocusEvent, FrameBuffer565, ListItemKey, PaintKey, RenderCtx, Screen, Verb, Widget};

use super::super::{AbrFloorState, CushionPolicyState, DisplaySettingsState, ScreenId, SettingsPickerKind};
use super::picker::{build_picker_view_screen, PickerOption};

/// The Settings screen's fixed title.
pub(crate) const SETTINGS_TITLE: &str = "Settings";

/// [`ListItemKey`]s for the Settings screen's three fixed rows.
///
/// `ROW_CUSHION_KEY` is first in this list (matching Uma's 2026-09-25
/// design's row *position*, "audio is the product; the two display rows
/// stay adjacent") even though its `ListItemKey` value is the highest --
/// row order is [`settings_rows`]'s vec order, not this constant's value,
/// so there is no reason to renumber the two pre-existing keys.
const ROW_MODE_KEY: ListItemKey = ListItemKey::from_u64(0);
const ROW_TIMEOUT_KEY: ListItemKey = ListItemKey::from_u64(1);
const ROW_CUSHION_KEY: ListItemKey = ListItemKey::from_u64(2);
/// Bead `pico-link-d42g.4` (F4) -- a fresh key, not renumbered into the
/// gap left by the other three (see this const block's own doc comment on
/// [`ROW_CUSHION_KEY`]: row order is `settings_rows`'s vec order, not this
/// value).
const ROW_ABR_FLOOR_KEY: ListItemKey = ListItemKey::from_u64(3);

/// Bead `pico-link-d42g.4` (F4), Uma's 2026-09-25 UX design sec 3/6: the
/// ABR floor options offered in the picker, **highest first**, each paired
/// with its secondary-colour note. ONE const list so a failed by-ear rung
/// (F2) can be dropped with a one-line change here -- nothing else in this
/// module enumerates the offered set.
const ABR_FLOOR_OPTIONS: &[(AbrFloor, &str)] = &[(AbrFloor::Kbps330, "default"), (AbrFloor::Kbps246, "untested"), (AbrFloor::Kbps198, "untested")];

/// Seed for [`SettingsView::projection_key`] -- only needs to differ from
/// other widgets'/views' own seeds.
const SETTINGS_PROJECTION_SEED: u64 = 61;

/// Bead `pico-link-8pp1.2` (S4): BUFFER is the FIRST row (Uma's design,
/// "audio is the product") -- the two display rows (`IDLE SCREEN`/
/// `IDLE AFTER`) stay adjacent underneath it. Bead `pico-link-d42g.4` (F4)
/// adds `LDAC MIN` directly under `BUFFER` (Uma's 2026-09-25 UX design sec
/// 6: "Settings row 2"), always visible -- Settings has no device whose
/// `QUALITY` could gate it.
fn settings_rows(current: DisplaySettings, cushion: CushionPolicy, abr_floor: AbrFloor) -> Vec<FieldRow> {
    vec![
        FieldRow::action("BUFFER").with_value(cushion.label(), palette::TEXT_PRIMARY).with_key(ROW_CUSHION_KEY),
        FieldRow::action("LDAC MIN").with_value(abr_floor.label(), palette::TEXT_PRIMARY).with_key(ROW_ABR_FLOOR_KEY),
        FieldRow::action("IDLE SCREEN").with_value(current.mode.label(), palette::TEXT_PRIMARY).with_key(ROW_MODE_KEY),
        FieldRow::action("IDLE AFTER").with_value(current.timeout.label(), palette::TEXT_PRIMARY).with_key(ROW_TIMEOUT_KEY),
    ]
}

fn settings_projection_key(current: DisplaySettings, cushion: CushionPolicy, abr_floor: AbrFloor) -> PaintKey {
    PaintKey::of(SETTINGS_PROJECTION_SEED)
        .fold(u64::from(current.mode.to_wire()))
        .fold(u64::from(current.timeout.as_secs()))
        .fold(u64::from(cushion.to_wire()))
        .fold(u64::from(abr_floor.to_wire()))
}

/// The Settings screen's two rows -- `IDLE SCREEN` (mode) and `IDLE AFTER`
/// (timeout), each pushing its own picker. Built once per push (bead
/// `pico-link-bgnd` M3) and never rebuilt again while it stays on the
/// stack -- [`SettingsView::sync`] re-reads `state` itself every frame this
/// screen is on top, so a pick made in either picker (which shares this
/// same `Rc<RefCell<DisplaySettingsState>>`) is reflected here with no
/// rebuild.
pub(crate) fn build_settings_screen(
    state: &Rc<RefCell<DisplaySettingsState>>,
    cushion_state: &Rc<RefCell<CushionPolicyState>>,
    abr_floor_state: &Rc<RefCell<AbrFloorState>>,
) -> Screen {
    let (rows, projection_key) = {
        let current = state.borrow().current;
        let cushion = cushion_state.borrow().current;
        let abr_floor = abr_floor_state.borrow().current;
        (settings_rows(current, cushion, abr_floor), settings_projection_key(current, cushion, abr_floor))
    };
    let state_for_activate = Rc::clone(state);
    let cushion_state_for_activate = Rc::clone(cushion_state);
    let abr_floor_state_for_activate = Rc::clone(abr_floor_state);
    let list = FieldList::new(rows).on_activate_key(move |key| {
        if key == ROW_CUSHION_KEY {
            let cushion_state = Rc::clone(&cushion_state_for_activate);
            return Action::PushView(Box::new(move || build_cushion_picker_screen(&cushion_state)));
        }
        if key == ROW_ABR_FLOOR_KEY {
            let abr_floor_state = Rc::clone(&abr_floor_state_for_activate);
            return Action::PushView(Box::new(move || build_abr_floor_picker_screen(&abr_floor_state)));
        }
        if key == ROW_MODE_KEY {
            let state = Rc::clone(&state_for_activate);
            return Action::PushView(Box::new(move || build_settings_picker_screen(SettingsPickerKind::ScreensaverMode, &state)));
        }
        if key == ROW_TIMEOUT_KEY {
            let state = Rc::clone(&state_for_activate);
            return Action::PushView(Box::new(move || build_settings_picker_screen(SettingsPickerKind::ScreensaverTimeout, &state)));
        }
        Action::None
    });
    let view =
        SettingsView { list, state: Rc::clone(state), cushion_state: Rc::clone(cushion_state), abr_floor_state: Rc::clone(abr_floor_state), projection_key };
    Screen::new(SETTINGS_TITLE, vec![Box::new(view)]).with_id(ScreenId::Settings)
}

/// Wraps [`FieldList`] to keep the Settings screen's two rows synced to the
/// live [`DisplaySettingsState`] -- see [`build_settings_screen`]'s doc
/// comment. Every method that isn't `sync` is a plain forward, the same
/// shape `DevicesListView`/`DevicePageView` already use.
struct SettingsView {
    list: FieldList,
    state: Rc<RefCell<DisplaySettingsState>>,
    /// Bead `pico-link-8pp1.2` (S4) -- the BUFFER row's live source, read
    /// alongside `state` on every [`Self::sync`] the same way.
    cushion_state: Rc<RefCell<CushionPolicyState>>,
    /// Bead `pico-link-d42g.4` (F4) -- the LDAC MIN row's live source, read
    /// alongside `state`/`cushion_state` on every [`Self::sync`] the same
    /// way.
    abr_floor_state: Rc<RefCell<AbrFloorState>>,
    /// The last [`settings_projection_key`] value -- see
    /// `DevicesListView::projection_key`'s doc comment for the full
    /// "allocation-saving skip, not a correctness dependency" rule this
    /// follows.
    projection_key: PaintKey,
}

impl Widget for SettingsView {
    fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size {
        self.list.measure(constraints, ctx)
    }

    fn is_focusable(&self) -> bool {
        self.list.is_focusable()
    }

    fn activation(&self) -> Option<Verb> {
        self.list.activation()
    }

    /// Re-reads `state` and updates `list`'s rows **in place** via
    /// [`FieldList::set_rows`] whenever [`settings_projection_key`] changed
    /// since the last call -- see this struct's own doc comment.
    fn sync(&mut self, _ctx: &RenderCtx) {
        let current = self.state.borrow().current;
        let cushion = self.cushion_state.borrow().current;
        let abr_floor = self.abr_floor_state.borrow().current;
        let key = settings_projection_key(current, cushion, abr_floor);
        if key != self.projection_key {
            self.list.set_rows(settings_rows(current, cushion, abr_floor));
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

    fn selected_key(&self) -> Option<ListItemKey> {
        self.list.selected_key()
    }

    fn scroll_top(&self) -> Option<usize> {
        self.list.scroll_top()
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        self.list.render(area, ctx, target)
    }

    fn redraw_after(&self, ctx: &RenderCtx) -> Option<Duration> {
        self.list.redraw_after(ctx)
    }

    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(SETTINGS_PROJECTION_SEED).fold_key(self.list.paint_key(ctx))
    }
}

/// The Screensaver-mode picker's two options -- shared by
/// [`build_settings_picker_screen`]'s initial construction and its live
/// [`PickerView`](super::picker::PickerView) projection closure.
fn screensaver_mode_options(current: DisplaySettings) -> (Vec<PickerOption>, Option<ListItemKey>) {
    let options = vec![
        PickerOption {
            key: ListItemKey::from_u64(u64::from(ScreensaverMode::Dim.to_wire())),
            label: String::from(ScreensaverMode::Dim.label()),
            note: Some((String::from("stays readable"), palette::TEXT_SECONDARY)),
            selectable: true,
        },
        PickerOption {
            key: ListItemKey::from_u64(u64::from(ScreensaverMode::Off.to_wire())),
            label: String::from(ScreensaverMode::Off.label()),
            note: Some((String::from("saves power"), palette::TEXT_SECONDARY)),
            selectable: true,
        },
    ];
    let checked = Some(ListItemKey::from_u64(u64::from(current.mode.to_wire())));
    (options, checked)
}

/// The Idle-after (screensaver timeout) picker's options -- see
/// [`screensaver_mode_options`]'s doc comment for the shared-by-construction-
/// and-projection reasoning.
fn screensaver_timeout_options(current: DisplaySettings) -> (Vec<PickerOption>, Option<ListItemKey>) {
    let options: Vec<PickerOption> = ScreensaverTimeout::ALL
        .iter()
        .map(|timeout| PickerOption { key: ListItemKey::from_u64(u64::from(timeout.as_secs())), label: String::from(timeout.label()), note: None, selectable: true })
        .collect();
    let checked = Some(ListItemKey::from_u64(u64::from(current.timeout.as_secs())));
    (options, checked)
}

/// The Buffer (congestion-cushion) picker's two options (bead
/// `pico-link-8pp1.2`, S4) -- see [`screensaver_mode_options`]'s doc
/// comment for the shared-by-construction-and-projection reasoning. Only
/// `Low`/`Stable` are listed: `Super stable` is reserved wire value `3`
/// (see [`CushionPolicy`]'s doc comment) and deliberately NOT rendered as a
/// disabled placeholder until the 64KB ring it needs actually ships (Uma's
/// 2026-09-25 design, part 1).
fn cushion_options(current: CushionPolicy) -> (Vec<PickerOption>, Option<ListItemKey>) {
    let options = vec![
        PickerOption {
            key: ListItemKey::from_u64(u64::from(CushionPolicy::Low.to_wire())),
            label: String::from(CushionPolicy::Low.label()),
            note: Some((String::from("for calls"), palette::TEXT_SECONDARY)),
            selectable: true,
        },
        PickerOption {
            key: ListItemKey::from_u64(u64::from(CushionPolicy::Stable.to_wire())),
            label: String::from(CushionPolicy::Stable.label()),
            note: Some((String::from("fewer skips"), palette::TEXT_SECONDARY)),
            selectable: true,
        },
    ];
    let checked = Some(ListItemKey::from_u64(u64::from(current.to_wire())));
    (options, checked)
}

/// The LDAC MIN row's picker options (bead `pico-link-d42g.4`, F4), built
/// from the single [`ABR_FLOOR_OPTIONS`] list -- see [`screensaver_mode_
/// options`]'s doc comment for the shared-by-construction-and-projection
/// reasoning.
fn abr_floor_options(current: AbrFloor) -> (Vec<PickerOption>, Option<ListItemKey>) {
    let options = ABR_FLOOR_OPTIONS
        .iter()
        .map(|&(floor, note)| PickerOption {
            key: ListItemKey::from_u64(u64::from(floor.to_wire())),
            label: String::from(floor.label()),
            note: Some((String::from(note), palette::TEXT_SECONDARY)),
            selectable: true,
        })
        .collect();
    let checked = Some(ListItemKey::from_u64(u64::from(current.to_wire())));
    (options, checked)
}

/// The LDAC MIN row's picker (bead `pico-link-d42g.4`, F4) -- built via
/// [`build_picker_view_screen`], same shape as [`build_cushion_picker_
/// screen`] (a different mailbox, [`AbrFloorState`], not [`DisplaySettings
/// State`]). Applies live and saves on pick (`Action::None`, stays open):
/// `core` has nothing of its own to apply a floor to, C applies it live via
/// `pl_codec_ldac_set_floor` (design `.planning/design/2026-09-25-
/// adaptive-floor.md` sec 4).
fn build_abr_floor_picker_screen(abr_floor_state: &Rc<RefCell<AbrFloorState>>) -> Screen {
    let state_for_projection = Rc::clone(abr_floor_state);
    let projection = move || abr_floor_options(state_for_projection.borrow().current);
    let state_for_pick = Rc::clone(abr_floor_state);
    let on_pick = move |key: ListItemKey| {
        let floor = AbrFloor::from_wire(u8::try_from(key.as_u64()).unwrap_or(1));
        let mut s = state_for_pick.borrow_mut();
        s.current = floor;
        s.save_pending = true;
        Action::None
    };
    build_picker_view_screen(ScreenId::SettingsPicker(SettingsPickerKind::AbrFloor), "LDAC minimum", projection, on_pick)
}

/// The BUFFER row's picker (bead `pico-link-8pp1.2`, S4) -- built via
/// [`build_picker_view_screen`] the same way the two [`build_settings_
/// picker_screen`] pickers are, but kept separate from that function
/// because it picks into a different mailbox ([`CushionPolicyState`], not
/// [`DisplaySettingsState`]). Applies live and saves on pick, same as the
/// screensaver pickers (`Action::None`, stays open) -- but unlike them,
/// there is no `apply_pending` latch to set: `core` has nothing of its own
/// to apply a cushion policy to (see [`CushionPolicyState`]'s doc comment).
fn build_cushion_picker_screen(cushion_state: &Rc<RefCell<CushionPolicyState>>) -> Screen {
    let state_for_projection = Rc::clone(cushion_state);
    let projection = move || cushion_options(state_for_projection.borrow().current);
    let state_for_pick = Rc::clone(cushion_state);
    let on_pick = move |key: ListItemKey| {
        let policy = CushionPolicy::from_wire(u8::try_from(key.as_u64()).unwrap_or(1));
        let mut s = state_for_pick.borrow_mut();
        s.current = policy;
        s.save_pending = true;
        Action::None
    };
    build_picker_view_screen(ScreenId::SettingsPicker(SettingsPickerKind::Cushion), "Buffer", projection, on_pick)
}

/// The two Settings pickers (mode, timeout) -- both built via
/// [`build_picker_view_screen`], picking straight into the shared
/// [`DisplaySettingsState`] mailbox. Applies live (`Action::None`, stays
/// open, per that function's rule 2); the checkmark itself moves as soon as
/// the picker's own [`PickerView::sync`] re-reads `state` on the very next
/// frame (bead `pico-link-bgnd` M3) -- there is no `refresh_pending` latch
/// any more, because nothing needs to force a rebuild: both this picker and
/// the Settings screen underneath it read the same live `state` handle
/// directly.
///
/// # Panics
///
/// Panics if `kind` is [`SettingsPickerKind::Cushion`] or
/// [`SettingsPickerKind::AbrFloor`] -- both are never routed here;
/// [`build_settings_screen`] pushes [`build_cushion_picker_screen`]/
/// [`build_abr_floor_picker_screen`] directly instead, since each needs a
/// different mailbox type than this function takes.
pub(crate) fn build_settings_picker_screen(kind: SettingsPickerKind, state: &Rc<RefCell<DisplaySettingsState>>) -> Screen {
    match kind {
        SettingsPickerKind::Cushion => {
            unreachable!("SettingsPickerKind::Cushion is built by build_cushion_picker_screen, never routed through build_settings_picker_screen")
        }
        SettingsPickerKind::AbrFloor => {
            unreachable!("SettingsPickerKind::AbrFloor is built by build_abr_floor_picker_screen, never routed through build_settings_picker_screen")
        }
        SettingsPickerKind::ScreensaverMode => {
            let state_for_projection = Rc::clone(state);
            let projection = move || screensaver_mode_options(state_for_projection.borrow().current);
            let state_for_pick = Rc::clone(state);
            let on_pick = move |key: ListItemKey| {
                let mode = ScreensaverMode::from_wire(u8::try_from(key.as_u64()).unwrap_or(1));
                let mut s = state_for_pick.borrow_mut();
                s.current.mode = mode;
                s.apply_pending = true;
                s.save_pending = true;
                Action::None
            };
            build_picker_view_screen(ScreenId::SettingsPicker(kind), "Idle screen", projection, on_pick)
        }
        SettingsPickerKind::ScreensaverTimeout => {
            let state_for_projection = Rc::clone(state);
            let projection = move || screensaver_timeout_options(state_for_projection.borrow().current);
            let state_for_pick = Rc::clone(state);
            let on_pick = move |key: ListItemKey| {
                let timeout = ScreensaverTimeout::from_secs(u16::try_from(key.as_u64()).unwrap_or(60));
                let mut s = state_for_pick.borrow_mut();
                s.current.timeout = timeout;
                s.apply_pending = true;
                s.save_pending = true;
                Action::None
            };
            build_picker_view_screen(ScreenId::SettingsPicker(kind), "Idle after", projection, on_pick)
        }
    }
}

#[cfg(test)]
mod tests {
    use embedded_graphics::prelude::RgbColor;

    use crate::app::App;
    use crate::audio::{AbrFloor, CushionPolicy};
    use crate::input::NavIntent;

    use super::*;

    /// [`settings_rows`]'s default (`CushionPolicy::Low`, `AbrFloor::
    /// Kbps330`, `DisplaySettings::default()`) puts BUFFER first with
    /// `LDAC MIN` directly under it, showing `330 kbps` -- bead
    /// `pico-link-d42g.4` (F4), Uma's 2026-09-25 UX design sec 6's row
    /// position and default.
    #[test]
    fn settings_rows_default_shows_buffer_first_with_ldac_min_underneath() {
        let rows = settings_rows(DisplaySettings::default(), CushionPolicy::Low, AbrFloor::Kbps330);
        assert_eq!(rows[0].label, "BUFFER");
        assert_eq!(rows[0].value(), Some("Low latency"));
        assert_eq!(rows[1].label, "LDAC MIN");
        assert_eq!(rows[1].value(), Some("330 kbps"));
        assert_eq!(rows[2].label, "IDLE SCREEN");
        assert_eq!(rows[3].label, "IDLE AFTER");
    }

    /// The BUFFER row's value tracks whatever `CushionPolicy` it's built
    /// with -- this is what [`SettingsView::sync`] relies on to reflect a
    /// live pick (or a `CushionPolicyLoaded` boot seed) with no rebuild.
    #[test]
    fn settings_rows_reflects_a_stable_cushion_policy() {
        let rows = settings_rows(DisplaySettings::default(), CushionPolicy::Stable, AbrFloor::Kbps330);
        assert_eq!(rows[0].value(), Some("Stable"));
    }

    /// The LDAC MIN row's value tracks whatever `AbrFloor` it's built with --
    /// the same "reflects a live pick or boot seed with no rebuild" property
    /// as the BUFFER row above, bead `pico-link-d42g.4` (F4).
    #[test]
    fn settings_rows_reflects_a_198_abr_floor() {
        let rows = settings_rows(DisplaySettings::default(), CushionPolicy::Low, AbrFloor::Kbps198);
        assert_eq!(rows[1].label, "LDAC MIN");
        assert_eq!(rows[1].value(), Some("198 kbps"));
    }

    /// End-to-end through real input: Home menu -> Settings -> BUFFER row
    /// (now row 0, no `Down` needed) -> its picker -> `Down` to `Stable` ->
    /// `Select`. Proves the pick reaches `App::take_cushion_policy_to_save`
    /// (what `ui-ffi`'s `pl_ui_poll_command` drains into
    /// `PlCommandTag::SetCushionPolicy`), and that the Settings row
    /// underneath reflects it live with no pop/re-push.
    #[test]
    fn picking_stable_in_the_buffer_picker_reaches_take_cushion_policy_to_save() {
        let mut app = App::new(240, 240);
        assert_eq!(app.cushion_policy(), CushionPolicy::Low, "default must be Low");

        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Down]); // Settings row
        app.handle_input(vec![NavIntent::Select]); // open Settings (BUFFER row focused, row 0)
        app.handle_input(vec![NavIntent::Select]); // open the Buffer picker
        app.handle_input(vec![NavIntent::Down]); // focus Stable
        app.handle_input(vec![NavIntent::Select]); // pick it

        assert_eq!(app.cushion_policy(), CushionPolicy::Stable, "the live policy must update immediately");
        assert_eq!(app.take_cushion_policy_to_save(), Some(CushionPolicy::Stable), "the pick must arm the save latch exactly once");
        assert_eq!(app.take_cushion_policy_to_save(), None, "the save latch must drain to None after being taken once");
    }

    /// A `CushionPolicyLoaded` boot seed (S3's `Event::CushionPolicyLoaded`)
    /// must be reflected on the BUFFER row with no user pick, and must not
    /// arm the save latch -- the seeding discipline `CushionPolicyState`'s
    /// doc comment states, exercised here through the actual Settings row
    /// rather than only through `App::cushion_policy()` (S3's own tests).
    #[test]
    fn a_cushion_policy_loaded_event_before_settings_is_opened_shows_on_the_buffer_row() {
        let mut app = App::new(240, 240);
        app.handle_event(crate::app::Event::CushionPolicyLoaded { policy: 2 }); // Stable

        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Down]); // Settings row
        app.handle_input(vec![NavIntent::Select]); // open Settings

        assert_eq!(app.current_screen_title(), SETTINGS_TITLE);
        assert_eq!(app.cushion_policy(), CushionPolicy::Stable);
        assert_eq!(app.take_cushion_policy_to_save(), None, "a boot-load seed must never arm the save latch");
    }

    /// End-to-end through real input: Home menu -> Settings -> `Down` once
    /// to LDAC MIN (row 1, under BUFFER) -> its picker -> `Down` twice to
    /// `198 kbps` -> `Select`. Proves the pick reaches `App::take_abr_floor_
    /// to_save` (what `ui-ffi`'s `pl_ui_poll_command` drains into
    /// `PlCommandTag::SetAbrFloor`), and that the Settings row underneath
    /// reflects it live with no pop/re-push -- bead `pico-link-d42g.4` (F4),
    /// mirroring [`picking_stable_in_the_buffer_picker_reaches_take_
    /// cushion_policy_to_save`] above.
    #[test]
    fn picking_198_in_the_abr_floor_picker_reaches_take_abr_floor_to_save() {
        let mut app = App::new(240, 240);
        assert_eq!(app.abr_floor(), AbrFloor::Kbps330, "default must be 330 kbps");

        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Down]); // Settings row
        app.handle_input(vec![NavIntent::Select]); // open Settings (BUFFER row focused, row 0)
        app.handle_input(vec![NavIntent::Down]); // focus LDAC MIN (row 1)
        app.handle_input(vec![NavIntent::Select]); // open the LDAC MIN picker
        app.handle_input(vec![NavIntent::Down]); // focus 246 kbps
        app.handle_input(vec![NavIntent::Down]); // focus 198 kbps
        app.handle_input(vec![NavIntent::Select]); // pick it

        assert_eq!(app.abr_floor(), AbrFloor::Kbps198, "the live floor must update immediately");
        assert_eq!(app.take_abr_floor_to_save(), Some(AbrFloor::Kbps198), "the pick must arm the save latch exactly once");
        assert_eq!(app.take_abr_floor_to_save(), None, "the save latch must drain to None after being taken once");
    }

    /// An `AbrFloorLoaded` boot seed (F3's `Event::AbrFloorLoaded`) must be
    /// reflected on the LDAC MIN row with no user pick, and must not arm
    /// the save latch -- mirrors [`a_cushion_policy_loaded_event_before_
    /// settings_is_opened_shows_on_the_buffer_row`] above, bead
    /// `pico-link-d42g.4` (F4).
    #[test]
    fn an_abr_floor_loaded_event_before_settings_is_opened_shows_on_the_ldac_min_row() {
        let mut app = App::new(240, 240);
        app.handle_event(crate::app::Event::AbrFloorLoaded { floor: 2 }); // 246 kbps

        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Down]); // Settings row
        app.handle_input(vec![NavIntent::Select]); // open Settings

        assert_eq!(app.current_screen_title(), SETTINGS_TITLE);
        assert_eq!(app.abr_floor(), AbrFloor::Kbps246);
        assert_eq!(app.take_abr_floor_to_save(), None, "a boot-load seed must never arm the save latch");
    }

    /// Headless PNG dump of the Settings screen (BUFFER row visible, on
    /// top) and the Buffer picker at zoom -- the same "dumped from here
    /// since this module's tests are the only place with `pub(crate)`
    /// access" shape `picker.rs::tests::picker_screenshot_at_zoom` uses.
    #[test]
    fn settings_and_buffer_picker_screenshots_at_zoom() {
        const ZOOM: u32 = 3;

        let out_dir = std::env::temp_dir().join("pico-link-settings-screenshot");
        std::fs::create_dir_all(&out_dir).expect("failed to create output dir");

        fn dump(app: &mut App, name: &str, out_dir: &std::path::Path) {
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
            let zoomed = image::imageops::resize(&image, framebuffer.width() * ZOOM, framebuffer.height() * ZOOM, image::imageops::FilterType::Nearest);
            let path = out_dir.join(name);
            zoomed.save(&path).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
            println!("wrote {}", path.display());
        }

        let mut app = App::new(240, 240);
        app.handle_input(vec![NavIntent::Select, NavIntent::Down, NavIntent::Select]); // open Settings
        dump(&mut app, "settings.png", &out_dir);

        app.handle_input(vec![NavIntent::Select]); // open the Buffer picker (row 0, BUFFER)
        dump(&mut app, "buffer_picker.png", &out_dir);

        app.handle_input(vec![NavIntent::Back]); // close the Buffer picker back to Settings
        app.handle_input(vec![NavIntent::Down]); // focus LDAC MIN (row 1)
        app.handle_input(vec![NavIntent::Select]); // open the LDAC MIN picker
        dump(&mut app, "abr_floor_picker.png", &out_dir);
    }
}
