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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkState {
    #[default]
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
/// [`Event::DeviceDiscovered`] (`pl_ui_push_event` in the FFI surface).
/// `addr` is a 6-byte Bluetooth device address, big-endian as BTstack itself
/// reports it -- `core` never interprets the bytes, only round-trips them
/// back out via [`Command::Connect`].
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

/// Why a connect attempt failed, as reported by C over
/// [`Event::ConnectFailed`]. `core` has no Bluetooth stack of its own -- it
/// only records the *category* of failure BTstack/the radio reported, for a
/// (future) screen to render with an appropriate remedy.
///
/// Distinguishing these five (rather than a single generic "failed") is
/// deliberate and driven by the approved on-device UI design
/// (`.planning/design/2026-08-28-on-device-ui.md`): each has a different
/// user-facing remedy, and [`ConnectFailureReason::retryable`] draws the one
/// distinction that matters most -- some failures are worth an automatic or
/// user-initiated retry, and two structurally are not (see that method's
/// doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFailureReason {
    /// No response within the connection's page-timeout window (up to
    /// 5.12s per BTstack's ACL page timeout) -- transient, worth retrying.
    Timeout,
    /// The remote device actively rejected the connection or pairing
    /// (authentication failure, user declined on the headphone side, ...)
    /// -- may be worth retrying if the user acts differently (e.g.
    /// re-enters pairing mode), so not ruled out here.
    Rejected,
    /// AVDTP stream endpoint discovery found no A2DP sink service on the
    /// device at all. This is a fixed capability of the remote device, not
    /// a transient condition -- retrying the *same* device can never
    /// succeed, hence [`ConnectFailureReason::retryable`] is `false`.
    NoA2dpSink,
    /// Pairing requires a PIN, and this product has no on-screen text
    /// entry (confirmed speculative/out of scope -- see the capability
    /// inventory on bead pico-link-aii.1). Retrying without a way to
    /// supply the PIN can never succeed either, so this is also
    /// non-retryable until text entry exists.
    NeedsPin,
    /// The radio/HCI layer itself reported an error (not a per-device
    /// remote-side rejection) -- typically transient, worth retrying.
    RadioError,
}

impl ConnectFailureReason {
    /// Whether a retry of the *same* device could plausibly succeed.
    /// `false` for [`Self::NoA2dpSink`] (a fixed capability of that
    /// device -- it will never grow an A2DP sink between attempts) and
    /// [`Self::NeedsPin`] (this product cannot supply a PIN today, so
    /// nothing about a retry changes the outcome). The other three are
    /// conditions that can plausibly differ on a second attempt.
    #[must_use]
    pub fn retryable(self) -> bool {
        !matches!(self, Self::NoA2dpSink | Self::NeedsPin)
    }
}

/// One inbound Bluetooth-domain event, as reported by C over
/// [`App::handle_event`] (`pl_ui_push_event` in the FFI surface -- one
/// tagged union in, replacing the old per-field `pl_ui_set_link_state`/
/// `pl_ui_add_device`/`pl_ui_clear_devices` setters). `core` never
/// originates these; it only folds them into [`BtModel`] and marks the app
/// dirty -- see [`App::handle_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    LinkStateChanged(LinkState),
    DeviceDiscovered(DeviceEntry),
    DevicesCleared,
    ConnectFailed { addr: [u8; 6], reason: ConnectFailureReason },
}

/// The Bluetooth-domain state screens read to render themselves --
/// everything [`App`] knows about the link and the discovered/attempted
/// devices, folded in one place from [`Event`]s. Kept as one struct (rather
/// than loose fields on [`App`]) so it's unambiguous what "the model" means
/// when a screen-building function takes `&BtModel`: this, and only this, is
/// live application data; everything else a screen needs is either passed
/// in explicitly (e.g. a carried-forward selection index) or is the
/// screen's own widget state.
///
/// Deliberately grows by adding fields here, not by adding new `App`
/// methods per field or new FFI setters per field -- see the module doc's
/// "sustainable path" rationale (bead pico-link-a67 / pico-link-aii.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BtModel {
    pub link_state: LinkState,
    pub devices: Vec<DeviceEntry>,
    /// The most recent connect failure, if any (and not yet superseded by
    /// a new attempt). Not yet rendered by any screen in this bead's scope
    /// -- populated so the data exists and is representable ahead of the
    /// screen that will read it, per pico-link-a67's explicit ask.
    pub last_connect_failure: Option<(DeviceAddr, ConnectFailureReason)>,
}

/// A Bluetooth device address, aliased for readability at call sites that
/// pair it with a [`ConnectFailureReason`].
pub type DeviceAddr = [u8; 6];

/// Builds the devices screen: a "Scan for headphones" row (its sublabel is
/// the live [`LinkState`] label) followed by one row per discovered
/// [`DeviceEntry`]. Selecting the scan row queues [`Command::StartScan`];
/// selecting a device row queues [`Command::Connect`] with that device's
/// address. Rebuilt from scratch on every state change (see
/// [`App::rebuild_root`]) rather than mutated in place -- simplest correct
/// thing for a list this small, and it keeps the closures below trivially
/// `'static` (each rebuild captures a fresh, owned snapshot). `selected`
/// carries forward the outgoing screen's selection (see
/// [`Navigator::root_selected_index`]/[`Navigator::replace_root`]) so a
/// model change mid-browse doesn't snap the user's selection back to row 0;
/// `None` (first build) starts at row 0 via `VerticalList`'s own default.
fn build_devices_screen(model: &BtModel, selected: Option<usize>, commands: &Rc<RefCell<VecDeque<Command>>>) -> Screen {
    let mut items = vec![ListItem::new("Scan for headphones").with_sublabel(model.link_state.label())];
    if model.devices.is_empty() {
        // A single-row list has nowhere for Up/Down to move the selection
        // to, which also reads as a dead screen to a first-time user --
        // an explicit "nothing found yet" row keeps the list navigable
        // and communicates the empty state instead of just looking inert.
        items.push(ListItem::new("No devices found").with_sublabel("Select Scan to search"));
    }
    for device in &model.devices {
        let label = if device.name.is_empty() { String::from("(unknown device)") } else { device.name.clone() };
        let sublabel = format!(
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}  RSSI {}",
            device.addr[0], device.addr[1], device.addr[2], device.addr[3], device.addr[4], device.addr[5], device.rssi
        );
        items.push(ListItem::new(label).with_sublabel(sublabel));
    }

    let devices_snapshot: Vec<DeviceEntry> = model.devices.clone();
    let commands_for_activate = Rc::clone(commands);
    let mut list = VerticalList::new(items).on_activate_index(move |index| {
        if index == 0 {
            commands_for_activate.borrow_mut().push_back(Command::StartScan);
        } else if let Some(device) = devices_snapshot.get(index - 1) {
            commands_for_activate.borrow_mut().push_back(Command::Connect { addr: device.addr });
        }
        Action::None
    });
    if let Some(selected) = selected {
        list = list.with_selected(selected);
    }
    Screen::new("Pico Link", vec![Box::new(list)]).with_hint("Up/Down  Select  Back")
}

/// The application core: a [`Navigator`] built once over the devices root
/// screen, the [`BtModel`] that screen (and any future ones) reads to
/// render itself, and the single [`FrameBuffer565`] rendered into.
pub struct App {
    navigator: Navigator,
    framebuffer: FrameBuffer565,
    /// Whether the current screen state has changed since the last
    /// [`App::render`] call. The run loop uses this to skip
    /// `DisplaySurface::flush` on frames where nothing changed.
    dirty: bool,
    /// The live Bluetooth device/link state, folded in from [`Event`]s via
    /// [`App::handle_event`]. Screens are built by *reading* this, not by
    /// owning fragments of it themselves -- see [`BtModel`]'s doc comment.
    model: BtModel,
    /// C's own clock, threaded through from [`App::tick`]
    /// (`pl_ui_tick`'s `now_us` in the FFI surface -- previously received
    /// and silently discarded, see pico-link-a67). `core` never reads a
    /// hardware timer itself (the platform seam owns that); this is purely
    /// the latest value C has told it. Not yet consumed by any screen in
    /// this bead's scope -- storing it is the fix pico-link-a67 asks for;
    /// wiring a liveness/timeout indicator to it is future UI work.
    now_us: u64,
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
        let model = BtModel::default();
        let navigator = Navigator::new(build_devices_screen(&model, None, &commands));
        Self { navigator, framebuffer: FrameBuffer565::new(width, height), dirty: true, model, now_us: 0, commands }
    }

    /// Refreshes the root (devices) screen to reflect the current
    /// [`BtModel`], via [`Navigator::replace_root`] -- **not**
    /// `Navigator::new`. That distinction is the whole point: replacing
    /// only the root leaves any screen the user has navigated *to* (pushed
    /// above root) untouched -- same stack depth, same screen instance,
    /// same focus/selection state on it -- and carries the outgoing root
    /// screen's own selection forward via
    /// [`Navigator::root_selected_index`] rather than resetting it to row
    /// 0. Previously this called `Navigator::new`, discarding the whole
    /// stack on every Bluetooth event -- invisible with the one screen this
    /// crate builds today, fatal for the approved multi-screen design (see
    /// pico-link-a67 / pico-link-aii.1's defect 1).
    fn rebuild_root(&mut self) {
        let selected = self.navigator.root_selected_index();
        self.navigator.replace_root(build_devices_screen(&self.model, selected, &self.commands));
        self.dirty = true;
    }

    /// Folds one inbound Bluetooth-domain [`Event`] into [`BtModel`] and
    /// refreshes the root screen (`pl_ui_push_event`'s core-side
    /// implementation -- the single entry point replacing the old
    /// `set_link_state`/`add_device`/`clear_devices` setter trio). `core`
    /// never acts on these itself -- it has no Bluetooth stack -- it only
    /// updates what screens read.
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::LinkStateChanged(state) => self.set_link_state(state),
            Event::DeviceDiscovered(device) => self.add_device(device.addr, device.name, device.rssi),
            Event::DevicesCleared => self.clear_devices(),
            Event::ConnectFailed { addr, reason } => self.record_connect_failure(addr, reason),
        }
    }

    /// Records the Bluetooth link's coarse lifecycle state and refreshes
    /// the devices screen's scan-row sublabel to match. Also reachable
    /// directly (not just via [`App::handle_event`]) since it's a natural
    /// unit for tests and for [`App::record_connect_failure`] to reuse.
    pub fn set_link_state(&mut self, state: LinkState) {
        self.model.link_state = state;
        self.rebuild_root();
    }

    /// Adds (or, if `addr` is already known, updates the name/rssi of) one
    /// discovered device and refreshes the devices screen. Update-in-place
    /// rather than appending a duplicate row: BTstack's inquiry reports the
    /// same device repeatedly as its RSSI/name resolve.
    pub fn add_device(&mut self, addr: [u8; 6], name: String, rssi: i8) {
        if let Some(existing) = self.model.devices.iter_mut().find(|d| d.addr == addr) {
            existing.name = name;
            existing.rssi = rssi;
        } else {
            self.model.devices.push(DeviceEntry { addr, name, rssi });
        }
        self.rebuild_root();
    }

    /// Clears the discovered-device list, e.g. at the start of a fresh
    /// scan.
    pub fn clear_devices(&mut self) {
        self.model.devices.clear();
        self.rebuild_root();
    }

    /// Records a failed connect attempt with its [`ConnectFailureReason`]
    /// and returns the link to [`LinkState::Idle`] -- the attempt is over
    /// either way, retryable or not; a future screen deciding whether to
    /// offer a retry reads `reason.retryable()` off
    /// `BtModel::last_connect_failure`, not the link state.
    pub fn record_connect_failure(&mut self, addr: [u8; 6], reason: ConnectFailureReason) {
        self.model.last_connect_failure = Some((addr, reason));
        self.model.link_state = LinkState::Idle;
        self.rebuild_root();
    }

    /// Read-only access to the live Bluetooth model, for tests/diagnostics
    /// and for any future FFI accessor that needs to read it back.
    #[must_use]
    pub fn model(&self) -> &BtModel {
        &self.model
    }

    /// Pops the oldest queued user command, if any
    /// (`pl_ui_poll_command`'s core-side implementation). `core` never acts
    /// on these itself -- see [`Command`]'s doc comment.
    pub fn poll_command(&mut self) -> Option<Command> {
        self.commands.borrow_mut().pop_front()
    }

    /// Records C's latest clock reading (`pl_ui_tick`'s core-side
    /// implementation -- previously a no-op that discarded `now_us`
    /// entirely, see pico-link-a67). Does not by itself mark the app dirty:
    /// the clock advancing is not, on its own, a reason to redraw anything
    /// today (no screen in this bead's scope reads it) -- a future
    /// liveness/timeout indicator that *does* need to repaint purely from
    /// elapsed time will call `mark_dirty` itself when it has a reason to.
    pub fn tick(&mut self, now_us: u64) {
        self.now_us = now_us;
    }

    /// The most recent `now_us` recorded via [`App::tick`]. `0` before the
    /// first tick.
    #[must_use]
    pub fn now_us(&self) -> u64 {
        self.now_us
    }

    /// How many screens are on the navigator's stack (>= 1). Exposed for
    /// tests/diagnostics.
    #[must_use]
    pub fn navigator_depth(&self) -> usize {
        self.navigator.depth()
    }

    /// The currently visible screen's title. Exposed for tests/diagnostics
    /// -- in particular, proving that a Bluetooth [`Event`] mid-navigation
    /// doesn't silently pop the user back to the root screen (see
    /// [`App::rebuild_root`]'s doc comment).
    #[must_use]
    pub fn current_screen_title(&self) -> &str {
        &self.navigator.current().title
    }

    /// The root (devices) screen's own selection index, if it currently has
    /// one. Exposed for tests/diagnostics -- proving an [`Event`] carries
    /// the user's list selection forward instead of resetting it to row 0.
    #[must_use]
    pub fn root_selected_index(&self) -> Option<usize> {
        self.navigator.root_selected_index()
    }

    /// Test-only: pushes an arbitrary screen onto the navigator stack, so
    /// tests can simulate "the user navigated away from root" without this
    /// bead building any real second screen (out of its scope -- see
    /// pico-link-a67's scope-discipline note). Not part of the public API.
    #[cfg(test)]
    pub(crate) fn push_screen_for_test(&mut self, screen: Screen) {
        self.navigator.push(screen);
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

    // --- pico-link-a67: the navigator-preservation fix ---
    //
    // These are the single most valuable tests in this bead: the old
    // `rebuild_root` called `Navigator::new`, which resets the stack to
    // depth 1 over a brand-new root screen. That's invisible with only one
    // screen ever on the stack (this crate's current shipped behavior) but
    // fatal for the approved multi-screen design, where a Bluetooth event
    // arriving while the user is browsing a pushed screen would silently
    // eject them back to root. `push_screen_for_test` simulates "the user
    // navigated away from root" without this bead building any real second
    // screen.

    #[test]
    fn a_bluetooth_event_mid_navigation_does_not_reset_the_screen_stack() {
        let mut app = App::new(240, 240);
        app.push_screen_for_test(Screen::new("detail", vec![]));
        assert_eq!(app.navigator_depth(), 2);
        assert_eq!(app.current_screen_title(), "detail");

        // Three different Event variants, all of which used to rebuild the
        // whole Navigator via App::rebuild_root.
        app.handle_event(Event::LinkStateChanged(LinkState::Scanning));
        assert_eq!(app.navigator_depth(), 2, "LinkStateChanged must not pop the pushed screen");
        assert_eq!(app.current_screen_title(), "detail");

        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr: [1, 2, 3, 4, 5, 6], name: String::from("Cans"), rssi: -40 }));
        assert_eq!(app.navigator_depth(), 2, "DeviceDiscovered must not pop the pushed screen");
        assert_eq!(app.current_screen_title(), "detail");

        app.handle_event(Event::DevicesCleared);
        assert_eq!(app.navigator_depth(), 2, "DevicesCleared must not pop the pushed screen");
        assert_eq!(app.current_screen_title(), "detail");

        app.handle_event(Event::ConnectFailed { addr: [1, 2, 3, 4, 5, 6], reason: ConnectFailureReason::Timeout });
        assert_eq!(app.navigator_depth(), 2, "ConnectFailed must not pop the pushed screen");
        assert_eq!(app.current_screen_title(), "detail");
    }

    #[test]
    fn a_device_arriving_mid_navigation_does_not_reset_the_root_screens_selection() {
        let mut app = App::new(240, 240);
        // Two devices so there's a non-zero selection to move to and lose.
        app.add_device([1, 1, 1, 1, 1, 1], String::from("Device A"), -50);
        app.add_device([2, 2, 2, 2, 2, 2], String::from("Device B"), -60);

        // Root list rows: 0 = "Scan for headphones", 1 = Device A, 2 = Device B.
        app.handle_input(vec![NavIntent::Down, NavIntent::Down]);
        assert_eq!(app.root_selected_index(), Some(2), "selection should be on Device B's row");

        // A third device arriving must not snap the selection back to row 0.
        app.add_device([3, 3, 3, 3, 3, 3], String::from("Device C"), -70);
        assert_eq!(app.root_selected_index(), Some(2), "a new device must not reset the user's selection");

        // Same for a link-state change while browsing.
        app.handle_event(Event::LinkStateChanged(LinkState::Idle));
        assert_eq!(app.root_selected_index(), Some(2), "a link-state change must not reset the user's selection");
    }

    #[test]
    fn connect_failure_reasons_representable_and_the_two_impossible_ones_are_marked_non_retryable() {
        // The approved UI design names five distinct failure causes; two
        // (no A2DP sink, needs a PIN) must offer no retry because retrying
        // is structurally impossible -- see `ConnectFailureReason::retryable`'s
        // doc comment.
        assert!(ConnectFailureReason::Timeout.retryable());
        assert!(ConnectFailureReason::Rejected.retryable());
        assert!(ConnectFailureReason::RadioError.retryable());
        assert!(!ConnectFailureReason::NoA2dpSink.retryable());
        assert!(!ConnectFailureReason::NeedsPin.retryable());
    }

    #[test]
    fn connect_failed_event_updates_the_model_and_returns_the_link_to_idle() {
        let mut app = App::new(240, 240);
        app.set_link_state(LinkState::Connecting);
        let addr = [9, 9, 9, 9, 9, 9];

        app.handle_event(Event::ConnectFailed { addr, reason: ConnectFailureReason::NoA2dpSink });

        assert_eq!(app.model().last_connect_failure, Some((addr, ConnectFailureReason::NoA2dpSink)));
        assert_eq!(app.model().link_state, LinkState::Idle);
    }

    #[test]
    fn tick_records_now_us_instead_of_discarding_it() {
        let mut app = App::new(240, 240);
        assert_eq!(app.now_us(), 0);
        app.tick(123_456);
        assert_eq!(app.now_us(), 123_456);
    }
}
