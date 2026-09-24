use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::render::theme::palette;
use crate::render::{Action, ListItemKey};

use super::picker::{build_single_select_screen, PickerOption};
use super::super::{BtModel, Command, DeviceAddr, PickerKind, Refresh, ScreenCarry, ScreenId};

/// LDAC's ADAPTIVE identity, 1-based, for [`PairedDevice::ldac_quality`]
/// (`persist.c`'s convention: 0 = never chosen, 1/2/3 = pinned, 4 =
/// Adaptive).
pub(crate) const LDAC_QUALITY_ADAPTIVE: u8 = 4;

/// `ListItemKey`s for the `QUALITY` picker's four rows (highest rate
/// first, Adaptive last). Reused for both the picker's `checked`/`on_pick`
/// plumbing and [`ldac_quality_fixed_kbps`]'s parallel ordering.
const LDAC_QUALITY_PICKER_KEYS: [ListItemKey; 4] =
    [ListItemKey::from_u64(101), ListItemKey::from_u64(102), ListItemKey::from_u64(103), ListItemKey::from_u64(104)];

/// LDAC's three named fixed rates, in kbps, highest first --
/// sample-rate dependent (909/606/303 at 44.1kHz), computed here in **one**
/// place rather than restated at each of the row/picker/Home call sites.
/// `None` (rate unknown, or disconnected) falls back to the 48kHz set --
/// this project's USB chain is 48k-only today and there is no
/// `sample_rate_hz` seam yet, so every call site below passes `None`.
fn ldac_quality_rates_kbps(sample_rate_hz: Option<u32>) -> [u32; 3] {
    if sample_rate_hz == Some(44_100) {
        [909, 606, 303]
    } else {
        [990, 660, 330]
    }
}

/// `ldac_quality` (1-based, [`PairedDevice::ldac_quality`]'s convention) to
/// its fixed kbps, or `None` for Adaptive (`4`) or any out-of-range value.
/// `0` ("never chosen") maps to the firmware's built-in default -- `0` is
/// a storage state, never a display state: the fresh-device row/picker
/// render the effective default as if it had been chosen, check included.
/// `codec_ldac.c`'s `pl_ldac_quality_to_initial_state` is the source of
/// truth this mirrors (today: HQ/990 kbps for both `0` and `1`).
pub(in crate::app) fn ldac_quality_fixed_kbps(ldac_quality: u8, sample_rate_hz: Option<u32>) -> Option<u32> {
    let rates = ldac_quality_rates_kbps(sample_rate_hz);
    match ldac_quality {
        0 | 1 => Some(rates[0]),
        2 => Some(rates[1]),
        3 => Some(rates[2]),
        _ => None,
    }
}

/// The device page's `QUALITY` row's checked-row key, mirroring
/// [`ldac_quality_fixed_kbps`]'s `0`-is-the-default convention so the
/// check never sits on nothing.
fn ldac_quality_checked_key(ldac_quality: u8) -> ListItemKey {
    match ldac_quality {
        2 => LDAC_QUALITY_PICKER_KEYS[1],
        3 => LDAC_QUALITY_PICKER_KEYS[2],
        LDAC_QUALITY_ADAPTIVE => LDAC_QUALITY_PICKER_KEYS[3],
        _ => LDAC_QUALITY_PICKER_KEYS[0], // 0 (never chosen) or 1 (990/HQ)
    }
}

/// The `QUALITY` picker's four entries: numbers leading, highest first,
/// `HQ`/`SQ`/`MQ` never shown. Returns [`Refresh::Gone`] when `addr` is
/// no longer in [`BtModel::paired`] --
/// same reasoning as [`build_device_page_screen`]'s own doc comment (the
/// picker sits one level above the page that already unwinds on this).
pub(in crate::app) fn build_ldac_quality_picker_screen(model: &BtModel, addr: DeviceAddr, carry: &ScreenCarry, commands: &Rc<RefCell<VecDeque<Command>>>) -> Refresh {
    const NOTES: [&str; 3] = ["best audio", "balanced", "most reliable"];

    let Some(device) = model.paired.iter().find(|d| d.addr == addr) else {
        return Refresh::Gone;
    };
    let connected = model.connected_addr == Some(addr);
    let streaming = connected && model.connected_codec.as_ref().is_some_and(|c| c.word == "LDAC");
    let rates = ldac_quality_rates_kbps(None);
    let mut options: Vec<PickerOption> = (0..3)
        .map(|i| PickerOption {
            key: LDAC_QUALITY_PICKER_KEYS[i],
            label: format!("{} kbps", rates[i]),
            note: Some((String::from(NOTES[i]), palette::TEXT_SECONDARY)),
            selectable: true,
        })
        .collect();
    // Adaptive's trailing note is live while streaming ("660 now",
    // mirroring the codec picker's existing "SBC now"), and `varies`
    // otherwise -- disconnected, or connected but not yet streaming LDAC
    // (never claim a number we don't have, same rule
    // `device_page_quality_row_value` follows).
    let adaptive_note = if streaming { model.ldac_live_kbps.map_or_else(|| String::from("varies"), |kbps| format!("{kbps} now")) } else { String::from("varies") };
    options.push(PickerOption {
        key: LDAC_QUALITY_PICKER_KEYS[3],
        label: String::from("Adaptive"),
        note: Some((adaptive_note, palette::TEXT_SECONDARY)),
        selectable: true,
    });

    let checked = Some(ldac_quality_checked_key(device.ldac_quality));
    let commands_for_pick = Rc::clone(commands);
    let on_pick = move |key: ListItemKey| {
        let ldac_quality = LDAC_QUALITY_PICKER_KEYS
            .iter()
            .position(|k| *k == key)
            .map_or(LDAC_QUALITY_ADAPTIVE, |i| u8::try_from(i + 1).unwrap_or(LDAC_QUALITY_ADAPTIVE));
        commands_for_pick.borrow_mut().push_back(Command::SetDeviceLdacQuality { addr, ldac_quality });
        // Applies live, no confirm, the picker stays open -- `Action::None`
        // (not `PopView`), per `build_single_select_screen`'s rule 2. The
        // check itself moves only once the model's own echo
        // (`Event::PairedDeviceUpserted`) lands and `refresh_stack` rebuilds
        // this screen from `checked` above -- never optimistically here.
        Action::None
    };
    Refresh::Rebuild(build_single_select_screen(
        ScreenId::Picker(PickerKind::LdacQuality, addr),
        "Quality",
        options,
        checked,
        carry,
        on_pick,
    ))
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec;
    use core::cell::RefCell;

    use crate::app::test_support::{open_devices, upsert};
    use crate::app::{App, ConnectedCodec, Event};
    use crate::input::NavIntent;

    use super::*;

    // --- The QUALITY row, its picker, and Home's live bitrate/ADAPTIVE tag ---

    #[test]
    fn ldac_quality_rates_default_to_the_48khz_ladder_and_switch_at_44_1khz() {
        assert_eq!(ldac_quality_rates_kbps(None), [990, 660, 330]);
        assert_eq!(ldac_quality_rates_kbps(Some(48_000)), [990, 660, 330]);
        assert_eq!(ldac_quality_rates_kbps(Some(44_100)), [909, 606, 303]);
    }

    #[test]
    fn ldac_quality_fixed_kbps_maps_never_chosen_to_the_same_default_as_a_990_pin() {
        assert_eq!(ldac_quality_fixed_kbps(0, None), ldac_quality_fixed_kbps(1, None), "design §7: 0 renders as the effective default");
        assert_eq!(ldac_quality_fixed_kbps(0, None), Some(990));
        assert_eq!(ldac_quality_fixed_kbps(2, None), Some(660));
        assert_eq!(ldac_quality_fixed_kbps(3, None), Some(330));
        assert_eq!(ldac_quality_fixed_kbps(LDAC_QUALITY_ADAPTIVE, None), None, "Adaptive has no single fixed rate");
    }

    #[test]
    fn opening_quality_from_the_device_page_pushes_the_picker_and_a_pick_queues_the_command() {
        let mut app = App::new(240, 240);
        let addr = [28; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice
        app.handle_event(upsert(addr, "Cans", 1));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // connected row -> device page
        assert_eq!(app.current_screen_title(), "Cans");
        app.handle_input(vec![NavIntent::Down]); // focus QUALITY (row 1)
        app.handle_input(vec![NavIntent::Select]); // -> picker
        assert_eq!(app.current_screen_title(), "Quality", "A on QUALITY must push the picker (design §2)");

        // Pick "660 kbps" (the second row).
        app.handle_input(vec![NavIntent::Down]);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(
            app.poll_command(),
            Some(Command::SetDeviceLdacQuality { addr, ldac_quality: 2 }),
            "A on a picker row must queue the pin, 1-based"
        );
        assert_eq!(app.current_screen_title(), "Quality", "design §5: the picker stays open, no confirm, no pop");
    }

    #[test]
    fn quality_picker_vanishes_the_stack_unwinds_when_the_device_is_forgotten() {
        let mut model = BtModel::default();
        let addr = [31; 6];
        // No paired push at all -- simulates the device having just been
        // forgotten out from under an open picker.
        let carry = ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None };
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        assert!(matches!(build_ldac_quality_picker_screen(&model, addr, &carry, &commands), Refresh::Gone));
        let _ = &mut model; // silence unused-mut if the assertion above is ever relaxed
    }
}
