#![cfg(test)]

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::input::NavIntent;

use super::{App, ConnectedCodec, DeviceAddr, Event, LinkState, PairedDevice, StoreStatus};

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

/// Bead `pico-link-ryw.14`, Ada's preset-id-allocation contract:
/// `App::presets_ready` defaults `false` in real firmware (a real boot must
/// not let anything allocate a preset id before C's own high-water mark has
/// arrived, see that field's doc comment) -- so any test that creates a
/// preset (New effect, import) needs this pushed first, same as C's own
/// real boot sequence would push it. One shared helper rather than seeding
/// every individual test: every call site that needs readiness routes
/// through [`open_effects_list`] or this function directly.
/// `next_id: 1` matches [`crate::dsp::PresetStore::new`]'s own starting
/// value -- these tests want an ordinary "nothing loaded yet" boot, not the
/// high-water-mark-specific scenario `pico-link-ryw.14`'s own "not ready"
/// regression test constructs by hand.
pub(in crate::app) fn ready_presets(app: &mut App) {
    app.handle_event(Event::PresetStoreLoaded { count: 0, status: StoreStatus::FirstBoot, next_id: 1 });
}

/// Home(1) -> Effects list(2): `MENU_ROW_EFFECTS` (`render::home`) is index
/// 1, one `Down` past the menu face's default Bluetooth selection -- see
/// [`open_devices`]'s doc comment for the first `Select`. Also readies the
/// preset store (see [`ready_presets`]) -- every caller of this helper goes
/// on to either read the effects list or create/import a preset, both of
/// which need it.
pub(in crate::app) fn open_effects_list(app: &mut App) {
    ready_presets(app);
    app.handle_input(vec![NavIntent::Select]); // Home status -> menu face (Bluetooth selected)
    app.handle_input(vec![NavIntent::Down]); // move selection onto the Effects row
    app.handle_input(vec![NavIntent::Select]); // Effects row -> pushes the effects list
}

/// [`open_effects_list`] -> editor(3): with an empty [`crate::dsp::
/// PresetStore`] the effects list's sole row is "New effect"
/// (`effects.rs`'s `effects_list_rows`), so one more `Select` activates
/// it -- which, per Andreas's save-immediately ruling, also queues one
/// `Command::SavePreset { preset_id: 0, .. }` before the editor is even
/// built.
pub(in crate::app) fn open_new_effect_editor(app: &mut App) {
    open_effects_list(app);
    app.handle_input(vec![NavIntent::Select]); // "New effect" row -> auto-saves, pushes the editor
}

/// The Devices screen reads `BtModel::paired` (not `BtModel::discovered`,
/// the wizard's own scan list -- see that field's doc comment), mutated
/// only by [`Event::PairedDeviceUpserted`]/[`Event::PairedDeviceForgotten`].
/// Shorthand for building one such event in these tests.
pub(in crate::app) fn upsert(addr: [u8; 6], name: &str, mru_seq: u32) -> Event {
    Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality: 0, preset_id: 0 })
}

/// Like [`upsert`] but with a real `ldac_quality`, for tests of the
/// `QUALITY` row/picker's stored-echo behaviour.
pub(in crate::app) fn upsert_with_quality(addr: [u8; 6], name: &str, mru_seq: u32, ldac_quality: u8) -> Event {
    Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from(name), mru_seq, ldac_quality, preset_id: 0 })
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
        "connected_codec must survive the navigation -- this is what pico-link-1v5 keys the hero word off"
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
