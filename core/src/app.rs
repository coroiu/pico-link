//! `App`: the platform-free application state the unified main loop
//! ([`crate::run::run`]) drives every frame.
//!
//! This is the minimal shell left after stripping the previous product
//! layer down to a generic UI-framework template: a [`Navigator`] built
//! once over a placeholder root screen, plus the single [`FrameBuffer565`]
//! it renders into. There is no domain model, no sync, and no output seam
//! wired up here — those are exactly the pieces a concrete product adds on
//! top of this shell: build real content [`Screen`]s, wire `Action`s to
//! push/pop them, and hand `App` whatever live state those screens need to
//! read.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::input::NavIntent;
use crate::render::{Action, FrameBuffer565, ListItem, Navigator, Screen, VerticalList};

/// The Bluetooth link's coarse lifecycle state, as reported by C over
/// [`App::set_link_state`] (`pl_ui_set_link_state` in the FFI surface).
/// Platform-free: `core` has no idea BTstack exists, it only knows these
/// four labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Idle,
    Scanning,
    Connecting,
    Connected,
}

impl LinkState {
    fn label(self) -> &'static str {
        match self {
            LinkState::Idle => "Idle",
            LinkState::Scanning => "Scanning...",
            LinkState::Connecting => "Connecting...",
            LinkState::Connected => "Connected",
        }
    }
}

/// One discovered Bluetooth device, as reported by C over
/// [`App::add_device`] (`pl_ui_add_device` in the FFI surface). `addr` is a
/// 6-byte Bluetooth device address, big-endian as BTstack itself reports it
/// -- `core` never interprets the bytes, only round-trips them back out via
/// [`Command::Connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    pub addr: [u8; 6],
    pub name: String,
    pub rssi: i8,
}

/// A user-initiated action queued by the devices screen for C to poll via
/// [`App::poll_command`] (`pl_ui_poll_command` in the FFI surface). `core`
/// never acts on these itself -- it has no Bluetooth stack to act with --
/// it only records "the user asked for this" and hands it back out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    StartScan,
    Connect { addr: [u8; 6] },
}

/// Builds the devices screen: a "Scan for headphones" row (its sublabel is
/// the live [`LinkState`] label) followed by one row per discovered
/// [`DeviceEntry`]. Selecting the scan row queues [`Command::StartScan`];
/// selecting a device row queues [`Command::Connect`] with that device's
/// address. Rebuilt from scratch on every state change (see
/// [`App::rebuild_root`]) rather than mutated in place -- simplest correct
/// thing for a list this small, and it keeps the closures below trivially
/// `'static` (each rebuild captures a fresh, owned snapshot).
fn build_devices_screen(link_state: LinkState, devices: &[DeviceEntry], commands: &Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let mut items = vec![ListItem::new("Scan for headphones").with_sublabel(link_state.label())];
    if devices.is_empty() {
        // A single-row list has nowhere for Up/Down to move the selection
        // to, which also reads as a dead screen to a first-time user --
        // an explicit "nothing found yet" row keeps the list navigable
        // and communicates the empty state instead of just looking inert.
        items.push(ListItem::new("No devices found").with_sublabel("Select Scan to search"));
    }
    for device in devices {
        let label = if device.name.is_empty() { String::from("(unknown device)") } else { device.name.clone() };
        let sublabel = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}  RSSI {}",
            device.addr[0], device.addr[1], device.addr[2], device.addr[3], device.addr[4], device.addr[5], device.rssi
        );
        items.push(ListItem::new(label).with_sublabel(sublabel));
    }

    let devices_snapshot: Vec<DeviceEntry> = devices.to_vec();
    let commands_for_activate = Rc::clone(commands);
    let list = VerticalList::new(items).on_activate_index(move |index| {
        if index == 0 {
            commands_for_activate.borrow_mut().push_back(Command::StartScan);
        } else if let Some(device) = devices_snapshot.get(index - 1) {
            commands_for_activate.borrow_mut().push_back(Command::Connect { addr: device.addr });
        }
        Action::None
    });
    Screen::new("Pico Link", vec![Box::new(list)]).with_hint("Up/Down  Select  Back")
}

/// The application core: a [`Navigator`] built once over a placeholder
/// root screen, and the single [`FrameBuffer565`] it renders into.
pub struct App {
    navigator: Navigator,
    framebuffer: FrameBuffer565,
    /// Whether the current screen state has changed since the last
    /// [`App::render`] call. The run loop uses this to skip
    /// `DisplaySurface::flush` on frames where nothing changed.
    dirty: bool,
    link_state: LinkState,
    devices: Vec<DeviceEntry>,
    /// Queued by the devices screen's `on_activate_index` closures (see
    /// [`build_devices_screen`]), drained by [`App::poll_command`]. `Rc`+
    /// `RefCell` because the closures live inside the `Navigator`'s screen
    /// stack, with no path back to `App` itself -- this is the shared
    /// mailbox between them.
    commands: Rc<RefCell<VecDeque<Command>>>,
}

impl App {
    /// Builds the app, rendering into a `width`x`height` framebuffer.
    /// `width`/`height` should match whatever the platform's
    /// `DisplaySurface` actually presents — the core has no way to
    /// discover this itself, so callers (each run mode's `main.rs`) pass
    /// in whatever their concrete surface is sized for.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let commands = Rc::new(RefCell::new(VecDeque::new()));
        let link_state = LinkState::Idle;
        let devices = Vec::new();
        let navigator = Navigator::new(build_devices_screen(link_state, &devices, &commands));
        Self { navigator, framebuffer: FrameBuffer565::new(width, height), dirty: true, link_state, devices, commands }
    }

    /// Rebuilds the navigator from scratch over a fresh devices screen
    /// reflecting the current `link_state`/`devices` -- see
    /// [`build_devices_screen`]'s doc comment for why a full rebuild
    /// rather than an in-place mutation.
    fn rebuild_root(&mut self) {
        self.navigator = Navigator::new(build_devices_screen(self.link_state, &self.devices, &self.commands));
        self.dirty = true;
    }

    /// Records the Bluetooth link's coarse lifecycle state
    /// (`pl_ui_set_link_state`'s core-side implementation) and refreshes
    /// the devices screen's scan-row sublabel to match.
    pub fn set_link_state(&mut self, state: LinkState) {
        self.link_state = state;
        self.rebuild_root();
    }

    /// Adds (or, if `addr` is already known, updates the name/rssi of) one
    /// discovered device and refreshes the devices screen
    /// (`pl_ui_add_device`'s core-side implementation). Update-in-place
    /// rather than appending a duplicate row: BTstack's inquiry reports the
    /// same device repeatedly as its RSSI/name resolve.
    pub fn add_device(&mut self, addr: [u8; 6], name: String, rssi: i8) {
        if let Some(existing) = self.devices.iter_mut().find(|d| d.addr == addr) {
            existing.name = name;
            existing.rssi = rssi;
        } else {
            self.devices.push(DeviceEntry { addr, name, rssi });
        }
        self.rebuild_root();
    }

    /// Clears the discovered-device list, e.g. at the start of a fresh scan
    /// (`pl_ui_clear_devices`'s core-side implementation).
    pub fn clear_devices(&mut self) {
        self.devices.clear();
        self.rebuild_root();
    }

    /// Pops the oldest queued user command, if any
    /// (`pl_ui_poll_command`'s core-side implementation). `core` never acts
    /// on these itself -- see [`Command`]'s doc comment.
    pub fn poll_command(&mut self) -> Option<Command> {
        self.commands.borrow_mut().pop_front()
    }

    /// How many screens are on the navigator's stack (>= 1). Exposed for
    /// tests/diagnostics.
    #[must_use]
    pub fn navigator_depth(&self) -> usize {
        self.navigator.depth()
    }

    /// Dispatches every polled `NavIntent` to the navigator, in order.
    /// A no-op (including leaving `dirty` untouched) if `intents` is empty.
    pub fn handle_input(&mut self, intents: Vec<NavIntent>) {
        if intents.is_empty() {
            return;
        }
        for intent in intents {
            self.navigator.dispatch(intent);
        }
        self.dirty = true;
    }

    /// Whether [`App::render`] would draw something different from the
    /// last time it was called.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Forces the next [`App::render`] to redraw, without any actual
    /// screen-state change. Platform-free: this is plumbing for the
    /// idle-screensaver run loop to force a repaint on wake (the
    /// framebuffer's *content* never changed while the display was off,
    /// but the display itself needs a fresh flush once it's powered back
    /// on).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Renders the current screen into the app's framebuffer and clears
    /// the dirty flag, returning the freshly rendered framebuffer for the
    /// caller to hand to a `DisplaySurface::flush`.
    ///
    /// # Panics
    ///
    /// Never, in practice: `Navigator::render`'s `Result` is over
    /// `Infallible`'s uninhabited error type (the core `DrawTarget` can
    /// never fail to draw). The `expect` exists only because
    /// `Result::expect` is how that's asserted at the call site.
    pub fn render(&mut self) -> &FrameBuffer565 {
        self.navigator
            .render(&mut self.framebuffer)
            .expect("core DrawTarget is Infallible");
        self.dirty = false;
        &self.framebuffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_app_is_dirty_and_renders_the_initial_screen() {
        let app = App::new(240, 240);
        assert!(app.dirty());
    }

    #[test]
    fn render_clears_the_dirty_flag() {
        let mut app = App::new(240, 240);
        assert!(app.dirty());
        app.render();
        assert!(!app.dirty());
    }

    #[test]
    fn mark_dirty_sets_the_flag_even_with_no_screen_state_change() {
        let mut app = App::new(240, 240);
        app.render();
        assert!(!app.dirty());

        app.mark_dirty();
        assert!(app.dirty(), "mark_dirty should force the flag on");
    }

    #[test]
    fn handle_input_with_no_intents_does_not_mark_dirty() {
        let mut app = App::new(240, 240);
        app.render();
        assert!(!app.dirty());
        app.handle_input(vec![]);
        assert!(!app.dirty());
    }

    #[test]
    fn handle_input_marks_dirty_and_moving_selection_changes_the_rendered_framebuffer() {
        use crate::render::theme::palette;
        use embedded_graphics::prelude::Point;

        let mut app = App::new(240, 240);

        // x=200: past the chip/accent area and these short labels' text,
        // so it samples the row's plain elevated fill rather than a glyph
        // pixel, and still inside the 240px-wide (Epic B2) panel.
        let frame_0 = app.render().pixel(Point::new(200, 18));
        assert_eq!(frame_0, palette::SURFACE_ELEVATED, "row 0 should start selected");

        app.handle_input(vec![NavIntent::Down]);
        assert!(app.dirty(), "moving selection should mark the app dirty");

        let frame_1_row_0 = app.render().pixel(Point::new(200, 18));
        assert_ne!(frame_1_row_0, palette::SURFACE_ELEVATED, "row 0 should no longer be selected");
    }

    #[test]
    fn navigator_starts_at_depth_one_with_the_placeholder_root_screen() {
        let app = App::new(240, 240);
        assert_eq!(app.navigator_depth(), 1);
    }
}
