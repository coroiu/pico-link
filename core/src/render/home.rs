//! Home: the app's root screen, with two **faces** rather than two screens
//! (design section 4's Home exception, section 6 status face, section 7
//! menu face -- bead `pico-link-znb.8`/E7).
//!
//! # Why a face, not a push
//!
//! As a pushed screen, `Home -> Menu -> Devices -> Pair` would sit at
//! navigator depth 3 (design's 0-indexed counting), which breaks `B, B` --
//! the *only* escape gesture this product has, because input is
//! press-edge-only with no long-press (`firmware/src/input.c:120`). Home
//! toggling between its status and menu faces must never touch
//! [`super::Navigator::push`]/[`super::Navigator::pop`] -- only
//! [`HomeView`]'s own internal state (via [`crate::app::HomeFace`]) changes.
//! This mirrors `wizard.rs`'s "phases replace, never push" shape exactly,
//! just with two faces instead of six phases.
//!
//! # Why the face lives in an `Rc<RefCell<_>>`, not a `HomeView` field
//!
//! Unlike the wizard (pushed once when the user opens it, then never
//! rebuilt -- see `wizard.rs`'s module doc), Home *is* the root screen,
//! rebuilt on every Bluetooth model change via
//! [`crate::app::App::rebuild_root`]. A face stored only on a `HomeView`
//! field would be silently discarded by the very next rebuild -- exactly
//! the defect class `pico-link-a67`'s `Navigator::replace_root` fix
//! already closed for screen-stack depth. [`crate::app::App::home_face`]
//! is the shared, rebuild-surviving source of truth; [`HomeView`] only
//! ever reads and writes through the `Rc` it's handed, the same shape
//! `PairingWizardView` uses for `wizard_phase`.
//!
//! # The Home input exception (design section 4, stated once, only here)
//!
//! Home has no focusable list on the status face, so "move focus" and
//! "activate the focused thing" are vacuous there. On Home *only*, centre
//! (and physical button A -- see the module doc on why those two are
//! indistinguishable at the `NavIntent` level) toggles the face. Up/Down
//! is Tier 2 volume (`E16`, gated on an AVRCP capability flag not built
//! yet) -- **left unbound** in Tier 1, per design section 13's "absent
//! together, not inert" rule: no gauge is drawn, so no binding exists
//! either, rather than a binding that visibly does nothing.
//!
//! # A and centre are the same signal -- this is spec-compliant, not a gap
//!
//! Design section 4's global input contract states "Centre: identical to
//! A" as an unconditional rule, and `firmware/src/input.c`'s `PINS` table
//! maps both the joystick's centre press and the physical A button to the
//! same `PL_INTENT_TAG_SELECT` accordingly ("Center joystick press and
//! button A both mean Select"). So on Home's status face, pressing
//! physical A does exactly what centre does -- opens the menu face,
//! landing on its pre-selected "Bluetooth" row (which is where "devs"
//! ultimately leads, one more `Select` away) -- rather than jumping to
//! Devices directly. [`HomeView::chrome_contribution`] still labels the A
//! slot "devs" (the design's literal text), which now overpromises by one
//! hop; that label mismatch is real but is a separate, low-priority,
//! already-filed follow-up, not this bead's to fix -- the toggle-first
//! behavior itself is exactly what the design's own global rule requires.
//!
//! # X is left inert on both faces
//!
//! Design section 4's table gives Home's status face `X: link`/`why?`
//! (Device detail) and the menu face `X: manage connected device` (also
//! Device detail). Device detail is `pico-link-znb.13` (E11), not built
//! yet. Per design section 4 rule 2 ("an unlabelled X or Y does nothing" --
//! implying a *labelled* one must do something), labelling X here without
//! a real destination would be worse than leaving it unlabelled: a
//! pressable-looking button that silently does nothing is exactly the
//! "mispress that isn't free" the rule exists to prevent. Left unlabelled
//! and unbound on both faces until E11 lands.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::convert::Infallible;

use embedded_graphics::prelude::Size;
use embedded_graphics::primitives::Rectangle;

use crate::app::{build_devices_screen, build_settings_screen, BtModel, Command, DeviceEntry, HomeFace, LinkState, WizardPhase};
use crate::input::NavIntent;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::hero::{BitrateStatus, CodecStatus, HeroStatusView, OutLevelDisplay};
use super::menu::{MenuItem, MenuList};
use super::rail::ButtonLabel;
use super::screen::Screen;
use super::widget::{Action, ChromeContribution, FocusEvent, Widget};

/// Home's fixed title -- the "Pico Link" brand mark, now that Home (not
/// Devices) is the navigator root (design section 5's screen inventory).
pub const HOME_TITLE: &str = "Pico Link";

/// The menu face's fixed row order (design section 7: "Rows: Bluetooth,
/// Settings", no "Home" row -- B already does that, and a row meaning "go
/// back" is wasted on a 240px screen).
const MENU_ROW_BLUETOOTH: usize = 0;
const MENU_ROW_SETTINGS: usize = 1;

/// Builds the Home screen at whatever face `home_face` currently holds --
/// see the module doc for why no further rebuild is needed as the face
/// toggles (unlike [`crate::app::App::rebuild_root`], which *does* rebuild
/// this screen on every Bluetooth model change, for the same reason it
/// always has: the status face's hero widget needs to reflect live data).
#[must_use]
pub fn build_home_screen(
    model: &BtModel,
    home_face: &Rc<RefCell<HomeFace>>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    wizard_phase: &Rc<RefCell<WizardPhase>>,
    wizard_devices: &Rc<RefCell<Vec<DeviceEntry>>>,
) -> Screen {
    let view = HomeView::new(model, Rc::clone(home_face), commands, wizard_phase, wizard_devices);
    // `handles_back(true)`: Home's B slot should render live even at
    // navigator depth 1 (`can_go_back` false, nothing behind the root to
    // pop to) whenever there's a face for B to back out of -- see
    // `Screen::handles_back`'s doc comment, which names this exact
    // scenario. Kept unconditionally true (not toggled per-face) since
    // `Screen`'s `handles_back` flag is fixed at construction time and
    // this screen is rebuilt on model changes, not on face toggles --
    // see the module doc's `Rc<RefCell<_>>` rationale. B on the status
    // face is a harmless no-op, same as any other screen's B at the root.
    Screen::new(HOME_TITLE, vec![Box::new(view)]).handles_back(true)
}

/// Home's sole top-level content widget -- one composite `Widget` owning
/// both faces' rendering and input handling, per the `HeroStatusView`/
/// `ConfirmView` "composite widget with its own internal vertical rhythm"
/// pattern (see `hero.rs`'s module doc) -- not two widgets stacked on the
/// `Screen`, since only one face is ever visible at a time and `Screen`'s
/// stacking model has no notion of "hide this widget, show that one".
struct HomeView {
    home_face: Rc<RefCell<HomeFace>>,
    hero: HeroStatusView,
    menu: MenuList,
    /// The live Bluetooth link state, for the title bar's Bluetooth
    /// glyph ([`ChromeContribution::link`]) -- the one piece of live
    /// `BtModel` data this widget *does* have plumbed in (unlike the hero
    /// widget's codec/bitrate fields; see [`HomeView::new`]'s doc
    /// comment).
    link_state: LinkState,
}

impl HomeView {
    fn new(
        model: &BtModel,
        home_face: Rc<RefCell<HomeFace>>,
        commands: &Rc<RefCell<VecDeque<Command>>>,
        wizard_phase: &Rc<RefCell<WizardPhase>>,
        wizard_devices: &Rc<RefCell<Vec<DeviceEntry>>>,
    ) -> Self {
        // The status face's hero widget: `NO LINK` whenever there is no
        // live codec (design section 15's "absent, never frozen or
        // faked" rule -- this covers Idle/Scanning/Connecting alike, not
        // just a bare disconnect), otherwise the connected device's name
        // plus the codec word and nominal bitrate `BtModel::
        // connected_codec` carries (bead pico-link-1v5). `fallback` stays
        // `None` -- the data needed to say *why* a codec fell back to a
        // lesser one (design section 6.2's amber banner) doesn't exist in
        // `BtModel` yet; that's a separate, later bead, and `None` here is
        // the honest "no reason recorded" value, not a guess.
        let hero = match &model.connected_codec {
            Some(codec) => {
                // Bead pico-link-4vb.4 (T4): reads `paired` (the remembered
                // list), not the old `discovered` scan list -- the whole
                // point of this bead is that a device's name must survive
                // long after the scan that first discovered it is gone.
                let device_name =
                    model.paired.iter().find(|d| d.addr == codec.addr).map(|d| d.name.clone()).unwrap_or_default();
                let bitrate = BitrateStatus::Kbps(codec.nominal_bitrate_bps / 1000);
                // Bead pico-link-du0 (design section 21 E17/C8): the
                // stereo OUT level meter -- a straight field-for-field
                // translation from `BtModel::out_level`'s own
                // `OutLevelSample` into the hero widget's decoupled
                // `OutLevelDisplay` vocabulary (see that type's doc
                // comment for why the translation lives here). `None`
                // when there's no live reading yet, which the hero widget
                // already renders as "no meter" -- no extra Idle/absent
                // branching needed on this side of the seam.
                let out_level = model.out_level.map(|level| OutLevelDisplay {
                    peak_l: level.peak_l,
                    peak_r: level.peak_r,
                    rms_l: level.rms_l,
                    rms_r: level.rms_r,
                    hold_l: level.hold_l,
                    hold_r: level.hold_r,
                    received_at: level.received_at,
                });
                HeroStatusView::new(
                    device_name,
                    CodecStatus::Connected { word: codec.word.clone(), fallback: None, bitrate },
                )
                .with_out_level(out_level)
            }
            None => HeroStatusView::new("", CodecStatus::NoLink),
        };
        let link_state = model.link_state;

        let model = model.clone();
        let commands_for_bluetooth = Rc::clone(commands);
        let wizard_phase_for_bluetooth = Rc::clone(wizard_phase);
        let wizard_devices_for_bluetooth = Rc::clone(wizard_devices);
        let menu = MenuList::new(vec![MenuItem::new("Bluetooth"), MenuItem::new("Settings")]).on_activate_index(
            move |index| match index {
                MENU_ROW_BLUETOOTH => {
                    let model = model.clone();
                    let commands = Rc::clone(&commands_for_bluetooth);
                    let wizard_phase = Rc::clone(&wizard_phase_for_bluetooth);
                    let wizard_devices = Rc::clone(&wizard_devices_for_bluetooth);
                    Action::PushView(Box::new(move || build_devices_screen(&model, None, 0, None, &commands, &wizard_phase, &wizard_devices)))
                }
                MENU_ROW_SETTINGS => Action::PushView(Box::new(build_settings_screen)),
                _ => Action::None,
            },
        );

        Self { home_face, hero, menu, link_state }
    }

    fn face(&self) -> HomeFace {
        *self.home_face.borrow()
    }
}

impl Widget for HomeView {
    fn measure(&self, constraints: Size, _ctx: &RenderCtx) -> Size {
        constraints
    }

    /// Always focusable, on both faces -- this is what makes centre
    /// (`NavIntent::Select`) reach [`Widget::on_focus`] even on the status
    /// face, which has no focusable *list* of its own (the module doc's
    /// Home input exception).
    fn is_focusable(&self) -> bool {
        true
    }

    fn on_focus(&mut self, event: FocusEvent) -> Action {
        match event {
            FocusEvent::Gained | FocusEvent::Lost => {
                // Forwarded to the menu unconditionally (not just while
                // its face is showing) purely so its own `focused` bool
                // stays in sync for whenever the menu face next renders --
                // harmless on the status face, where the menu isn't drawn
                // at all.
                self.menu.on_focus(event)
            }
            FocusEvent::Activated => match self.face() {
                // Status face: centre/A toggles to the menu face -- see
                // the module doc for why physical A does this too, rather
                // than jumping straight to Devices.
                HomeFace::Status => {
                    *self.home_face.borrow_mut() = HomeFace::Menu;
                    self.menu.on_focus(FocusEvent::Gained)
                }
                // Menu face: centre/A activates whichever row is
                // selected (Bluetooth/Settings) -- ordinary menu
                // activation, delegated to the wrapped `MenuList`.
                HomeFace::Menu => self.menu.on_focus(FocusEvent::Activated),
            },
        }
    }

    fn on_intent(&mut self, intent: NavIntent) -> Action {
        match intent {
            NavIntent::Back => {
                // B returns from the menu face to the status face. On the
                // status face this is a no-op (Home is the root; nothing
                // to back out of) -- `Navigator::dispatch`'s `Back` arm
                // discards whatever `Action` this returns and pops
                // unconditionally, which itself no-ops at the root, so
                // returning `Action::None` here is correct either way.
                if self.face() == HomeFace::Menu {
                    *self.home_face.borrow_mut() = HomeFace::Status;
                }
                // pico-link-l4d POLICY: this is the only place a manual B
                // ever changes `home_face`, and it's a local toggle, not a
                // pop. "B from Devices back to Home" (`Navigator`'s plain
                // pop) deliberately does NOT also reset the face -- see
                // `wizard.rs`'s `(Back, Succeeded)` fallthrough for the
                // full policy statement: automatic dismiss chooses the
                // destination (always the status hero); manual B retraces
                // the user's own steps one screen at a time.
                Action::None
            }
            NavIntent::Up | NavIntent::Down | NavIntent::JumpBy(_) => match self.face() {
                // Tier 1 scope boundary (design section 13): Up/Down means
                // volume on the status face, but that binding is Tier 2
                // (E16, gated on an AVRCP capability flag). The gauge and
                // the binding are absent together, not inert -- so no
                // binding here, rather than one that visibly does
                // nothing.
                HomeFace::Status => Action::None,
                HomeFace::Menu => self.menu.on_intent(intent),
            },
            // Y: Settings, on the status face only (design section 4's
            // rail table gives the menu face's Y slot no meaning at all --
            // "-" -- since Settings is already one of its two rows).
            // Unlike A, Y is a genuinely distinct `NavIntent` from centre,
            // so this one *can* be wired to its real destination directly.
            NavIntent::ShortcutY => match self.face() {
                HomeFace::Status => Action::PushView(Box::new(build_settings_screen)),
                HomeFace::Menu => Action::None,
            },
            // X is left inert on both faces -- see the module doc's "X is
            // left inert on both faces" section for why (Device detail,
            // E11, isn't built yet). `Left`/`Right` have no meaning on
            // Home (design section 4: identical to B/A elsewhere, but
            // Home's exception already covers A/B/centre explicitly and
            // doesn't mention them). `Select` is handled entirely in
            // `on_focus` (`NavIntent::Select` reaches `Widget::on_focus`'s
            // `FocusEvent::Activated`, not `on_intent` -- see
            // `Navigator::dispatch`), so it never reaches here.
            NavIntent::ShortcutX | NavIntent::Left | NavIntent::Right | NavIntent::Select => Action::None,
        }
    }

    fn chrome_contribution(&self, ctx: &RenderCtx) -> Option<ChromeContribution> {
        match self.face() {
            HomeFace::Status => {
                let mut contribution = self.hero.chrome_contribution(ctx).unwrap_or_default();
                // "devs" is the design's literal label (section 4's rail
                // table) -- see the module doc for why the actual
                // immediate action is "open the menu face", not a direct
                // jump to Devices.
                contribution.a = Some(ButtonLabel::Live(String::from("devs")));
                contribution.y = Some(ButtonLabel::Live(String::from("set")));
                contribution.link = Some(self.link_state);
                Some(contribution)
            }
            HomeFace::Menu => {
                // A must not render dim here: unlike X/Y (design section
                // 4 rule 2's "unlabelled means inert" exemption is
                // granted only to X and Y), A's global meaning --
                // "activate the focused thing" -- is unconditional, and
                // the menu face plainly has a focused thing
                // (`HomeView::on_focus`'s `Activated` arm delegates
                // straight to `self.menu`, which pushes Devices/Settings
                // on the selected row). Leaving this `None` would fall
                // back to `ButtonLabels::default()` (all four slots
                // `Inert`), rendering A dim while it still silently
                // navigates on press -- exactly the "mispress on a dim
                // button is not free" violation `rail.rs`'s own
                // `ButtonLabel::Inert` doc comment promises can't happen.
                Some(ChromeContribution { a: Some(ButtonLabel::Live(String::from("select"))), ..Default::default() })
            }
        }
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        match self.face() {
            HomeFace::Status => self.hero.render(area, ctx, target),
            HomeFace::Menu => self.menu.render(area, ctx, target),
        }
    }

    /// The `min` over `hero` and `menu`'s own answers (pico-link-vxc, D2) --
    /// matching [`Screen::redraw_after`]'s fold over multiple widgets.
    /// Without this override the default (`None`) would silently swallow
    /// either child's time-driven request the moment one exists (neither
    /// does today), freezing Home under the dirty gate. Folded over both
    /// children regardless of which face is showing, same as `render`
    /// only draws the active one but `chrome_contribution` only asks the
    /// focused widget -- `redraw_after` intentionally does neither: a
    /// hidden face isn't rendered so it can't go stale, but computing the
    /// `min` unconditionally is simpler than face-gating it and costs
    /// nothing since both children default to `None` today.
    fn redraw_after(&self, ctx: &RenderCtx) -> Option<core::time::Duration> {
        [self.hero.redraw_after(ctx), self.menu.redraw_after(ctx)].into_iter().flatten().min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::BtModel;
    use crate::platform::Instant;

    fn test_ctx() -> RenderCtx {
        RenderCtx::at(Instant::from_micros(0))
    }

    fn fresh_home_view() -> HomeView {
        let model = BtModel::default();
        let home_face = Rc::new(RefCell::new(HomeFace::default()));
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let wizard_phase = Rc::new(RefCell::new(WizardPhase::default()));
        let wizard_devices = Rc::new(RefCell::new(Vec::new()));
        HomeView::new(&model, home_face, &commands, &wizard_phase, &wizard_devices)
    }

    /// Regression test for the review finding on this bead: `chrome_
    /// contribution` returning `None` for `HomeFace::Menu` fell back to
    /// `ButtonLabels::default()` (all four slots `Inert`), rendering A dim
    /// while `HomeView::on_focus`'s `Activated` arm still delegates
    /// straight to the wrapped `MenuList` -- so A silently navigated on
    /// press despite looking inert. `rail.rs`'s own `ButtonLabel::Inert`
    /// doc comment promises "a mispress on a dim button is always free";
    /// a labelled-but-dim A on the menu face broke that promise on the
    /// product's own root screen.
    #[test]
    fn a_is_live_on_the_menu_face_since_it_actually_activates_the_selected_row() {
        let mut view = fresh_home_view();
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.face(), HomeFace::Menu);

        let contribution = view.chrome_contribution(&test_ctx()).expect("the menu face must have a chrome opinion");
        assert!(
            matches!(contribution.a, Some(ButtonLabel::Live(_))),
            "A must render live on the menu face -- it activates the selected row, not a no-op"
        );
    }

    #[test]
    fn a_is_live_on_the_status_face_too_labelled_devs() {
        let view = fresh_home_view();
        assert_eq!(view.face(), HomeFace::Status);

        let contribution = view.chrome_contribution(&test_ctx()).expect("the status face must have a chrome opinion");
        assert!(matches!(contribution.a, Some(ButtonLabel::Live(_))), "A must render live on the status face too (it toggles the face)");
    }
}
