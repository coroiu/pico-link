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
//! As of `pico-link-bgnd` M1, `HomeView` is built exactly once (at
//! `App::new`) and lives for the app's lifetime -- it is no longer rebuilt
//! on every Bluetooth model event (see [`build_home_screen`]'s doc comment
//! and [`crate::app::App::refresh_stack`]'s `ScreenId::Home` arm, which now
//! returns `Refresh::Keep`). A rebuild-survival reason for the indirection
//! no longer applies, but a genuine *second writer* does:
//! [`crate::app::App`]'s `on_wizard_auto_dismiss` fold method forces the
//! face back to `Status` when the pairing wizard auto-dismisses, and it has
//! no other path to reach the live `HomeView` instance sitting inside the
//! `Navigator`'s stack (screens/widgets are opaque to `App` -- see
//! `wizard_phase`'s doc comment for the identical shape). So
//! [`crate::app::App::home_face`] stays the shared mailbox: not because a
//! plain field would be discarded by a rebuild (it no longer would), but
//! because `App` itself is the other writer, the same "genuine two-writer
//! state, not a rebuild workaround" exemption the design's WHAT ScreenCarry
//! BECOMES section grants `wizard_phase`.
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
//! Devices directly. [`HomeView::activation`] still labels the A slot
//! "devs" (the design's literal text, `Verb::Exception("devs")`), which
//! now overpromises by one hop; that label mismatch is real but is a
//! separate, low-priority, already-filed follow-up, not this bead's to
//! fix -- the toggle-first behavior itself is exactly what the design's
//! own global rule requires.
//!
//! # X is left inert on both faces (pico-link-hr30 superseding ruling)
//!
//! ANDREAS RULING 2026-09-07 (`pico-link-hr30`), superseding the older
//! split-by-face ruling this doc comment used to describe: **X opens the
//! fault strip detail, Y opens the device page, both on both faces** --
//! same destination regardless of which face is showing. Y is wired below
//! (device page when connected, an explicit "no device" message
//! otherwise -- see [`HomeView::on_intent`]'s `ShortcutY` arm). X is NOT
//! wired yet: the fault strip detail screen it targets does not exist
//! (`pico-link-9eq2.3`), and design section 4 rule 2 ("an unlabelled X or Y
//! does nothing" -- implying a *labelled* one must do something) means
//! labelling X without a real destination would be worse than leaving it
//! unlabelled -- a pressable-looking button that silently does nothing is
//! exactly the "mispress that isn't free" the rule exists to prevent. So X
//! stays `Action::None` and unlabelled (rail defaults it to
//! [`ButtonLabel::Inert`]) on both faces until `pico-link-9eq2.3` lands and
//! gives it a screen to push.

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

use crate::app::{
    build_device_page_screen, build_devices_screen, build_settings_screen, build_why_page_screen, BtModel, Command, DeviceAddr,
    DisplaySettingsState, FaultKey, FaultLog, HomeFace, LinkState, ModelHandle, Refresh, ScreenCarry, ScreenId, VolumeSource, WizardPhase,
    LDAC_QUALITY_ADAPTIVE,
};
use crate::input::NavIntent;
use crate::platform::Instant;

use super::ctx::RenderCtx;
use super::framebuffer::FrameBuffer565;
use super::hero::{BitrateStatus, CodecStatus, HeroStatusView, HeroVolume, HeroVolumeSource, OutLevelDisplay};
use super::menu::{MenuItem, MenuList};
use super::message::MessageView;
use super::paint_key::PaintKey;
use super::rail::ButtonLabel;
use super::screen::Screen;
use super::widget::{Action, ChromeContribution, FocusEvent, LinkGlyph, Verb, Widget};

/// Home's title for the explicit "no device connected" screen `ShortcutY`
/// pushes when there's nothing to open a device page for (bead
/// `pico-link-hr30`: "do not leave it a silent no-op, and do not crash").
const NO_DEVICE_TITLE: &str = "Device";

/// Seed for [`HomeView::paint_key`] -- only needs to differ from other
/// widgets' own seeds.
const HOME_PAINT_KEY_SEED: u64 = 15;

/// Home's fixed title -- the "Pico Link" brand mark, now that Home (not
/// Devices) is the navigator root (design section 5's screen inventory).
pub const HOME_TITLE: &str = "Pico Link";

/// The menu face's fixed row order (design section 7: "Rows: Bluetooth,
/// Settings", no "Home" row -- B already does that, and a row meaning "go
/// back" is wasted on a 240px screen).
const MENU_ROW_BLUETOOTH: usize = 0;
const MENU_ROW_SETTINGS: usize = 1;

/// Builds the Home screen exactly once -- `pico-link-bgnd`'s only caller is
/// `App::new`; `App::refresh_stack`'s `ScreenId::Home` arm now returns
/// `Refresh::Keep` and never calls this again (see that arm's doc comment).
/// The status face's hero widget stays live by reading `model` (a live
/// [`ModelHandle`], not a snapshot) inside [`HomeView::sync`] every frame
/// Home is on top of the stack, not by this function being re-invoked --
/// see the module doc's "why the face lives in an `Rc<RefCell<_>>`" section
/// for what *does* still need rebuild-era-style sharing (a genuine second
/// writer, not rebuild survival).
#[must_use]
#[allow(clippy::too_many_arguments)] // Mirrors `HomeView::new`'s own allow -- this is a straight passthrough into it, plus the wizard Rc every other screen builder in this crate threads through too.
pub(crate) fn build_home_screen(
    model: &ModelHandle,
    home_face: &Rc<RefCell<HomeFace>>,
    commands: &Rc<RefCell<VecDeque<Command>>>,
    wizard_phase: &Rc<RefCell<WizardPhase>>,
    now: Instant,
    display_settings: &Rc<RefCell<DisplaySettingsState>>,
) -> Screen {
    let view = HomeView::new(model, Rc::clone(home_face), commands, wizard_phase, now, display_settings);
    // B's liveness at depth 1 is now `HomeView::handles_back` (pico-link-
    // 4a2) -- dynamic per-face, unlike the old `Screen::handles_back(true)`
    // this replaced, which rendered B live on the status face too even
    // though there was nothing there to back out of.
    Screen::new(HOME_TITLE, vec![Box::new(view)]).with_id(ScreenId::Home)
}

/// Home's sole top-level content widget -- one composite `Widget` owning
/// both faces' rendering and input handling, per the `HeroStatusView`/
/// `ConfirmView` "composite widget with its own internal vertical rhythm"
/// pattern (see `hero.rs`'s module doc) -- not two widgets stacked on the
/// `Screen`, since only one face is ever visible at a time and `Screen`'s
/// stacking model has no notion of "hide this widget, show that one".
struct HomeView {
    home_face: Rc<RefCell<HomeFace>>,
    /// Re-projected from `model` every [`Widget::sync`] call (the "leaf
    /// display widget" rule, design section 3: cheap enough to rebuild
    /// fresh each frame rather than updated in place) -- see
    /// [`Self::project_hero`].
    hero: HeroStatusView,
    /// Built exactly once in [`Self::new`] and never rebuilt -- the row
    /// order/labels never change, so there is nothing to re-project; its
    /// own selection simply persists for `HomeView`'s lifetime now that
    /// `HomeView` itself does (bead `pico-link-hu97`'s carry is no longer
    /// needed for that reason -- see `build_home_screen`'s doc comment).
    menu: MenuList,
    /// The live Bluetooth link state, for the title bar's Bluetooth
    /// glyph ([`ChromeContribution::link`]) -- re-read from `model` by
    /// [`Widget::sync`] every frame Home is visible, same staleness
    /// tolerance ("never more than one frame stale") every field below
    /// shares.
    link_state: LinkState,
    /// Whether the radio is currently running a GAP inquiry -- the second,
    /// independent input `chrome_contribution` resolves alongside
    /// `link_state` into one [`LinkGlyph`] (bead `pico-link-88xs`, design
    /// `.planning/design/2026-09-08-link-state-vs-discovery-axis.md`
    /// section 5).
    discovering: bool,
    /// The connected device's address, if any -- read by `ShortcutY` to
    /// decide whether to push the device page or the explicit "no device"
    /// message (bead `pico-link-hr30`).
    connected_addr: Option<DeviceAddr>,
    /// The live model handle -- borrowed fresh inside `ShortcutY`/
    /// `ShortcutX`'s `Action::PushView` closures and the menu's own
    /// activation closures at the moment they actually run (press time,
    /// not construction time), so a device forgotten/upserted between
    /// `HomeView::new` and a much later press is never read stale. See
    /// `crate::app::ModelHandle`'s doc comment for the shared-handle
    /// shape and its borrow rule.
    model: ModelHandle,
    /// For `ShortcutY`'s device-page push -- forwarded straight through to
    /// [`crate::app::build_device_page_screen`], same as every other
    /// `Command`-emitting `PushView` closure in this crate.
    commands: Rc<RefCell<VecDeque<Command>>>,
    /// Re-read from `model.fault_log` every sync, for `ShortcutX`'s "is the
    /// strip non-empty" gate (both `on_intent` and `chrome_contribution`) --
    /// design `.planning/design/2026-09-07-home-fault-strip.md` §8.1: "X is
    /// labelled and live only while the strip is non-empty." `Copy`, so
    /// this is a plain field, not an `Rc`.
    fault_log: FaultLog,
    /// The render-time clock, updated by every [`Widget::sync`] call from
    /// `ctx.now()` -- `on_intent` has no `RenderCtx` of its own to read, so
    /// this is what its two `ShortcutX` call sites fall back to instead.
    /// Never more than one frame stale, the same tolerance every other
    /// field on this struct has.
    now: Instant,
}

impl HomeView {
    /// Builds the status face's hero widget from a live `&BtModel` read --
    /// called once by [`Self::new`] and once per [`Widget::sync`] call
    /// (the "leaf display widget" rule, design section 3: `HeroStatusView`
    /// has no interaction state of its own worth updating in place, so
    /// rebuilding it fresh each sync is simpler than a setter and no more
    /// expensive than the rebuild-on-every-event path it replaces).
    fn project_hero(model: &BtModel) -> HeroStatusView {
        // `NO LINK` whenever there is no live codec (design section 15's
        // "absent, never frozen or faked" rule -- this covers Idle/
        // Scanning/Connecting alike, not just a bare disconnect),
        // otherwise the connected device's name plus the codec word and
        // nominal bitrate `BtModel::connected_codec` carries (bead
        // pico-link-1v5). `fallback` stays `None` -- the data needed to
        // say *why* a codec fell back to a lesser one (design section
        // 6.2's amber banner) doesn't exist in `BtModel` yet; that's a
        // separate, later bead, and `None` here is the honest "no reason
        // recorded" value, not a guess.
        //
        // Volume (design `.planning/design/2026-09-07-volume-on-display.md`
        // section 10, bead pico-link-4v2.6/VT6): built from `model.volume`
        // directly, OUTSIDE the `connected_codec` match below and applied
        // to `hero` regardless of which arm produced it. `BtModel::volume`
        // is deliberately NOT cleared on disconnect (see its own doc
        // comment: the host feature-unit volume it most commonly reflects
        // is a USB-side concept, not an A2DP-link-lifetime one), so tying
        // the title-bar volume element to `connected_codec` would be
        // wrong, not just simpler -- the two are independent axes exactly
        // like `readout`/`volume` are on `ChromeContribution`.
        let hero_volume = model.volume.map(|volume| HeroVolume {
            percent: volume.percent(),
            muted: volume.muted,
            source: match volume.source {
                VolumeSource::Host => HeroVolumeSource::Host,
                VolumeSource::Sink | VolumeSource::Device => HeroVolumeSource::Other,
            },
        });
        match &model.connected_codec {
            Some(codec) => {
                // Bead pico-link-4vb.4 (T4): reads `paired` (the remembered
                // list), not the old `discovered` scan list -- the whole
                // point of this bead is that a device's name must survive
                // long after the scan that first discovered it is gone.
                let device_name =
                    model.paired.iter().find(|d| d.addr == codec.addr).map(|d| d.name.clone()).unwrap_or_default();
                // Bead pico-link-7jol.5, design §6 amendment 4: Home's
                // bitrate line ALWAYS shows the live rate, in every mode.
                // `ldac_live_kbps` is only ever populated while the
                // connected codec is LDAC (see that field's doc comment),
                // so a non-LDAC codec here always falls through to its own
                // nominal figure, honestly -- there is no live figure to
                // ask for on any other codec today. `adaptive` is the
                // connected device's *stored* quality pick (`4` ==
                // Adaptive), not a per-frame "did it just step" flag --
                // see `BitrateStatus`'s own doc comment.
                let is_ldac = codec.word == "LDAC";
                let device_ldac_quality = model.paired.iter().find(|d| d.addr == codec.addr).map_or(0, |d| d.ldac_quality);
                // Never tag a fallback codec (design table: "Not LDAC:
                // ... no tag, ever") even though `ldac_quality` is a
                // per-device stored preference that outlives a fallback to
                // SBC.
                let adaptive = is_ldac && device_ldac_quality == LDAC_QUALITY_ADAPTIVE;
                let kbps = if is_ldac { model.ldac_live_kbps.unwrap_or(codec.nominal_bitrate_bps / 1000) } else { codec.nominal_bitrate_bps / 1000 };
                let bitrate = BitrateStatus::Kbps { kbps, adaptive };
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
                    attack_peak_l: level.attack_peak_l,
                    attack_peak_r: level.attack_peak_r,
                    attack_peak_l_at: level.attack_peak_l_at,
                    attack_peak_r_at: level.attack_peak_r_at,
                });
                HeroStatusView::new(
                    device_name,
                    CodecStatus::Connected { word: codec.word.clone(), fallback: None, bitrate },
                )
                .with_out_level(out_level)
            }
            None => HeroStatusView::new("", CodecStatus::NoLink),
        }
        .with_volume(hero_volume)
        // The Home fault strip (design `.planning/design/2026-09-07-home-
        // fault-strip.md`, bead `pico-link-9eq2.3.3`) -- a straight field
        // pass-through, `Copy`, same "translate BtModel into the widget's
        // own vocabulary here" shape `out_level` above already uses. Tiers/
        // retirement/ordering are all derived at RENDER time from
        // `RenderCtx::now` (design §6.3) -- this builder only hands the raw
        // log across.
        .with_fault_log(model.fault_log)
    }

    #[allow(clippy::too_many_arguments)] // Mirrors every other screen builder in this crate that threads the wizard's shared Rcs through -- see `build_devices_screen`.
    fn new(
        model: &ModelHandle,
        home_face: Rc<RefCell<HomeFace>>,
        commands: &Rc<RefCell<VecDeque<Command>>>,
        wizard_phase: &Rc<RefCell<WizardPhase>>,
        now: Instant,
        display_settings: &Rc<RefCell<DisplaySettingsState>>,
    ) -> Self {
        let (hero, link_state, discovering, connected_addr, fault_log) = {
            let snapshot = model.borrow();
            (Self::project_hero(&snapshot), snapshot.link_state, snapshot.discovering, snapshot.connected_addr, snapshot.fault_log)
        };

        let model_for_shortcut_y = Rc::clone(model);
        let commands_for_shortcut_y = Rc::clone(commands);
        let model_for_bluetooth = Rc::clone(model);
        let commands_for_bluetooth = Rc::clone(commands);
        let wizard_phase_for_bluetooth = Rc::clone(wizard_phase);
        let display_settings_for_settings_row = Rc::clone(display_settings);
        let menu = MenuList::new(vec![MenuItem::new("Bluetooth"), MenuItem::new("Settings")]).on_activate_index(
            // `Verb::Open`: both rows push a deeper screen and draw a
            // caret (design section 4's assignment table -- Home menu's A
            // word changed from "select" to "open" as part of the rule 4
            // ruling, deliberate consistency work, not a typo to "fix"
            // back).
            Verb::Open,
            move |index| match index {
                MENU_ROW_BLUETOOTH => {
                    // Borrowed fresh at press time, not captured as a
                    // snapshot at `HomeView::new` time -- this menu is
                    // built exactly once for the app's lifetime (bead
                    // `pico-link-bgnd` M1), so a value snapshot captured
                    // here would go stale forever after the first press.
                    let model = Rc::clone(&model_for_bluetooth);
                    let commands = Rc::clone(&commands_for_bluetooth);
                    let wizard_phase = Rc::clone(&wizard_phase_for_bluetooth);
                    Action::PushView(Box::new(move || build_devices_screen(&model, None, 0, None, &commands, &wizard_phase)))
                }
                MENU_ROW_SETTINGS => {
                    let display_settings = Rc::clone(&display_settings_for_settings_row);
                    Action::PushView(Box::new(move || build_settings_screen(&display_settings, &ScreenCarry::default())))
                }
                _ => Action::None,
            },
        );
        // No `with_selected` carry-forward needed any more (bead
        // `pico-link-hu97`'s original fix): `HomeView` -- and therefore
        // this `MenuList` -- is now built exactly once and lives for the
        // app's lifetime, so its own `selected` field simply persists
        // across every subsequent model event with no help from this
        // constructor.

        Self {
            home_face,
            hero,
            menu,
            link_state,
            discovering,
            connected_addr,
            model: model_for_shortcut_y,
            commands: commands_for_shortcut_y,
            fault_log,
            now,
        }
    }

    fn face(&self) -> HomeFace {
        *self.home_face.borrow()
    }

    /// Resolves `link_state` + `discovering` down to one [`LinkGlyph`]
    /// (bead `pico-link-88xs`, design section 5). `Connected` always wins
    /// -- a scan running concurrently with a live link must render exactly
    /// as it did before the scan started (design section 1.1: the whole
    /// point is that the glyph must never demote on a scan).
    fn resolved_link_glyph(&self) -> LinkGlyph {
        match (self.link_state, self.discovering) {
            (LinkState::Connected, _) => LinkGlyph::Live,
            (LinkState::Connecting, _) | (_, true) => LinkGlyph::Busy,
            (LinkState::Idle, false) => LinkGlyph::Idle,
        }
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

    /// Re-projects every field this widget reads from the live model
    /// (bead `pico-link-bgnd` M1) -- called once per frame Home is on top
    /// of the navigator's stack, before render/input (see
    /// [`super::navigator::Navigator::sync_top`]'s doc comment for exactly
    /// when). `hero` is rebuilt fresh (the leaf rule, design section 3);
    /// `menu` has no live-model state of its own but is still forwarded to
    /// (the "wrappers must forward sync" rule, same as `redraw_after`/
    /// `activation`).
    fn sync(&mut self, ctx: &RenderCtx) {
        self.now = ctx.now();
        let model = self.model.borrow();
        self.link_state = model.link_state;
        self.discovering = model.discovering;
        self.connected_addr = model.connected_addr;
        self.fault_log = model.fault_log;
        self.hero = Self::project_hero(&model);
        drop(model);
        self.menu.sync(ctx);
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
            // Y: the device page, on BOTH faces -- `pico-link-hr30`,
            // Andreas's ruling superseding the old "Y: Settings on the
            // status face only" binding (Settings stays reachable via A ->
            // menu face -> Settings row; that shortcut is deliberately
            // dropped so the same physical button does the same thing on
            // either face, per the ruling's "no face-dependent behaviour
            // to learn"). Connected: push the real device page, same
            // `build_device_page_screen`/`Refresh` dance
            // `build_devices_screen`'s own connected-row activation uses
            // (`core/src/app.rs`). Not connected: an explicit "no device
            // connected" message, not a silent no-op -- the bead is
            // explicit that Y must never look like a dead button when
            // there's nothing to open.
            NavIntent::ShortcutY => {
                if let Some(addr) = self.connected_addr {
                    let model = Rc::clone(&self.model);
                    let commands = Rc::clone(&self.commands);
                    Action::PushView(Box::new(move || match build_device_page_screen(&model, addr, &ScreenCarry::default(), &commands) {
                        Refresh::Rebuild(screen) => screen,
                        // The device we just read `connected_addr` for
                        // cannot have vanished between that read and this
                        // closure running on the very same input event --
                        // structurally unreachable, but a same-titled
                        // empty screen is a harmless fallback rather than
                        // a panic if it ever is (same shape as
                        // `build_devices_screen`'s own `fallback_title`
                        // handling). `Refresh::Keep` is likewise
                        // unreachable: `build_device_page_screen` never
                        // returns it (bead pico-link-bgnd M0 -- no builder
                        // does yet).
                        Refresh::Gone | Refresh::Keep => Screen::new(NO_DEVICE_TITLE, vec![]),
                    }))
                } else {
                    Action::PushView(Box::new(|| {
                        Screen::new(NO_DEVICE_TITLE, vec![Box::new(MessageView::new("No device connected"))])
                    }))
                }
            }
            // X: the fault strip's `why?` page, live only while the strip
            // is non-empty (design `.planning/design/2026-09-07-home-
            // fault-strip.md` §8.1, bead `pico-link-9eq2.3.3` -- supersedes
            // the older "X is inert" ruling this doc comment used to carry;
            // `pico-link-9eq2.3` has now landed). Empty strip: `Action::
            // None`, matching `chrome_contribution`'s unlabelled-X state
            // below so a mispress is always free (design rule 4). Non-
            // empty: computes a FRESH most-recently-active-first sort of
            // every key that has ever fired (including retired ones -- the
            // `why?` page shows session history, design §8.2) and seeds
            // `WhyPageView` with it. This is the one and only place this
            // order is ever sorted -- as of bead `pico-link-bgnd` M4,
            // `WhyPageView` owns the order from here on and only ever
            // appends to it in [`super::app::screens::why_page::
            // WhyPageView::sync`], never re-sorts (see that struct's own
            // doc comment).
            NavIntent::ShortcutX => {
                if !self.fault_log.has_visible_entry(self.now) {
                    return Action::None;
                }
                let mut fresh_order: Vec<FaultKey> = FaultKey::ALL.into_iter().filter(|key| self.fault_log.entry(*key).is_some()).collect();
                fresh_order.sort_by(|a, b| {
                    let a_last = self.fault_log.entry(*a).map_or(Instant::from_micros(0), |e| e.last_seen);
                    let b_last = self.fault_log.entry(*b).map_or(Instant::from_micros(0), |e| e.last_seen);
                    b_last.cmp(&a_last) // descending: most-recently-active first
                });
                let model = Rc::clone(&self.model);
                Action::PushView(Box::new(move || build_why_page_screen(&model, fresh_order)))
            }
            // `Left`/`Right` have no meaning on Home (design section 4:
            // identical to B/A elsewhere, but Home's exception already
            // covers A/B/centre explicitly and doesn't mention them).
            // `Select` is handled entirely in `on_focus` (`NavIntent::
            // Select` reaches `Widget::on_focus`'s `FocusEvent::Activated`,
            // not `on_intent` -- see `Navigator::dispatch`), so it never
            // reaches here.
            NavIntent::Left | NavIntent::Right | NavIntent::Select => Action::None,
        }
    }

    fn chrome_contribution(&self, ctx: &RenderCtx) -> Option<ChromeContribution> {
        // X: "why?", live only while the fault strip is non-empty (design
        // `.planning/design/2026-09-07-home-fault-strip.md` §8.1) -- on
        // BOTH faces, same as Y, per `on_intent`'s `ShortcutX` arm having
        // no face-dependent behaviour either. Computed against `ctx.now()`
        // (not `self.now`) so the label demotes to inert at exactly the
        // instant `render::hero`'s strip itself retires the last row --
        // `on_intent` has no `RenderCtx` and falls back to `self.now`
        // there (see that field's doc comment), but this method does, so
        // it uses the fresher clock.
        let why_label = self.fault_log.has_visible_entry(ctx.now()).then(|| ButtonLabel::Live(String::from("why?")));
        match self.face() {
            HomeFace::Status => {
                let mut contribution = self.hero.chrome_contribution(ctx).unwrap_or_default();
                // "link": the device page, per `pico-link-hr30`'s ruling --
                // was "set" (Settings) before that ruling repurposed Y; see
                // `on_intent`'s `ShortcutY` arm and this bead's doc comment.
                contribution.y = Some(ButtonLabel::Live(String::from("link")));
                contribution.link = Some(self.resolved_link_glyph());
                contribution.x = why_label;
                Some(contribution)
            }
            // A's word is entirely `activation()`'s job now (design rule
            // 4), and there's no title-bar Bluetooth glyph on this face --
            // but Y still needs a label here too (`pico-link-hr30`: both
            // faces bind and label the same way), so this can no longer be
            // a flat `None`.
            HomeFace::Menu => {
                Some(ChromeContribution { x: why_label, y: Some(ButtonLabel::Live(String::from("link"))), ..ChromeContribution::default() })
            }
        }
    }

    /// A's liveness and label, per design rule 4 -- see
    /// `.planning/design/2026-09-02-a-button-label-rule.md` §4's
    /// assignment table. The status face keeps the one named exception
    /// (`devs`, design section 4's rail table); the menu face reports
    /// `open` since both rows push a deeper screen and draw a caret
    /// (`HomeView::on_focus`'s `Activated` arm delegates straight to
    /// `self.menu`, which pushes Devices/Settings on the selected row --
    /// A is never dim here, unlike X/Y, whose "unlabelled means inert"
    /// exemption A does not share).
    fn activation(&self) -> Option<Verb> {
        Some(match self.face() {
            HomeFace::Status => Verb::Exception("devs"),
            HomeFace::Menu => Verb::Open,
        })
    }

    /// B is live on the menu face (folds back to the status face) but
    /// genuinely dead on the status face -- Home is the navigator root, so
    /// at depth 1 with the status face showing there is nothing to back
    /// out of (pico-link-4a2: B previously rendered live there
    /// unconditionally, via `Screen::handles_back(true)`, and did nothing
    /// on press).
    fn handles_back(&self) -> bool {
        self.face() == HomeFace::Menu
    }

    /// Reports the menu face's selected row so [`Navigator::selected_index_at`]
    /// / [`crate::app::App::refresh_stack`] can carry it forward into the
    /// next rebuild (bead `pico-link-hu97`) -- mirrors `DevicesListView::
    /// selected_index` forwarding to its wrapped list. `None` on the status
    /// face: there is nothing selected there (the module doc's Home input
    /// exception), and reporting the menu's index anyway would make a
    /// status-face rebuild spuriously seed the menu's selection from a
    /// value the user never actually chose on this face.
    fn selected_index(&self) -> Option<usize> {
        match self.face() {
            HomeFace::Status => None,
            HomeFace::Menu => Some(self.menu.selected_index()),
        }
    }

    fn render(&self, area: Rectangle, ctx: &RenderCtx, target: &mut FrameBuffer565) -> Result<(), Infallible> {
        match self.face() {
            HomeFace::Status => self.hero.render(area, ctx, target),
            HomeFace::Menu => self.menu.render(area, ctx, target),
        }
    }

    /// Forwards to `hero`'s own narrowing on the status face -- `HomeView`
    /// is a composite (design section 5's R3, `pico-link-yn5i.1`'s B1):
    /// `hero` occupies exactly `HomeView`'s own `area` when it's the
    /// visible face, so its meter-footprint hint applies unchanged. The
    /// menu face has no sub-widget narrowing of its own (`MenuList`
    /// doesn't override this), so `None` there -- exactly the inherited
    /// default, meaning a menu-face change damages the whole widget area,
    /// same as before this override existed.
    fn damage_hint(&self, area: Rectangle, ctx: &RenderCtx) -> Option<Rectangle> {
        match self.face() {
            HomeFace::Status => self.hero.damage_hint(area, ctx),
            HomeFace::Menu => None,
        }
    }

    /// The B1 half of `damage_hint`'s contract: "everything outside the
    /// hint rectangle," diffed by `Screen` against its own cache, never by
    /// this widget (see [`Widget::damage_region_key`]'s doc comment -- a
    /// `HomeView` instance no longer dies every frame the way it used to,
    /// but the contract is the same regardless: `Screen` owns the diff).
    ///
    /// Folds the active face into the key, the same shape [`Self::
    /// paint_key`] uses just below, for the same reason: a face toggle is
    /// itself a change to "everything outside the hint," so it must never
    /// compare equal to the previous frame's key even though the folded
    /// face bit isn't drawn by either child directly. On the status face
    /// this forwards `hero`'s own `damage_region_key` (its "did anything
    /// but the meter change" body key) unchanged. On the menu face there
    /// is no hint to protect (`damage_hint` returns `None` there, so
    /// `Screen` always falls back to the whole widget area regardless of
    /// this value) -- folding `PaintKey::ALWAYS` keeps that path honest
    /// rather than accidentally comparing equal across frames.
    fn damage_region_key(&self, ctx: &RenderCtx) -> PaintKey {
        let face = self.face();
        PaintKey::of(HOME_PAINT_KEY_SEED)
            .fold(match face {
                HomeFace::Status => 0,
                HomeFace::Menu => 1,
            })
            .fold_key(match face {
                HomeFace::Status => self.hero.damage_region_key(ctx),
                HomeFace::Menu => PaintKey::ALWAYS,
            })
    }

    /// Folds which face is active plus that active child's own
    /// [`Widget::paint_key`] (via [`PaintKey::fold_key`]) -- the same
    /// "forward to whichever child is actually showing" shape [`Self::
    /// render`] itself uses, since only one face is ever drawn at a time.
    /// The hidden face's key is not folded at all: it contributes nothing
    /// to what's on screen, and folding it anyway would make `HomeView`
    /// spuriously dirty every time the *other* face's state changed.
    ///
    /// `HomeView` overrides [`Widget::redraw_after`] (forwarding the `min`
    /// over both children), so per the mechanical review rule this
    /// `paint_key` must fold time -- and it does, transitively: whichever
    /// child is active folds its own quantised time consequence into the
    /// key this method returns (`hero.rs`'s `paint_key` does; `menu.rs`'s
    /// does not, correctly, since `MenuList` has no time-driven appearance
    /// of its own). There is nothing further for this wrapper to fold
    /// directly.
    fn paint_key(&self, ctx: &RenderCtx) -> PaintKey {
        let face = self.face();
        let child_key = match face {
            HomeFace::Status => self.hero.paint_key(ctx),
            HomeFace::Menu => self.menu.paint_key(ctx),
        };
        PaintKey::of(HOME_PAINT_KEY_SEED)
            .fold(match face {
                HomeFace::Status => 0,
                HomeFace::Menu => 1,
            })
            .fold_key(child_key)
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
        let model = Rc::new(RefCell::new(BtModel::default()));
        let home_face = Rc::new(RefCell::new(HomeFace::default()));
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let wizard_phase = Rc::new(RefCell::new(WizardPhase::default()));
        let display_settings = Rc::new(RefCell::new(DisplaySettingsState::default()));
        HomeView::new(&model, home_face, &commands, &wizard_phase, Instant::from_micros(0), &display_settings)
    }

    /// A [`HomeView`] whose model has a connected, paired device at
    /// `addr` -- for `pico-link-hr30`'s `ShortcutY` -> device-page tests.
    fn connected_home_view(addr: crate::app::DeviceAddr) -> HomeView {
        let mut model = BtModel { connected_addr: Some(addr), ..BtModel::default() };
        model.paired.push(crate::app::PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0 });
        let model = Rc::new(RefCell::new(model));
        let home_face = Rc::new(RefCell::new(HomeFace::default()));
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let wizard_phase = Rc::new(RefCell::new(WizardPhase::default()));
        let display_settings = Rc::new(RefCell::new(DisplaySettingsState::default()));
        HomeView::new(&model, home_face, &commands, &wizard_phase, Instant::from_micros(0), &display_settings)
    }

    /// Runs an [`Action::PushView`]'s builder and returns the resulting
    /// [`Screen`], panicking if `action` isn't `PushView` -- shared shape
    /// for the `ShortcutY` tests below, which only care about the pushed
    /// screen's identity, not the raw `Action`.
    fn pushed_screen(action: Action) -> Screen {
        match action {
            Action::PushView(build) => build(),
            // `Action` isn't `Debug` (it carries a boxed `FnOnce`), so this
            // can't print what it actually got -- the assertion still
            // fails loudly, just without an interpolated value.
            _ => panic!("expected Action::PushView"),
        }
    }

    // --- pico-link-hr30: ShortcutY -> the device page (or an explicit
    // "no device" message), on both faces ---

    #[test]
    fn shortcut_y_pushes_the_device_page_when_connected_on_the_status_face() {
        let addr = [9; 6];
        let mut view = connected_home_view(addr);
        assert_eq!(view.face(), HomeFace::Status);
        let screen = pushed_screen(view.on_intent(NavIntent::ShortcutY));
        assert_eq!(screen.id(), Some(ScreenId::DevicePage(addr)), "status-face Y must open the connected device's device page");
    }

    #[test]
    fn shortcut_y_pushes_the_device_page_when_connected_on_the_menu_face() {
        let addr = [9; 6];
        let mut view = connected_home_view(addr);
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.face(), HomeFace::Menu);
        let screen = pushed_screen(view.on_intent(NavIntent::ShortcutY));
        assert_eq!(screen.id(), Some(ScreenId::DevicePage(addr)), "menu-face Y must open the same device page as the status face");
    }

    #[test]
    fn shortcut_y_is_not_a_silent_no_op_when_nothing_is_connected_on_the_status_face() {
        let mut view = fresh_home_view();
        assert_eq!(view.face(), HomeFace::Status);
        let screen = pushed_screen(view.on_intent(NavIntent::ShortcutY));
        assert_eq!(screen.id(), None, "the 'no device connected' screen must not be mistaken for a real device page by refresh_stack");
    }

    #[test]
    fn shortcut_y_is_not_a_silent_no_op_when_nothing_is_connected_on_the_menu_face() {
        let mut view = fresh_home_view();
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.face(), HomeFace::Menu);
        let screen = pushed_screen(view.on_intent(NavIntent::ShortcutY));
        assert_eq!(screen.id(), None, "the disconnected case must push the same 'no device' screen on the menu face too");
    }

    #[test]
    fn y_is_labelled_link_on_both_faces() {
        let mut view = fresh_home_view();
        assert_eq!(
            view.chrome_contribution(&test_ctx()).and_then(|c| c.y),
            Some(ButtonLabel::Live(String::from("link"))),
            "status face Y must be labelled -- an unlabelled Y binding breaks design rule 2"
        );
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(
            view.chrome_contribution(&test_ctx()).and_then(|c| c.y),
            Some(ButtonLabel::Live(String::from("link"))),
            "menu face Y must be labelled the same way -- same destination, same label"
        );
    }

    /// `pico-link-9eq2.3.3` (superseding the old `pico-link-hr30`-era "X is
    /// permanently inert" ruling, now that the fault strip's `why?` page
    /// exists): X stays unbound and unlabelled on BOTH faces whenever the
    /// fault strip is empty (design `.planning/design/2026-09-07-home-
    /// fault-strip.md` §8.1 -- `fresh_home_view()`'s model has an empty
    /// `FaultLog`, so this is exactly the empty-strip case). Rail default
    /// for an absent `chrome_contribution` entry is [`ButtonLabel::Inert`]
    /// (`ButtonLabels::default`), so `None` here is the correct assertion,
    /// not an oversight. See `shortcut_x_*` below for the non-empty case.
    #[test]
    fn x_stays_unbound_and_unlabelled_on_both_faces_while_the_strip_is_empty() {
        let mut view = fresh_home_view();
        assert_eq!(view.chrome_contribution(&test_ctx()).and_then(|c| c.x), None);
        assert!(matches!(view.on_intent(NavIntent::ShortcutX), Action::None));

        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.chrome_contribution(&test_ctx()).and_then(|c| c.x), None);
        assert!(matches!(view.on_intent(NavIntent::ShortcutX), Action::None));
    }

    // --- pico-link-9eq2.3.3: ShortcutX -> the `why?` page, live only
    // while the fault strip is non-empty ---

    fn home_view_with_one_fault() -> HomeView {
        let mut model = BtModel::default();
        model.fault_log.record(crate::app::FaultKey::BufOverflow, Instant::from_micros(0), None, 1);
        let model = Rc::new(RefCell::new(model));
        let home_face = Rc::new(RefCell::new(HomeFace::default()));
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let wizard_phase = Rc::new(RefCell::new(WizardPhase::default()));
        let display_settings = Rc::new(RefCell::new(DisplaySettingsState::default()));
        HomeView::new(&model, home_face, &commands, &wizard_phase, Instant::from_micros(0), &display_settings)
    }

    #[test]
    fn x_is_labelled_why_on_both_faces_once_the_strip_is_non_empty() {
        let mut view = home_view_with_one_fault();
        assert_eq!(view.chrome_contribution(&test_ctx()).and_then(|c| c.x), Some(ButtonLabel::Live(String::from("why?"))));
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.chrome_contribution(&test_ctx()).and_then(|c| c.x), Some(ButtonLabel::Live(String::from("why?"))));
    }

    #[test]
    fn shortcut_x_pushes_the_why_page_when_the_strip_is_non_empty() {
        let mut view = home_view_with_one_fault();
        let screen = pushed_screen(view.on_intent(NavIntent::ShortcutX));
        assert_eq!(screen.id(), Some(ScreenId::WhyPage), "X must push the why? page's own identified ScreenId");
    }

    /// Regression test for the review finding on this bead: a `None`
    /// activation for `HomeFace::Menu` would render A dim while
    /// `HomeView::on_focus`'s `Activated` arm still delegates straight to
    /// the wrapped `MenuList` -- so A would silently navigate on press
    /// despite looking inert. Design rule 4 makes this unrepresentable
    /// (`Screen::activate_focused` refuses to dispatch when `activation()`
    /// is `None`), but this widget-level test pins the actual value down.
    #[test]
    fn a_is_live_on_the_menu_face_since_it_actually_activates_the_selected_row() {
        let mut view = fresh_home_view();
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.face(), HomeFace::Menu);

        assert_eq!(
            view.activation(),
            Some(Verb::Open),
            "A must render live (\"open\") on the menu face -- it activates the selected row, not a no-op"
        );
    }

    #[test]
    fn a_is_live_on_the_status_face_too_labelled_devs() {
        let view = fresh_home_view();
        assert_eq!(view.face(), HomeFace::Status);

        assert_eq!(
            view.activation(),
            Some(Verb::Exception("devs")),
            "A must render live on the status face too (it toggles the face)"
        );
    }

    // --- paint_key (bead pico-link-7h5.5) ---

    #[test]
    fn paint_key_is_stable_across_calls_with_no_state_change() {
        let view = fresh_home_view();
        assert_eq!(view.paint_key(&test_ctx()), view.paint_key(&test_ctx()));
    }

    #[test]
    fn paint_key_changes_when_toggling_faces() {
        let mut view = fresh_home_view();
        let status_key = view.paint_key(&test_ctx());
        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.face(), HomeFace::Menu);
        let menu_key = view.paint_key(&test_ctx());
        assert_ne!(status_key, menu_key, "toggling faces must change the paint key even if a child's own state happens to match");
    }

    #[test]
    fn paint_key_on_the_menu_face_changes_when_the_menus_selection_moves() {
        let mut view = fresh_home_view();
        view.on_focus(FocusEvent::Activated); // status -> menu
        let before = view.paint_key(&test_ctx());
        view.on_intent(NavIntent::Down);
        let after = view.paint_key(&test_ctx());
        assert_ne!(before, after, "moving the menu's selection must change HomeView's own paint key");
    }

    #[test]
    fn b_is_inert_on_the_status_face_but_live_on_the_menu_face() {
        // pico-link-4a2: the status face is Home's root state -- there is
        // nothing to fold back to, so B must not claim otherwise.
        let mut view = fresh_home_view();
        assert_eq!(view.face(), HomeFace::Status);
        assert!(!view.handles_back(), "B must be inert on the status face -- there is nothing to back out of");

        view.on_focus(FocusEvent::Activated); // status -> menu
        assert_eq!(view.face(), HomeFace::Menu);
        assert!(view.handles_back(), "B must be live on the menu face -- it folds back to the status face");
    }
}
