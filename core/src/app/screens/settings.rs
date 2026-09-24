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

use crate::input::NavIntent;
use crate::power::{DisplaySettings, ScreensaverMode, ScreensaverTimeout};
use crate::render::theme::palette;
use crate::render::{Action, FieldList, FieldRow, FocusEvent, FrameBuffer565, ListItemKey, PaintKey, RenderCtx, Screen, Verb, Widget};

use super::super::{DisplaySettingsState, ScreenId, SettingsPickerKind};
use super::picker::{build_picker_view_screen, PickerOption};

/// The Settings screen's fixed title.
pub(crate) const SETTINGS_TITLE: &str = "Settings";

/// [`ListItemKey`]s for the Settings screen's two fixed rows.
const ROW_MODE_KEY: ListItemKey = ListItemKey::from_u64(0);
const ROW_TIMEOUT_KEY: ListItemKey = ListItemKey::from_u64(1);

/// Seed for [`SettingsView::projection_key`] -- only needs to differ from
/// other widgets'/views' own seeds.
const SETTINGS_PROJECTION_SEED: u64 = 61;

fn settings_rows(current: DisplaySettings) -> Vec<FieldRow> {
    vec![
        FieldRow::action("IDLE SCREEN").with_value(current.mode.label(), palette::TEXT_PRIMARY).with_key(ROW_MODE_KEY),
        FieldRow::action("IDLE AFTER").with_value(current.timeout.label(), palette::TEXT_PRIMARY).with_key(ROW_TIMEOUT_KEY),
    ]
}

fn settings_projection_key(current: DisplaySettings) -> PaintKey {
    PaintKey::of(SETTINGS_PROJECTION_SEED).fold(u64::from(current.mode.to_wire())).fold(u64::from(current.timeout.as_secs()))
}

/// The Settings screen's two rows -- `IDLE SCREEN` (mode) and `IDLE AFTER`
/// (timeout), each pushing its own picker. Built once per push (bead
/// `pico-link-bgnd` M3) and never rebuilt again while it stays on the
/// stack -- [`SettingsView::sync`] re-reads `state` itself every frame this
/// screen is on top, so a pick made in either picker (which shares this
/// same `Rc<RefCell<DisplaySettingsState>>`) is reflected here with no
/// rebuild.
pub(crate) fn build_settings_screen(state: &Rc<RefCell<DisplaySettingsState>>) -> Screen {
    let (rows, projection_key) = {
        let current = state.borrow().current;
        (settings_rows(current), settings_projection_key(current))
    };
    let state_for_activate = Rc::clone(state);
    let list = FieldList::new(rows).on_activate_key(move |key| {
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
    let view = SettingsView { list, state: Rc::clone(state), projection_key };
    Screen::new(SETTINGS_TITLE, vec![Box::new(view)]).with_id(ScreenId::Settings)
}

/// Wraps [`FieldList`] to keep the Settings screen's two rows synced to the
/// live [`DisplaySettingsState`] -- see [`build_settings_screen`]'s doc
/// comment. Every method that isn't `sync` is a plain forward, the same
/// shape `DevicesListView`/`DevicePageView` already use.
struct SettingsView {
    list: FieldList,
    state: Rc<RefCell<DisplaySettingsState>>,
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
        let key = settings_projection_key(current);
        if key != self.projection_key {
            self.list.set_rows(settings_rows(current));
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

/// The two Settings pickers (mode, timeout) -- both built via
/// [`build_picker_view_screen`], picking straight into the shared
/// [`DisplaySettingsState`] mailbox. Applies live (`Action::None`, stays
/// open, per that function's rule 2); the checkmark itself moves as soon as
/// the picker's own [`PickerView::sync`] re-reads `state` on the very next
/// frame (bead `pico-link-bgnd` M3) -- there is no `refresh_pending` latch
/// any more, because nothing needs to force a rebuild: both this picker and
/// the Settings screen underneath it read the same live `state` handle
/// directly.
pub(crate) fn build_settings_picker_screen(kind: SettingsPickerKind, state: &Rc<RefCell<DisplaySettingsState>>) -> Screen {
    match kind {
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
