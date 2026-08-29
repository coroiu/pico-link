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
//!   kept deliberately separate from `list`'s two-line row style.
//! - [`message`]: [`MessageView`] — the shared "nothing to show here"
//!   widget (icon + headline + subline), reusable by any placeholder
//!   content state.
//! - [`confirm`]: [`ConfirmView`] — a self-contained
//!   destructive-confirmation screen (headline + a wrapped [`MenuList`]),
//!   for any "are you sure?" flow.
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

pub mod chrome;
pub mod confirm;
pub mod framebuffer;
pub mod hero;
pub mod list;
pub mod menu;
pub mod message;
pub mod navigator;
pub mod rail;
pub mod screen;
pub mod theme;
pub mod widget;

pub use chrome::{compute_chrome, compute_chrome_for, ChromeLayout};
pub use confirm::ConfirmView;
pub use framebuffer::FrameBuffer565;
pub use hero::{BitrateStatus, CodecStatus, HeroStatusView};
pub use list::{ListItem, ListItemKey, VerticalList, ROW_HEIGHT};
pub use menu::{MenuItem, MenuList};
pub use message::MessageView;
pub use navigator::Navigator;
pub use rail::{Button, ButtonLabel, ButtonLabels};
pub use screen::Screen;
pub use widget::{Action, ChromeContribution, ChromeStatus, FocusEvent, Widget};
