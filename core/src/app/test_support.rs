#![cfg(test)]

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::input::NavIntent;

use super::{App, ConnectedCodec, DeviceAddr, Event, LinkState, PairedDevice, ScreenCarry};

/// Home(1) -> Devices(2): reaching the Devices screen takes two
/// `Select`s -- centre toggles Home to its menu face (Bluetooth
/// pre-selected), centre again activates that row.
pub(in crate::app) fn open_devices(app: &mut App) {
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    app.handle_input(vec![NavIntent::Select]); // Bluetooth row -> pushes Devices
}

/// Home(1) -> Devices(2) -> Wizard(3) -- see `open_devices` above; one
/// further `Select` activates the fixed "Pair new headphones" row
/// (there being no other paired devices at this point), pushing the
/// wizard straight into `WizardPhase::Scanning`. Local copy of
/// `render::wizard`'s own test-only helper of the same name -- that
/// one is private to its module's test mod, and this crate has no
/// shared test-support module to hoist it into.
pub(in crate::app) fn open_wizard(app: &mut App) {
    open_devices(app);
    app.handle_input(vec![NavIntent::Select]); // "Pair new headphones" row -> pushes the wizard
}

/// The Devices screen reads `BtModel::paired` (not `BtModel::discovered`,
/// the wizard's own scan list -- see that field's doc comment), mutated
/// only by [`Event::PairedDeviceUpserted`]/[`Event::PairedDeviceForgotten`].
/// Shorthand for building one such event in these tests.
pub(in crate::app) fn upsert(addr: [u8; 6], name: &str, mru_seq: u32) -> Event {
    Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality: 0 })
}

/// Like [`upsert`] but with a real `ldac_quality`, for tests of the
/// `QUALITY` row/picker's stored-echo behaviour.
pub(in crate::app) fn upsert_with_quality(addr: [u8; 6], name: &str, mru_seq: u32, ldac_quality: u8) -> Event {
    Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality })
}

pub(in crate::app) const DGX_ADDR: DeviceAddr = [9, 8, 7, 6, 5, 4];

/// Folds the two events that take a fresh `App` from Idle to a fully
/// connected, codec-reporting link -- exactly what `firmware/src/a2dp.c`
/// fires in sequence on a real successful pairing.
pub(in crate::app) fn connect_link(app: &mut App) {
    app.handle_event(Event::LinkStateChanged(LinkState::Connected));
    app.handle_event(Event::CodecChanged(ConnectedCodec {
        addr: DGX_ADDR,
        word: String::from("LDAC"),
        nominal_bitrate_bps: 909_000,
    }));
}

pub(in crate::app) fn assert_link_still_connected(app: &App) {
    assert_eq!(app.model().link_state, LinkState::Connected, "link_state must still read Connected");
    assert_eq!(
        app.model().connected_codec.as_ref().map(|c| c.word.as_str()),
        Some("LDAC"),
        "connected_codec must survive the navigation -- this is what the hero word keys off"
    );
}

/// Drains the command queue and asserts it was completely empty -- not
/// just absent of one variant. A test that only checks for a Disconnect
/// that cannot exist in the FFI (`pico_link_ui.h:51-76` has no such tag)
/// would prove nothing.
pub(in crate::app) fn assert_no_commands_queued(app: &mut App) {
    let mut drained = Vec::new();
    while let Some(cmd) = app.poll_command() {
        drained.push(cmd);
    }
    assert!(drained.is_empty(), "navigating to Home must not queue any command, got {drained:?}");
}

/// Renders Home (must be at depth 1, status face) and checks the hero
/// paints the connected codec word in `TEXT_PRIMARY` (nominal, non-
/// fallback connection -- see `render::hero`'s own
/// `nominal_codec_renders_the_hero_word_in_text_primary` for the same
/// pixel-presence technique) and paints no `STATUS_ERROR` ink anywhere
/// -- `STATUS_ERROR` is exactly what `CodecStatus::NoLink` uses for the
/// "NO LINK" word (`render/hero.rs`'s `no_link_renders_the_hero_word_
/// in_status_error`), so its presence would mean Home rendered
/// disconnected even though the model says otherwise.
pub(in crate::app) fn assert_home_hero_renders_connected(app: &mut App) {
    use crate::render::theme::palette;

    assert_eq!(app.navigator_depth(), 1, "hero only renders on Home's status face at the root");
    let fb = app.render();
    assert!(
        fb.pixels().any(|p| p.1 == palette::TEXT_PRIMARY),
        "a nominal connected codec word should paint TEXT_PRIMARY ink somewhere"
    );
    assert!(
        !fb.pixels().any(|p| p.1 == palette::STATUS_ERROR),
        "STATUS_ERROR ink anywhere means Home rendered NO LINK despite a connected model"
    );
}

pub(in crate::app) fn no_carry() -> ScreenCarry {
    ScreenCarry { selected_key: None, selected_index: 0, scroll_top: None }
}
