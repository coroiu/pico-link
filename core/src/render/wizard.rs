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
//! *does* need its `VerticalList` rebuilt when `BtModel::discovered`
//! changes shape -- handled internally by
//! [`PairingWizardView::sync_list`], not by replacing the `Screen`.
//!
//! As of bead `pico-link-bgnd` M4, phase 2's scan list reads
//! `BtModel::discovered` straight off the live [`crate::app::ModelHandle`]
//! this widget holds -- there is no more `App::wizard_devices` mirror to
//! keep in lockstep (design §6/§7 M4: "wizard reads discovered through the
//! handle; delete `wizard_devices`").

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

use crate::app::{
    cancel_connect, cancel_scan, connect, is_audio_sink, start_scan, truncate_device_name, Command, ConnectFailureReason, ConnectInitiator,
    ConnectStep,
    DeviceEntry, ModelHandle, ScanOwner, ScreenId, WizardPhase, MAX_SCAN_LIST_ITEMS,
};
use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::list::{name_top_offset, ListItem, ListItemKey, VerticalList};
use super::message::MessageView;
use super::rail::ButtonLabel;
use super::screen::Screen;
use super::theme::{font, palette};
use super::widget::{Action, ChromeContribution, FocusEvent, Verb, Widget};

/// The wizard screen's fixed title -- also doubles as this crate's only
/// (pragmatic, not general) way to tell "is the top of the navigator
/// stack the wizard" apart from any other pushed screen, since nothing
/// else in this crate pushes a second screen today. See any call site
/// that compares against it for the caveat.
pub const WIZARD_TITLE: &str = "Pair headphones";

/// Builds the wizard screen at whatever phase `phase` currently holds
/// (typically [`WizardPhase::Scanning`], since `build_devices_screen`
/// resets it right before pushing this and queues `Command::StartScan` in
/// the same step -- pico-link-4vb.2 removed the old instructions phase 1)
/// -- see the module doc for why no further rebuild is needed as the
/// phase advances.
#[must_use]
pub fn build_wizard_screen(phase: Rc<RefCell<WizardPhase>>, model: ModelHandle, commands: Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let view = PairingWizardView::new(phase, model, commands);
    Screen::new(WIZARD_TITLE, vec![Box::new(view)]).with_id(ScreenId::PairingWizard)
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

/// Vertical gap (px) between the last connect-step line and the elapsed-
/// time readout below it.
const ELAPSED_GAP: i32 = 12;

/// Refresh cadence for phase 4's elapsed-time readout -- see
/// [`PairingWizardView::redraw_after`]. Coarse on purpose: the readout
/// itself only has one-second resolution, so anything finer would just be
/// extra renders of pixels that didn't change. `pub(crate)` so the
/// pico-link-vxc freshness-invariant table test (`crate::app`'s test
/// module) can assert against the real value instead of duplicating it.
pub(crate) const ELAPSED_REDRAW_INTERVAL: core::time::Duration = core::time::Duration::from_millis(250);

/// Draws phase 4 (connecting): all four named sub-steps
/// ([`ConnectStep::all`]'s fixed order), each colored by whether it's
/// already completed (`TEXT_SECONDARY`), the one currently in progress
/// (`BRAND_BRIGHT`), or not yet reached (`DIVIDER`) -- naming *every* step
/// (not just the current one) is what lets the user see how far the
/// attempt has gotten, per design section 9's "each step fails
/// differently; naming the current one tells the user *and us* where it
/// stalled" -- plus, below them, an elapsed-seconds readout
/// (`RenderCtx`/`WizardPhase::Connecting::started`'s first real consumer,
/// pico-link-znb.10 step 6): this is the "the screen isn't hung, it's
/// still working" liveness signal payoff 1 of the frame-scoped clock ADR
/// names, replacing a step list that could otherwise sit static and
/// indistinguishable from a wedge for the several real seconds a connect
/// attempt can take.
fn render_connecting_steps(area: Rectangle, current: ConnectStep, elapsed: core::time::Duration, target: &mut FrameBuffer565) {
    let center_x = area.top_left.x + area.size.width as i32 / 2;
    let steps = ConnectStep::all();

    // Bead pico-link-sfw6, design sec 3: `Disconnecting` is deliberately
    // outside `ConnectStep::all()` (see that enum's doc comment) -- it's a
    // virtual "step -1" shown as its own highlighted line, with the four
    // normal steps below it all rendered not-yet-reached, rather than
    // being spliced into the fixed four-element array. `row_offset` shifts
    // every normal-step row down by one line to make room for it;
    // `current_index` is signed (`isize`, not `usize`) so "step -1 is
    // current" correctly leaves every real index (0..=3) `Greater` --
    // not-yet-reached -- rather than wrapping to `usize::MAX` and reading
    // every real step as already-completed.
    let (current_index, row_offset): (isize, i32) = if current == ConnectStep::Disconnecting {
        let y = area.top_left.y + STEPS_TOP_PADDING + name_top_offset();
        let _ = font::name().render_aligned(
            format!("- {}", ConnectStep::Disconnecting.label()).as_str(),
            Point::new(center_x, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(palette::BRAND_BRIGHT),
            target,
        );
        (-1, 1)
    } else {
        (steps.iter().position(|&s| s == current).unwrap_or(0) as isize, 0)
    };

    for (index, step) in steps.iter().enumerate() {
        let (color, prefix) = match (index as isize).cmp(&current_index) {
            core::cmp::Ordering::Less => (palette::TEXT_SECONDARY, "> "),
            core::cmp::Ordering::Equal => (palette::BRAND_BRIGHT, "- "),
            core::cmp::Ordering::Greater => (palette::DIVIDER, "  "),
        };
        let line = format!("{prefix}{}", step.label());
        let y = area.top_left.y + STEPS_TOP_PADDING + name_top_offset() + (index as i32 + row_offset) * STEP_ROW_HEIGHT;
        let _ = font::name().render_aligned(
            line.as_str(),
            Point::new(center_x, y),
            VerticalPosition::Top,
            HorizontalAlignment::Center,
            FontColor::Transparent(color),
            target,
        );
    }

    let elapsed_y =
        area.top_left.y + STEPS_TOP_PADDING + name_top_offset() + (steps.len() as i32 + row_offset) * STEP_ROW_HEIGHT + ELAPSED_GAP;
    let elapsed_line = format!("{}s", elapsed.as_secs());
    let _ = font::username().render_aligned(
        elapsed_line.as_str(),
        Point::new(center_x, elapsed_y),
        VerticalPosition::Top,
        HorizontalAlignment::Center,
        FontColor::Transparent(palette::TEXT_SECONDARY),
        target,
    );
}

/// The single content widget on the wizard [`Screen`], covering all six
/// phases (design section 9). See the module doc for the overall
/// architecture; see individual method doc comments for per-phase
/// behavior.
struct PairingWizardView {
    phase: Rc<RefCell<WizardPhase>>,
    /// The live model handle -- as of bead `pico-link-bgnd` M4, phase 2's
    /// scan list reads `model.discovered` straight off this fresh every
    /// [`Self::sync_list`] call, rather than a separate `App`-maintained
    /// mirror (`App::wizard_devices`, since deleted). See
    /// [`crate::app::ModelHandle`]'s doc comment for the borrow rule.
    model: ModelHandle,
    commands: Rc<RefCell<VecDeque<Command>>>,
    /// Phase 2's scan-result list, lazily (re)built by [`Self::sync_list`]
    /// whenever `model.discovered` no longer matches `list_devices` --
    /// `RefCell` because [`Widget::render`] takes `&self`.
    /// `with_selected_identity` is used exactly the way
    /// `build_devices_screen` uses it, per design section 9 rule 1/2
    /// (stable sort, first-seen/append-only order -- `model.discovered`'s
    /// own order is never touched here, so this is automatic -- and
    /// identity-keyed selection so a late name or a new arrival never
    /// moves the cursor out from under the user).
    list: RefCell<VerticalList>,
    /// The `model.discovered` snapshot `list` was last built from --
    /// compared against the live value on every [`Self::sync_list`] call
    /// to decide whether a rebuild is needed at all.
    list_devices: RefCell<Vec<DeviceEntry>>,
}

impl PairingWizardView {
    fn new(phase: Rc<RefCell<WizardPhase>>, model: ModelHandle, commands: Rc<RefCell<VecDeque<Command>>>) -> Self {
        let list_devices = model.borrow().discovered.clone();
        let list = build_scan_list(&list_devices, &phase, &model, &commands, None, 0);
        Self { phase, model, commands, list: RefCell::new(list), list_devices: RefCell::new(list_devices) }
    }

    /// Rebuilds `list` from `model.discovered` iff the device set actually
    /// changed since the last build -- cheap no-op on every render/input
    /// call that isn't reacting to a new/updated/cleared device.
    fn sync_list(&self) {
        let current = self.model.borrow().discovered.clone();
        if *self.list_devices.borrow() == current {
            return;
        }
        let prev_key = self.list.borrow().selected_key();
        let prev_index = self.list.borrow().selected_index();
        let new_list = build_scan_list(&current, &self.phase, &self.model, &self.commands, prev_key, prev_index);
        *self.list.borrow_mut() = new_list;
        *self.list_devices.borrow_mut() = current;
    }
}

/// Builds phase 2's device-row list. A free function (not a method) so
/// [`PairingWizardView::new`] and [`PairingWizardView::sync_list`] share
/// the exact same construction, mirroring `crate::app::build_devices_screen`'s
/// own snapshot-and-rebuild shape.
///
/// Filters `devices` (`BtModel::discovered`, unfiltered raw inquiry
/// results) down to audio sinks via [`is_audio_sink`] -- design section 9
/// phase 2 rule 3 -- then caps the result at [`MAX_SCAN_LIST_ITEMS`]
/// (design section 21 Tier 1 row E9 / section 13's Class-of-Device row:
/// built as a backstop regardless of whether the filter above is doing
/// anything, since Class-of-Device is only *Expected*, not *Confirmed*).
/// The filter runs here, at the render layer, rather than at
/// `App::add_device` -- `BtModel::discovered` stays the complete raw
/// inquiry feed either way (nothing else reads it today, but nothing here
/// should force a future reader to reconstruct filtered-out devices from
/// C's device-discovered events again), and this bead's own text calls out
/// that the wizard must render a correct scan list whether or not this
/// filter exists.
///
/// Stable first-seen order (design section 9 rule 1) falls out for free:
/// `devices`' own order is never touched, only truncated. Row identity
/// (rule 2, [`ListItemKey::from`] on the address) survives the filter/cap
/// for the same reason -- a device already shown never moves or drops out
/// just because a later-discovered non-audio device gets filtered ahead of
/// where it would otherwise land.
fn build_scan_list(
    devices: &[DeviceEntry],
    phase: &Rc<RefCell<WizardPhase>>,
    model: &ModelHandle,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    prev_key: Option<ListItemKey>,
    prev_index: usize,
) -> VerticalList {
    let audio_devices: Vec<DeviceEntry> = devices.iter().filter(|d| is_audio_sink(d.class_of_device)).cloned().collect();
    let total_audio = audio_devices.len();
    let capped: Vec<DeviceEntry> = audio_devices.into_iter().take(MAX_SCAN_LIST_ITEMS).collect();
    let capped_len = capped.len();

    let mut items: Vec<ListItem> = capped
        .iter()
        .map(|d| {
            let label = if d.name.is_empty() { String::from("(unknown device)") } else { d.name.clone() };
            ListItem::new(label).with_signal_bars(signal_bar_level(d.rssi)).with_key(ListItemKey::from(d.addr))
        })
        .collect();

    // Backstop readout (design section 13's Class-of-Device row fallback):
    // a non-device informational row appended past the cap. Its index
    // (`capped_len`) is deliberately past the end of `devices_snapshot`
    // below, so activating it (Select/A) is a harmless no-op rather than
    // connecting to whatever the stale index would otherwise resolve to.
    // Deliberately no "press X to rescan" hint: this bead does not wire X
    // to a fresh scan from here (that would need `CancelScan` + `StartScan`
    // sequencing this bead's scope doesn't cover), and the label's ~180px
    // budget (240px screen minus the button rail, minus the row's leading
    // icon) truncates with an ellipsis well before a sentence that long
    // finishes anyway -- see this bead's zoomed fixture screenshot. A short
    // count-only readout is honest about what's actually on screen.
    if total_audio > MAX_SCAN_LIST_ITEMS {
        // `.inert()`: this row is not a device -- activating it is a
        // no-op (see the comment above), so it must report no `Verb`
        // rather than inheriting the list's default `Verb::Pair`, or A
        // would render live on a row that does nothing (design rule 4).
        items.push(ListItem::new(format!("Showing {capped_len} of {total_audio}")).inert());
    }

    let devices_snapshot: Vec<DeviceEntry> = capped;
    let phase_for_activate = Rc::clone(phase);
    let model_for_activate = Rc::clone(model);
    let commands_for_activate = Rc::clone(commands);
    VerticalList::new(items)
        .on_activate_index(Verb::Pair, move |index| {
            if let Some(device) = devices_snapshot.get(index) {
                connect(&model_for_activate, &commands_for_activate, device.addr, truncate_device_name(&device.name), ConnectInitiator::Device);
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
            WizardPhase::NothingFound => {
                // Phase 3 -> phase 2 (re-scan). `start_scan` clears
                // `model.discovered` proactively (rather than waiting for
                // C's own `DevicesCleared` event), avoiding a stale-row
                // flash from a previous scan between this press and that
                // event arriving.
                start_scan(&self.model, &self.commands, ScanOwner::Device);
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
                cancel_scan(&self.commands);
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
                // Bead pico-link-jyhk.25 review fix: routes through
                // `radio_actions::cancel_connect` (not a bare `Command::
                // CancelConnect` push) so `BtModel::attempt`/`last_outcome`
                // conclude immediately, rather than never -- see that
                // function's doc comment for why a fire-and-forget C cancel
                // can't be the thing that concludes them.
                cancel_connect(&self.model, &self.commands, addr);
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
            //
            // `name` is empty here, deliberately -- `WizardPhase` carries no
            // name of its own for `NotResponding`/`Failed` (bead
            // pico-link-4vb.4, T4/design section 5.3's last paragraph), and
            // C's read-modify-write persist path treats an empty name as
            // "keep whatever name the record already has" rather than
            // erasing it, so a retry can never regress an already-known
            // name to nameless.
            (NavIntent::ShortcutX, WizardPhase::NotResponding { addr, .. }) => {
                connect(&self.model, &self.commands, addr, String::new(), ConnectInitiator::Device);
                *self.phase.borrow_mut() = WizardPhase::connecting_pending(addr, ConnectStep::Connecting);
                Action::None
            }
            (NavIntent::ShortcutX, WizardPhase::Failed { addr, reason }) if reason.retryable() => {
                connect(&self.model, &self.commands, addr, String::new(), ConnectInitiator::Device);
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
            //
            // pico-link-l4d POLICY: `(Back, Succeeded { .. })` also falls
            // through to here -- a plain, side-effect-free pop, deliberately
            // NOT touching `App::home_face`. Manual B is the user retracing
            // their own steps one screen at a time; only the *automatic*
            // dismiss (`App::on_wizard_auto_dismiss`) resets Home to its
            // status face, because there the app itself is choosing the
            // destination. A user who backs out manually lands on Devices,
            // then a further B on Home's menu face (wherever it was left),
            // exactly retracing Home -> A -> Devices -> wizard.
            _ => Action::None,
        }
    }

    fn chrome_contribution(&self, _ctx: &RenderCtx) -> Option<ChromeContribution> {
        let phase = self.phase.borrow().clone();
        let mut contribution = ChromeContribution { y: Some(ButtonLabel::Inert), ..ChromeContribution::default() };
        match phase {
            WizardPhase::NothingFound | WizardPhase::Scanning { .. } | WizardPhase::Connecting { .. } => {
                contribution.x = Some(ButtonLabel::Inert);
            }
            WizardPhase::NotResponding { .. } => {
                contribution.x = Some(ButtonLabel::Live(String::from("keep")));
            }
            WizardPhase::Succeeded { degraded: _ } => {
                // pico-link-qdc: the codec picker (E11) doesn't exist yet,
                // so X must render Inert here regardless of `degraded` --
                // a live "codec" label with nowhere to go is exactly the
                // labelled-but-dead violation design rule 2 forbids.
                // Restore this to `Live("codec")` only alongside E11
                // actually landing (see `on_intent`'s `ShortcutX` comment,
                // which already treats this as a deliberate no-op).
                contribution.x = Some(ButtonLabel::Inert);
            }
            WizardPhase::Failed { reason, .. } => {
                contribution.x =
                    Some(if reason.retryable() { ButtonLabel::Live(String::from("retry")) } else { ButtonLabel::Inert });
            }
        }
        Some(contribution)
    }

    /// A's liveness and label, per design rule 4 -- see
    /// `.planning/design/2026-09-02-a-button-label-rule.md` §4's
    /// assignment table. `Scanning` delegates to the wrapped `list`
    /// (`Verb::Pair` for a real device row, `None` for the non-device
    /// backstop readout row -- see `build_scan_list`'s `.inert()` call).
    fn activation(&self) -> Option<Verb> {
        match &*self.phase.borrow() {
            WizardPhase::NothingFound => Some(Verb::Scan),
            WizardPhase::Scanning { .. } => {
                self.sync_list();
                self.list.borrow().activation()
            }
            WizardPhase::Connecting { .. }
            | WizardPhase::NotResponding { .. }
            | WizardPhase::Succeeded { .. }
            | WizardPhase::Failed { .. } => None,
        }
    }

    /// Requests a periodic redraw while phase 4 (Connecting) is showing,
    /// so its elapsed-seconds readout (see [`render_connecting_steps`])
    /// actually advances instead of freezing at whatever value happened to
    /// be on screen when the last real event arrived. Every other phase
    /// has nothing time-driven to show, so this returns `None` -- see the
    /// frame-scoped clock ADR's "hacks to retire" section for why this
    /// stays scoped rather than becoming an unconditional per-tick redraw.
    ///
    /// Folded with the wrapped `list`'s own answer (pico-link-vxc, D2),
    /// `min`-of-children matching [`Screen::redraw_after`]'s fold over
    /// multiple widgets -- `VerticalList` has no time-driven content
    /// today, but the wrapper must not be the thing that silently drops a
    /// future one under the dirty gate.
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        let own = if matches!(*self.phase.borrow(), WizardPhase::Connecting { .. }) {
            Some(ELAPSED_REDRAW_INTERVAL)
        } else {
            None
        };
        [own, self.list.borrow().redraw_after(ctx)].into_iter().flatten().min()
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
            WizardPhase::Connecting { step, started, .. } => {
                render_connecting_steps(area, step, ctx.elapsed_since(started), target);
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
    use crate::app::{
        App, ConnectFailureReason, ConnectOutcomeResult, ConnectStep, DeviceEntry, Event, HomeFace, LinkState, WizardPhase, DEVICES_TITLE,
    };
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
    fn selecting_scan_opens_the_wizard_directly_into_scanning() {
        // pico-link-4vb.2: the old instructions phase 1 is gone -- opening
        // the wizard now enters `WizardPhase::Scanning` immediately and
        // queues `Command::StartScan` in the same step, with no A press
        // required first.
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        assert_eq!(app.navigator_depth(), 3);
        assert_eq!(app.current_screen_title(), WIZARD_TITLE);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Scanning { started: untimed() });
        assert_eq!(app.poll_command(), Some(Command::StartScan));
    }

    #[test]
    fn a_device_arriving_while_scanning_does_not_change_the_phase() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1; 6], name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Scanning { started: untimed() });
        assert_eq!(app.navigator_depth(), 3, "the wizard must still be the top screen at Home(1)/Devices(2)/Wizard(3)");
    }

    #[test]
    fn scan_ending_with_zero_devices_moves_to_nothing_found() {
        // Bead pico-link-88xs: scan-end detection moved off
        // `LinkStateChanged` onto `Event::DiscoveryStateChanged` -- see
        // `App::on_scan_ended_if_applicable`'s doc comment.
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });
        app.handle_event(Event::DiscoveryStateChanged { scanning: false });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NothingFound);
    }

    #[test]
    fn scan_ending_with_devices_present_stays_on_scanning() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [2; 6], name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
        app.handle_event(Event::DiscoveryStateChanged { scanning: false });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Scanning { started: untimed() });
    }

    #[test]
    fn selecting_a_device_row_queues_connect_and_enters_the_connecting_phase() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [3; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]); // activate the (only) row
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: untimed() });
        assert_eq!(app.poll_command(), Some(Command::StartScan));
        app.expect_connect_command_for_test(addr, "Cans");
    }

    #[test]
    fn a_non_audio_class_of_device_is_excluded_from_the_scan_list_while_an_audio_sink_is_included() {
        // Bead pico-link-znb.11 (E9), design section 9 phase 2 rule 3.
        // Discover a phone first (major device class 0x01), then a real
        // pair of headphones (major device class 0x04, Audio/Video) --
        // if the phone is correctly filtered out, the headphones must be
        // the *only* row left, so pressing Select on the default (index 0)
        // selection connects to them, not the phone.
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let phone_addr = [1; 6];
        let phone_cod = 0x20_02_0C; // major device class 0x02 (Phone), minor 0x0C (smartphone)
        app.handle_event(Event::DeviceDiscovered(DeviceEntry {
            addr: phone_addr,
            name: String::from("Somebody's Phone"),
            rssi: -40,
            class_of_device: phone_cod,
        }));
        let headphones_addr = [2; 6];
        let headphones_cod = 0x24_04_04; // major device class 0x04 (Audio/Video), wearable headset
        app.handle_event(Event::DeviceDiscovered(DeviceEntry {
            addr: headphones_addr,
            name: String::from("Cans"),
            rssi: -40,
            class_of_device: headphones_cod,
        }));

        app.handle_input(vec![NavIntent::Select]); // activate whatever landed at index 0
        assert_eq!(app.poll_command(), Some(Command::StartScan));
        app.expect_connect_command_for_test(headphones_addr, "Cans");
    }

    #[test]
    fn a_device_with_unreported_class_of_device_is_never_silently_excluded() {
        // Judgement call made in this bead: `class_of_device == 0` (no
        // "available" flag exists for this inquiry-result field, unlike
        // name/rssi) means BTstack reported nothing, not "not audio" --
        // see `is_audio_sink`'s doc comment. A device with an unreported
        // class must still be selectable, or a device the user can see in
        // the room becomes permanently, silently unpairable.
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [7; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry {
            addr,
            name: String::from("Mystery Cans"),
            rssi: -40,
            class_of_device: 0,
        }));
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::StartScan));
        app.expect_connect_command_for_test(addr, "Mystery Cans");
    }

    #[test]
    fn the_scan_list_caps_at_max_scan_list_items_but_the_cap_itself_stays_selectable() {
        // Design section 21 Tier 1 row E9 / section 13's Class-of-Device
        // row: the 12-item cap is a backstop built regardless of whether
        // filtering works, because Class-of-Device is only *Expected*, not
        // *Confirmed*. 13 audio-classed devices discovered; the 12th (the
        // cap boundary) must still be reachable and connectable.
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        for i in 1..=13u8 {
            app.handle_event(Event::DeviceDiscovered(DeviceEntry {
                addr: [i; 6],
                name: format!("Device {i}"),
                rssi: -40,
                class_of_device: 0x24_04_04,
            }));
        }
        // 11 Down presses from the default index 0 reaches index 11 -- the
        // 12th (last, cap-boundary) device row.
        app.handle_input(vec![NavIntent::Down; 11]);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::StartScan));
        app.expect_connect_command_for_test([12; 6], "Device 12");
    }

    #[test]
    fn past_the_cap_the_readout_row_is_a_harmless_no_op_not_the_13th_device() {
        // Companion to the cap test above: the row one past
        // `MAX_SCAN_LIST_ITEMS` is the "showing N of M" backstop readout,
        // not a 14th device row -- activating it must not queue a Connect
        // for anything (in particular, not for the 13th discovered device,
        // which the cap excludes from the list entirely).
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        for i in 1..=13u8 {
            app.handle_event(Event::DeviceDiscovered(DeviceEntry {
                addr: [i; 6],
                name: format!("Device {i}"),
                rssi: -40,
                class_of_device: 0x24_04_04,
            }));
        }
        // 12 Down presses from index 0 reaches index 12 -- one past the
        // last real device row (index 11), landing on the readout row.
        app.handle_input(vec![NavIntent::Down; 12]);
        app.handle_input(vec![NavIntent::Select]);
        assert_eq!(app.poll_command(), Some(Command::StartScan));
        assert_eq!(
            app.poll_command(),
            None,
            "the backstop readout row must not resolve to a device -- selecting it must be a pure no-op"
        );
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
        assert_eq!(
            app.wizard_phase_for_test(),
            WizardPhase::Scanning { started: Instant::from_micros(999_000) },
            "a freshly entered Scanning phase must be stamped with App's real now_us, not left pending or zero"
        );

        let addr = [7; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
        app.tick(1_500_000);
        app.handle_input(vec![NavIntent::Select]); // Scanning -> Connecting
        assert_eq!(
            app.wizard_phase_for_test(),
            WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: Instant::from_micros(1_500_000) },
            "a freshly entered Connecting phase must be stamped with App's real now_us at the instant it was entered"
        );
    }

    #[test]
    fn connecting_phase_liveness_end_to_end_tick_alone_marks_dirty_and_the_elapsed_readout_changes() {
        // pico-link-znb.10 step 6's real consumer, as a regression test:
        // the whole RenderCtx/redraw_after/event-timestamp seam, proven
        // end to end through phase 4's elapsed-seconds readout. See
        // `core/examples/wizard_liveness_probe.rs` for the zoomed-PNG
        // version of this same scenario (this project's rendering-
        // verification rule: a pixel-difference assertion alone is not
        // sufficient evidence for a render change, only a necessary one).
        let mut app = App::new(240, 240);
        app.tick(0);
        open_wizard(&mut app);
        let addr = [9; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from("Cans"), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]); // Scanning -> Connecting, started == 0

        app.tick(0);
        let pixels_t0: Vec<_> = app.render().pixels().collect();
        assert!(!app.dirty(), "render() must have cleared dirty");

        // No input, no event -- purely the passage of time. This is
        // exactly the case a naive "no timestamp seam" implementation
        // cannot handle: nothing *happened*, but the screen must still
        // show the connect attempt is still alive.
        app.tick(2_000_000);
        assert!(app.dirty(), "redraw_after must mark the app dirty from tick alone, 2s into a Connecting attempt");
        let pixels_t1: Vec<_> = app.render().pixels().collect();

        assert_ne!(
            pixels_t0, pixels_t1,
            "the elapsed-seconds readout must actually change the rendered pixels between t=0s and t=2s"
        );
    }

    #[test]
    fn connect_step_events_advance_the_named_sub_steps() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [4; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);

        app.handle_event(Event::ConnectStepChanged(ConnectStep::Pairing, 1));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Pairing, started: untimed() });

        app.handle_event(Event::ConnectStepChanged(ConnectStep::SettingUpAudio, 1));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::SettingUpAudio, started: untimed() });
    }

    #[test]
    fn connect_step_changed_is_ignored_outside_the_connecting_phases() {
        let mut app = App::new(240, 240);
        // Wizard not even open -- default phase is NothingFound.
        app.handle_event(Event::ConnectStepChanged(ConnectStep::Pairing, 1));
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NothingFound);
    }

    #[test]
    fn retrying_surfaces_not_responding_with_an_incrementing_attempt_counter() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [5; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);

        app.handle_event(Event::ConnectRetrying { attempt: 1, seq: 1 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NotResponding { addr, attempt: 1 });

        app.handle_event(Event::ConnectRetrying { attempt: 2, seq: 1 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::NotResponding { addr, attempt: 2 });
    }

    #[test]
    fn keep_trying_from_not_responding_reissues_connect_and_returns_to_connecting() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [6; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);
        app.handle_event(Event::ConnectRetrying { attempt: 1, seq: 1 });
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain the first Connect

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: untimed() });
        app.expect_connect_command_for_test(addr, "");
    }

    #[test]
    fn connect_succeeded_reaches_the_plain_success_outcome() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [7; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);

        let seq_for_test_1041 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1041 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded: false });
    }

    #[test]
    fn wizard_auto_dismiss_pops_all_the_way_to_home_only_after_plain_success() {
        // pico-link-4vb.2: Andreas wanted auto-dismiss to land on Home, not
        // Devices, so he doesn't have to press Back a bunch of times.
        let mut app = App::new(240, 240);
        open_wizard(&mut app); // Home status -> menu face -> Devices -> wizard
        let addr = [8; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);
        let seq_for_test_1042 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1042 });
        assert_eq!(app.navigator_depth(), 3);

        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 1, "a plain success must auto-dismiss all the way back to Home");
        // pico-link-l4d: `open_wizard` reaches the wizard via Home's MENU
        // face (Home -> A -> Devices -> pair), so before the fix this
        // landed back on the Bluetooth/Settings list, not the hero --
        // `navigator_depth() == 1` alone can't see that, only the face can.
        assert_eq!(
            app.home_face_for_test(),
            HomeFace::Status,
            "auto-dismiss must land on the status hero, not whatever face was showing when the wizard was opened"
        );
    }

    #[test]
    fn background_connect_success_does_not_navigate_away_from_another_screen() {
        // pico-link-vuou: C arms the auto-dismiss timer on *every*
        // `ConnectSucceeded`, not just wizard-initiated ones (a device-page
        // relink, a boot auto-reconnect, a future web CONNECT). Before the
        // fix, `on_wizard_auto_dismiss` only checked `WizardPhase` -- which
        // `on_connect_succeeded` sets unconditionally -- so this scenario
        // yanked the user back to Home ~2s later from wherever they were.
        let mut app = App::new(240, 240);
        app.handle_input(vec![NavIntent::Select]); // Home status -> menu face
        app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
        assert_eq!(app.navigator_depth(), 2, "sitting on Devices, not the wizard");

        let addr = [10; 6];
        let seq_for_test_1043 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1043 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded: false }, "C sets this regardless of screen");

        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 2, "a connect success must not pop a screen the wizard didn't push");
        assert_eq!(app.home_face_for_test(), HomeFace::Menu, "must not force Home's face either when the wizard isn't on top");
    }

    #[test]
    fn degraded_success_does_not_auto_dismiss() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [9; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);
        let seq_for_test_1044 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: true, seq: seq_for_test_1044 });

        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 3, "degraded success must require acknowledgement, never auto-dismiss");
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Succeeded { degraded: true });
    }

    #[test]
    fn connect_failed_reaches_the_failed_phase_with_its_reason() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [10; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);

        let seq_for_test_1045 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink, seq: seq_for_test_1045 });
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Failed { addr, reason: ConnectFailureReason::NoA2dpSink });
    }

    #[test]
    fn retry_is_offered_only_for_retryable_failure_reasons() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [11; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);
        let seq_for_test_1046 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink, seq: seq_for_test_1046 });
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
        let addr = [12; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]);
        let seq_for_test_1047 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::Timeout, seq: seq_for_test_1047 });
        app.poll_command();
        app.poll_command();

        app.handle_input(vec![NavIntent::ShortcutX]);
        assert_eq!(app.wizard_phase_for_test(), WizardPhase::Connecting { addr, step: ConnectStep::Connecting, started: untimed() });
        app.expect_connect_command_for_test(addr, "");
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
    fn b_aborts_from_freshly_opened_scanning() {
        // pico-link-4vb.2: the wizard opens straight into Scanning now (the
        // old instructions phase 1 is gone), so this is what "B from the
        // very first phase" means today. `b_aborts_from_scanning_and_
        // cancels_the_scan` below covers the CancelScan side effect
        // specifically; this one just proves the generic pop.
        assert_back_aborts_from(|_app| {});
    }

    #[test]
    fn b_aborts_from_scanning_and_cancels_the_scan() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
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
            app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        });
    }

    #[test]
    fn b_aborts_from_connecting_and_queues_cancel_connect() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [13; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        app.handle_input(vec![NavIntent::Back]);
        assert_eq!(app.navigator_depth(), 2, "B must pop the wizard back to Devices");
        assert_eq!(
            app.poll_command(),
            Some(Command::CancelConnect { addr, seq: 1 }),
            "B during the connecting phase must queue CancelConnect (design section 9: 'B genuinely aborts') --              leaving the screen without this leaves the abandoned ACL/SSP/AVDTP attempt running in C"
        );
    }

    /// Bead pico-link-jyhk.25 review fix: B during `Connecting` must
    /// conclude `BtModel::attempt` immediately (not just queue
    /// `Command::CancelConnect` and leave the radio-session record
    /// dangling) -- a fire-and-forget C cancel only ever echoes back
    /// `LinkStateChanged(Idle)`, which doesn't conclude an attempt on its
    /// own (see `App::set_link_state`'s doc comment), so if `radio_actions::
    /// cancel_connect` didn't conclude it at queue time, `attempt` would
    /// stay `Some` forever and a future `GET_RADIO` snapshot would report a
    /// phantom in-flight connect.
    #[test]
    fn b_during_connecting_concludes_the_attempt_as_cancelled() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [16; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        let seq = app.model().attempt.expect("connect() must have recorded an attempt").seq;
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        app.handle_input(vec![NavIntent::Back]);
        app.poll_command(); // drain CancelConnect

        assert_eq!(app.model().attempt, None, "cancelling must conclude the attempt, not leave it dangling forever");
        let outcome = app.model().last_outcome.expect("cancelling must record a last_outcome");
        assert_eq!(outcome.seq, seq);
        assert_eq!(outcome.addr, addr);
        assert_eq!(outcome.result, ConnectOutcomeResult::Cancelled);

        // The only core-visible echo of a cancel C actually sends (see
        // `radio_actions::cancel_connect`'s doc comment) must not disturb
        // what the cancel already concluded.
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        assert_eq!(app.model().attempt, None);
        assert_eq!(app.model().last_outcome.expect("must survive the Idle echo").result, ConnectOutcomeResult::Cancelled);
    }

    /// ADA DESIGN v2 (bead `pico-link-chc3`): a `ConnectSucceeded` arriving
    /// after the user cancelled the attempt that requested it is a real,
    /// new connection -- C's cancel is fire-and-forget and does not
    /// suppress an outcome already in flight on the wire, so if the link
    /// genuinely comes up, `core` must report it as connected, not swallow
    /// it as a stray echo. Supersedes bead `pico-link-jyhk.25`'s
    /// `stray_connect_succeeded_after_cancel_does_not_resurrect_the_attempt`,
    /// which asserted the opposite and, by keying suppression on address
    /// alone, also would have false-dropped a headset-initiated reconnect
    /// to the same address.
    #[test]
    fn connect_succeeded_after_cancel_is_reported_as_a_real_connection() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [17; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        app.handle_input(vec![NavIntent::Back]);
        app.poll_command(); // drain CancelConnect
        assert_eq!(app.model().attempt, None, "cancelling must conclude the attempt");

        // The link actually came up after the cancel was sent -- C's cancel
        // doesn't suppress an outcome already in flight on the wire.
        let seq_for_test_1048 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1048 });

        assert_eq!(app.model().connected_addr, Some(addr), "a genuine post-cancel success must be reported as connected");
        assert_eq!(app.model().attempt, None, "a concluded success leaves no in-flight attempt");
        assert_eq!(
            app.model().last_outcome.expect("must record the new outcome").result,
            ConnectOutcomeResult::Ok,
            "the real success must overwrite Cancelled, not be swallowed by it"
        );
    }

    #[test]
    fn b_aborts_from_not_responding_and_queues_cancel_connect() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        let addr = [14; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        app.handle_event(Event::ConnectRetrying { attempt: 1, seq: 1 }); // -> NotResponding
        app.poll_command(); // drain StartScan
        app.poll_command(); // drain Connect

        app.handle_input(vec![NavIntent::Back]);
        assert_eq!(app.navigator_depth(), 2, "B must pop the wizard back to Devices");
        assert_eq!(
            app.poll_command(),
            Some(Command::CancelConnect { addr, seq: 1 }),
            "B during the not-responding phase must queue CancelConnect, same as the connecting phase"
        );
    }

    #[test]
    fn b_aborts_from_succeeded() {
        assert_back_aborts_from(|app| {
            app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [15; 6], name: String::new(), rssi: -40, class_of_device: 0 }));
            app.handle_input(vec![NavIntent::Select]);
            let seq_for_test_17 = app.seed_connect_attempt_for_test([15; 6]);
            app.handle_event(Event::ConnectSucceeded { addr: [15; 6], degraded: true, seq: seq_for_test_17 });
        });
    }

    #[test]
    fn b_aborts_from_failed() {
        assert_back_aborts_from(|app| {
            app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [16; 6], name: String::new(), rssi: -40, class_of_device: 0 }));
            app.handle_input(vec![NavIntent::Select]);
            let seq_for_test_18 = app.seed_connect_attempt_for_test([16; 6]);
            app.handle_event(Event::ConnectFailed { addr: [16; 6], reason: ConnectFailureReason::RadioError, seq: seq_for_test_18 });
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

        let addr = [17; 6];
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::new(), rssi: -40, class_of_device: 0 }));
        assert_eq!(app.navigator_depth(), 3);
        app.handle_input(vec![NavIntent::Select]); // -> Connecting
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::ConnectStepChanged(ConnectStep::Pairing, 1));
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::ConnectRetrying { attempt: 1, seq: 1 });
        assert_eq!(app.navigator_depth(), 3);
        let seq_for_test_1049 = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1049 });
        assert_eq!(app.navigator_depth(), 3);
        app.handle_event(Event::WizardAutoDismiss);
        assert_eq!(app.navigator_depth(), 1, "auto-dismiss returns all the way to Home (pico-link-4vb.2: no more Back-Back-Back)");
    }

    // --- bead pico-link-bgnd M4: the scan list reads `model.discovered`
    // live, and a device arriving mid-scan must not disturb the cursor ---

    #[test]
    fn a_device_arriving_mid_scan_does_not_move_the_selection_off_the_row_the_user_is_on() {
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        for i in 1..=3u8 {
            app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [i; 6], name: format!("Device {i}"), rssi: -40, class_of_device: 0 }));
        }
        app.render(); // establish the list
        app.handle_input(vec![NavIntent::Down]); // focus row 1 ("Device 2")
        assert_eq!(app.wizard_selected_index_for_test(), Some(1));

        // A fourth device arrives -- purely additive, must not reset the
        // cursor back to row 0 or move it off "Device 2".
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [4; 6], name: String::from("Device 4"), rssi: -40, class_of_device: 0 }));
        app.render();
        assert_eq!(
            app.wizard_selected_index_for_test(),
            Some(1),
            "a device arriving mid-scan must not move the selection off the row the user is on"
        );
    }

    #[test]
    fn a_device_arriving_mid_scan_keeps_the_selection_on_the_last_row_past_the_default_viewport() {
        // `PairingWizardView` doesn't forward `Widget::scroll_top` (a
        // pre-existing gap, not this bead's to fix), so this proves the
        // same "no cursor jump" guarantee at a selection index deep enough
        // that a naive rebuild-from-scratch would have reset it to row 0.
        let mut app = App::new(240, 240);
        open_wizard(&mut app);
        for i in 1..=13u8 {
            app.handle_event(Event::DeviceDiscovered(DeviceEntry {
                addr: [i; 6],
                name: format!("Device {i}"),
                rssi: -40,
                class_of_device: 0x24_04_04,
            }));
        }
        app.handle_input(vec![NavIntent::Down; 11]); // the 12th (cap-boundary) device row
        app.render();
        assert_eq!(app.wizard_selected_index_for_test(), Some(11));

        // A late-arriving name update for an already-shown device (the
        // same in-place-update path `add_device` uses for a repeat report)
        // must not move the cursor.
        app.handle_event(Event::DeviceDiscovered(DeviceEntry {
            addr: [5; 6],
            name: String::from("Device 5 (resolved)"),
            rssi: -40,
            class_of_device: 0x24_04_04,
        }));
        app.render();
        assert_eq!(app.wizard_selected_index_for_test(), Some(11), "an in-place device update must not move the selection");
    }
}
