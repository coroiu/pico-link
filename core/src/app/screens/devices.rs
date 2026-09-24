use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::convert::Infallible;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::input::NavIntent;
use crate::render::theme::palette;
use crate::render::wizard::build_wizard_screen;
use crate::render::{
    Action, ButtonLabel, ChromeContribution, ConfirmView, FocusEvent, FrameBuffer565, ListItem, ListItemKey, MenuItem, RenderCtx, Screen, Verb,
    VerticalList, Widget,
};

use super::device_page::build_device_page_screen;
use super::super::model::MAX_PAIRED_DEVICES;
use super::super::{
    truncate_device_name, BtModel, Command, ConnectStep, DeviceAddr, DeviceEntry, PairedDevice, Refresh, ScreenCarry, ScreenId, WizardPhase,
};

/// The devices screen's "Pair new headphones" row's identity key. Not
/// backed by a `DeviceAddr` (it isn't a device), so it's a fixed sentinel
/// instead -- see [`ListItemKey::from`]'s doc comment for why this can
/// never collide with a real device's key (a device key's top two bytes
/// are always `0`; this sentinel's are always `0xFE`).
const PAIR_NEW_ROW_KEY: ListItemKey = ListItemKey::from_bytes([0xFE; 8]);

/// The Devices screen's fixed title -- reached by "A"/the menu face's
/// "Bluetooth" row from Home, which owns the "Pico Link" brand title
/// instead.
pub(crate) const DEVICES_TITLE: &str = "Devices";

/// A paired device's display label -- its name, or, if C never reported one
/// (or it hasn't resolved yet), `(unknown device)` plus the address's last
/// three bytes as the discriminator, so a nameless row is still
/// distinguishable from every other nameless row.
pub(in crate::app) fn paired_device_label(device: &PairedDevice) -> String {
    if device.name.is_empty() {
        format!("(unknown device) {:02X}:{:02X}:{:02X}", device.addr[3], device.addr[4], device.addr[5])
    } else {
        device.name.clone()
    }
}

/// Builds the devices screen: the connected device (if any) pinned first,
/// sublabelled `Connected`; then every other paired device,
/// MRU-descending, sublabelled `Paired`; then `Pair new headphones` last.
/// **No RSSI, no address, no availability dot** -- never claim
/// availability that hasn't been verified, and the recurring "switch
/// device" job belongs under the cursor while the rare "pair a new one"
/// job belongs at the end.
///
/// Replaces the old scan-result rendering entirely -- see
/// [`BtModel::discovered`]/[`BtModel::paired`]'s doc comments for the
/// "wizard-only" / "Devices-screen-only" reader split this enforces.
///
/// Every row carries a [`ListItemKey`] (`device.addr` via `From<[u8; 6]>`,
/// or [`PAIR_NEW_ROW_KEY`] for the fixed last row) so `prev_key`/
/// `prev_index` can carry the user's selection forward **by identity**
/// through [`VerticalList::with_selected_identity`] -- a device arriving,
/// being renamed in place, reordering by a fresh `mru_seq`, or dropping out
/// entirely no longer moves the selection just because the *index* it used
/// to occupy now means something else. `prev_key` of `None`/not-found
/// falls back to clamping `prev_index` -- see that method's doc comment.
pub(crate) fn build_devices_screen(
    model: &BtModel,
    prev_key: Option<ListItemKey>,
    prev_index: usize,
    prev_scroll_top: Option<usize>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    wizard_phase: &Rc<RefCell<WizardPhase>>,
    wizard_devices: &Rc<RefCell<Vec<DeviceEntry>>>,
) -> Screen {
    let connected = model.connected_addr.and_then(|addr| model.paired.iter().find(|d| d.addr == addr));
    let mut others: Vec<&PairedDevice> =
        model.paired.iter().filter(|d| Some(d.addr) != model.connected_addr).collect();
    others.sort_by_key(|d| core::cmp::Reverse(d.mru_seq));

    let mut ordered: Vec<PairedDevice> = Vec::with_capacity(model.paired.len());
    if let Some(device) = connected {
        ordered.push(device.clone());
    }
    ordered.extend(others.into_iter().cloned());

    let mut items: Vec<ListItem> = ordered
        .iter()
        .map(|device| {
            let sublabel = if Some(device.addr) == model.connected_addr { "Connected" } else { "Paired" };
            // `Verb::Open`: every paired row pushes a deeper screen --
            // device detail for the connected row, the wizard's
            // `Connecting` phase for any other.
            ListItem::new(paired_device_label(device))
                .with_sublabel(sublabel)
                .with_key(ListItemKey::from(device.addr))
                .with_verb(Verb::Open)
        })
        .collect();
    // `Verb::Pair`: begins pairing -- including at the 8-device cap, where
    // the forget-picker is the app making room, not a different intent.
    items.push(ListItem::new("Pair new headphones").with_key(PAIR_NEW_ROW_KEY).with_verb(Verb::Pair));

    let paired_len = model.paired.len();
    let connected_addr = model.connected_addr;
    let ordered_for_activate = ordered.clone();
    let paired_for_full = model.paired.clone();
    // Snapshot for the connected row's push -- `Action::PushView`'s
    // builder is `FnOnce`, with no path back to a live `&BtModel` at the
    // moment it actually runs (it's called from inside
    // `Navigator::apply_action`, not from `App`). `BtModel` is `Clone` for
    // exactly this reason (see [`build_forget_picker_screen`]'s own
    // `model.paired.clone()` precedent above). The very next model event
    // replaces this page with a live-read one via
    // [`App::build_identified_screen`]'s `ScreenId::DevicePage` arm -- this
    // snapshot only has to be right for the single frame between the press
    // and that next refresh.
    let model_for_device_page = model.clone();
    let commands_for_activate = Rc::clone(commands);
    let wizard_phase_for_activate = Rc::clone(wizard_phase);
    let wizard_devices_for_activate = Rc::clone(wizard_devices);
    let list = VerticalList::new(items)
        // The `Verb::Open` here is only the list's fallback default; every
        // row above carries its own override, so this value is never
        // actually read.
        .on_activate_index(Verb::Open, move |index| {
            if let Some(device) = ordered_for_activate.get(index) {
                if Some(device.addr) == connected_addr {
                    // A on the connected row: no reconnect to do -- push
                    // the real device page.
                    let addr = device.addr;
                    let fallback_title = paired_device_label(device);
                    let model = model_for_device_page.clone();
                    let commands = Rc::clone(&commands_for_activate);
                    return Action::PushView(Box::new(move || {
                        let carry = ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None };
                        match build_device_page_screen(&model, addr, &carry, &commands) {
                            Refresh::Rebuild(screen) => screen,
                            // The connected device we just pressed cannot
                            // have vanished between the press and this
                            // closure running -- `Refresh::Gone` is
                            // structurally unreachable here, but a
                            // same-titled empty screen is a harmless
                            // fallback rather than a panic if it ever is.
                            // `Refresh::Keep` is likewise unreachable:
                            // `build_device_page_screen` never returns it
                            // -- no builder does yet.
                            Refresh::Gone | Refresh::Keep => Screen::new(fallback_title, vec![]),
                        }
                    }));
                }
                // A on any other paired row: switch to it, reusing the
                // wizard -- `Command::Connect` + pushing straight into
                // `Connecting`, no new phase.
                commands_for_activate
                    .borrow_mut()
                    .push_back(Command::Connect { addr: device.addr, name: truncate_device_name(&device.name) });
                *wizard_phase_for_activate.borrow_mut() = WizardPhase::connecting_pending(device.addr, ConnectStep::Connecting);
                let phase = Rc::clone(&wizard_phase_for_activate);
                let devices = Rc::clone(&wizard_devices_for_activate);
                let commands = Rc::clone(&commands_for_activate);
                return Action::PushView(Box::new(move || build_wizard_screen(phase, devices, commands)));
            }
            // "Pair new headphones", the fixed last row. Gated on capacity
            // *before* any radio work: under the cap, open the wizard
            // exactly as before; at the cap, open the pick-one-to-forget
            // flow instead.
            if paired_len < MAX_PAIRED_DEVICES {
                *wizard_phase_for_activate.borrow_mut() = WizardPhase::scanning_pending();
                wizard_devices_for_activate.borrow_mut().clear();
                commands_for_activate.borrow_mut().push_back(Command::StartScan);
                let phase = Rc::clone(&wizard_phase_for_activate);
                let devices = Rc::clone(&wizard_devices_for_activate);
                let commands = Rc::clone(&commands_for_activate);
                Action::PushView(Box::new(move || build_wizard_screen(phase, devices, commands)))
            } else {
                let paired = paired_for_full.clone();
                let commands = Rc::clone(&commands_for_activate);
                Action::PushView(Box::new(move || build_forget_picker_screen(paired, commands)))
            }
        })
        .with_selected_identity(prev_key, prev_index);
    let list = if let Some(top) = prev_scroll_top { list.with_scroll_top(top) } else { list };

    let view = DevicesListView { list, row_devices: ordered, commands: Rc::clone(commands) };
    Screen::new(DEVICES_TITLE, vec![Box::new(view)]).with_id(ScreenId::Devices)
}

/// Wraps [`VerticalList`] to add the Devices screen's X action ("X opens
/// a forget confirm") on top of it -- `VerticalList` itself has no
/// opinion about `ShortcutX` (see `crate::input::NavIntent::
/// ShortcutX`'s doc comment), so this is the same "small wrapper widget
/// intercepts one `NavIntent` variant, delegates the rest" shape
/// `crate::render::wizard::PairingWizardView` already uses for its own
/// phase-specific `ShortcutX` handling.
struct DevicesListView {
    list: VerticalList,
    /// Parallel to `list`'s rows *up to* the fixed "Pair new headphones"
    /// row (which carries no entry here) -- lets `on_intent` map the
    /// currently selected index back to a real device without needing a
    /// `ListItemKey` -> address lookup.
    row_devices: Vec<PairedDevice>,
    commands: Rc<RefCell<VecDeque<Command>>>,
}

impl Widget for DevicesListView {
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

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        if intent == NavIntent::ShortcutX {
            let index = self.list.selected_index();
            return if let Some(device) = self.row_devices.get(index) {
                let addr = device.addr;
                let label = paired_device_label(device);
                let commands = Rc::clone(&self.commands);
                Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)))
            } else {
                // The fixed "Pair new headphones" row has nothing to forget.
                Action::None
            };
        }
        self.list.on_intent(intent)
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let index = self.list.selected_index();
        let x = if index < self.row_devices.len() {
            ButtonLabel::Live(String::from("forget"))
        } else {
            ButtonLabel::Inert
        };
        Some(ChromeContribution { x: Some(x), ..ChromeContribution::default() })
    }

    fn selected_index(&self) -> Option<usize> {
        Some(self.list.selected_index())
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        self.list.selected_key()
    }

    /// Forwards `list`'s own answer — see `Widget::scroll_top`'s doc
    /// comment on why a wrapper must forward this rather than let the
    /// default `None` silently swallow it (the same hazard
    /// `redraw_after` below already guards against).
    fn scroll_top(&self) -> Option<usize> {
        self.list.scroll_top()
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        self.list.render(area, ctx, target)
    }

    /// Forwards `list`'s own answer -- without this the default (`None`)
    /// would swallow it. `VerticalList` has no time-driven
    /// content today, but the wrapper must not be the thing that silently
    /// drops a future one under the dirty gate.
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        self.list.redraw_after(ctx)
    }
}

/// The pick-one-to-forget screen: reached only when `Pair new headphones`
/// is activated while `paired.len() == MAX_PAIRED_DEVICES` -- fullness
/// discovered and resolved entirely before any radio work. Every row
/// pushes the same [`build_forget_confirm_screen`] a device row's X
/// action does.
const FORGET_PICKER_TITLE: &str = "Pick one to forget";

fn build_forget_picker_screen(paired: Vec<PairedDevice>, commands: Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let items: Vec<ListItem> =
        paired.iter().map(|device| ListItem::new(paired_device_label(device)).with_key(ListItemKey::from(device.addr))).collect();
    let paired_for_activate = paired;
    let list = VerticalList::new(items).on_activate_index(Verb::Select, move |index| {
        if let Some(device) = paired_for_activate.get(index) {
            let addr = device.addr;
            let label = paired_device_label(device);
            let commands = Rc::clone(&commands);
            return Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)));
        }
        Action::None
    });
    Screen::new(FORGET_PICKER_TITLE, vec![Box::new(list)])
}

/// The forget-confirmation screen -- destructive-action confirm, per
/// [`ConfirmView`]'s own precedent (the Devices X action, and the
/// pick-one-to-forget flow above). Cancel is row 0 (the safe default);
/// Forget is row 1, styled in [`palette::STATUS_ERROR`], and is the only
/// row that queues [`Command::ForgetDevice`] -- `core` does not remove
/// `addr` from [`BtModel::paired`] itself (see that command's doc comment
/// for the single-writer rule this preserves).
const FORGET_CONFIRM_TITLE: &str = "Forget device?";

pub(in crate::app) fn build_forget_confirm_screen(addr: DeviceAddr, label: &str, commands: Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let headline = format!("Forget {label}?");
    let rows = vec![MenuItem::new("Cancel"), MenuItem::new("Forget").with_label_color(palette::STATUS_ERROR)];
    let view = ConfirmView::new(headline, rows).on_activate_index(Verb::Select, move |index| {
        if index == 1 {
            commands.borrow_mut().push_back(Command::ForgetDevice { addr });
        }
        Action::PopView
    });
    Screen::new(FORGET_CONFIRM_TITLE, vec![Box::new(view)])
}

#[cfg(test)]
mod tests {
    use crate::app::test_support::{open_devices, upsert};
    use crate::app::{App, Event, LinkState};
    use crate::input::NavIntent;

    use super::*;

    // --- Selection carried by identity, not index ---
    //
    // These prove the selection resolves to the *same device address* (not
    // just the same index), across three cases: reordering by a fresh
    // `mru_seq`, an update-in-place rename, and a forgotten device
    // vanishing.

    #[test]
    fn selecting_a_paired_device_survives_reordering_identified_by_address_not_just_index() {
        let mut app = App::new(240, 240);
        let device_a = [1, 1, 1, 1, 1, 1];
        app.handle_event(upsert(device_a, "Device A", 1));
        open_devices(&mut app);

        // Devices rows: 0 = Device A, 1 = "Pair new headphones" -- default
        // selection starts at row 0, already Device A.
        assert_eq!(app.devices_selected_index_for_test(), Some(0));
        assert_eq!(app.model().paired[0].addr, device_a);

        // Two more devices, with HIGHER mru_seq, push Device A down the list.
        app.handle_event(upsert([2, 2, 2, 2, 2, 2], "Device B", 5));
        app.handle_event(upsert([3, 3, 3, 3, 3, 3], "Device C", 6));

        // Rows now: 0 = C (6), 1 = B (5), 2 = A (1), 3 = Pair new.
        let selected = app.devices_selected_index_for_test().expect("a device must still be selected");
        assert_eq!(selected, 2, "the selection must follow Device A's address even though it moved rows");
    }

    #[test]
    fn a_paired_device_upserted_for_a_known_addr_updates_its_row_in_place() {
        let mut app = App::new(240, 240);
        let addr = [7, 7, 7, 7, 7, 7];
        app.handle_event(upsert(addr, "", 1)); // nameless first report
        open_devices(&mut app);

        assert_eq!(app.devices_selected_index_for_test(), Some(0));

        // The name resolves later, same address, higher mru_seq.
        app.handle_event(upsert(addr, "Sony WH-1000XM5", 2));

        assert_eq!(app.model().paired.len(), 1, "a re-upsert for a known addr must update the existing row, not append a second one");
        assert_eq!(app.model().paired[0].name, "Sony WH-1000XM5");
        assert_eq!(app.devices_selected_index_for_test(), Some(0), "the update must not disturb the selection");
    }

    #[test]
    fn forgetting_the_selected_paired_device_clamps_selection_instead_of_resetting_to_row_zero() {
        let mut app = App::new(240, 240);
        let addr_a = [1, 1, 1, 1, 1, 1];
        let addr_b = [2, 2, 2, 2, 2, 2];
        app.handle_event(upsert(addr_a, "Device A", 2));
        app.handle_event(upsert(addr_b, "Device B", 1));
        open_devices(&mut app);

        // Rows: 0 = A, 1 = B, 2 = Pair new. Select B.
        app.handle_input(vec![NavIntent::Down]);
        assert_eq!(app.devices_selected_index_for_test(), Some(1));

        app.handle_event(Event::PairedDeviceForgotten { addr: addr_b });

        // Only Device A and "Pair new headphones" remain (rows 0 and 1);
        // B's key is gone, so `with_selected_identity` falls back to
        // clamping the previous index (1) into the new list's bounds --
        // landing on row 1, not snapping back past it to row 0.
        assert_eq!(
            app.devices_selected_index_for_test(),
            Some(1),
            "losing the selected row must clamp to the nearest surviving row, not reset to row 0"
        );
        assert!(app.model().paired.iter().all(|d| d.addr != addr_b), "the forgotten device must be gone from the model");
    }

    // --- The Devices screen's own behaviors ---

    #[test]
    fn the_connected_row_pins_first_and_a_pushes_its_device_detail_screen() {
        let mut app = App::new(240, 240);
        let addr = [4, 4, 4, 4, 4, 4];
        app.handle_event(upsert(addr, "Connected Cans", 1));
        open_devices(&mut app);

        app.handle_event(Event::LinkStateChanged(LinkState::Connected));
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice
        // C echoes the upsert once the persist write actually lands --
        // this is what actually refreshes the Devices screen's pinned row.
        app.handle_event(upsert(addr, "Connected Cans", 2));

        // The only paired device, now connected, pins at row 0; "Pair new
        // headphones" is row 1.
        assert_eq!(app.devices_selected_index_for_test(), Some(0));
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.current_screen_title(), "Connected Cans", "A on the connected row must push its device-detail screen");
    }

    #[test]
    fn selecting_a_paired_non_connected_row_queues_connect_and_pushes_the_wizard_at_connecting() {
        let mut app = App::new(240, 240);
        let addr = [5, 5, 5, 5, 5, 5];
        app.handle_event(upsert(addr, "Headphones", 1));
        open_devices(&mut app);

        app.handle_input(vec![NavIntent::Select]); // row 0: the sole paired (not-connected) device
        assert_eq!(app.navigator_depth(), 3, "selecting a paired row must push the wizard");
        assert!(
            matches!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr: a, step: ConnectStep::Connecting, .. } if a == addr),
            "the wizard must enter straight into Connecting for this device"
        );
        assert_eq!(app.poll_command(), Some(Command::Connect { addr, name: String::from("Headphones") }));
    }

    #[test]
    fn pair_new_headphones_opens_the_wizard_when_under_capacity() {
        let mut app = App::new(240, 240);
        open_devices(&mut app); // no paired devices -- the sole row is "Pair new headphones"

        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.navigator_depth(), 3, "under capacity, Pair new headphones must open the wizard");
        assert_eq!(app.poll_command(), Some(Command::StartScan));
    }

    #[test]
    fn pair_new_headphones_opens_the_forget_picker_when_at_capacity() {
        let mut app = App::new(240, 240);
        for i in 0..8u8 {
            app.handle_event(upsert([i; 6], "Device", u32::from(i)));
        }
        open_devices(&mut app);

        // Rows: 8 paired devices (MRU-descending) then "Pair new
        // headphones" at index 8 -- gated on capacity *before* any radio
        // work.
        app.handle_input(vec![
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
            NavIntent::Down,
        ]);
        assert_eq!(app.devices_selected_index_for_test(), Some(8));

        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.navigator_depth(), 3, "at capacity, Pair new headphones must open a picker screen, not the wizard");
        assert_eq!(app.poll_command(), None, "no StartScan should be queued when gated by capacity");
    }

    #[test]
    fn x_on_a_paired_row_opens_forget_confirm_and_confirming_queues_forget_device() {
        let mut app = App::new(240, 240);
        let addr = [6, 6, 6, 6, 6, 6];
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.navigator_depth(), 3, "X on a paired row must push the forget-confirm screen");

        // Cancel is the default selection (row 0, the safe default per
        // `ConfirmView`'s own precedent) -- Forget is row 1.
        app.handle_input(vec![NavIntent::Down]);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::ForgetDevice { addr }));
        assert_eq!(app.navigator_depth(), 2, "confirming must pop back to Devices");
    }

    #[test]
    fn x_on_the_pair_new_row_does_nothing() {
        let mut app = App::new(240, 240);
        open_devices(&mut app); // no paired devices -- the sole row is "Pair new headphones"

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.navigator_depth(), 2, "X on the fixed Pair-new row must be a no-op");
    }

    #[test]
    fn a_nameless_paired_device_renders_the_unknown_device_fallback_label() {
        let device = PairedDevice { addr: [0xAA, 0xBB, 0xCC, 0x11, 0x22, 0x33], name: String::new(), mru_seq: 1, ldac_quality: 0 };
        assert_eq!(paired_device_label(&device), "(unknown device) 11:22:33");
    }
}
