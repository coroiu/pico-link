//! The pairing wizard: one screen at navigator depth 2 whose *content*
//! advances through six phases (design section 9 / bead pico-link-znb.7 /
//! E5). Phases replace, they never push -- this module never touches
//! `Navigator::push`/`pop` for a phase transition, only for entering (from
//! the devices screen, see `crate::app::build_devices_screen`) and leaving
//! (B, handled generically by `Navigator::dispatch`'s `NavIntent::Back`
//! arm -- see that function's doc comment for why forwarding to this
//! widget first is safe) the wizard as a whole. That is what keeps the
//! stack at depth <= 2, which is what makes `B, B` a reliable escape from
//! anywhere in the flow.
//!
//! # Why phase transitions need no screen replacement
//!
//! [`PairingWizardView`] is constructed once (when the wizard is pushed)
//! and never rebuilt or swapped for the lifetime of the wizard being open.
//! Its phase lives in a shared `Rc<RefCell<WizardPhase>>` -- see
//! `App::wizard_phase`'s doc comment -- that both this widget (on local,
//! input-driven transitions: pressing A/X) and `App` (on C-event-driven
//! transitions: a connect sub-step changing, a retry, an outcome) mutate
//! directly. Because [`Widget::render`] reads through that same `Rc` fresh
//! on every call rather than a cached snapshot, either side's write is
//! picked up on the very next frame with no `Navigator` operation
//! involved at all. The one exception is the phase-2 scan list, which
//! *does* need its `VerticalList` rebuilt when `BtModel::devices` changes
//! shape -- handled internally by [`PairingWizardView::sync_list`], not by
//! replacing the `Screen`.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::convert::Infallible;

use embedded_graphics::prelude::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use u8g2_fonts::types::{FontColor, HorizontalAlignment, VerticalPosition};

use crate::app::{Command, ConnectFailureReason, ConnectStep, DeviceEntry, WizardPhase};
use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::list::{name_top_offset, ListItem, ListItemKey, VerticalList};
use super::message::MessageView;
use super::rail::ButtonLabel;
use super::screen::Screen;
use super::theme::{font, palette};
use super::widget::{Action, ChromeContribution, FocusEvent, Widget};

/// The wizard screen's fixed title -- also doubles as this crate's only
/// (pragmatic, not general) way to tell "is the top of the navigator
/// stack the wizard" apart from any other pushed screen, since nothing
/// else in this crate pushes a second screen today. See any call site
/// that compares against it for the caveat.
pub const WIZARD_TITLE: &str = "Pair headphones";

/// Builds the wizard screen at whatever phase `phase` currently holds
/// (typically [`WizardPhase::Instructions`], since `build_devices_screen`
/// resets it right before pushing this) -- see the module doc for why no
/// further rebuild is needed as the phase advances.
#[must_use]
pub fn build_wizard_screen(
    phase: Rc<RefCell<WizardPhase>>,
    devices: Rc<RefCell<Vec<DeviceEntry>>>,
    commands: Rc<RefCell<VecDeque<Command>>>,
) -> Screen {
    let view = PairingWizardView::new(phase, devices, commands);
    Screen::new(WIZARD_TITLE, vec![Box::new(view)])
}

/// Phase 2's 4-bar signal glyph (design section 9 rule 4: "signal as a
/// 4-bar glyph here, dBm only in detail"; section 14 F10: "unconditional
/// for the scan list"). Maps raw inquiry RSSI to a `0..=4` bar count,
/// drawn as a real graphical glyph by [`super::theme::draw_signal_bars`]
/// via [`ListItem::with_signal_bars`] -- pico-link-0r3 replaced the
/// previous ASCII `#`/`.` stand-in (see that bead for why the stand-in
/// existed and why four hash characters mattered enough to fix: this is
/// the first screen a new user meets).
///
/// The thresholds themselves are an unchanged, unscientific coarse
/// bucketing (not calibrated against real hardware RSSI distributions) --
/// carried over from the stand-in this replaces. Good enough for "glance
/// at four bars", not for anything quantitative; a real per-device dBm
/// value stays available in phase-2's underlying `DeviceEntry`, this
/// glyph never claims otherwise.
fn signal_bar_level(rssi: i8) -> u8 {
    match rssi {
        r if r >= -50 => 4,
        r if r >= -60 => 3,
        r if r >= -70 => 2,
        r if r >= -80 => 1,
        _ => 0,
    }
}

/// The headline/subline pair for one of the five named failure causes
/// (design section 9's table). Not `pub`: only [`PairingWizardView::render`]
/// needs this.
/// Headline/subline text, kept short enough to fit one line at
/// `font::name()`/`font::username()` size within the wizard's content
/// width (240px screen minus the 34px button rail -- see
/// `chrome::RAIL_WIDTH` -- leaves ~206px, not the full 240px `MessageView`'s
/// own doc-comment example assumes). `MessageView` has no wrapping/
/// truncation of its own (unlike `VerticalList`'s row labels), so this is
/// a real, screenshot-caught constraint on the copy itself, not a
/// stylistic choice -- see this bead's completion report.
fn failure_text(reason: ConnectFailureReason) -> (&'static str, &'static str) {
    match reason {
        ConnectFailureReason::Timeout => ("No response", "On and in range?"),
        ConnectFailureReason::Rejected => ("Pairing refused", "Remove dongle from paired list"),
        ConnectFailureReason::NoA2dpSink => ("Can't play audio", "Doesn't support audio"),
        ConnectFailureReason::NeedsPin => ("Needs a PIN", "Can't enter a PIN code"),
        ConnectFailureReason::RadioError => ("Bluetooth error", "Try again"),
    }
}

/// Top padding (px) before phase 4's first step line -- same value as
/// `message::MESSAGE_TOP_PADDING`, kept as a private local constant rather
/// than importing that one since the two are only *coincidentally* equal,
/// not conceptually the same budget.
const STEPS_TOP_PADDING: i32 = 20;
/// Vertical spacing (px) between phase 4's four step lines.
const STEP_ROW_HEIGHT: i32 = 26;

/// Draws phase 4 (connecting): all four named sub-steps
/// ([`ConnectStep::all`]'s fixed order), each colored by whether it's
/// already completed (`TEXT_SECONDARY`), the one currently in progress
/// (`BRAND_BRIGHT`), or not yet reached (`DIVIDER`) -- naming *every* step
/// (not just the current one) is what lets the user see how far the
/// attempt has gotten, per design section 9's "each step fails
/// differently; naming the current one tells the user *and us* where it
/// stalled".
fn render_connecting_steps(area: Rectangle, current: ConnectStep, target: &mut FrameBuffer565) {
    let center_x = area.top_left.x + area.size.width as i32 / 2;
    let steps = ConnectStep::all();
    let current_index = steps.iter().position(|&s| s == current).unwrap_or(0);

    for (index, step) in steps.iter().enumerate() {
        let (color, prefix) = match index.cmp(&current_index) {
            core::cmp::Ordering::Less => (palette::TEXT_SECONDARY, "> "),
            core::cmp::Ordering::Equal => (palette::BRAND_BRIGHT, "- "),
            core::cmp::Ordering::Greater => (palette::DIVIDER, "  "),
        };
        let line = format!("{prefix}{}", step.label());
        let y = area.top_left.y + STEPS_TOP_PADDING + name_top_offset() + index as i32 * STEP_ROW_HEIGHT;
        let _ = font::name().render_aligned(
            line.as_str(),
            Point::new(center_x, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(color),
            target,
        );
    }
}

/// The single content widget on the wizard [`Screen`], covering all six
/// phases (design section 9). See the module doc for the overall
/// architecture; see individual method doc comments for per-phase
/// behavior.
struct PairingWizardView {
    phase: Rc<RefCell<WizardPhase>>,
    devices: Rc<RefCell<Vec<DeviceEntry>>>,
    commands: Rc<RefCell<VecDeque<Command>>>,
    /// Phase 2's scan-result list, lazily (re)built by [`Self::sync_list`]
    /// whenever `devices` no longer matches `list_devices` -- `RefCell`
    /// because [`Widget::render`] takes `&self`. `with_selected_identity`
    /// is used exactly the way `build_devices_screen` uses it, per design
    /// section 9 rule 1/2 (stable sort, first-seen/append-only order --
    /// `devices`' own order is never touched here, so this is automatic --
    /// and identity-keyed selection so a late name or a new arrival never
    /// moves the cursor out from under the user).
    list: RefCell<VerticalList>,
    /// The device snapshot `list` was last built from -- compared against
    /// `devices` on every [`Self::sync_list`] call to decide whether a
    /// rebuild is needed at all.
    list_devices: RefCell<Vec<DeviceEntry>>,
}

impl PairingWizardView {
    fn new(phase: Rc<RefCell<WizardPhase>>, devices: Rc<RefCell<Vec<DeviceEntry>>>, commands: Rc<RefCell<VecDeque<Command>>>) -> Self {
        let list = build_scan_list(&devices.borrow(), &phase, &commands, None, 0);
        let list_devices = devices.borrow().clone();
        Self { phase, devices, commands, list: RefCell::new(list), list_devices: RefCell::new(list_devices) }
    }

    /// Rebuilds `list` from `devices` iff the device set actually changed
    /// since the last build -- cheap no-op on every render/input call that
    /// isn't reacting to a new/updated/cleared device.
    fn sync_list(&self) {
        let current = self.devices.borrow().clone();
        if *self.list_devices.borrow() == current {
            return;
        }
        let prev_key = self.list.borrow().selected_key();
        let prev_index = self.list.borrow().selected_index();
        let new_list = build_scan_list(&current, &self.phase, &self.commands, prev_key, prev_index);
        *self.list.borrow_mut() = new_list;
        *self.list_devices.borrow_mut() = current;
    }
}

/// Builds phase 2's device-row list. A free function (not a method) so
/// [`PairingWizardView::new`] and [`PairingWizardView::sync_list`] share
/// the exact same construction, mirroring `crate::app::build_devices_screen`'s
/// own snapshot-and-rebuild shape.
fn build_scan_list(
    devices: &[DeviceEntry],
    phase: &Rc<RefCell<WizardPhase>>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    prev_key: Option<ListItemKey>,
    prev_index: usize,
) -> VerticalList {
    let items: Vec<ListItem> = devices
        .iter()
        .map(|d| {
            let label = if d.name.is_empty() { String::from("(unknown device)") } else { d.name.clone() };
            ListItem::new(label).with_signal_bars(signal_bar_level(d.rssi)).with_key(ListItemKey::from(d.addr))
        })
        .collect();

    let devices_snapshot: Vec<DeviceEntry> = devices.to_vec();
    let phase_for_activate = Rc::clone(phase);
    let commands_for_activate = Rc::clone(commands);
    VerticalList::new(items)
        .on_activate_index(move |index| {
            if let Some(device) = devices_snapshot.get(index) {
                commands_for_activate.borrow_mut().push_back(Command::Connect { addr: device.addr });
                *phase_for_activate.borrow_mut() = WizardPhase::connecting_pending(device.addr, ConnectStep::Connecting);
            }
            Action::None
        })
        .with_selected_identity(prev_key, prev_index)
        .with_focused(true)
}

impl Widget for PairingWizardView {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        constraints
    }

    fn is_focusable(&self) -> bool {
        // Always -- the wizard screen has exactly this one top-level
        // widget, and it must be focusable in every phase (including
        // phase 1/3/6, which have no internal list at all) for `Screen`'s
        // focus machinery to ever deliver it a `NavIntent`/`FocusEvent` --
        // see `Screen::initialize_focus`.
        true
    }

    /// `NavIntent::Select` (joystick press / button A) routes through
    /// `Screen::activate_focused` -> `Widget::on_focus(Activated)`, not
    /// `on_intent` -- see `Screen::activate_focused`'s doc comment.
    fn on_focus(&mut self, event: FocusEvent) -> Action {
        if event != FocusEvent::Activated {
            return Action::None;
        }
        let phase = self.phase.borrow().clone();
        match phase {
            WizardPhase::Instructions | WizardPhase::NothingFound => {
                // Phase 1/3 -> phase 2. Clearing `devices` proactively
                // (rather than waiting for C's own `DevicesCleared` event)
                // avoids a stale-row flash from a previous scan between
                // this press and that event arriving.
                self.devices.borrow_mut().clear();
                self.commands.borrow_mut().push_back(Command::StartScan);
                *self.phase.borrow_mut() = WizardPhase::scanning_pending();
                Action::None
            }
            WizardPhase::Scanning { .. } => {
                self.sync_list();
                self.list.borrow_mut().on_focus(FocusEvent::Activated)
            }
            // Phases 4/5/6 have no A-driven action of their own (phase 6
            // success auto-dismisses via a C timer event; phase 6
            // degraded success and phase 5's retry are X-driven -- see
            // `on_intent`'s `ShortcutX` arm).
            _ => Action::None,
        }
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        let phase = self.phase.borrow().clone();
        match (intent, phase) {
            // Design section 9: "B cancels the scan" -- without this, B
            // during the 10.24s inquiry does nothing (the scan runs to
            // completion regardless of the pop), which is exactly the
            // dead-button trust cost E1/pico-link-znb.2 exists to avoid.
            // `Navigator::dispatch`'s `NavIntent::Back` arm forwards here
            // for precisely this side effect, then unconditionally pops
            // regardless of what's returned -- see that arm's doc comment.
            (NavIntent::Back, WizardPhase::Scanning { .. }) => {
                self.commands.borrow_mut().push_back(Command::CancelScan);
                Action::None
            }
            // Code-review fix (post-merge-review of this bead): B was
            // previously handled generically by `Navigator::dispatch`
            // popping the screen with no side effect at all in phases
            // 4/5 -- the screen disappeared but the abandoned ACL/SSP/
            // AVDTP attempt kept running in C with nothing telling it to
            // stop, still delivering `ConnectStepChanged`/
            // `ConnectRetrying`/`ConnectFailed`/`ConnectSucceeded` events
            // for a device the user already walked away from. Design
            // section 9 phase 4: "B genuinely aborts" -- not "B leaves
            // the screen". Mirrors the `CancelScan` arm above exactly.
            (NavIntent::Back, WizardPhase::Connecting { addr, .. } | WizardPhase::NotResponding { addr, .. }) => {
                self.commands.borrow_mut().push_back(Command::CancelConnect { addr });
                Action::None
            }
            (NavIntent::Up | NavIntent::Down | NavIntent::JumpBy(_), WizardPhase::Scanning { .. }) => {
                self.sync_list();
                self.list.borrow_mut().on_intent(intent)
            }
            // Phase 5 "Keep trying" / phase 6 failure "Try again"
            // (retryable reasons only -- design section 9: "no retry
            // offered for structurally impossible cases"). Both reissue
            // `Command::Connect` for the same `addr` and drop straight
            // back into phase 4 at its first sub-step, exactly like
            // selecting the device fresh from the scan list would.
            (NavIntent::ShortcutX, WizardPhase::NotResponding { addr, .. }) => {
                self.commands.borrow_mut().push_back(Command::Connect { addr });
                *self.phase.borrow_mut() = WizardPhase::connecting_pending(addr, ConnectStep::Connecting);
                Action::None
            }
            (NavIntent::ShortcutX, WizardPhase::Failed { addr, reason }) if reason.retryable() => {
                self.commands.borrow_mut().push_back(Command::Connect { addr });
                *self.phase.borrow_mut() = WizardPhase::connecting_pending(addr, ConnectStep::Connecting);
                Action::None
            }
            // Degraded success's X ("Codec settings", design section 9
            // phase 6 / section 6.2 link 4) has no destination yet -- the
            // codec picker screen is design section 11 / E11, not built
            // by this bead. Consumed as a deliberate no-op rather than
            // left unhandled, so pressing it is inert rather than
            // silently falling through to some other behavior. See this
            // bead's completion report for the explicit callout.
            _ => Action::None,
        }
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let phase = self.phase.borrow().clone();
        let mut contribution = ChromeContribution { y: Some(ButtonLabel::Inert), ..ChromeContribution::default() };
        match phase {
            WizardPhase::Instructions | WizardPhase::NothingFound => {
                contribution.a = Some(ButtonLabel::Live(String::from("scan")));
                contribution.x = Some(ButtonLabel::Inert);
            }
            WizardPhase::Scanning { .. } => {
                // No A-rail label of its own -- matches the devices
                // screen's own list, which likewise leaves A unlabelled
                // (Select still activates the focused row regardless).
                contribution.x = Some(ButtonLabel::Inert);
            }
            WizardPhase::Connecting { .. } => {
                contribution.x = Some(ButtonLabel::Inert);
            }
            WizardPhase::NotResponding { .. } => {
                contribution.x = Some(ButtonLabel::Live(String::from("keep")));
            }
            WizardPhase::Succeeded { degraded } => {
                contribution.x = Some(if degraded { ButtonLabel::Live(String::from("codec")) } else { ButtonLabel::Inert });
            }
            WizardPhase::Failed { reason, .. } => {
                contribution.x =
                    Some(if reason.retryable() { ButtonLabel::Live(String::from("retry")) } else { ButtonLabel::Inert });
            }
        }
        Some(contribution)
    }

    fn selected_index(&self) -> Option<usize> {
        if matches!(*self.phase.borrow(), WizardPhase::Scanning { .. }) {
            Some(self.list.borrow().selected_index())
        } else {
            None
        }
    }

    fn selected_key(&self) -> Option<ListItemKey> {
        if matches!(*self.phase.borrow(), WizardPhase::Scanning { .. }) {
            self.list.borrow().selected_key()
        } else {
            None
        }
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        let phase = self.phase.borrow().clone();
        match phase {
            WizardPhase::Instructions => {
                // Short enough to fit one line within the wizard's content
                // width (~206px, screen width minus the button rail) at
                // `font::name()` size -- see `failure_text`'s doc comment
                // for why this budget is a hard constraint, not a style
                // choice.
                MessageView::new("Enable pairing mode")
                    .with_subline("Hold power ~5s until it flashes")
                    .render(area, ctx, target)?;
            }
            WizardPhase::NothingFound => {
                MessageView::new("No headphones found")
                    .with_headline_color(palette::STATUS_WARNING)
                    .with_subline("Press A to scan again")
                    .render(area, ctx, target)?;
            }
            WizardPhase::Scanning { .. } => {
                self.sync_list();
                self.list.borrow().render(area, ctx, target)?;
            }
            WizardPhase::Connecting { step, .. } => {
                render_connecting_steps(area, step, target);
            }
            WizardPhase::NotResponding { attempt, .. } => {
                MessageView::new("Not responding")
                    .with_headline_color(palette::STATUS_WARNING)
                    .with_subline(format!("Still trying ({attempt})"))
                    .render(area, ctx, target)?;
            }
            WizardPhase::Succeeded { degraded } => {
                if degraded {
                    MessageView::new("Connected")
                        .with_headline_color(palette::STATUS_WARNING)
                        .with_subline("Using a fallback codec")
                        .render(area, ctx, target)?;
                } else {
                    MessageView::new("Connected").with_headline_color(palette::STATUS_SUCCESS).render(area, ctx, target)?;
                }
            }
            WizardPhase::Failed { reason, .. } => {
                let (headline, subline) = failure_text(reason);
                MessageView::new(headline).with_headline_color(palette::STATUS_ERROR).with_subline(subline).render(area, ctx, target)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, ConnectFailureReason, ConnectStep, DeviceEntry, Event, LinkState, WizardPhase, DEVICES_TITLE};
    use crate::input::NavIntent;
    use crate::platform::Instant;

    /// Every test in this module drives `App` without ever calling
    /// `App::tick`, so `App`'s `now_us` stays at its constructed default
    /// of 0 throughout -- meaning `stamp_pending_wizard_timestamp` always
    /// backfills a freshly entered `Scanning`/`Connecting` phase's
    /// `started` with exactly this value. Named so assertions read as "the
    /// timestamp a fresh phase gets in these tests", not a magic literal.
    fn untimed() -> Instant {
        Instant::from_micros(0)
    }

    /// Home(1) -> Devices(2) -> Wizard(3), all three `Select`s -- since
    /// `pico-link-znb.8`/E7, Home (not Devices) is the navigator root, so
    /// reaching the wizard takes one more step than it used to: centre
    /// toggles Home to its menu face (Bluetooth pre-selected), centre
    /// again activates that row (pushing Devices, "Scan for headphones"
    /// pre-selected on a fresh screen), centre again opens the wizard.
    fn open_wizard(app: &mut App) {
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
        app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
        app.handle_input(vec![NavIntent::Select]); // "Scan for headphones" row -> pushes the wizard
    }

    // --- pico-link-0r3: F10, RSSI -> bar-count thresholds ---

    #[test]
    fn signal_bar_level_thresholds() {
        assert_eq!(signal_bar_level(-30), 4, "well above the -50 cutoff");
        assert_eq!(signal_bar_level(-50), 4, "exactly at the -50 cutoff is still 4 bars");
        assert_eq!(signal_bar_level(-51), 3);
        assert_eq!(signal_bar_level(-60), 3, "exactly at the -60 cutoff is still 3 bars");
        assert_eq!(signal_bar_level(-61), 2);
        assert_eq!(signal_bar_level(-70), 2, "exactly at the -70 cutoff is still 2 bars");
        assert_eq!(signal_bar_level(-71), 1);
        assert_eq!(signal_bar_level(-80), 1, "exactly at the -80 cutoff is still 1 bar");
        assert_eq!(signal_bar_level(-81), 0);
        assert_eq!(signal_bar_level(-128), 0, "the weakest representable RSSI is still 0 bars, not a panic");
    }

    #[test]
    fn a_scan_row_built_the_way_build_scan_list_does_carries_signal_bars_not_sublabel_text() {
        // Mirrors `build_scan_list`'s exact row-construction line for a
        // -40 dBm device -- `ListItem`'s fields are `pub` (see its own
        // doc comment), but `VerticalList` has no accessor to read a row
        // back out once built, so this pins down the *construction* side
        // directly rather than round-tripping through the widget.
        // `list.rs`'s
        // `a_row_with_signal_bars_draws_the_graphical_glyph_not_sublabel_text`
        // covers the render-level pixel proof for the same field.
        let item = ListItem::new("Cans").with_signal_bars(signal_bar_level(-40)).with_key(ListItemKey::from([1; 6]));
        assert_eq!(item.signal_bars, Some(4));
        assert_eq!(item.sublabel, None, "the scan list must not also carry a textual dBm sublabel");
    }

    #[test]
    fn selecting_scan_opens_the_wizard_at_the_instructions_phase() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        assert_eq!(app.navigator_depth(), 3);
        assert_eq!(app.current_screen_title(), WIZARD_TITLE);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Instructions);
    }

    #[test]
    fn pressing_a_on_instructions_starts_the_scan_and_queues_start_scan() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Scanning { started: untimed() });
        assert_eq!(app.poll_command(), Some(Command::StartScan));
    }

    #[test]
    fn a_device_arriving_while_scanning_does_not_change_the_phase() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1; 6], name: String::from("Cans"), rssi: -40 }));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Scanning { started: untimed() });
        assert_eq!(app.navigator_depth(), 3, "the wizard must still be the top screen at Home(1)/Devices(2)/Wizard(3)");
    }

    #[test]
    fn scan_ending_with_zero_devices_moves_to_nothing_found() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        app.handle_event(Event::LinkStateChanged(LinkState::Scanning));
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NothingFound);
    }

    #[test]
    fn scan_ending_with_devices_present_stays_on_scanning() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [2; 6], name: String::from("Cans"), rssi: -40 }));
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Scanning { started: untimed() });
    }

    #[test]
    fn selecting_a_device_row_queues_connect_and_enters_the_connecting_phase() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        let addr = [3; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Cans"), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]); // activate the (only) row
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: untimed() });
        assert_eq!(app.poll_command(), Some(Command::StartScan));
        assert_eq!(app.poll_command(), Some(Command::Connect { addr }));
    }

    #[test]
    fn a_freshly_entered_phase_is_stamped_with_the_real_current_time_not_zero() {
        // Regression test for pico-link-znb.10 step 5: `WizardPhase::
        // Scanning`/`Connecting` must carry the app's real current time as
        // `started`, not the `PENDING_TIMESTAMP` sentinel the widget uses
        // internally (it has no clock of its own -- see that constant's
        // doc comment) and not a stale `0` from before `App::tick` was
        // ever called.
        let mut app = App::new(240, 240);
        app.tick(999_000);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // Instructions -> Scanning
        assert_eq!(
            app.wizard_phase_for_test(),
            WizardPhase::Scanning { started: Instant::from_micros(999_000) },
            "a freshly entered Scanning phase must be stamped with App's real now_us, not left pending or zero"
        );

        let addr = [7; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Cans"), rssi: -40 }));
        app.tick(1_500_000);
        app.handle_input(vec![NavIntent::Select]); // Scanning -> Connecting
        assert_eq!(
            app.wizard_phase_for_test(),
            WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: Instant::from_micros(1_500_000) },
            "a freshly entered Connecting phase must be stamped with App's real now_us at the instant it was entered"
        );
    }

    #[test]
    fn connect_step_events_advance_the_named_sub_steps() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [4; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);

        app.handle_event(Event::ConnectStepChanged(ConnectStep::Pairing));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Pairing, started: untimed() });

        app.handle_event(Event::ConnectStepChanged(ConnectStep::SettingUpAudio));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::SettingUpAudio, started: untimed() });
    }

    #[test]
    fn connect_step_changed_is_ignored_outside_the_connecting_phases() {
        let mut app = App::new(240, 240);
        // Wizard not even open -- default phase is Instructions.
        app.handle_event(Event::ConnectStepChanged(ConnectStep::Pairing));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Instructions);
    }

    #[test]
    fn retrying_surfaces_not_responding_with_an_incrementing_attempt_counter() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [5; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);

        app.handle_event(Event::ConnectRetrying { attempt: 1 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NotResponding { addr, attempt: 1 });

        app.handle_event(Event::ConnectRetrying { attempt: 2 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NotResponding { addr, attempt: 2 });
    }

    #[test]
    fn keep_trying_from_not_responding_reissues_connect_and_returns_to_connecting() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [6; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);
        app.handle_event(Event::ConnectRetrying { attempt: 1 });
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain the first Connect

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: untimed() });
        assert_eq!(app.poll_command(), Some(Command::Connect { addr }));
    }

    #[test]
    fn connect_succeeded_reaches_the_plain_success_outcome() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [7; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);

        app.handle_event(Event::ConnectSucceeded { degraded: false });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded: false });
    }

    #[test]
    fn wizard_auto_dismiss_pops_back_to_devices_only_after_plain_success() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [8; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);
        app.handle_event(Event::ConnectSucceeded { degraded: false });
        assert_eq!(app.navigator_depth(), 3);

        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 2, "a plain success must auto-dismiss back to Devices");
    }

    #[test]
    fn degraded_success_does_not_auto_dismiss() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [9; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);
        app.handle_event(Event::ConnectSucceeded { degraded: true });

        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 3, "degraded success must require acknowledgement, never auto-dismiss");
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded: true });
    }

    #[test]
    fn connect_failed_reaches_the_failed_phase_with_its_reason() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [10; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);

        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Failed { addr, reason: ConnectFailureReason::NoA2dpSink });
    }

    #[test]
    fn retry_is_offered_only_for_retryable_failure_reasons() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [11; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);
        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink });
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        // Non-retryable: X must do nothing.
        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.poll_command(), None, "NoA2dpSink is not retryable -- X must not reissue Connect");
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Failed { addr, reason: ConnectFailureReason::NoA2dpSink });
    }

    #[test]
    fn retry_reissues_connect_for_a_retryable_failure_reason() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]);
        let addr = [12; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]);
        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::Timeout });
        app.poll_command();
        app.poll_command();

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: untimed() });
        assert_eq!(app.poll_command(), Some(Command::Connect { addr }));
    }

    // --- B always aborts the whole flow, from every phase (design section 9) ---

    fn assert_back_aborts_from(phase_setup: impl FnOnce(&mut App)) {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        phase_setup(&mut app);
        assert_eq!(app.navigator_depth(), 3, "test setup should leave the wizard open");

        app.handle_input(vec![NavIntent::Back]);
        assert_eq!(app.navigator_depth(), 2, "B must pop the wizard back to Devices");
        assert_eq!(app.current_screen_title(), DEVICES_TITLE);
    }

    #[test]
    fn b_aborts_from_instructions() {
        assert_back_aborts_from(|_app| {});
    }

    #[test]
    fn b_aborts_from_scanning_and_cancels_the_scan() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        app.poll_command(); // drain StartScan

        app.handle_input(vec![NavIntent::Back]);
        assert_eq!(app.navigator_depth(), 2, "B must pop the wizard back to Devices");
        assert_eq!(
            app.poll_command(),
            Some(Command::CancelScan),
            "B during the scanning phase must queue CancelScan (design section 9: 'B cancels the scan')"
        );
    }

    #[test]
    fn b_aborts_from_nothing_found() {
        assert_back_aborts_from(|app| {
            app.handle_input(vec![NavIntent::Select]);
            app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        });
    }

    #[test]
    fn b_aborts_from_connecting_and_queues_cancel_connect() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        let addr = [13; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        app.handle_input(vec![NavIntent::Back]);
        assert_eq!(app.navigator_depth(), 2, "B must pop the wizard back to Devices");
        assert_eq!(
            app.poll_command(),
            Some(Command::CancelConnect { addr }),
            "B during the connecting phase must queue CancelConnect (design section 9: 'B genuinely aborts') --              leaving the screen without this leaves the abandoned ACL/SSP/AVDTP attempt running in C"
        );
    }

    #[test]
    fn b_aborts_from_not_responding_and_queues_cancel_connect() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        let addr = [14; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        app.handle_event(Event::ConnectRetrying { attempt: 1 }); // -> NotResponding
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        app.handle_input(vec![NavIntent::Back]);
        assert_eq!(app.navigator_depth(), 2, "B must pop the wizard back to Devices");
        assert_eq!(
            app.poll_command(),
            Some(Command::CancelConnect { addr }),
            "B during the not-responding phase must queue CancelConnect, same as the connecting phase"
        );
    }

    #[test]
    fn b_aborts_from_succeeded() {
        assert_back_aborts_from(|app| {
            app.handle_input(vec![NavIntent::Select]);
            app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [15; 6], name: String::new(), rssi: -40 }));
            app.handle_input(vec![NavIntent::Select]);
            app.handle_event(Event::ConnectSucceeded { degraded: true });
        });
    }

    #[test]
    fn b_aborts_from_failed() {
        assert_back_aborts_from(|app| {
            app.handle_input(vec![NavIntent::Select]);
            app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [16; 6], name: String::new(), rssi: -40 }));
            app.handle_input(vec![NavIntent::Select]);
            app.handle_event(Event::ConnectFailed { addr: [16; 6], reason: ConnectFailureReason::RadioError });
        });
    }

    // --- Depth never exceeds Home(1)/Devices(2)/Wizard(3) (design depth
    // 2), across every phase transition ---

    #[test]
    fn the_wizard_never_exceeds_depth_two_across_every_phase() {
        let mut app = App::new(240, 240);
        assert_eq!(app.navigator_depth(), 1, "a fresh app starts on Home alone");
        open_wizard(&mut app);
        assert_eq!(app.navigator_depth(), 3, "Home(1)/Devices(2)/Wizard(3)");

        app.handle_input(vec![NavIntent::Select]); // -> Scanning
        assert_eq!(app.navigator_depth(), 3);
        let addr = [17; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40 }));
        assert_eq!(app.navigator_depth(), 3);
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::ConnectStepChanged(ConnectStep::Pairing));
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::ConnectRetrying { attempt: 1 });
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::ConnectSucceeded { degraded: false });
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 2, "auto-dismiss returns to Devices (Home(1)/Devices(2))");
    }
}
