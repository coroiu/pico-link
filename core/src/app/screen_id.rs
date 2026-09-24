use super::model::DeviceAddr;

/// Identity for a screen that must stay live-synced to [`BtModel`] while it
/// sits on the [`Navigator`]'s stack -- see [`App::refresh_stack`]'s doc
/// comment for why this exists. `Screen::id()` returns `None` for every
/// screen that never calls
/// [`Screen::with_id`] (the wizard, `ConfirmView`s, Settings): `None` is
/// the "never refresh me" sentinel, so tagging a screen is opt-in and every
/// untagged screen is behaviour-identical to before this type existed.
///
/// `Copy`/`Eq`, like [`ListItemKey`] and for the same reason: cheap to
/// carry around and compare on every [`App::refresh_stack`] pass.
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
    /// payload: there is exactly one, reached only from `X` on Home.
    /// Refreshed like every other identified screen so a fault that fires
    /// while it's open updates counts/times/tier in place (see
    /// [`build_why_page_screen`]'s doc comment for the append-only
    /// ordering rule this refresh enforces).
    WhyPage,
    /// The Settings screen -- a singleton, no payload, reached only from
    /// Home's menu-face "Settings" row.
    Settings,
    /// A depth-3 single-select picker pushed from [`ScreenId::Settings`] --
    /// same shape as [`ScreenId::Picker`], just rooted under Settings
    /// instead of a device page (there is no device address in scope
    /// here).
    SettingsPicker(SettingsPickerKind),
}

/// Which picker a [`ScreenId::Picker`] identifies -- distinguishes screens
/// that would otherwise share the same `(kind, addr)`-less identity if a
/// device page ever grows a second picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    LdacQuality,
}

/// Which picker a [`ScreenId::SettingsPicker`] identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPickerKind {
    ScreensaverMode,
    ScreensaverTimeout,
}

