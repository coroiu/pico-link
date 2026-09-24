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
    Action, ButtonLabel, ChromeContribution, ConfirmView, FocusEvent, FrameBuffer565, ListItem, ListItemKey, MenuItem, PaintKey, RenderCtx, Screen,
    Verb, VerticalList, Widget,
};

use super::device_page::build_device_page_screen;
use super::super::model::MAX_PAIRED_DEVICES;
use super::super::{truncate_device_name, BtModel, Command, ConnectStep, DeviceAddr, ModelHandle, PairedDevice, ScreenId, WizardPhase};

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

/// Seed for [`DevicesListView`]'s own projection key -- see
/// [`DevicesListView::projection_key`]'s doc comment. Unrelated to any
/// widget's [`PaintKey`]-the-paint-key -- this is a private "did the fields
/// I read from the model change" hash, not anything compared against a
/// previous frame's drawn pixels.
const DEVICES_PROJECTION_SEED: u64 = 41;

/// Builds the devices screen exactly once per push -- `App::new` never
/// calls this (Devices isn't the root), but once pushed (from Home's
/// Bluetooth row, `render::home`'s menu closure) the resulting
/// [`DevicesListView`] is long-lived for as long as Devices stays on the
/// navigator's stack (bead `pico-link-bgnd` M2): a Bluetooth event no
/// longer rebuilds this screen -- `DevicesListView::sync` re-reads the live
/// model itself every frame Devices is on top of the stack instead (same
/// shape as `render::home`'s M1).
///
/// The connected device (if any) is pinned first, sublabelled `Connected`;
/// then every other paired device, MRU-descending, sublabelled `Paired`;
/// then `Pair new headphones` last. **No RSSI, no address, no availability
/// dot** -- never claim availability that hasn't been verified, and the
/// recurring "switch device" job belongs under the cursor while the rare
/// "pair a new one" job belongs at the end.
///
/// Every row carries a [`ListItemKey`] (`device.addr` via `From<[u8; 6]>`,
/// or [`PAIR_NEW_ROW_KEY`] for the fixed last row) so `prev_key`/
/// `prev_index` can carry the user's selection forward **by identity** --
/// see [`VerticalList::with_selected_identity`]'s doc comment for the full
/// rule this still uses for the widget's *initial* construction (a fresh
/// push, e.g. after a pop-and-repush, still wants that carry; every
/// subsequent in-place update goes through [`VerticalList::set_items`]'s
/// own identical rule instead).
pub(crate) fn build_devices_screen(
    model: &ModelHandle,
    prev_key: Option<ListItemKey>,
    prev_index: usize,
    prev_scroll_top: Option<usize>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    wizard_phase: &Rc<RefCell<WizardPhase>>,
) -> Screen {
    let (items, projection_key) = {
        let snapshot = model.borrow();
        (DevicesListView::build_items(&snapshot), DevicesListView::projection_key(&snapshot))
    };

    let model_for_activate = Rc::clone(model);
    let commands_for_activate = Rc::clone(commands);
    let wizard_phase_for_activate = Rc::clone(wizard_phase);
    let list = VerticalList::new(items)
        // The `Verb::Open` here is only the list's fallback default; every
        // row above carries its own override, so this value is never
        // actually read.
        .on_activate_key(Verb::Open, move |key| {
            if key == PAIR_NEW_ROW_KEY {
                // "Pair new headphones", the fixed last row. Gated on
                // capacity *before* any radio work: under the cap, open
                // the wizard exactly as before; at the cap, open the
                // pick-one-to-forget flow instead. Read fresh from the
                // live model at press time, not a count captured when
                // this list was last built.
                let (under_capacity, paired_for_full) = {
                    let model = model_for_activate.borrow();
                    (model.paired.len() < MAX_PAIRED_DEVICES, model.paired.clone())
                };
                if under_capacity {
                    *wizard_phase_for_activate.borrow_mut() = WizardPhase::scanning_pending();
                    // Clear `model.discovered` proactively -- see
                    // `PairingWizardView::on_focus`'s identical rationale
                    // for the `NothingFound` re-scan case.
                    model_for_activate.borrow_mut().discovered.clear();
                    commands_for_activate.borrow_mut().push_back(Command::StartScan);
                    let phase = Rc::clone(&wizard_phase_for_activate);
                    let model = Rc::clone(&model_for_activate);
                    let commands = Rc::clone(&commands_for_activate);
                    return Action::PushView(Box::new(move || build_wizard_screen(phase, model, commands)));
                }
                let commands = Rc::clone(&commands_for_activate);
                return Action::PushView(Box::new(move || build_forget_picker_screen(paired_for_full, commands)));
            }
            // A real device row: resolve the key back to a live
            // `PairedDevice` against the model at press time -- this is
            // the whole point of `on_activate_key` over the old index-
            // based callback (see its own doc comment). A key that no
            // longer resolves (the device was forgotten between the last
            // render and this press) is a real but narrow race; `Action::
            // None` is the harmless fallback rather than a panic, same
            // shape every other "structurally rare, not unreachable" arm
            // in this crate uses.
            let (device, connected_addr) = {
                let model = model_for_activate.borrow();
                (model.paired.iter().find(|d| ListItemKey::from(d.addr) == key).cloned(), model.connected_addr)
            };
            let Some(device) = device else {
                return Action::None;
            };
            if Some(device.addr) == connected_addr {
                // A on the connected row: no reconnect to do -- push the
                // real device page.
                let addr = device.addr;
                let fallback_title = paired_device_label(&device);
                let model = Rc::clone(&model_for_activate);
                let commands = Rc::clone(&commands_for_activate);
                return Action::PushView(Box::new(move || {
                    // The connected device we just resolved cannot have
                    // vanished between that read and this closure running
                    // on the very same press -- structurally unreachable,
                    // but a same-titled empty screen is a harmless
                    // fallback rather than a panic if it ever is.
                    build_device_page_screen(&model, addr, &commands).unwrap_or_else(|| Screen::new(fallback_title, vec![]))
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
            let model = Rc::clone(&model_for_activate);
            let commands = Rc::clone(&commands_for_activate);
            Action::PushView(Box::new(move || build_wizard_screen(phase, model, commands)))
        })
        .with_selected_identity(prev_key, prev_index);
    let list = if let Some(top) = prev_scroll_top { list.with_scroll_top(top) } else { list };

    let view = DevicesListView { list, model: Rc::clone(model), commands: Rc::clone(commands), projection_key };
    Screen::new(DEVICES_TITLE, vec![Box::new(view)]).with_id(ScreenId::Devices)
}

/// Wraps [`VerticalList`] to add the Devices screen's X action ("X opens
/// a forget confirm") on top of it -- `VerticalList` itself has no
/// opinion about `ShortcutX` (see `crate::input::NavIntent::
/// ShortcutX`'s doc comment), so this is the same "small wrapper widget
/// intercepts one `NavIntent` variant, delegates the rest" shape
/// `crate::render::wizard::PairingWizardView` already uses for its own
/// phase-specific `ShortcutX` handling.
///
/// As of bead `pico-link-bgnd` M2, long-lived for as long as Devices stays
/// on the navigator's stack -- [`Widget::sync`] re-projects `list`'s rows
/// from `model` **in place** via [`VerticalList::set_items`] every frame
/// Devices is on top of the stack, instead of [`build_devices_screen`]
/// being re-invoked on every model change (contrast the module's own
/// pre-M2 doc history, and see `render::home`'s M1 for the identical
/// shape on the root screen). `ShortcutX` and `chrome_contribution` below
/// resolve the selected row back to a live [`PairedDevice`] by
/// [`ListItemKey`] identity against `model` at the moment they run,
/// rather than from a `row_devices` snapshot parallel to `list` -- the
/// same "resolve by key against the live model, not a captured index"
/// rule `on_activate_key`'s own doc comment states.
struct DevicesListView {
    list: VerticalList,
    /// The live model handle, read fresh by [`Self::sync`] every frame and
    /// by `on_intent`'s `ShortcutX` arm at press time -- see
    /// [`crate::app::ModelHandle`]'s doc comment for the borrow rule.
    model: ModelHandle,
    commands: Rc<RefCell<VecDeque<Command>>>,
    /// The last [`PaintKey`]-shaped hash of every model field this view
    /// reads (`model.connected_addr` plus each paired device's `addr`/
    /// `name`/`mru_seq`) -- see [`Self::projection_key`]'s own doc comment.
    /// [`Self::sync`] recomputes this every frame and only calls
    /// [`VerticalList::set_items`] when it changed: an allocation-saving
    /// skip, not a correctness dependency -- `set_items` is itself
    /// selection-preserving, so calling it unconditionally would still be
    /// correct, just wasteful on every one of the ~20 frames/s while
    /// streaming that carry no Devices-relevant change at all.
    projection_key: PaintKey,
}

impl DevicesListView {
    /// Builds this screen's rows from a live `&BtModel` read: the connected
    /// device (if any) pinned first, sublabelled `Connected`; then every
    /// other paired device, MRU-descending, sublabelled `Paired`; then
    /// `Pair new headphones` last. See [`build_devices_screen`]'s doc
    /// comment for the product rule this implements. Shared between the
    /// screen's initial construction and every subsequent [`Self::sync`]
    /// call, so the two can never drift into building rows two different
    /// ways.
    fn build_items(model: &BtModel) -> Vec<ListItem> {
        let connected = model.connected_addr.and_then(|addr| model.paired.iter().find(|d| d.addr == addr));
        let mut others: Vec<&PairedDevice> =
            model.paired.iter().filter(|d| Some(d.addr) != model.connected_addr).collect();
        others.sort_by_key(|d| core::cmp::Reverse(d.mru_seq));

        let mut ordered: Vec<&PairedDevice> = Vec::with_capacity(model.paired.len());
        if let Some(device) = connected {
            ordered.push(device);
        }
        ordered.extend(others);

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
        // `Verb::Pair`: begins pairing -- including at the 8-device cap,
        // where the forget-picker is the app making room, not a different
        // intent.
        items.push(ListItem::new("Pair new headphones").with_key(PAIR_NEW_ROW_KEY).with_verb(Verb::Pair));
        items
    }

    /// A cheap, total summary of every field [`Self::build_items`] reads
    /// from `model` -- `model.connected_addr` plus each paired device's
    /// `addr`/`name`/`mru_seq`, in `model.paired`'s own storage order
    /// (stable between events; MRU re-sorting only happens inside
    /// `build_items` itself, not to the underlying `Vec`). Reuses
    /// [`PaintKey`]'s fold mechanism purely as a cheap allocation-free
    /// hash accumulator -- this is a **projection key** (design
    /// `.planning/design/2026-09-24-live-widgets-retire-refresh-stack.md`
    /// §5's R2 vocabulary: "everything the view READS"), never compared
    /// against anything a widget draws, and never itself returned from
    /// [`Widget::paint_key`]/[`Widget::damage_region_key`] -- folding it in
    /// there would be exactly R2's "fold a projection key into a paint
    /// key" mistake this design explicitly forbids.
    fn projection_key(model: &BtModel) -> PaintKey {
        let mut key = PaintKey::of(DEVICES_PROJECTION_SEED).fold(u64::from(model.connected_addr.is_some()));
        if let Some(addr) = model.connected_addr {
            for byte in addr {
                key = key.fold(u64::from(byte));
            }
        }
        key = key.fold(model.paired.len() as u64);
        for device in &model.paired {
            for byte in device.addr {
                key = key.fold(u64::from(byte));
            }
            key = key.fold_str(&device.name);
            key = key.fold(u64::from(device.mru_seq));
        }
        key
    }

    /// Resolves the selected row's [`ListItemKey`] back to a live
    /// [`PairedDevice`] against `model` -- shared by `on_intent`'s
    /// `ShortcutX` arm and `chrome_contribution` below, both of which need
    /// "is a real (forgettable) device row selected, and if so which one"
    /// resolved the same way. `None` for the fixed "Pair new headphones"
    /// row (whose key is [`PAIR_NEW_ROW_KEY`], never a real device's) or
    /// for a key that no longer resolves (a narrow forget-race, same
    /// "structurally rare, not unreachable" shape [`build_devices_screen`]'s
    /// activation closure documents).
    fn selected_device(&self) -> Option<PairedDevice> {
        let key = self.list.selected_key()?;
        if key == PAIR_NEW_ROW_KEY {
            return None;
        }
        self.model.borrow().paired.iter().find(|d| ListItemKey::from(d.addr) == key).cloned()
    }
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

    /// Re-reads the live model and updates `list`'s rows **in place** via
    /// [`VerticalList::set_items`] whenever [`Self::projection_key`]
    /// changed since the last call -- see this struct's own doc comment
    /// and [`Self::projection_key`]'s for the skip-is-an-optimization-not-
    /// a-correctness-dependency rule. Called once per frame Devices is on
    /// top of the navigator's stack, before `render`/`on_intent` (see
    /// `Navigator::sync_top`'s doc comment).
    fn sync(&mut self, _ctx: &RenderCtx) {
        let model = self.model.borrow();
        let key = Self::projection_key(&model);
        if key != self.projection_key {
            let items = Self::build_items(&model);
            drop(model);
            self.list.set_items(items);
            self.projection_key = key;
        }
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        if intent == NavIntent::ShortcutX {
            return match self.selected_device() {
                Some(device) => {
                    let addr = device.addr;
                    let label = paired_device_label(&device);
                    let commands = Rc::clone(&self.commands);
                    Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)))
                }
                // Either the fixed "Pair new headphones" row (nothing to
                // forget) or a vanished-between-frames key (see
                // `Self::selected_device`'s doc comment).
                None => Action::None,
            };
        }
        self.list.on_intent(intent)
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let x = if self.selected_device().is_some() { ButtonLabel::Live(String::from("forget")) } else { ButtonLabel::Inert };
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
        // Bead `pico-link-bgnd` M2: `DevicesListView` is long-lived and only
        // re-projects its rows from the model at `Widget::sync` time (see
        // `Navigator::sync_top`'s doc comment) -- a bare `handle_event` with
        // no following `render`/`handle_input` leaves it stale by design, so
        // a render is needed here to observe the reordering.
        app.render();

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
        app.render(); // bead pico-link-bgnd M2: sync runs at render time, not fold time.

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
        app.render(); // bead pico-link-bgnd M2: sync runs at render time, not fold time.

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

    // --- pico-link-bgnd M2: Devices is long-lived, not rebuilt ---

    /// The design's T3 shape (`.planning/design/2026-09-24-live-widgets-
    /// retire-refresh-stack.md`, mirroring `pico-link-bgnd` M1's
    /// `home_level_event_does_not_damage_the_whole_frame_or_the_title_bar`
    /// in `crate::app::tests`): before this bead, `ScreenId::Devices`
    /// returned `Refresh::Rebuild`, which always went through `Navigator::
    /// replace_at` and therefore always forced `force_full_damage` -- a
    /// device's name resolving would repaint the ENTIRE frame, title bar
    /// included, even though nothing about the title bar changed. Fails on
    /// pre-M2 `main`; passes once `ScreenId::Devices` returns `Refresh::
    /// Keep` and `DevicesListView::sync` updates `list` in place.
    #[test]
    fn a_paired_device_rename_does_not_force_a_whole_frame_repaint() {
        use embedded_graphics::prelude::{OriginDimensions, Point};
        use embedded_graphics::primitives::Rectangle;

        let mut app = App::new(240, 240);
        let addr = [11, 11, 11, 11, 11, 11];
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        // Establish a clean baseline -- the very first render of a freshly
        // pushed screen is always a full-frame cache miss (`Screen::
        // render`'s `cache_miss` branch), so it proves nothing about the
        // rename event's own damage on its own.
        app.render();

        app.handle_event(upsert(addr, "Renamed Cans", 2));
        let output = app.render();
        let whole_frame = Rectangle::new(Point::zero(), output.size());

        assert_ne!(
            output.damage, whole_frame,
            "a paired-device rename must not force the whole frame to repaint -- Devices is being rebuilt, not kept live"
        );
        assert!(
            output.damage.top_left.y >= i32::try_from(crate::render::chrome::TITLE_BAR_HEIGHT).expect("TITLE_BAR_HEIGHT is a small constant, fits in i32"),
            "the title bar must not repaint on a Devices content change: damage={:?}",
            output.damage
        );
    }

    /// Proves the list actually reflects a live model change (the
    /// complement to the damage test above, which only proves the SCOPE of
    /// the repaint, not that anything repainted at all): a device's label
    /// updates on screen without the screen having been popped/re-pushed.
    #[test]
    fn a_paired_device_rename_is_reflected_in_the_rendered_label() {
        use crate::render::theme::palette;

        let mut app = App::new(240, 240);
        let addr = [12, 12, 12, 12, 12, 12];
        app.handle_event(upsert(addr, "Old Name", 1));
        open_devices(&mut app);
        app.render();

        app.handle_event(upsert(addr, "New Name", 2));
        let output = app.render();
        // A crude but real pixel-presence check, same technique
        // `test_support::assert_home_hero_renders_connected` uses: the row
        // is selected/focused, so it paints in the selection-fill ink, not
        // `TEXT_PRIMARY` -- confirm some non-background ink is present in
        // the damaged region rather than asserting a specific glyph.
        assert!(
            output.pixels().any(|p| p.0.y >= output.damage.top_left.y && p.1 != palette::BACKGROUND),
            "the renamed row must actually repaint something inside the damaged region"
        );
    }

    /// Proves focus/scroll survive [`VerticalList::set_items`] rather than
    /// being reset by a rebuild -- with 8 paired devices plus the fixed
    /// "Pair new headphones" row, the list must actually scroll on a 240px
    /// panel; an unrelated field resolving on a device that is NOT the
    /// selected one must not reset either the scroll position or the
    /// selected row.
    #[test]
    fn scroll_and_selection_survive_an_in_place_update_from_a_scrolled_position() {
        let mut app = App::new(240, 240);
        for i in 0..8u8 {
            app.handle_event(upsert([i; 6], "Device", u32::from(i)));
        }
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Down; 8]); // lands on the fixed "Pair new headphones" row (index 8)
        app.render();

        let scroll_before = app.devices_scroll_top_for_test().expect("a scrolled Devices list must report a scroll-top row");
        assert!(scroll_before > 0, "8 devices plus Pair-new on a 240px panel must actually need to scroll for this test to prove anything");
        assert_eq!(app.devices_selected_index_for_test(), Some(8));

        // Device 0's name resolves -- an unrelated field change, not a
        // reorder or removal.
        app.handle_event(upsert([0; 6], "Resolved Name", 0));
        app.render();

        assert_eq!(
            app.devices_scroll_top_for_test(),
            Some(scroll_before),
            "an unrelated model change must not reset the scroll position"
        );
        assert_eq!(
            app.devices_selected_index_for_test(),
            Some(8),
            "an unrelated model change must not disturb the selected row"
        );
    }
}
