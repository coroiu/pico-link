use crate::render::{ListItemKey, Screen};

/// The focus/scroll state [`App::refresh_stack`] reads from a screen
/// *before* replacing it, so the freshly built replacement can carry it
/// forward instead of resetting to row 0 -- the same carry-forward
/// [`App::build_identified_screen`]'s `ScreenId::Devices` arm already did
/// pre-refactor, generalized to any identified screen at any depth.
#[derive(Default)]
pub(crate) struct ScreenCarry {
    pub(crate) selected_key: Option<ListItemKey>,
    pub(crate) selected_index: usize,
    pub(crate) scroll_top: Option<usize>,
}

/// What [`App::build_identified_screen`] found for a given [`ScreenId`] --
/// [`App::refresh_stack`]'s two possible outcomes per identified screen.
///
/// `pub(crate)`: `render::home`'s `ShortcutY` binding
/// (`pico-link-hr30`) pushes a device page the same way this module's own
/// connected-row activation does (see `build_devices_screen`'s
/// `model_for_device_page` closure), so it needs to see both arms too.
pub(crate) enum Refresh {
    /// Replace the screen at this stack index with this freshly built one.
    Rebuild(Screen),
    /// This screen's subject no longer exists in the model (e.g. a
    /// [`ScreenId::DevicePage`] for a forgotten device) -- drop it and
    /// everything above it.
    Gone,
    /// Leave the screen already on the stack in place -- no
    /// [`Navigator::replace_at`], no forced full-frame damage. This is the
    /// migration seam for the live-widgets refactor (bead `pico-link-bgnd`
    /// M0, `.planning/design/2026-09-24-live-widgets-retire-refresh-
    /// stack.md` section 7): a screen kind migrates to reading live model
    /// state via `Widget::sync` (`crate::render::widget::Widget::sync`)
    /// simply by having its `App::build_identified_screen` arm return this
    /// instead of [`Self::Rebuild`]. **No builder returns this yet** -- as
    /// of M0 this variant exists and is handled, but is otherwise dead
    /// code; the first arm to actually return it lands with M1 (Home).
    #[allow(dead_code)] // Not constructed until M1 -- see the variant's own doc comment.
    Keep,
}
