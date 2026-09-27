//! One action function per Bluetooth-radio verb -- design
//! `.planning/design/2026-09-27-iface6-eq-management-protocol.md` sec
//! 13.5: "device call sites switch to them with no behaviour change ...
//! host ops call the same helpers. This is the 'reuse the device's flows'
//! guarantee, enforced by code shape rather than review vigilance."
//!
//! Every function here does exactly what its device-screen call site used
//! to do inline: queue the one [`Command`] C already services, and fold
//! the radio-session bookkeeping ([`BtModel::scan_owner`]/`scan_seq`/
//! `attempt`) that a future web `GET_RADIO` snapshot (design sec 13.4,
//! bead `pico-link-jyhk.27`) will read. Deliberately thin: gating (store
//! full, device busy, radio busy, ...) is a *caller* concern -- the
//! device screens already gate before calling these (e.g. `build_devices_
//! screen`'s capacity check before `start_scan`), and the future `HOST_OP`
//! handlers (bead `pico-link-jyhk.26`) will gate the same way before
//! calling the same functions, returning an `OpError` instead of pushing
//! the screen the device UI would. Neither caller's gating logic belongs
//! here, or the other caller would inherit UI-shaped or wire-shaped
//! assumptions it has no business depending on.
//!
//! Takes the shared `model`/`commands` handles explicitly (not `&App`):
//! every call site already holds its own clones of exactly these two
//! `Rc<RefCell<_>>`s (see [`crate::app::ModelHandle`]'s doc comment), and a
//! future `HOST_OP` dispatcher will too, via `App`'s own fields -- neither
//! needs (or should gain) a path to the rest of `App`.

use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::RefCell;

use super::events::Command;
use super::model::{ConnectAttempt, ConnectInitiator, ScanOwner};
use super::{DeviceAddr, ModelHandle};

/// SCAN_START (design sec 13.3, op 7). Clears the previous scan's results,
/// claims [`ScanOwner`] for `origin`, bumps [`BtModel::scan_seq`], and
/// queues [`Command::StartScan`] -- exactly what `build_devices_screen`'s
/// "Pair new headphones" row and [`crate::render::wizard`]'s `NothingFound`
/// re-scan used to do inline, plus the new bookkeeping.
///
/// Callers gate first: the device screens already refuse to call this at
/// [`super::model::MAX_PAIRED_DEVICES`] capacity or while a wizard/attempt
/// holds the radio; a future `HOST_OP` SCAN_START handler must do the same
/// (`NOT_READY`/`PAIRED_FULL`/`DEVICE_BUSY`/`RADIO_BUSY`, design sec
/// 13.3) before ever reaching here. This function does not re-check any of
/// that, nor does it check "already scanning" -- a caller that wants
/// SCAN_START's idempotent "already discovering with owner Host: no-op"
/// rule implements that check itself before calling.
pub(crate) fn start_scan(model: &ModelHandle, commands: &Rc<RefCell<VecDeque<Command>>>, origin: ScanOwner) {
    {
        let mut model = model.borrow_mut();
        model.discovered.clear();
        model.scan_owner = origin;
        model.bump_scan_seq();
    }
    commands.borrow_mut().push_back(Command::StartScan);
}

/// SCAN_STOP (design sec 13.3, op 8) / the wizard's B-during-`Scanning`
/// cancel. Queues [`Command::CancelScan`] -- [`BtModel::scan_owner`] is
/// left untouched here (a cancel is a *request*; the owner clears only
/// once the inquiry genuinely ends, i.e. [`App::set_discovering`]'s
/// `scanning == false` edge, same "echo, not the request" discipline every
/// other `Command`/`Event` pair in this crate follows).
pub(crate) fn cancel_scan(commands: &Rc<RefCell<VecDeque<Command>>>) {
    commands.borrow_mut().push_back(Command::CancelScan);
}

/// CONNECT (design sec 13.3, op 9) / every device-driven "connect to this
/// address" site (the devices screen's paired-row switch, the wizard's
/// scan-row pick and its phase 5/6 retry, the device page's relink, and
/// [`App::on_store_loaded`]'s auto-reconnect). Records a fresh
/// [`ConnectAttempt`] in [`BtModel::attempt`] -- `seq`/`addr`/`initiator`
/// are known here, before C has echoed anything back, which is exactly why
/// this (not a folded [`Event`]) is where `attempt` starts existing -- and
/// queues [`Command::Connect`].
///
/// Callers gate first (paired-device-full, device/radio busy, resolving
/// `addr` against `paired`/the scan list rather than trusting a caller-
/// supplied name -- design sec 13.3's CONNECT semantics), same division of
/// responsibility as [`start_scan`]'s doc comment describes.
pub(crate) fn connect(model: &ModelHandle, commands: &Rc<RefCell<VecDeque<Command>>>, addr: DeviceAddr, name: String, initiator: ConnectInitiator) {
    {
        let mut model = model.borrow_mut();
        let seq = model.bump_attempt_seq();
        model.attempt = Some(ConnectAttempt { seq, addr, initiator, step: None, retries: 0 });
    }
    commands.borrow_mut().push_back(Command::Connect { addr, name });
}

/// DISCONNECT (design sec 13.3, op 10) / the device page's X on the
/// connected device. `Command::Disconnect` carries no address (see that
/// variant's doc comment) -- a caller-side `addr == connected_addr` check
/// is the intent guard, not anything this function enforces.
pub(crate) fn disconnect(commands: &Rc<RefCell<VecDeque<Command>>>) {
    commands.borrow_mut().push_back(Command::Disconnect);
}

/// FORGET (design sec 13.3, op 11) / the devices screen's X-then-confirm
/// and pick-one-to-forget flows. `core` does not remove `addr` from
/// [`BtModel::paired`] here -- see [`Command::ForgetDevice`]'s doc comment
/// for the single-writer rule this preserves.
pub(crate) fn forget(commands: &Rc<RefCell<VecDeque<Command>>>, addr: DeviceAddr) {
    commands.borrow_mut().push_back(Command::ForgetDevice { addr });
}

/// SET_DEVICE_QUALITY (design sec 13.3, op 12) / the `QUALITY` picker's
/// `A` handler. Applies live, no confirm -- see [`Command::
/// SetDeviceLdacQuality`]'s doc comment.
pub(crate) fn set_quality(commands: &Rc<RefCell<VecDeque<Command>>>, addr: DeviceAddr, ldac_quality: u8) {
    commands.borrow_mut().push_back(Command::SetDeviceLdacQuality { addr, ldac_quality });
}
