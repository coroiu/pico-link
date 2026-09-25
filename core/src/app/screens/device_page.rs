use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::convert::Infallible;
use core::time::Duration;

use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use u8g2_fonts::types::{HorizontalAlignment, VerticalPosition};
use u8g2_fonts::FontRenderer;

use crate::input::NavIntent;
use crate::render::theme::{self, palette};
use crate::render::{
    Action, ButtonLabel, ChromeContribution, FieldList, FieldRow, FocusEvent, FrameBuffer565, ListItemKey, PaintKey, RenderCtx, Screen, Spacer,
    Verb, Widget,
};

use crate::dsp::PresetStore;
use crate::dsp::store::NO_PRESET_ID;

use super::devices::{build_forget_confirm_screen, paired_device_label};
use super::effects::resolve_effect_name;
use super::ldac_quality::{build_ldac_quality_picker_screen, ldac_quality_fixed_kbps, LDAC_QUALITY_ADAPTIVE};
use super::picker::{build_picker_view_screen, PickerOption};
use super::super::{truncate_device_name, BtModel, Command, DeviceAddr, ModelHandle, PairedDevice, PickerKind, ScreenId};

/// A placeholder for a live value this page cannot honestly report yet --
/// `core` has no `SetDeviceCodecPref`/`CodecAvailability`/
/// `A2dpStreamStateChanged` seam. A *live* field dashes when the value is
/// unknown; a *stored* setting never dashes, because it is still true
/// when nothing is connected. `CODEC`/`ADDRESS` are the latter and never
/// use this.
///
/// Plain ASCII hyphen-minus, not a typographic em dash (`\u{2014}`):
/// `theme::font`'s `u8g2_font_helv*_tf` faces are built with
/// `with_ignore_unknown_chars(true)` and cover only the Latin-1 range, so
/// U+2014 silently draws NOTHING rather than a placeholder box -- found by
/// screenshotting this exact row (`core/examples/
/// device_page_screenshots.rs`) and seeing an empty value where the dash
/// should be. `-` is in range and renders.
const DASH: &str = "-";

/// [`ListItemKey`]s for each of [`device_page_rows`]'s fixed rows --
/// stable identities, not positions, so [`DevicePageView`]'s activation can
/// resolve "which row was pressed" the same way regardless of whether
/// `QUALITY` (the one row whose presence varies) is showing (bead
/// `pico-link-bgnd` M3: `on_activate_key`, not `on_activate_index`).
const CODEC_ROW_KEY: ListItemKey = ListItemKey::from_u64(0);
const SAMPLE_RATE_ROW_KEY: ListItemKey = ListItemKey::from_u64(1);
const USB_IN_ROW_KEY: ListItemKey = ListItemKey::from_u64(2);
const A2DP_ROW_KEY: ListItemKey = ListItemKey::from_u64(3);
const ADDRESS_ROW_KEY: ListItemKey = ListItemKey::from_u64(4);
const FORGET_ROW_KEY: ListItemKey = ListItemKey::from_u64(5);
const QUALITY_ROW_KEY: ListItemKey = ListItemKey::from_u64(6);
/// The `EFFECT` row's identity key -- bead `pico-link-ryw.7`.
const EFFECT_ROW_KEY: ListItemKey = ListItemKey::from_u64(7);

/// Whether the device page's `QUALITY` row (and its picker) should be
/// shown at all -- "the row is absent, not dim" when LDAC isn't
/// effective-or-pinned. Connected: keyed off the *live* codec (the only
/// truth available -- there is no real codec pin seam yet, same honesty
/// rule [`device_page_rows`]'s `CODEC` value already follows).
/// Disconnected: keyed off `ldac_quality != 0` -- a device that was
/// manually put in Adaptive or pinned to a rate at some point is "LDAC in
/// play" even while off; a device nobody ever touched has no such
/// evidence and stays hidden (no first-run prompt).
fn device_page_quality_present(model: &BtModel, addr: DeviceAddr) -> bool {
    if model.connected_addr == Some(addr) {
        model.connected_codec.as_ref().is_some_and(|c| c.word == "LDAC")
    } else {
        model.paired.iter().find(|d| d.addr == addr).is_some_and(|d| d.ldac_quality != 0)
    }
}

/// The device-page value-column budget (px) the `QUALITY` row's Adaptive
/// form must fit inside, against a `QUALITY` label of ~35px against the
/// row's 182px total. Measured, not assumed:
/// [`format_adaptive_row_value`] checks this at build time and falls back
/// to the no-separator form if it disagrees.
const DEVICE_PAGE_ADAPTIVE_VALUE_BUDGET_PX: u32 = 182;

/// The horizontal pixel footprint `text` would render at in `font` --
/// duplicated from `hero.rs`'s private `text_width` for the same "no
/// shared home for a helper this small, used by only one module" reason
/// that helper's own doc comment gives.
fn text_width(font: &FontRenderer, text: &str) -> u32 {
    font.get_rendered_dimensions_aligned(text, Point::zero(), VerticalPosition::Top, HorizontalAlignment::Left)
        .unwrap_or(None)
        .map_or(0, |bbox| bbox.size.width)
}

/// The `QUALITY` row's value under Adaptive while a live figure exists --
/// `Adaptive · <n>`, no `kbps` unit (it's stated everywhere else already),
/// falling back to the no-separator `Adaptive <n>` form if the middle
/// dot's measured width overruns the row's value budget.
fn format_adaptive_row_value(kbps: u32) -> String {
    let preferred = format!("Adaptive \u{b7} {kbps}");
    if text_width(&theme::font::value(), &preferred) <= DEVICE_PAGE_ADAPTIVE_VALUE_BUDGET_PX {
        preferred
    } else {
        format!("Adaptive {kbps}")
    }
}

/// The `QUALITY` row's trailing value.
fn device_page_quality_row_value(model: &BtModel, device: &PairedDevice, connected: bool) -> String {
    if device.ldac_quality == LDAC_QUALITY_ADAPTIVE {
        let streaming = connected && model.connected_codec.as_ref().is_some_and(|c| c.word == "LDAC");
        match (streaming, model.ldac_live_kbps) {
            (true, Some(kbps)) => format_adaptive_row_value(kbps),
            // Disconnected, or connected-but-no-live-reading-yet: never
            // claim a number we don't have -- the picker's own "Adaptive's
            // trailing note reads `varies`" is the same honesty rule.
            _ => String::from("Adaptive"),
        }
    } else {
        format!("{} kbps", ldac_quality_fixed_kbps(device.ldac_quality, None).unwrap_or(990))
    }
}

/// The device page's rows, in order: `CODEC`, `QUALITY` (present only
/// when LDAC is effective-or-pinned), `SAMPLE RATE`, `USB IN`, `A2DP`,
/// `ADDRESS`, `Forget this device`.
///
/// **PURE.** Model in, rows out -- no `Screen`, no `Navigator`, no
/// framebuffer, testable directly. `CODEC` is [`FieldKind::Readonly`], not
/// `Action`: there is no codec picker to open yet (Ada's
/// `CodecAvailability` seam doesn't exist), and a bright, caret-growing row
/// that does nothing on `A` would be exactly the "an unlabelled/live A
/// lies" defect. Wiring `CODEC` back to `Action` is the device-page
/// follow-up that lands alongside the codec picker. `QUALITY`, by
/// contrast, IS `Action` -- it builds the picker it opens.
fn device_page_rows(model: &BtModel, presets: &PresetStore, addr: DeviceAddr) -> Vec<FieldRow> {
    let connected = model.connected_addr == Some(addr);
    // A *stored* setting: never dashes, even disconnected. Today that's
    // only ever "the live codec, or Automatic" -- there is no real
    // pin to read yet (`PairedDevice` carries no `codec_id` field), so a
    // disconnected device always reads `Automatic`, honestly.
    let codec_value =
        if connected { model.connected_codec.as_ref().map_or_else(|| String::from("Automatic"), |c| c.word.clone()) } else { String::from("Automatic") };

    let mut rows = vec![FieldRow::readonly("CODEC").with_value(codec_value, palette::TEXT_PRIMARY).with_key(CODEC_ROW_KEY)];

    if device_page_quality_present(model, addr) {
        // `unwrap_or` fallback below only matters for the pathological
        // case of a present-but-not-actually-paired addr (never happens
        // via `build_device_page_screen`, which bails to `None` first) --
        // kept defensive since this function is pure and called directly
        // by tests with hand-built models.
        let default_device = PairedDevice { addr, name: String::new(), mru_seq: 0, ldac_quality: 0, preset_id: 0 };
        let device = model.paired.iter().find(|d| d.addr == addr).unwrap_or(&default_device);
        rows.push(
            FieldRow::action("QUALITY")
                .with_value(device_page_quality_row_value(model, device, connected), palette::TEXT_PRIMARY)
                .with_key(QUALITY_ROW_KEY),
        );
    }

    // `EFFECT` (bead `pico-link-ryw.7`, design sec 7): ALWAYS present,
    // connected or not -- a stored setting, never dashes (same rule
    // `CODEC`/`ADDRESS` already follow). Its value is the resolved effect
    // name, or `Off` -- `resolve_effect_name` already collapses "never
    // assigned" (`preset_id == 0`) and "assigned to a since-deleted
    // preset" to the same word.
    let device_preset_id =
        model.paired.iter().find(|d| d.addr == addr).map_or(NO_PRESET_ID, |d| d.preset_id);
    rows.push(
        FieldRow::action("EFFECT")
            .with_value(resolve_effect_name(presets, device_preset_id), palette::TEXT_PRIMARY)
            .with_key(EFFECT_ROW_KEY),
    );

    rows.push(FieldRow::readonly("SAMPLE RATE").with_value(DASH, palette::TEXT_SECONDARY).with_key(SAMPLE_RATE_ROW_KEY));
    rows.push(FieldRow::readonly("USB IN").with_value(DASH, palette::TEXT_SECONDARY).with_key(USB_IN_ROW_KEY));
    rows.push(FieldRow::readonly("A2DP").with_value(DASH, palette::TEXT_SECONDARY).with_key(A2DP_ROW_KEY));
    rows.push(
        FieldRow::readonly("ADDRESS")
            .with_value(format_device_address(addr), palette::TEXT_PRIMARY)
            .with_small_value()
            .with_key(ADDRESS_ROW_KEY),
    );
    rows.push(FieldRow::action("Forget this device").with_label_color(palette::STATUS_ERROR).with_key(FORGET_ROW_KEY));
    rows
}

/// Formats a device address exactly like the address a phone or laptop
/// shows for the same device -- colons kept.
fn format_device_address(addr: DeviceAddr) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]
    )
}

/// The connected-or-paired device's detail page. Returns `None` when `addr`
/// is no longer in [`BtModel::paired`] at push time. Built once per push
/// (bead `pico-link-bgnd` M3, generalising M2's `DevicesListView` shape) and
/// never rebuilt again while it stays on the navigator's stack --
/// [`DevicePageView::sync`] re-reads the live model itself every frame this
/// page is on top, and [`crate::app::App`]'s own per-fold liveness check
/// (`prune_stack`, not this function) is what unwinds the stack if `addr`
/// vanishes later.
pub(crate) fn build_device_page_screen(
    model: &ModelHandle,
    addr: DeviceAddr,
    presets: &Rc<RefCell<PresetStore>>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
) -> Option<Screen> {
    let (title, rows, projection_key) = {
        let snapshot = model.borrow();
        let device = snapshot.paired.iter().find(|d| d.addr == addr)?;
        let presets_ref = presets.borrow();
        (
            paired_device_label(device),
            device_page_rows(&snapshot, &presets_ref, addr),
            DevicePageView::projection_key(&snapshot, &presets_ref, addr),
        )
    };

    let model_for_activate = Rc::clone(model);
    let presets_for_activate = Rc::clone(presets);
    let commands_for_activate = Rc::clone(commands);
    let list = FieldList::new(rows).on_activate_key(move |key| {
        if key == QUALITY_ROW_KEY {
            let quality_present = device_page_quality_present(&model_for_activate.borrow(), addr);
            if !quality_present {
                // The row is currently gone, but a race could still deliver
                // a stale key from a frame where it was showing --
                // structurally rare, not unreachable; `Action::None` is the
                // harmless fallback rather than acting on a row that isn't
                // there.
                return Action::None;
            }
            // A fresh push -- there is nothing to carry forward into a
            // screen that has never been open before.
            let model = Rc::clone(&model_for_activate);
            let commands = Rc::clone(&commands_for_activate);
            return Action::PushView(Box::new(move || {
                // The device we just descended from cannot have vanished
                // between the press and this closure running --
                // structurally unreachable, but a same-titled empty screen
                // is a harmless fallback rather than a panic if it ever is.
                build_ldac_quality_picker_screen(&model, addr, &commands).unwrap_or_else(|| Screen::new("Quality", vec![]))
            }));
        }
        if key == FORGET_ROW_KEY {
            let label = {
                let model = model_for_activate.borrow();
                model.paired.iter().find(|d| d.addr == addr).map_or_else(|| String::from("device"), paired_device_label)
            };
            let commands = Rc::clone(&commands_for_activate);
            return Action::PushView(Box::new(move || build_forget_confirm_screen(addr, &label, commands)));
        }
        if key == EFFECT_ROW_KEY {
            let model = Rc::clone(&model_for_activate);
            let presets = Rc::clone(&presets_for_activate);
            let commands = Rc::clone(&commands_for_activate);
            return Action::PushView(Box::new(move || build_effect_picker_screen(&model, &presets, addr, &commands)));
        }
        Action::None
    });
    let view = DevicePageView { list, addr, model: Rc::clone(model), presets: Rc::clone(presets), commands: Rc::clone(commands), projection_key };
    Some(Screen::new(title, vec![Box::new(Spacer::new(12)), Box::new(view)]).with_id(ScreenId::DevicePage(addr)))
}

/// The device page's `EFFECT` picker -- a single-select over `Off` plus
/// every stored effect, in list order (design sec 7). No "New effect" row
/// here: creation lives in one place (the effects list), which keeps it
/// clear that effects are global.
const EFFECT_PICKER_TITLE: &str = "Effect";
const EFFECT_PICKER_OFF_KEY: ListItemKey = ListItemKey::from_u64(0);

fn build_effect_picker_screen(
    model: &ModelHandle,
    presets: &Rc<RefCell<PresetStore>>,
    addr: DeviceAddr,
    commands: &Rc<RefCell<VecDeque<Command>>>,
) -> Screen {
    let model_for_projection = Rc::clone(model);
    let presets_for_projection = Rc::clone(presets);
    let projection = move || {
        let model = model_for_projection.borrow();
        let presets = presets_for_projection.borrow();
        let mut options =
            vec![PickerOption { key: EFFECT_PICKER_OFF_KEY, label: String::from("Off"), note: None, selectable: true, locked: false }];
        for (id, preset) in presets.iter() {
            options.push(PickerOption {
                key: ListItemKey::from_u64(u64::from(id)),
                label: preset.name.clone(),
                note: None,
                selectable: true,
                // Bead `pico-link-ryw.12.4`: the picker carries the same
                // padlock as the effects list for an imported effect
                // (Uma's design, `ryw12-3-ux.md` sec 1/5).
                locked: preset.eq_locked,
            });
        }
        let current_id = model.paired.iter().find(|d| d.addr == addr).map_or(NO_PRESET_ID, |d| d.preset_id);
        let checked = if presets.resolve(current_id).is_some() { ListItemKey::from_u64(u64::from(current_id)) } else { EFFECT_PICKER_OFF_KEY };
        (options, Some(checked))
    };
    let commands_for_pick = Rc::clone(commands);
    let on_pick = move |key: ListItemKey| {
        let preset_id = if key == EFFECT_PICKER_OFF_KEY { NO_PRESET_ID } else { u16::try_from(key.as_u64()).unwrap_or(NO_PRESET_ID) };
        commands_for_pick.borrow_mut().push_back(Command::AssignPreset { addr, preset_id });
        // Applies live, no confirm, the picker stays open -- the check
        // follows the `PairedDeviceUpserted` echo, never the local press
        // (same discipline as the LDAC quality picker's own `on_pick`).
        Action::None
    };
    build_picker_view_screen(ScreenId::Picker(PickerKind::Effect, addr), EFFECT_PICKER_TITLE, projection, on_pick)
}

/// Wraps [`FieldList`] to add the device page's `X` action (`drop` when
/// connected, `link` when not -- both labelled, both real, no confirm
/// needed since neither is destructive/irreversible) -- the same "small
/// wrapper widget intercepts one `NavIntent` variant, delegates the rest"
/// shape [`DevicesListView`] already uses for its own `ShortcutX`
/// handling, and every method below that isn't `sync`/`on_intent`/
/// `chrome_contribution` is a forward, not an override -- see
/// [`Widget::activation`]'s doc comment for why a wrapper must forward
/// rather than let the default silently swallow one.
///
/// Long-lived for as long as this page stays on the navigator's stack (bead
/// `pico-link-bgnd` M3) -- holds a [`ModelHandle`], not a `connected`/`name`
/// snapshot: `on_intent`'s `ShortcutX` arm and `chrome_contribution` both
/// read the model fresh at the moment they run, and [`Self::sync`] re-reads
/// it every frame to keep `list`'s rows current, the same "resolve against
/// the live model at press/sync time, never a captured snapshot" rule
/// `DevicesListView` already follows.
struct DevicePageView {
    list: FieldList,
    addr: DeviceAddr,
    /// See [`crate::app::ModelHandle`]'s doc comment for the borrow rule.
    model: ModelHandle,
    /// The live DSP effects preset store -- bead `pico-link-ryw.7`, for
    /// the `EFFECT` row's resolved name (see [`resolve_effect_name`]).
    presets: Rc<RefCell<PresetStore>>,
    commands: Rc<RefCell<VecDeque<Command>>>,
    /// The last [`Self::projection_key`]-shaped hash of every model field
    /// [`device_page_rows`] reads for `addr` -- see that method's own doc
    /// comment. [`Self::sync`] recomputes this every frame and only calls
    /// [`FieldList::set_rows`] when it changed (allocation-saving skip, not
    /// a correctness dependency).
    projection_key: PaintKey,
}

/// Seed for [`DevicePageView::paint_key`] -- only needs to differ from
/// other widgets' own seeds.
const DEVICE_PAGE_PAINT_KEY_SEED: u64 = 15;

/// Seed for [`DevicePageView::projection_key`] -- deliberately a different
/// constant than [`DEVICE_PAGE_PAINT_KEY_SEED`] even though nothing requires
/// it: a projection key and a paint key are never compared against each
/// other, but keeping their seeds distinct is the same defensive habit
/// `devices.rs`'s `DEVICES_PROJECTION_SEED` follows.
const DEVICE_PAGE_PROJECTION_SEED: u64 = 16;

impl DevicePageView {
    /// A cheap, total summary of every field [`device_page_rows`] reads for
    /// `addr` -- connectedness, the live codec word while connected, the
    /// paired device's name/`ldac_quality`, and the live LDAC kbps reading.
    /// Reused `PaintKey` purely as an allocation-free hash accumulator, same
    /// shape as `DevicesListView::projection_key` (see that method's own
    /// doc comment for the full "projection key, never a paint key" rule
    /// this follows).
    fn projection_key(model: &BtModel, presets: &PresetStore, addr: DeviceAddr) -> PaintKey {
        let connected = model.connected_addr == Some(addr);
        let mut key = PaintKey::of(DEVICE_PAGE_PROJECTION_SEED).fold(u64::from(connected));
        key = key.fold_opt_str(if connected { model.connected_codec.as_ref().map(|c| c.word.as_str()) } else { None });
        let device_preset_id = model.paired.iter().find(|d| d.addr == addr).map(|device| {
            key = key.fold_str(&device.name);
            key = key.fold(u64::from(device.ldac_quality));
            device.preset_id
        });
        key = key.fold(u64::from(model.ldac_live_kbps.unwrap_or(0)));
        // The `EFFECT` row's resolved name -- folds the whole store's
        // shape (via `resolve_effect_name`), not just `device_preset_id`,
        // so a rename of the assigned effect (done from the effects list
        // while this page sits underneath it on the stack) is also caught.
        key = key.fold_str(&resolve_effect_name(presets, device_preset_id.unwrap_or(NO_PRESET_ID)));
        key
    }
}

impl Widget for DevicePageView {
    fn measure(&self, constraints: Size, ctx: &RenderCtx) -> Size {
        self.list.measure(constraints, ctx)
    }

    fn is_focusable(&self) -> bool {
        self.list.is_focusable()
    }

    /// Forwards `list`'s own answer -- see this struct's doc comment.
    fn activation(&self) -> Option<Verb> {
        self.list.activation()
    }

    /// Re-reads the live model and updates `list`'s rows **in place** via
    /// [`FieldList::set_rows`] whenever [`Self::projection_key`] changed
    /// since the last call -- see this struct's own doc comment.
    fn sync(&mut self, _ctx: &RenderCtx) {
        let model = self.model.borrow();
        let presets = self.presets.borrow();
        let key = Self::projection_key(&model, &presets, self.addr);
        if key != self.projection_key {
            let rows = device_page_rows(&model, &presets, self.addr);
            drop(presets);
            drop(model);
            self.list.set_rows(rows);
            self.projection_key = key;
        }
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        self.list.on_focus(event)
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        if intent == NavIntent::ShortcutX {
            let (connected, name) = {
                let model = self.model.borrow();
                (model.connected_addr == Some(self.addr), model.paired.iter().find(|d| d.addr == self.addr).map(|d| d.name.clone()).unwrap_or_default())
            };
            if connected {
                self.commands.borrow_mut().push_back(Command::Disconnect);
            } else {
                self.commands.borrow_mut().push_back(Command::Connect { addr: self.addr, name: truncate_device_name(&name) });
            }
            return Action::None;
        }
        self.list.on_intent(intent)
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let connected = self.model.borrow().connected_addr == Some(self.addr);
        let label = if connected { "drop" } else { "link" };
        Some(ChromeContribution { x: Some(ButtonLabel::Live(String::from(label))), ..ChromeContribution::default() })
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

    /// Folds `list`'s own key and nothing else -- deliberately does NOT
    /// fold `connected`/`LinkState`/the X label: the rail has its own key
    /// fed by the already-resolved `ButtonLabel`s (`chrome_contribution` above),
    /// so folding link state into the *body* key would repaint the body on
    /// every link-state change for zero changed body pixels -- exactly the
    /// "fold something you don't draw" defect this project has already
    /// shipped once.
    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        PaintKey::of(DEVICE_PAGE_PAINT_KEY_SEED).fold_key(self.list.paint_key(ctx))
    }
}

#[cfg(test)]
mod tests {
    use alloc::rc::Rc;
    use alloc::string::String;
    use core::cell::RefCell;

    use embedded_graphics::prelude::RgbColor;

    use crate::app::test_support::{open_devices, upsert, upsert_with_quality};
    use crate::app::{App, ConnectedCodec, Event, LinkState};
    use crate::input::NavIntent;
    use crate::render::FieldList;

    use super::*;

    // --- device_page_rows (pure) ---

    #[test]
    fn device_page_rows_shows_the_live_codec_when_connected() {
        let mut model = BtModel::default();
        let addr = [1; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });

        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        assert_eq!(rows[0].label, "CODEC");
        assert_eq!(rows[0].value(), Some("LDAC"), "a connected device must show its live codec, not Automatic");
    }

    #[test]
    fn device_page_rows_shows_automatic_when_disconnected_and_never_dashes_the_codec() {
        let mut model = BtModel::default();
        let addr = [2; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
        // Not connected: `connected_addr` stays `None`.

        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        assert_eq!(rows[0].value(), Some("Automatic"), "CODEC is a stored-setting-shaped row: it never dashes (design §3.0)");
    }

    #[test]
    fn device_page_rows_dashes_the_three_unimplemented_live_fields() {
        let model = BtModel::default();
        let addr = [3; 6];
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        for label in ["SAMPLE RATE", "USB IN", "A2DP"] {
            let row = rows.iter().find(|r| r.label == label).unwrap_or_else(|| panic!("missing row {label}"));
            assert_eq!(row.value(), Some(DASH), "{label} has no seam yet and must dash honestly, not fake a value");
        }
    }

    #[test]
    fn device_page_rows_address_row_renders_colon_separated_hex() {
        let model = BtModel::default();
        let addr = [0x94, 0xDB, 0x56, 0x54, 0x7C, 0xF2];
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let address_row = rows.iter().find(|r| r.label == "ADDRESS").expect("ADDRESS row must exist");
        assert_eq!(address_row.value(), Some("94:DB:56:54:7C:F2"));
    }

    #[test]
    fn device_page_rows_forget_is_the_only_pressable_row_and_sits_last() {
        let model = BtModel::default();
        let addr = [4; 6];
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row_count = rows.len();
        assert_eq!(rows.last().expect("device page must have at least one row").label, "Forget this device");

        // Black-box, per this crate's own `Navigator`/`FieldList` test
        // convention (selection/kind state lives inside the widget, not
        // exposed on `FieldRow` directly): CODEC/SAMPLE RATE/USB IN/A2DP/
        // ADDRESS must be `Readonly` (A does nothing); `EFFECT` (bead
        // `pico-link-ryw.7`) and the last row (Forget) are the only
        // pressable rows -- no codec picker to open yet (see
        // `device_page_rows`'s doc comment).
        let mut list = FieldList::new(rows);
        for i in 0..row_count - 1 {
            let is_effect_row = list.selected_key() == Some(EFFECT_ROW_KEY);
            if is_effect_row {
                assert_eq!(list.activation(), Some(Verb::Open), "EFFECT must be pressable (row {i})");
            } else {
                assert_eq!(list.activation(), None, "only EFFECT/Forget should be pressable on this bead's device page (row {i})");
            }
            list.on_intent(NavIntent::Down);
        }
        assert_eq!(list.activation(), Some(Verb::Open), "the focused last row (Forget) must be pressable");
    }

    // --- The device page's own screen/wrapper ---

    #[test]
    fn the_connected_devices_page_x_binding_is_drop() {
        let mut app = App::new(240, 240);
        let addr = [8; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice, queued by ConnectSucceeded
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.poll_command(), Some(Command::Disconnect), "X on a connected device's page must queue Disconnect");
    }

    #[test]
    fn a_disconnected_devices_page_x_binding_is_link_and_reconnects() {
        // Reach a disconnected device's page the only way it's wired:
        // connect once (so the page is reachable via the connected row),
        // then let the link drop while the page stays open --
        // `DevicePageView`'s live model read must flip `connected` (and
        // therefore the X binding) live, no screen rebuild involved.
        let mut app = App::new(240, 240);
        let addr = [9; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice, queued by ConnectSucceeded
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page, connected
        app.handle_event(Event::LinkStateChanged(LinkState::Idle)); // drops -- the live model read must catch up in place

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(
            app.poll_command(),
            Some(Command::Connect { addr, name: String::from("Cans") }),
            "X on a disconnected device's page must queue a reconnect, not Disconnect"
        );
    }

    #[test]
    fn device_page_scroll_and_selection_survive_a_live_refresh() {
        let mut app = App::new(240, 240);
        let addr = [10; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.handle_event(upsert(addr, "Cans", 1));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        app.handle_input(vec![NavIntent::Down, NavIntent::Down]); // focus row 2 (USB IN)

        // An unrelated model event must not reset the user's focus on the
        // page they're looking at -- `DevicePageView` is never rebuilt
        // while it stays on the stack (bead `pico-link-bgnd` M3), so there
        // is nothing to reset it.
        app.handle_event(Event::LevelsChanged { peak_l: 10, peak_r: 10, rms_l: 10, rms_r: 10 });

        assert_eq!(app.navigator.selected_index_at(2), Some(2), "focus must survive a live refresh of the page underneath it");
    }

    #[test]
    fn device_page_quality_row_absent_for_a_never_touched_device() {
        let mut model = BtModel::default();
        let addr = [20; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
        // Not connected, ldac_quality == 0 -- no first-run prompt, the
        // row simply doesn't exist yet.
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        assert!(!rows.iter().any(|r| r.label == "QUALITY"), "a never-touched, disconnected device must not show QUALITY");
    }

    #[test]
    fn device_page_quality_row_present_when_connected_and_ldac() {
        let mut model = BtModel::default();
        let addr = [21; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").expect("QUALITY must be present when the live codec is LDAC");
        assert_eq!(row.value(), Some("990 kbps"), "ldac_quality==0 (never chosen) renders the effective default, checked, per design §7");
    }

    #[test]
    fn device_page_quality_row_absent_when_connected_but_not_ldac() {
        let mut model = BtModel::default();
        let addr = [22; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE, preset_id: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("SBC"), nominal_bitrate_bps: 328_000 });
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        assert!(!rows.iter().any(|r| r.label == "QUALITY"), "a live SBC fallback must not show QUALITY even if a stale ldac_quality pick exists");
    }

    #[test]
    fn device_page_quality_row_present_while_disconnected_if_previously_chosen() {
        let mut model = BtModel::default();
        let addr = [23; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 2, preset_id: 0 });
        // Not connected -- "picking a quality while disconnected is
        // allowed", and a previously pinned device stays visible.
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").expect("a previously-pinned device must show QUALITY even while disconnected");
        assert_eq!(row.value(), Some("660 kbps"), "a stored pin never dashes, any link state (design §4.2)");
    }

    #[test]
    fn device_page_quality_row_adaptive_streaming_shows_the_live_number_with_the_middle_dot_form() {
        let mut model = BtModel::default();
        let addr = [24; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE, preset_id: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        model.ldac_live_kbps = Some(660);
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").unwrap();
        assert_eq!(row.value(), Some("Adaptive \u{b7} 660"));
    }

    #[test]
    fn device_page_quality_row_adaptive_but_no_live_reading_yet_is_plain() {
        let mut model = BtModel::default();
        let addr = [25; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE, preset_id: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        // No `ldac_live_kbps` yet -- fresh connect, before the first
        // reading arrives.
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").unwrap();
        assert_eq!(row.value(), Some("Adaptive"), "must never claim a number that hasn't actually arrived yet");
    }

    #[test]
    fn device_page_quality_row_adaptive_disconnected_is_plain() {
        let mut model = BtModel::default();
        let addr = [26; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: LDAC_QUALITY_ADAPTIVE, preset_id: 0 });
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "QUALITY").unwrap();
        assert_eq!(row.value(), Some("Adaptive"));
    }

    #[test]
    fn device_page_forget_row_stays_last_and_pressable_when_quality_is_present() {
        let mut model = BtModel::default();
        let addr = [27; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 0 });
        model.connected_addr = Some(addr);
        model.connected_codec = Some(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 });
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        assert_eq!(rows.len(), 8, "CODEC, QUALITY, EFFECT, SAMPLE RATE, USB IN, A2DP, ADDRESS, Forget");
        assert_eq!(rows.last().unwrap().label, "Forget this device");
        assert_eq!(rows[1].label, "QUALITY", "QUALITY sits directly under CODEC (design §2/§4.2)");
    }

    #[test]
    fn the_quality_pickers_check_follows_the_stored_echo_not_the_press() {
        let mut app = App::new(240, 240);
        let addr = [29; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert(addr, "Cans", 1));
        app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> device page
        app.handle_input(vec![NavIntent::Down]); // focus QUALITY
        app.handle_input(vec![NavIntent::Select]); // -> picker

        app.handle_input(vec![NavIntent::Down]); // focus "660 kbps"
        app.handle_input(vec![NavIntent::Select]); // press it
        let queued = app.poll_command();
        assert_eq!(queued, Some(Command::SetDeviceLdacQuality { addr, ldac_quality: 2 }));

        // Before the echo: the device page underneath still reads the OLD
        // value (990 kbps, the effective default) -- there is no
        // optimistic local state anywhere in this path.
        app.handle_input(vec![NavIntent::Back]); // -> device page
        let rows_before_echo = device_page_rows(&app.model(), &PresetStore::new(), addr);
        assert_eq!(rows_before_echo.iter().find(|r| r.label == "QUALITY").unwrap().value(), Some("990 kbps"), "no optimistic update before the echo");

        // The echo lands (C's PairedDeviceUpserted, same write that
        // produced the command above) -- the live model read must now show 660.
        app.handle_event(upsert_with_quality(addr, "Cans", 2, 2));
        let rows_after_echo = device_page_rows(&app.model(), &PresetStore::new(), addr);
        assert_eq!(rows_after_echo.iter().find(|r| r.label == "QUALITY").unwrap().value(), Some("660 kbps"), "the check follows the stored echo");
    }

    #[test]
    fn picking_while_disconnected_is_allowed_and_the_note_reads_varies_not_a_live_number() {
        let mut model = BtModel::default();
        let addr = [30; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 1, preset_id: 0 });
        // Not connected.
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let handle: ModelHandle = Rc::new(RefCell::new(model));
        let Some(_screen) = build_ldac_quality_picker_screen(&handle, addr, &commands) else {
            panic!("a paired, disconnected device's picker must build, not vanish");
        };
        let mut model = handle.borrow_mut();
        // Behavioural check via the row-building helper the picker itself
        // uses for Adaptive's note -- disconnected must never read "N now".
        model.paired[0].ldac_quality = LDAC_QUALITY_ADAPTIVE;
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        assert_eq!(rows.iter().find(|r| r.label == "QUALITY").unwrap().value(), Some("Adaptive"));
    }

    // --- EFFECT row + picker (bead pico-link-ryw.7) ---

    #[test]
    fn effect_row_is_always_present_and_reads_off_for_an_unassigned_device() {
        let model = BtModel::default();
        let addr = [40; 6];
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "EFFECT").expect("EFFECT must always be present");
        assert_eq!(row.value(), Some("Off"));
    }

    #[test]
    fn effect_row_resolves_the_assigned_effects_name() {
        let mut model = BtModel::default();
        let addr = [41; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 5 });
        let mut presets = PresetStore::new();
        presets.load(5, crate::dsp::Preset::new("Relaxed"));
        let rows = device_page_rows(&model, &presets, addr);
        let row = rows.iter().find(|r| r.label == "EFFECT").unwrap();
        assert_eq!(row.value(), Some("Relaxed"));
    }

    #[test]
    fn effect_row_reads_off_for_a_dangling_preset_id() {
        let mut model = BtModel::default();
        let addr = [42; 6];
        model.paired.push(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 99 });
        let rows = device_page_rows(&model, &PresetStore::new(), addr);
        let row = rows.iter().find(|r| r.label == "EFFECT").unwrap();
        assert_eq!(row.value(), Some("Off"), "a since-deleted preset id must never render a dangling name");
    }

    #[test]
    fn opening_effect_from_the_device_page_pushes_the_picker_and_a_pick_queues_assign_preset() {
        let mut app = App::new(240, 240);
        let addr = [43; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command(); // drain PersistDevice
        app.handle_event(upsert(addr, "Cans", 1));
        app.handle_event(Event::PresetLoaded { id: 1, blob: crate::dsp::Preset::new("Relaxed").to_wire().to_vec() });
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]); // connected row -> device page
        // Rows: CODEC, SAMPLE RATE(no QUALITY, not LDAC), USB IN, A2DP,
        // ADDRESS, EFFECT is inserted right after the (absent) QUALITY
        // block -- one Down from CODEC.
        app.handle_input(vec![NavIntent::Down]);
        app.handle_input(vec![NavIntent::Select]); // -> EFFECT picker
        assert_eq!(app.current_screen_title(), "Effect");

        app.handle_input(vec![NavIntent::Down]); // focus "Relaxed" (after Off)
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::AssignPreset { addr, preset_id: 1 }));
        assert_eq!(app.current_screen_title(), "Effect", "the picker stays open, no confirm");
    }

    #[test]
    fn device_page_and_effect_picker_screenshots_at_zoom() {
        let out_dir = std::env::temp_dir().join("pico-link-device-page-effect-screenshots");
        std::fs::create_dir_all(&out_dir).expect("failed to create output dir");

        let mut app = App::new(240, 240);
        let addr = [44; 6];
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false });
        app.poll_command();
        app.handle_event(upsert_with_quality(addr, "Sony WH-1000XM5", 1, 0));
        app.handle_event(Event::PresetLoaded { id: 1, blob: crate::dsp::Preset::new("Relaxed").to_wire().to_vec() });
        open_devices(&mut app);
        app.handle_input(vec![NavIntent::Select]);

        fn save_zoomed_png(app: &mut App, path: &std::path::Path) {
            const ZOOM: u32 = 3;
            let framebuffer = app.render();
            let mut image = image::RgbImage::new(framebuffer.width(), framebuffer.height());
            for pixel in framebuffer.pixels() {
                let color = pixel.1;
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
                image.put_pixel(
                    pixel.0.x as u32,
                    pixel.0.y as u32,
                    image::Rgb([(color.r() << 3) | (color.r() >> 2), (color.g() << 2) | (color.g() >> 4), (color.b() << 3) | (color.b() >> 2)]),
                );
            }
            let zoomed = image::imageops::resize(&image, framebuffer.width() * ZOOM, framebuffer.height() * ZOOM, image::imageops::FilterType::Nearest);
            zoomed.save(path).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        }

        save_zoomed_png(&mut app, &out_dir.join("01_device_page.png"));
        app.handle_input(vec![NavIntent::Down]); // EFFECT row
        app.handle_input(vec![NavIntent::Select]); // -> picker
        save_zoomed_png(&mut app, &out_dir.join("02_effect_picker.png"));
        println!("wrote device page/effect screenshots to {}", out_dir.display());
    }
}
