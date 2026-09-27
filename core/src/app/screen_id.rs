use super::model::DeviceAddr;

/// Identity/liveness tag for a screen that reads [`BtModel`] itself via
/// `Widget::sync` while it sits on the [`Navigator`]'s stack -- see
/// [`App::prune_stack`]'s doc comment for why this exists: a device-scoped
/// screen's subject can vanish out from under it (a forgotten device), and
/// this is what lets `prune_stack` find and unwind it. `Screen::id()`
/// returns `None` for every screen that never calls [`Screen::with_id`]
/// (the wizard, `ConfirmView`s): `None` means "no identity to check", so
/// tagging a screen is opt-in and every untagged screen is
/// behaviour-identical to before this type existed.
///
/// `Copy`/`Eq`, like [`ListItemKey`] and for the same reason: cheap to
/// carry around and compare on every [`App::prune_stack`] pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenId {
    Home,
    Devices,
    DevicePage(DeviceAddr),
    /// A depth-2 single-select picker pushed from a [`ScreenId::DevicePage`]
    /// row. The only picker built so far is [`PickerKind::LdacQuality`] --
    /// the codec picker itself waits on Ada's `CodecAvailability` seam.
    Picker(PickerKind, DeviceAddr),
    /// The Home fault strip's `why?` detail page -- a singleton, no
    /// payload: there is exactly one, reached only from `X` on Home. Built
    /// once per push and never rebuilt again (bead `pico-link-bgnd` M4) --
    /// its widget reads the live model itself via `Widget::sync` so a
    /// fault that fires while it's open updates counts/times in place (see
    /// `crate::app::screens::why_page::WhyPageView`'s doc comment for the
    /// append-only ordering rule this enforces).
    WhyPage,
    /// The Settings screen -- a singleton, no payload, reached only from
    /// Home's menu-face "Settings" row.
    Settings,
    /// A depth-3 single-select picker pushed from [`ScreenId::Settings`] --
    /// same shape as [`ScreenId::Picker`], just rooted under Settings
    /// instead of a device page (there is no device address in scope
    /// here).
    SettingsPicker(SettingsPickerKind),
    /// The DSP effects list -- a singleton, no payload, reached only from
    /// Home's menu-face "Effects" row. Bead `pico-link-ryw.12.4`: this id
    /// is what lets `App::import_preset` tell whether the effects list is
    /// currently on top of the navigator's stack, for its import-focus-
    /// follow (Uma's design, `ryw12-3-ux.md` sec 2: "Effects list on top:
    /// ... move FOCUS to the new/updated row. Anywhere else: nothing on
    /// screen").
    EffectsList,
    /// The pairing wizard -- a singleton, no payload, reached only from
    /// Devices' "Pair new" row. Added by bead `pico-link-vuou` so
    /// `App::on_wizard_auto_dismiss` can require this screen actually be on
    /// top of the [`Navigator`] stack before popping to Home, rather than
    /// trusting `WizardPhase` alone (which C's dismiss timer arms on
    /// *every* successful connect, wizard-initiated or not).
    PairingWizard,
}

/// Which picker a [`ScreenId::Picker`] identifies -- distinguishes screens
/// that would otherwise share the same `(kind, addr)`-less identity if a
/// device page ever grows a second picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    LdacQuality,
    /// The device page's `EFFECT` row's picker -- bead `pico-link-ryw.7`.
    Effect,
}

/// Which picker a [`ScreenId::SettingsPicker`] identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPickerKind {
    ScreensaverMode,
    ScreensaverTimeout,
    /// The BUFFER row's picker (bead `pico-link-8pp1.2`, S4) -- picks the
    /// global congestion-cushion policy ([`crate::audio::CushionPolicy`]).
    Cushion,
    /// The LDAC MIN row's picker (bead `pico-link-d42g.4`, F4) -- picks the
    /// global LDAC Adaptive floor ([`crate::audio::AbrFloor`]).
    AbrFloor,
}

