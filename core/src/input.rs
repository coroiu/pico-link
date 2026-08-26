use serde::{Deserialize, Serialize};

/// Semantic navigation intent: the hardware-independent "Tier 2" input
/// abstraction the app core reacts to. Raw platform events (joystick GPIO
/// edges, button presses, keyboard keys, headless HTTP JSON) are mapped to
/// this enum by the platform/emulator layer; app code never mentions
/// "GPIO" or "keyboard".
///
/// Modeled directly on the real input hardware: a 5-way joystick (up/
/// down/left/right + center press) plus four dedicated buttons (A/B/X/Y).
/// This is strictly richer than a single-axis rotary encoder — up/down and
/// left/right are two independent axes rather than one, the center press
/// is a distinct event from any button, and there's a real, dedicated
/// `Back` (button B) rather than a long-press timer standing in for one.
///
/// This is a placeholder module seam only: nothing in the core wires this
/// up to a real product screen yet — only the generic placeholder list in
/// `crate::app`. It exists now so the boundary is visible in the workspace
/// split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NavIntent {
    /// Joystick up: move focus/selection up one step.
    Up,
    /// Joystick down: move focus/selection down one step.
    Down,
    /// Joystick left: move along the secondary (horizontal) axis. No
    /// widget in this crate consumes this yet (every content widget today
    /// is a single vertical list/menu) — it's forwarded to the focused
    /// widget and otherwise a no-op, ready for a future horizontal-paging
    /// or tabbed widget.
    Left,
    /// Joystick right: see [`NavIntent::Left`].
    Right,
    /// Jump by `n` steps along the primary (vertical) axis in one input
    /// event — e.g. a held direction with repeat-acceleration, or a
    /// headless-driven "page" jump. Positive is downward/forward,
    /// negative is upward/backward: signed, unlike the old rotary
    /// vocabulary's direction-less jump, because `Up`/`Down` are now two
    /// distinct intents rather than two directions of one rotation.
    JumpBy(i16),
    /// Select the focused item: joystick center press, or button A.
    Select,
    /// Return to the parent screen / dismiss the current one: button B. A
    /// real, dedicated back button — not a long-press timer standing in
    /// for one.
    Back,
    /// Button X: unbound today. Reserved for a future per-screen
    /// shortcut/context action — one of the things four dedicated buttons
    /// buy over a single rotary control.
    ShortcutX,
    /// Button Y: unbound today. See [`NavIntent::ShortcutX`].
    ShortcutY,
}
