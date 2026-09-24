use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::power::{ScreensaverMode, ScreensaverTimeout};
use crate::render::theme::palette;
use crate::render::{Action, FieldList, FieldRow, ListItemKey, Screen};

use super::super::{DisplaySettingsState, ScreenCarry, ScreenId, SettingsPickerKind};
use super::picker::{build_single_select_screen, PickerOption};

/// The Settings screen's fixed title. Originally a placeholder (bead
/// `pico-link-znb.8`/E7, giving Home's menu-face "Settings" row a real,
/// reachable destination); real content landed with the screensaver
/// dim/off + timeout setting (bead pico-link-qivj.2).
pub(crate) const SETTINGS_TITLE: &str = "Settings";

/// The Settings screen's two rows -- `IDLE SCREEN` (mode) and `IDLE AFTER`
/// (timeout), each pushing its own picker. Design pico-link-qivj.1 S6.
pub(crate) fn build_settings_screen(state: &Rc<RefCell<DisplaySettingsState>>, carry: &ScreenCarry) -> Screen {
    const ROW_MODE: u64 = 0;
    const ROW_TIMEOUT: u64 = 1;

    let current = state.borrow().current;
    let rows = vec![
        FieldRow::action("IDLE SCREEN")
            .with_value(current.mode.label(), palette::TEXT_PRIMARY)
            .with_key(ListItemKey::from_u64(ROW_MODE)),
        FieldRow::action("IDLE AFTER")
            .with_value(current.timeout.label(), palette::TEXT_PRIMARY)
            .with_key(ListItemKey::from_u64(ROW_TIMEOUT)),
    ];
    let state_for_activate = Rc::clone(state);
    let list = FieldList::new(rows)
        .with_selected_identity(carry.selected_key, carry.selected_index)
        .on_activate_index(move |index| {
            match index {
                0 => Action::PushView(Box::new({
                    let state = Rc::clone(&state_for_activate);
                    move || build_settings_picker_screen(SettingsPickerKind::ScreensaverMode, &state, &ScreenCarry::default())
                })),
                1 => Action::PushView(Box::new({
                    let state = Rc::clone(&state_for_activate);
                    move || build_settings_picker_screen(SettingsPickerKind::ScreensaverTimeout, &state, &ScreenCarry::default())
                })),
                _ => Action::None,
            }
        });
    let list = if let Some(top) = carry.scroll_top { list.with_scroll_top(top) } else { list };
    Screen::new(SETTINGS_TITLE, vec![Box::new(list)]).with_id(ScreenId::Settings)
}

/// The two Settings pickers (mode, timeout) -- both built via
/// [`build_single_select_screen`], picking straight into the shared
/// [`DisplaySettingsState`] mailbox. Applies live (`Action::None`, stays
/// open, per that function's rule 2); the checkmark itself moves only once
/// `App::refresh_stack` rebuilds this screen from the freshly stored
/// value (`refresh_pending`), never optimistically here.
pub(crate) fn build_settings_picker_screen(kind: SettingsPickerKind, state: &Rc<RefCell<DisplaySettingsState>>, carry: &ScreenCarry) -> Screen {
    let current = state.borrow().current;
    match kind {
        SettingsPickerKind::ScreensaverMode => {
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
            let state_for_pick = Rc::clone(state);
            let on_pick = move |key: ListItemKey| {
                let mode = ScreensaverMode::from_wire(u8::try_from(key.as_u64()).unwrap_or(1));
                let mut s = state_for_pick.borrow_mut();
                s.current.mode = mode;
                s.apply_pending = true;
                s.save_pending = true;
                s.refresh_pending = true;
                Action::None
            };
            build_single_select_screen(ScreenId::SettingsPicker(kind), "Idle screen", options, checked, carry, on_pick)
        }
        SettingsPickerKind::ScreensaverTimeout => {
            let options: Vec<PickerOption> = ScreensaverTimeout::ALL
                .iter()
                .map(|timeout| PickerOption {
                    key: ListItemKey::from_u64(u64::from(timeout.as_secs())),
                    label: String::from(timeout.label()),
                    note: None,
                    selectable: true,
                })
                .collect();
            let checked = Some(ListItemKey::from_u64(u64::from(current.timeout.as_secs())));
            let state_for_pick = Rc::clone(state);
            let on_pick = move |key: ListItemKey| {
                let timeout = ScreensaverTimeout::from_secs(u16::try_from(key.as_u64()).unwrap_or(60));
                let mut s = state_for_pick.borrow_mut();
                s.current.timeout = timeout;
                s.apply_pending = true;
                s.save_pending = true;
                s.refresh_pending = true;
                Action::None
            };
            build_single_select_screen(ScreenId::SettingsPicker(kind), "Idle after", options, checked, carry, on_pick)
        }
    }
}
