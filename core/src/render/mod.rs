//! The render core: platform-free UI rendering and navigation built on
//! `embedded-graphics`, replacing the retired `gui`/`simple_gui` engines
//! (custom RGBA rasterizer, baked ASCII fonts, character-skip marquee
//! clipping — see
//! `.planning/decisions/2026-08-11-ui-framework-reuse-vs-rewrite.md`).
//!
//! Module map:
//! - [`framebuffer`]: the canonical Rgb565 in-RAM framebuffer
//!   ([`FrameBuffer565`]), the app core's single render output.
//! - [`widget`]: the retained-mode [`Widget`] trait, [`Action`], and
//!   [`FocusEvent`].
//! - [`chrome`]: fixed title/content/rail region layout
//!   ([`compute_chrome`]).
//! - [`rail`]: the labelled A/B/X/Y button rail
//!   ([`rail::draw_rail`]/[`ButtonLabel`]/[`ButtonLabels`]), drawn into the
//!   chrome's rail region on every screen.
//! - [`list`]: [`VerticalList`], the primary scrolling content widget.
//! - [`menu`][]: [`MenuList`]/[`MenuItem`] — the chip-less, single-line
//!   action-row style used by action menus and other single-line rows,
//!   kept deliberately separate from `list`'s two-line row style. Also
//!   owns the shared row-drawing primitive (`menu::draw_row`) [`fields`]
//!   reuses.
//! - [`fields`]: [`FieldList`]/[`FieldRow`] — a scrolling label/value
//!   sheet whose rows are mostly inert (the device page, codec/quality
//!   pickers), drawn via `menu`'s shared row primitive. See
//!   `.planning/design/2026-09-02-field-list-widget-ruling.md`.
//! - [`message`]: [`MessageView`] — the shared "nothing to show here"
//!   widget (icon + headline + subline), reusable by any placeholder
//!   content state.
//! - [`confirm`]: [`ConfirmView`] — a self-contained
//!   destructive-confirmation screen (headline + a wrapped [`MenuList`]),
//!   for any "are you sure?" flow.
//! - [`spacer`]: [`Spacer`] — a fixed-height, non-focusable, non-drawing
//!   widget that reserves vertical space in a screen's widget stack (the
//!   device page's 12px top gutter; see
//!   `.planning/design/2026-09-07-device-page-and-single-select-picker.md`
//!   §3.3).
//! - [`screen`]: [`Screen`], one entry in the navigation stack.
//! - [`navigator`]: [`Navigator`], owning the screen stack.
//! - [`theme`]: the visual design language — the semantic color palette,
//!   per-role `u8g2-fonts` accessors, `open_iconic` icon codepoints, and
//!   the shared chip/selection drawing primitives. `screen`/`list` and
//!   other views render through this instead of `embedded-graphics`'
//!   built-in `MonoFont`/`WebColors`.
//!
//! What's deliberately NOT here yet:
//! - Any `DisplaySurface`/`InputSource`/`Clock`/`Storage` *implementation*
//!   (those traits themselves are frozen in `crate::platform` from W1).

// `embedded-graphics` represents position as `Point` (`i32`) and extent as
// `Size` (`u32`) — a mismatch baked into the upstream library, not
// introduced here. Converting between the two throughout a layout/render
// module is therefore idiomatic embedded-graphics usage, not sloppiness;
// every display this project targets is a few hundred pixels per side, so
// none of these conversions can realistically wrap, truncate, or lose a
// sign. Allowed at the module level rather than peppering every call site.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

// `RenderCtx` is `Copy`/8 bytes, so pedantic clippy would rather every
// `measure`/`render`/`chrome_contribution`/`redraw_after` took it by
// value. The frame-scoped clock ADR (`.planning/decisions/2026-08-31-
// render-ctx-frame-scoped-clock.md`) specifies `&RenderCtx` deliberately
// across every one of those signatures, so the next frame-scoped field
// this type grows (dim state, a reduce-motion setting, a frame counter --
// see `RenderCtx`'s own doc comment) doesn't force reconsidering pass-by-
// value against `Copy`'s size threshold at every call site again. Allowed
// module-wide rather than re-litigating it at each of the ~dozen sites.
#![allow(clippy::trivially_copy_pass_by_ref)]

pub mod chrome;
pub mod confirm;
pub mod ctx;
pub mod fault_glyph;
pub mod fields;
pub mod framebuffer;
pub mod hero;
pub mod home;
pub mod list;
pub mod menu;
pub mod message;
pub mod navigator;
pub mod paint_key;
pub mod rail;
pub mod screen;
pub mod spacer;
pub mod theme;
pub mod widget;
pub mod wizard;

pub use chrome::{compute_chrome, compute_chrome_for, ChromeLayout};
pub use confirm::ConfirmView;
pub use ctx::RenderCtx;
pub use framebuffer::FrameBuffer565;
pub use crate::platform::Instant;
pub use fields::{FieldKind, FieldList, FieldRow, ValueFont};
pub use hero::{BitrateStatus, CodecStatus, HeroStatusView, HeroVolume, HeroVolumeSource};
pub use home::{build_home_screen, HOME_TITLE};
pub use list::{ListItem, ListItemKey, VerticalList, ROW_HEIGHT};
pub use menu::{MenuItem, MenuList};
pub use message::MessageView;
pub use navigator::Navigator;
pub use paint_key::PaintKey;
pub use rail::{Button, ButtonLabel, ButtonLabels};
pub use screen::Screen;
pub use spacer::Spacer;
pub use widget::{Action, ChromeContribution, ChromeStatus, FocusEvent, Verb, VolumeChrome, Widget};
pub use wizard::{build_wizard_screen, WIZARD_TITLE};
