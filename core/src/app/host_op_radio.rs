//! `HOST_OP` (`0x06`) ops 7..13 -- the web companion's device-management
//! write side (design `.planning/design/2026-09-27-iface6-eq-management-
//! protocol.md` sec 13.3/13.6/13.7, bead `pico-link-jyhk.26`). Split out of
//! `host_op.rs` (already 1198 lines before this bead) purely for size; the
//! wire protocol, `OpError` vocabulary and `GET_OP_STATUS` reply are all
//! shared with it -- see that module's doc comment for the parts of the
//! contract this module doesn't repeat.
//!
//! # Ops (design sec 13.3's table)
//!
//! | op | Name | Body | Result payload |
//! |---|---|---|---|
//! | 7 | `SCAN_START` | - | `u16 scan_seq` |
//! | 8 | `SCAN_STOP` | - | - |
//! | 9 | `CONNECT` | `addr[6]` | `u16 attempt_seq` |
//! | 10 | `DISCONNECT` | `addr[6]` | - |
//! | 11 | `FORGET` | `addr[6]` | - |
//! | 12 | `SET_DEVICE_QUALITY` | `addr[6], u8 ldac_quality` (1..3, 4 = Adaptive) | - |
//! | 13 | `CONNECT_CANCEL` | `addr[6]` | - |
//!
//! `scan_seq`/`attempt_seq` ride in [`super::host_op::HostOpStatus::done_word`]'s
//! reused `effect_id` wire slot -- see that constructor's doc comment.
//!
//! # `DEVICE_BUSY`/`RADIO_BUSY` (design sec 13.7)
//!
//! "The person holding the dongle wins": [`wizard_open`] or a device-/
//! auto-reconnect-initiated [`ConnectAttempt`] in flight makes every radio
//! op [`OpError::DeviceBusy`], **except**:
//! - `CONNECT`'s `addr == connected_addr` no-op (design sec 13.3: checked
//!   *before* the busy gate -- asking to connect to the device you're
//!   already on is never contentious, even mid-wizard).
//! - `SCAN_STOP`, whose own narrower rule (`DEVICE_BUSY` iff
//!   `scan_owner == Device`) already implies this: a wizard that isn't
//!   currently scanning has nothing for `SCAN_STOP` to no-op past anyway.
//! - `FORGET`, whose own narrower rule (design sec 13.3: `DEVICE_BUSY` iff
//!   *that address* has a connect attempt in flight, regardless of
//!   initiator) is deliberately not the general rule -- forgetting a
//!   *different* paired device must work while the wizard pairs a new one.
//! - `SET_DEVICE_QUALITY`, which touches no radio state at all (a
//!   persisted-setting write, same as the device page's own picker) and so
//!   design sec 13.3 gives it no busy gate.
//!
//! [`OpError::RadioBusy`] is the narrower "a *Host* attempt is already in
//! flight" case that only `SCAN_START`/`CONNECT` can reach (a device-owned
//! attempt is already caught by `DeviceBusy` above).

use alloc::string::String;

#[allow(unused_imports)] // only used from intra-doc links (`Command::Disconnect` etc.) in this module's non-test code; every test reaches it through `use super::*`
use super::events::Command;
use super::host_op::{HostOpStatus, OpError};
use super::model::{is_audio_sink, truncate_device_name, ConnectInitiator, MAX_PAIRED_DEVICES};
use super::radio_actions::{cancel_connect, cancel_scan, connect, disconnect, forget, set_quality, start_scan};
use super::screen_id::ScreenId;
use super::App;
use crate::app::model::ScanOwner;

const ADDR_BODY_LEN: usize = 6;
const SET_QUALITY_BODY_LEN: usize = ADDR_BODY_LEN + 1;

/// `ldac_quality`'s valid wire range -- design sec 13.3: "1..3, 4 =
/// Adaptive". `0` ("never chosen") is a *stored*-only value
/// ([`super::model::PairedDevice::ldac_quality`]'s doc comment); a host can
/// never request it.
const LDAC_QUALITY_MIN: u8 = 1;
const LDAC_QUALITY_MAX: u8 = 4;

fn decode_addr(body: &[u8]) -> Option<[u8; 6]> {
    if body.len() < ADDR_BODY_LEN {
        return None;
    }
    let mut addr = [0u8; 6];
    addr.copy_from_slice(&body[..ADDR_BODY_LEN]);
    Some(addr)
}

impl App {
    /// Design sec 13.7: the pairing wizard is on top of the navigator
    /// stack. Same check [`super::fold`]'s `on_wizard_auto_dismiss` uses
    /// (bead `pico-link-vuou`'s R0a fix) -- a navigator query, not
    /// [`super::ui_state::WizardPhase`], because the latter is written
    /// unconditionally regardless of which screen is open (design sec
    /// 13.5).
    fn wizard_open(&self) -> bool {
        self.navigator.depth() > 0 && self.navigator.id_at(self.navigator.depth() - 1) == Some(ScreenId::PairingWizard)
    }

    /// Design sec 13.7's general `DEVICE_BUSY` gate: the wizard is open, or
    /// the in-flight attempt (if any) wasn't started by a web `HOST_OP`.
    fn device_busy(&self) -> bool {
        self.wizard_open() || self.model.borrow().attempt.as_ref().is_some_and(|a| a.initiator != ConnectInitiator::Host)
    }

    /// Design sec 13.7's narrower `RADIO_BUSY`: a `Host`-initiated attempt
    /// is already in flight. Only ever reached once [`Self::device_busy`]
    /// has already returned `false` -- a device-owned attempt is
    /// `DEVICE_BUSY`, not this.
    fn radio_busy(&self) -> bool {
        self.model.borrow().attempt.is_some()
    }

    /// Design sec 13.6's generalised host lease: "Sec 7's 2 s SETUP-recency
    /// lease generalises: `pl_ui_host_preview_end` becomes `pl_ui_host_
    /// lease_expired` ... which ends a host preview AND, if `scan_owner ==
    /// Host` and discovering, queues `CancelScan`." Replaces the body
    /// [`super::host_op::App::host_preview_end`] used to have directly --
    /// that method is now a thin alias kept for the current `ui-ffi`
    /// binding name (`pl_ui_host_preview_end`) until C switches to the new
    /// one (beads `pico-link-jyhk.28`/`.29`).
    ///
    /// Deliberately does NOT touch [`super::model::BtModel::attempt`] --
    /// design sec 13.6: "Connect needs no lease (bounded, and a
    /// half-finished pairing the user asked for should complete and
    /// persist -- memory: save the pairing at pairing time)." A `CONNECT`
    /// in flight (host- or device-owned) survives a lease expiry
    /// untouched, same as it survives everything except its own
    /// conclusion or an explicit `CONNECT_CANCEL`.
    pub fn host_lease_expired(&mut self) {
        self.host_preview = None;
        let host_scan_running = {
            let model = self.model.borrow();
            model.discovering && model.scan_owner == ScanOwner::Host
        };
        if host_scan_running {
            cancel_scan(&self.commands);
        }
    }

    /// `SCAN_START` (design sec 13.3, op 7).
    pub(super) fn host_op_scan_start(&mut self, seq: u8, op: u8) -> HostOpStatus {
        if self.model.borrow().store_status.is_none() {
            return HostOpStatus::rejected(seq, op, OpError::NotReady);
        }
        if self.model.borrow().paired.len() >= MAX_PAIRED_DEVICES {
            return HostOpStatus::rejected(seq, op, OpError::PairedFull);
        }
        if self.device_busy() {
            return HostOpStatus::rejected(seq, op, OpError::DeviceBusy);
        }
        if self.radio_busy() {
            return HostOpStatus::rejected(seq, op, OpError::RadioBusy);
        }
        let (discovering, scan_owner, scan_seq) = {
            let model = self.model.borrow();
            (model.discovering, model.scan_owner, model.scan_seq)
        };
        if discovering && scan_owner == ScanOwner::Host {
            // Idempotent -- design sec 13.3: "never restarts an inquiry."
            return HostOpStatus::done_word(seq, op, scan_seq);
        }
        start_scan(&self.model, &self.commands, ScanOwner::Host);
        let scan_seq = self.model.borrow().scan_seq;
        HostOpStatus::done_word(seq, op, scan_seq)
    }

    /// `SCAN_STOP` (design sec 13.3, op 8).
    pub(super) fn host_op_scan_stop(&mut self, seq: u8, op: u8) -> HostOpStatus {
        let (discovering, scan_owner) = {
            let model = self.model.borrow();
            (model.discovering, model.scan_owner)
        };
        if !discovering {
            return HostOpStatus::done(seq, op);
        }
        if scan_owner == ScanOwner::Device {
            return HostOpStatus::rejected(seq, op, OpError::DeviceBusy);
        }
        cancel_scan(&self.commands);
        HostOpStatus::done(seq, op)
    }

    /// `CONNECT` (design sec 13.3, op 9).
    pub(super) fn host_op_connect(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        let Some(addr) = decode_addr(body) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };

        if self.model.borrow().connected_addr == Some(addr) {
            // Design sec 13.3: checked before the busy gate -- see this
            // module's doc comment.
            return HostOpStatus::done_word(seq, op, 0);
        }
        if self.device_busy() {
            return HostOpStatus::rejected(seq, op, OpError::DeviceBusy);
        }
        if self.radio_busy() {
            return HostOpStatus::rejected(seq, op, OpError::RadioBusy);
        }

        let (name, is_new): (String, bool) = {
            let model = self.model.borrow();
            if let Some(paired) = model.paired.iter().find(|d| d.addr == addr) {
                (paired.name.clone(), false)
            } else if let Some(discovered) = model.discovered.iter().find(|d| d.addr == addr && is_audio_sink(d.class_of_device)) {
                (discovered.name.clone(), true)
            } else {
                return HostOpStatus::rejected(seq, op, OpError::UnknownDevice);
            }
        };
        if is_new && self.model.borrow().paired.len() >= MAX_PAIRED_DEVICES {
            return HostOpStatus::rejected(seq, op, OpError::PairedFull);
        }

        let host_scan_running = {
            let model = self.model.borrow();
            model.discovering && model.scan_owner == ScanOwner::Host
        };
        if host_scan_running {
            // Design sec 13.3: "If a host scan is running, queue
            // CancelScan first (same as the device, which ends the scan by
            // leaving it)."
            cancel_scan(&self.commands);
        }

        connect(&self.model, &self.commands, addr, truncate_device_name(&name), ConnectInitiator::Host);
        let attempt_seq = self.model.borrow().attempt.as_ref().map_or(0, |a| a.seq);
        HostOpStatus::done_word(seq, op, attempt_seq)
    }

    /// `DISCONNECT` (design sec 13.3, op 10). `addr` is an intent guard
    /// only -- [`Command::Disconnect`] stays addressless (that variant's
    /// doc comment).
    pub(super) fn host_op_disconnect(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        let Some(addr) = decode_addr(body) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };
        if self.device_busy() {
            return HostOpStatus::rejected(seq, op, OpError::DeviceBusy);
        }
        if self.model.borrow().connected_addr != Some(addr) {
            return HostOpStatus::rejected(seq, op, OpError::NotConnected);
        }
        disconnect(&self.commands);
        HostOpStatus::done(seq, op)
    }

    /// `FORGET` (design sec 13.3, op 11). Deliberately does NOT use
    /// [`Self::device_busy`]'s general wizard-open/device-attempt gate --
    /// design sec 13.3's own narrower rule (`DEVICE_BUSY` iff *this*
    /// `addr` has a connect attempt in flight, regardless of initiator)
    /// lets a web `FORGET` of some other paired device proceed while the
    /// wizard pairs a new one.
    pub(super) fn host_op_forget(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        let Some(addr) = decode_addr(body) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };
        if !self.model.borrow().paired.iter().any(|d| d.addr == addr) {
            return HostOpStatus::rejected(seq, op, OpError::UnknownDevice);
        }
        if self.model.borrow().attempt.as_ref().is_some_and(|a| a.addr == addr) {
            return HostOpStatus::rejected(seq, op, OpError::DeviceBusy);
        }
        forget(&self.commands, addr);
        HostOpStatus::done(seq, op)
    }

    /// `SET_DEVICE_QUALITY` (design sec 13.3, op 12). No busy gate -- see
    /// this module's doc comment: it's a persisted-setting write, same as
    /// the device page's own `QUALITY` picker, and touches no radio state.
    pub(super) fn host_op_set_device_quality(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        if body.len() < SET_QUALITY_BODY_LEN {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        let Some(addr) = decode_addr(body) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };
        let ldac_quality = body[ADDR_BODY_LEN];
        if !self.model.borrow().paired.iter().any(|d| d.addr == addr) {
            return HostOpStatus::rejected(seq, op, OpError::UnknownDevice);
        }
        if !(LDAC_QUALITY_MIN..=LDAC_QUALITY_MAX).contains(&ldac_quality) {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        }
        set_quality(&self.commands, addr, ldac_quality);
        HostOpStatus::done(seq, op)
    }

    /// `CONNECT_CANCEL` (design sec 13.3, op 13; new since the design's
    /// original "reserved" placeholder -- bead `pico-link-jyhk.26`'s
    /// INVESTIGATION comment: C's `PL_COMMAND_TAG_CANCEL_CONNECT` merged
    /// (`pico-link-chc3`) so this is implemented in core now, ahead of the
    /// C-side `op_mask` bit that gates whether a real host can reach it
    /// (a later bead). Only cancels a `Host`-initiated attempt for the
    /// requested `addr` -- a device- or auto-reconnect-owned attempt (or
    /// one for a *different* address) is left alone, matching
    /// [`radio_actions::cancel_connect`]'s "the person holding the dongle
    /// wins" discipline (design sec 13.7) rather than that helper's own
    /// unconditional-queue shape, which is safe only because its caller
    /// (the wizard's B handler) can only ever be looking at its own
    /// attempt.
    pub(super) fn host_op_connect_cancel(&mut self, seq: u8, op: u8, body: &[u8]) -> HostOpStatus {
        let Some(addr) = decode_addr(body) else {
            return HostOpStatus::rejected(seq, op, OpError::InvalidRequest);
        };
        let attempt = self.model.borrow().attempt;
        match attempt {
            Some(a) if a.addr == addr && a.initiator == ConnectInitiator::Host => {
                cancel_connect(&self.model, &self.commands, addr);
                HostOpStatus::done(seq, op)
            }
            Some(a) if a.addr == addr => {
                // Device- or auto-reconnect-owned -- design sec 13.7's
                // "the person holding the dongle wins".
                HostOpStatus::rejected(seq, op, OpError::DeviceBusy)
            }
            // No attempt at all, or one for a different address: nothing
            // for this request to cancel -- tolerant no-op, same
            // "cancelling something already gone is fine" discipline
            // `radio_actions::cancel_connect`'s own doc comment describes
            // for a stray B press.
            _ => HostOpStatus::done(seq, op),
        }
    }
}


#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::*;
    use crate::app::model::{DeviceEntry, PairedDevice};
    use crate::app::test_support::open_wizard;
    use crate::app::{App, Event, StoreStatus};
    use crate::app::host_op::OP_PROTO;

    const ADDR_A: [u8; 6] = [1, 2, 3, 4, 5, 6];
    const ADDR_B: [u8; 6] = [9, 8, 7, 6, 5, 4];
    const AUDIO_SINK_COD: u32 = 0x24_04_04;

    fn req(op: u8, seq: u8, body: &[u8]) -> Vec<u8> {
        let mut req = vec![OP_PROTO, op, seq, 0];
        req.extend_from_slice(body);
        req
    }

    fn decode_status(buf: &[u8]) -> (u8, u8, u8, u16) {
        let op = buf[2];
        let state = buf[3];
        let error = buf[4];
        let word = u16::from_le_bytes([buf[6], buf[7]]);
        (op, state, error, word)
    }

    fn ready(app: &mut App) {
        // `SCAN_START`'s `NOT_READY` gate reads `BtModel::store_status`
        // (folded from `Event::StoreLoaded`) -- NOT `App::presets_ready`
        // (folded from the unrelated `Event::PresetStoreLoaded`, the DSP
        // preset store's own readiness flag).
        app.handle_event(Event::StoreLoaded { status: StoreStatus::FirstBoot });
    }

    fn upsert(app: &mut App, addr: [u8; 6], name: &str, mru_seq: u32) {
        app.handle_event(Event::PairedDeviceUpserted(PairedDevice {
            addr,
            name: String::from(name),
            mru_seq,
            ldac_quality: 0,
            preset_id: 0,
        }));
    }

    fn discover(app: &mut App, addr: [u8; 6], name: &str, class_of_device: u32) {
        app.handle_event(Event::DeviceDiscovered(DeviceEntry { addr, name: String::from(name), rssi: -40, class_of_device }));
    }

    /// Seeds `connected_addr == addr` via a real `Host`-owned attempt that
    /// concludes successfully -- the only path `BtModel::connected_addr`
    /// has a writer for (fold.rs's `on_connect_succeeded`). Drains the
    /// `Command::PersistDevice` that echo queues, so callers can assert on
    /// an empty command queue afterwards without knowing about it.
    fn connect_and_succeed(app: &mut App, addr: [u8; 6]) {
        let seq = app.seed_connect_attempt_for_test(addr);
        app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq });
        assert_eq!(app.poll_command(), Some(Command::PersistDevice { addr }));
    }

    fn run(app: &mut App, req: &[u8]) -> (u8, u8, u8, u16) {
        app.host_op(req);
        let mut buf = [0u8; 64];
        let n = app.host_op_status(&mut buf);
        decode_status(&buf[..n])
    }

    // --- SCAN_START (op 7) ---------------------------------------------

    #[test]
    fn scan_start_not_ready_before_store_loaded() {
        let mut app = App::new(240, 240);
        let (_, state, error, _) = run(&mut app, &req(7, 1, &[]));
        assert_eq!(state, 2, "rejected");
        assert_eq!(error, OpError::NotReady as u8);
    }

    #[test]
    fn scan_start_queues_start_scan_and_returns_scan_seq() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let (_, state, error, scan_seq) = run(&mut app, &req(7, 1, &[]));
        assert_eq!(state, 1, "done: {error}");
        assert_eq!(scan_seq, app.model().scan_seq);
        assert_eq!(app.model().scan_owner, ScanOwner::Host);
        assert_eq!(app.poll_command(), Some(Command::StartScan));
    }

    #[test]
    fn scan_start_is_idempotent_against_its_own_running_scan() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let _ = run(&mut app, &req(7, 1, &[]));
        let _ = app.poll_command(); // drain the first StartScan
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });
        let (_, state, _, _) = run(&mut app, &req(7, 2, &[]));
        assert_eq!(state, 1, "done, no-op");
        assert!(app.poll_command().is_none(), "must not queue a second StartScan");
    }

    #[test]
    fn scan_start_paired_full_at_capacity() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        for i in 0..8u8 {
            upsert(&mut app, [i, 0, 0, 0, 0, 0], "", u32::from(i));
        }
        let (_, state, error, _) = run(&mut app, &req(7, 1, &[]));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::PairedFull as u8);
    }

    #[test]
    fn scan_start_device_busy_while_wizard_open() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        open_wizard(&mut app);
        let (_, state, error, _) = run(&mut app, &req(7, 1, &[]));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::DeviceBusy as u8);
    }

    #[test]
    fn scan_start_radio_busy_while_a_host_connect_is_in_flight() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        discover(&mut app, ADDR_A, "Buds", AUDIO_SINK_COD);
        let (_, connect_state, ..) = run(&mut app, &req(9, 1, &ADDR_A));
        assert_eq!(connect_state, 1, "connect must have gone through");
        let (_, state, error, _) = run(&mut app, &req(7, 2, &[]));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::RadioBusy as u8);
    }

    // --- SCAN_STOP (op 8) -----------------------------------------------

    #[test]
    fn scan_stop_is_a_no_op_when_not_discovering() {
        let mut app = App::new(240, 240);
        let (_, state, _, _) = run(&mut app, &req(8, 1, &[]));
        assert_eq!(state, 1);
        assert!(app.poll_command().is_none());
    }

    #[test]
    fn scan_stop_device_busy_when_device_owns_the_scan() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        open_wizard(&mut app); // pushes the wizard into Scanning, scan_owner = Device
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });
        let (_, state, error, _) = run(&mut app, &req(8, 1, &[]));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::DeviceBusy as u8);
    }

    #[test]
    fn scan_stop_queues_cancel_scan_for_a_host_owned_scan() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let _ = run(&mut app, &req(7, 1, &[]));
        let _ = app.poll_command(); // drain StartScan
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });
        let (_, state, _, _) = run(&mut app, &req(8, 2, &[]));
        assert_eq!(state, 1);
        assert_eq!(app.poll_command(), Some(Command::CancelScan));
    }

    // --- host_lease_expired (design sec 13.6) -----------------------------

    #[test]
    fn lease_expiry_cancels_a_host_owned_scan() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let _ = run(&mut app, &req(7, 1, &[])); // SCAN_START -> scan_owner = Host
        let _ = app.poll_command(); // drain StartScan
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });

        app.host_lease_expired();

        assert_eq!(app.poll_command(), Some(Command::CancelScan));
    }

    #[test]
    fn lease_expiry_leaves_a_device_owned_scan_alone() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        open_wizard(&mut app); // scan_owner = Device
        let _ = app.poll_command(); // drain the wizard's own StartScan
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });

        app.host_lease_expired();

        assert!(app.poll_command().is_none(), "a device-owned scan is never a web lease's business");
    }

    #[test]
    fn lease_expiry_is_a_no_op_with_no_scan_running() {
        let mut app = App::new(240, 240);
        ready(&mut app);

        app.host_lease_expired();

        assert!(app.poll_command().is_none());
    }

    #[test]
    fn lease_expiry_does_not_cancel_an_in_flight_connect() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "Headphones", 1);
        let _ = run(&mut app, &req(9, 1, &ADDR_A)); // CONNECT (Host-initiated)
        let _ = app.poll_command(); // drain the Connect command itself
        assert!(app.model().attempt.is_some());

        app.host_lease_expired();

        assert!(app.model().attempt.is_some(), "design sec 13.6: connect needs no lease");
        assert!(app.poll_command().is_none(), "must not queue CancelConnect either");
    }

    #[test]
    fn host_preview_end_is_an_alias_for_host_lease_expired() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let _ = run(&mut app, &req(7, 1, &[]));
        let _ = app.poll_command();
        app.handle_event(Event::DiscoveryStateChanged { scanning: true });

        app.host_preview_end();

        assert_eq!(app.poll_command(), Some(Command::CancelScan), "the current ui-ffi binding name must keep the new behaviour");
    }

    // --- CONNECT (op 9) --------------------------------------------------

    #[test]
    fn connect_unknown_device_rejects() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let (_, state, error, _) = run(&mut app, &req(9, 1, &ADDR_A));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::UnknownDevice as u8);
    }

    #[test]
    fn connect_to_already_connected_addr_is_a_no_op_even_while_busy() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        connect_and_succeed(&mut app, ADDR_A);
        assert_eq!(app.model().connected_addr, Some(ADDR_A));
        upsert(&mut app, ADDR_A, "Headphones", 1);
        open_wizard(&mut app); // would otherwise be DEVICE_BUSY
        let _ = app.poll_command(); // drain the wizard's own StartScan
        let (_, state, _, word) = run(&mut app, &req(9, 1, &ADDR_A));
        assert_eq!(state, 1);
        assert_eq!(word, 0);
        assert!(app.poll_command().is_none());
    }

    #[test]
    fn connect_switches_to_a_paired_device_and_returns_attempt_seq() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "Headphones", 1);
        let (_, state, _, attempt_seq) = run(&mut app, &req(9, 1, &ADDR_A));
        assert_eq!(state, 1);
        assert_eq!(attempt_seq, app.model().attempt.expect("recorded").seq);
        app.expect_connect_command_for_test(ADDR_A, "Headphones");
    }

    #[test]
    fn connect_paired_full_when_pairing_a_new_device_at_capacity() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        for i in 0..8u8 {
            upsert(&mut app, [i, 0, 0, 0, 0, 0], "", u32::from(i));
        }
        discover(&mut app, ADDR_B, "Buds", AUDIO_SINK_COD);
        let (_, state, error, _) = run(&mut app, &req(9, 1, &ADDR_B));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::PairedFull as u8);
    }

    #[test]
    fn connect_device_busy_while_wizard_open() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "Headphones", 1);
        open_wizard(&mut app);
        let (_, state, error, _) = run(&mut app, &req(9, 1, &ADDR_A));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::DeviceBusy as u8);
    }

    // --- DISCONNECT (op 10) ----------------------------------------------

    #[test]
    fn disconnect_not_connected_rejects() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let (_, state, error, _) = run(&mut app, &req(10, 1, &ADDR_A));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::NotConnected as u8);
    }

    #[test]
    fn disconnect_queues_disconnect_when_addr_matches() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        connect_and_succeed(&mut app, ADDR_A);
        let (_, state, ..) = run(&mut app, &req(10, 1, &ADDR_A));
        assert_eq!(state, 1);
        assert_eq!(app.poll_command(), Some(Command::Disconnect));
    }

    // --- FORGET (op 11) ---------------------------------------------------

    #[test]
    fn forget_unknown_device_rejects() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let (_, state, error, _) = run(&mut app, &req(11, 1, &ADDR_A));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::UnknownDevice as u8);
    }

    #[test]
    fn forget_queues_forget_device_for_a_paired_device() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "", 1);
        let (_, state, ..) = run(&mut app, &req(11, 1, &ADDR_A));
        assert_eq!(state, 1);
        assert_eq!(app.model().paired.len(), 1, "forget does not mutate paired locally");
        assert_eq!(app.poll_command(), Some(Command::ForgetDevice { addr: ADDR_A }));
    }

    #[test]
    fn forget_device_busy_only_for_its_own_in_flight_attempt() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "", 1);
        upsert(&mut app, ADDR_B, "", 2);
        let _ = app.seed_connect_attempt_for_test(ADDR_A);

        let (_, state, error, _) = run(&mut app, &req(11, 1, &ADDR_A));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::DeviceBusy as u8);

        // A DIFFERENT paired device is unaffected by ADDR_A's in-flight attempt.
        let (_, state_b, ..) = run(&mut app, &req(11, 2, &ADDR_B));
        assert_eq!(state_b, 1);
    }

    // --- SET_DEVICE_QUALITY (op 12) ---------------------------------------

    #[test]
    fn set_device_quality_unknown_device_rejects() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let mut body = ADDR_A.to_vec();
        body.push(2);
        let (_, state, error, _) = run(&mut app, &req(12, 1, &body));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::UnknownDevice as u8);
    }

    #[test]
    fn set_device_quality_out_of_range_is_invalid_request() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "", 1);
        let mut body = ADDR_A.to_vec();
        body.push(5);
        let (_, state, error, _) = run(&mut app, &req(12, 1, &body));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::InvalidRequest as u8);
    }

    #[test]
    fn set_device_quality_zero_is_invalid_request() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "", 1);
        let mut body = ADDR_A.to_vec();
        body.push(0);
        let (_, state, error, _) = run(&mut app, &req(12, 1, &body));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::InvalidRequest as u8);
    }

    #[test]
    fn set_device_quality_queues_set_device_ldac_quality() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "", 1);
        let mut body = ADDR_A.to_vec();
        body.push(4); // Adaptive
        let (_, state, ..) = run(&mut app, &req(12, 1, &body));
        assert_eq!(state, 1);
        assert_eq!(app.poll_command(), Some(Command::SetDeviceLdacQuality { addr: ADDR_A, ldac_quality: 4 }));
    }

    // --- CONNECT_CANCEL (op 13) -------------------------------------------

    #[test]
    fn connect_cancel_no_attempt_is_a_tolerant_no_op() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let (_, state, ..) = run(&mut app, &req(13, 1, &ADDR_A));
        assert_eq!(state, 1);
        assert!(app.poll_command().is_none());
    }

    #[test]
    fn connect_cancel_device_busy_for_a_device_owned_attempt() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        let _ = app.seed_connect_attempt_for_test(ADDR_A);
        let (_, state, error, _) = run(&mut app, &req(13, 1, &ADDR_A));
        assert_eq!(state, 2);
        assert_eq!(error, OpError::DeviceBusy as u8);
        assert!(app.model().attempt.is_some(), "a device-owned attempt must survive a rejected cancel");
    }

    #[test]
    fn connect_cancel_cancels_its_own_host_initiated_attempt() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "Headphones", 1);
        let _ = run(&mut app, &req(9, 1, &ADDR_A));
        assert!(app.model().attempt.is_some());

        let (_, state, ..) = run(&mut app, &req(13, 2, &ADDR_A));
        assert_eq!(state, 1);
        assert!(app.model().attempt.is_none(), "cancel must conclude the attempt immediately");
        assert_eq!(
            app.model().last_outcome.expect("recorded").result,
            crate::app::model::ConnectOutcomeResult::Cancelled
        );
    }

    #[test]
    fn connect_cancel_ignores_an_attempt_for_a_different_address() {
        let mut app = App::new(240, 240);
        ready(&mut app);
        upsert(&mut app, ADDR_A, "Headphones", 1);
        let _ = run(&mut app, &req(9, 1, &ADDR_A));

        let (_, state, ..) = run(&mut app, &req(13, 2, &ADDR_B));
        assert_eq!(state, 1, "tolerant no-op for a mismatched addr");
        assert!(app.model().attempt.is_some(), "ADDR_A's own attempt must be untouched");
    }
}
