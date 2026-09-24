use alloc::string::String;

use crate::power::DisplaySettings;
use crate::render::Instant;

use super::events::{Command, ConnectFailureReason, ConnectStep, Event, StoreStatus, VolumeSource, VolumeState};
use super::fault::{FaultKey, FaultValue};
use super::model::{decay_peak, truncate_device_name, DeviceEntry, OutLevelSample, OUT_LEVEL_HOLD_DURATION};
use super::ui_state::{HomeFace, PENDING_TIMESTAMP, WizardPhase};
use super::{App, ConnectedCodec, DeviceAddr, LinkState, PairedDevice};

impl App {
    /// Folds one inbound Bluetooth-domain [`Event`] into [`BtModel`] and
    /// refreshes the root screen (`pl_ui_push_event`'s core-side
    /// implementation -- the single entry point replacing the old
    /// `set_link_state`/`add_device`/`clear_devices` setter trio). `core`
    /// never acts on these itself -- it has no Bluetooth stack -- it only
    /// updates what screens read.
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::LinkStateChanged(state) => self.set_link_state(state),
            Event::DiscoveryStateChanged { scanning } => self.set_discovering(scanning),
            Event::DeviceDiscovered(device) => {
                self.add_device(device.addr, device.name, device.rssi, device.class_of_device);
            }
            Event::DevicesCleared => self.clear_devices(),
            Event::ConnectFailed { addr, reason } => self.record_connect_failure(addr, reason),
            Event::ConnectStepChanged(step) => self.on_connect_step_changed(step),
            Event::ConnectRetrying { attempt } => self.on_connect_retrying(attempt),
            Event::ConnectSucceeded { addr, degraded } => self.on_connect_succeeded(addr, degraded),
            Event::WizardAutoDismiss => self.on_wizard_auto_dismiss(),
            Event::CodecChanged(codec) => self.set_connected_codec(codec),
            Event::StoreLoaded { status } => self.on_store_loaded(status),
            Event::PairedDeviceUpserted(device) => self.on_paired_device_upserted(device),
            Event::PairedDeviceForgotten { addr } => self.on_paired_device_forgotten(addr),
            Event::PairedStoreFull => self.on_paired_store_full(),
            Event::LevelsChanged { peak_l, peak_r, rms_l, rms_r } => self.on_levels_changed(peak_l, peak_r, rms_l, rms_r),
            Event::VolumeChanged { level, muted, source } => self.on_volume_changed(level, muted, source),
            Event::LdacBitrateChanged { kbps } => self.on_ldac_bitrate_changed(kbps),
            Event::FaultRaised { key, value, count } => self.on_fault_raised(key, value, count),
            Event::DisplaySettingsLoaded { mode, timeout_s } => {
                self.set_display_settings(DisplaySettings::from_wire(mode, timeout_s));
                self.refresh_stack();
            }
        }
        self.stamp_pending_wizard_timestamp();
    }

    /// Backfills [`WizardPhase::Scanning`]/[`WizardPhase::Connecting`]'s
    /// `started` field with the real current time (`self.now_us`) if it's
    /// still [`PENDING_TIMESTAMP`] -- see that constant's doc comment for
    /// why the widget itself can't do this. Called after every
    /// [`App::handle_input`] and [`App::handle_event`], since either path
    /// can produce a freshly entered phase (`handle_input` for a direct
    /// d-pad-driven transition, `handle_event` for the `NotResponding` ->
    /// `Connecting` retry-succeeding case in [`App::on_connect_step_
    /// changed`]). A no-op whenever the current phase isn't pending (the
    /// overwhelmingly common case), or isn't `Scanning`/`Connecting` at
    /// all.
    pub(in crate::app) fn stamp_pending_wizard_timestamp(&mut self) {
        let mut phase = self.wizard_phase.borrow_mut();
        match &mut *phase {
            WizardPhase::Scanning { started } | WizardPhase::Connecting { started, .. } if *started == PENDING_TIMESTAMP => {
                *started = Instant::from_micros(self.now_us);
            }
            _ => {}
        }
    }

    /// Phase 2 -> phase 3 transition (design section 9): when a GAP
    /// inquiry ends (bead `pico-link-88xs`: called only from
    /// [`App::set_discovering`] on the `scanning == false` edge, i.e. a
    /// genuine [`Event::DiscoveryStateChanged`] -- no longer a
    /// `LinkStateChanged(Idle)`, which could also fire on a connect
    /// failure or a disconnect and spuriously flip the wizard to
    /// `NothingFound`) while the wizard is still on
    /// [`WizardPhase::Scanning`] and nothing was found, moves it to
    /// [`WizardPhase::NothingFound`]. A no-op in every other case --
    /// devices *were* found (the phase just stays `Scanning`, now showing
    /// a selectable list instead of an actively-filling one -- design
    /// section 9 draws no rendering distinction between those), the
    /// wizard isn't open, or it's already past phase 2 (e.g. the user
    /// already selected a device and moved on to phase 4 before this
    /// event arrived).
    fn on_scan_ended_if_applicable(&mut self) {
        let mut phase = self.wizard_phase.borrow_mut();
        if matches!(*phase, WizardPhase::Scanning { .. }) && self.wizard_devices.borrow().is_empty() {
            *phase = WizardPhase::NothingFound;
            drop(phase);
            self.dirty = true;
        }
    }

    /// Folds one [`Event::ConnectStepChanged`] into [`WizardPhase`] --
    /// only meaningful while the wizard is mid-connect (`Connecting` or
    /// already surfaced as `NotResponding`, e.g. the step name changing
    /// right as a retry succeeds); silently ignored otherwise, per
    /// [`Event::ConnectStepChanged`]'s doc comment.
    fn on_connect_step_changed(&mut self, step: ConnectStep) {
        let mut phase = self.wizard_phase.borrow_mut();
        match &*phase {
            // Already `Connecting`: this is the same connect attempt
            // continuing, so its `started` carries forward unchanged.
            WizardPhase::Connecting { addr, started, .. } => {
                *phase = WizardPhase::Connecting { addr: *addr, step, started: *started };
            }
            // Coming back from `NotResponding` (which carries no
            // `started` of its own): the original attempt's start time is
            // gone, so this re-enters as pending, same as a brand-new
            // connect -- `stamp_pending_wizard_timestamp` (called by
            // `handle_event` right after this) backfills it.
            WizardPhase::NotResponding { addr, .. } => {
                *phase = WizardPhase::connecting_pending(*addr, step);
            }
            _ => {}
        }
        drop(phase);
        self.dirty = true;
    }

    /// Folds one [`Event::ConnectRetrying`] into [`WizardPhase`] -- the
    /// phase 4 -> phase 5 transition (or a further phase-5 retry
    /// incrementing its own counter), per [`Event::ConnectRetrying`]'s
    /// doc comment.
    fn on_connect_retrying(&mut self, attempt: u16) {
        let mut phase = self.wizard_phase.borrow_mut();
        match &*phase {
            WizardPhase::Connecting { addr, .. } | WizardPhase::NotResponding { addr, .. } => {
                *phase = WizardPhase::NotResponding { addr: *addr, attempt };
            }
            _ => {}
        }
        drop(phase);
        self.dirty = true;
    }

    /// Folds one [`Event::ConnectSucceeded`] into [`WizardPhase`] -- the
    /// phase 6 success outcome, from either `Connecting` or
    /// `NotResponding`.
    ///
    /// Also queues [`Command::PersistDevice { addr }`] (bead pico-link-cz0.6,
    /// M5 persistence design point 7 -- `core`'s auto-reconnect/remember-
    /// this-device POLICY: a connect that actually succeeded is worth
    /// remembering), unconditionally -- `addr` comes straight off the event
    /// itself (see [`Event::ConnectSucceeded`]'s doc comment for why that,
    /// not `WizardPhase`, is the source of truth: it works identically for
    /// a wizard-driven connect and the `PL_DEBUG_REMOTE` bypass, which
    /// never touches `WizardPhase` at all).
    fn on_connect_succeeded(&mut self, addr: [u8; 6], degraded: bool) {
        self.commands.borrow_mut().push_back(Command::PersistDevice { addr });
        self.model.borrow_mut().connected_addr = Some(addr);
        *self.wizard_phase.borrow_mut() = WizardPhase::Succeeded { degraded };
        self.dirty = true;
    }

    /// Folds one [`Event::StoreLoaded`] -- records `status` in [`BtModel`]
    /// and runs the auto-reconnect policy: `core` decides *whether* and
    /// *which* device to reconnect to, C only loads/stages/flushes (design
    /// point 7). Reshaped by bead pico-link-4vb.4 (T4), design section 5.2:
    /// this event no longer carries an address -- by the time it arrives,
    /// every `Event::PairedDeviceUpserted` C pushed ahead of it (its own
    /// boot sequence's `count` records) has already folded into
    /// [`BtModel::paired`] (see [`App::on_paired_device_upserted`]), so the
    /// target is simply the highest `mru_seq` in that list. Queues the
    /// exact same [`Command::Connect`] a manual paired-row activation uses.
    /// Does not touch the wizard or rebuild the root screen -- this fires
    /// once at boot, before the user has done anything, and the queued
    /// `Connect` drives the same `LinkStateChanged`/`ConnectStepChanged`/
    /// `ConnectSucceeded` event flow a manual connect would, which is what
    /// actually updates the UI as the auto-reconnect proceeds.
    fn on_store_loaded(&mut self, status: StoreStatus) {
        let auto_reconnect = {
            let mut model = self.model.borrow_mut();
            model.store_status = Some(status);
            model.paired.iter().max_by_key(|d| d.mru_seq).map(|device| (device.addr, truncate_device_name(&device.name)))
        };
        if let Some((addr, name)) = auto_reconnect {
            self.commands.borrow_mut().push_back(Command::Connect { addr, name });
        }
        self.refresh_stack();
    }

    /// Folds one [`Event::PairedDeviceUpserted`] into [`BtModel::paired`] --
    /// update-in-place if `addr` is already known (a rename, an `mru_seq`
    /// bump), append otherwise. One of exactly two writers of `paired`
    /// (design section 3's single-writer rule -- see that event's doc
    /// comment). Bead pico-link-4vb.4 (T4).
    fn on_paired_device_upserted(&mut self, device: PairedDevice) {
        {
            let mut model = self.model.borrow_mut();
            if let Some(existing) = model.paired.iter_mut().find(|d| d.addr == device.addr) {
                *existing = device;
            } else {
                model.paired.push(device);
            }
        }
        self.refresh_stack();
    }

    /// Folds one [`Event::PairedDeviceForgotten`] into [`BtModel::paired`] --
    /// the other of the two writers (design section 3). A no-op if `addr`
    /// isn't currently known (e.g. a stray/duplicate echo).
    fn on_paired_device_forgotten(&mut self, addr: DeviceAddr) {
        self.model.borrow_mut().paired.retain(|d| d.addr != addr);
        self.refresh_stack();
    }

    /// Folds one [`Event::PairedStoreFull`] -- see that event's and
    /// [`BtModel::store_full`]'s doc comments for why this is populated
    /// ahead of any screen actually reading it.
    fn on_paired_store_full(&mut self) {
        self.model.borrow_mut().store_full = true;
        self.dirty = true;
    }

    /// Folds one [`Event::WizardAutoDismiss`] -- pops all the way back to
    /// Home, but **only** if it's currently showing a plain (non-degraded)
    /// success; see that event's doc comment for why this guard exists.
    /// pico-link-4vb.2 (Andreas's ruling): landing on Devices left him
    /// pressing Back repeatedly to get back to Home, so this now calls
    /// [`crate::render::Navigator::pop_to_root`] instead of
    /// [`crate::render::Navigator::pop`] -- a no-op if the wizard isn't
    /// actually the top of the stack any more (e.g. this event arrived
    /// after the user already backed out via B), same as `pop` was.
    fn on_wizard_auto_dismiss(&mut self) {
        let should_pop = matches!(*self.wizard_phase.borrow(), WizardPhase::Succeeded { degraded: false });
        if should_pop {
            self.navigator.pop_to_root();
            *self.wizard_phase.borrow_mut() = WizardPhase::default();
            self.wizard_devices.borrow_mut().clear();
            // pico-link-l4d: `pop_to_root` only restores navigation depth --
            // it doesn't touch which of Home's two faces (`HomeFace::Status`
            // vs `HomeFace::Menu`) is showing. The user reached the wizard
            // via Home's Menu face (Home -> A -> Devices -> pair), so without
            // this the auto-dismiss silently landed back on the
            // Bluetooth/Settings list instead of the hero -- which is the
            // entire point of auto-dismissing: showing the codec just paired.
            // This is a deliberate, automatic choice of destination (see the
            // policy note on `on_devices_back`/wizard-success-B for why the
            // *manual* B routes are treated differently).
            *self.home_face.borrow_mut() = HomeFace::Status;
            self.dirty = true;
        }
    }

    /// Records the Bluetooth link's coarse lifecycle state and refreshes
    /// the devices screen's scan-row sublabel to match. Also reachable
    /// directly (not just via [`App::handle_event`]) since it's a natural
    /// unit for tests and for [`App::record_connect_failure`] to reuse.
    ///
    /// Also clears [`BtModel::connected_codec`] whenever `state` isn't
    /// [`LinkState::Connected`] -- a stale codec word surviving a
    /// disconnect is worse than `NO LINK` (design section 15).
    /// Deliberately keyed off the link state itself rather than a
    /// dedicated disconnect event: every path off `Connected` already
    /// flows through this one method (bead pico-link-1v5), so this can't
    /// race with a disconnect notification C forgot to send, and it needs
    /// zero new firmware plumbing in `bt.c`.
    ///
    /// `LinkState` no longer carries a scan (bead `pico-link-88xs`, design
    /// `.planning/design/2026-09-08-link-state-vs-discovery-axis.md`):
    /// this method's own clear-on-not-`Connected` rule is unchanged --
    /// see [`LinkState`]'s doc comment for why narrowing the type, not
    /// relaxing this rule, is what fixed the "a scan wipes the connected
    /// model" bug. [`BtModel::link_state`] has exactly one writer: this
    /// method (INVARIANT L2) -- see [`App::record_connect_failure`].
    pub fn set_link_state(&mut self, state: LinkState) {
        {
            let mut model = self.model.borrow_mut();
            model.link_state = state;
            if state != LinkState::Connected {
                model.connected_codec = None;
                // `connected_addr` (bead pico-link-4vb.4, T5) follows the exact
                // same lifecycle as `connected_codec`, for the same reason --
                // see `BtModel::connected_addr`'s doc comment.
                model.connected_addr = None;
                // `out_level` (bead pico-link-du0) follows the exact same
                // lifecycle for the exact same reason -- see
                // `BtModel::out_level`'s doc comment.
                model.out_level = None;
                // `ldac_live_kbps` (bead pico-link-7jol.5) follows the exact
                // same lifecycle for the exact same reason -- see
                // `BtModel::ldac_live_kbps`'s doc comment.
                model.ldac_live_kbps = None;
            }
        }
        self.refresh_stack();
    }

    /// Records whether the radio is running an inquiry. The SECOND,
    /// independent axis (bead `pico-link-88xs`) -- deliberately does NOT
    /// touch [`BtModel::link_state`] and does NOT clear any connected-model
    /// field: an inquiry does not disconnect A2DP. [`BtModel::discovering`]
    /// has exactly one writer: this method (INVARIANT L2).
    pub fn set_discovering(&mut self, scanning: bool) {
        self.model.borrow_mut().discovering = scanning;
        if !scanning {
            self.on_scan_ended_if_applicable();
        }
        self.refresh_stack();
    }

    /// Records the live A2DP link's negotiated codec (or a renegotiation)
    /// and refreshes the Home hero widget to match. See
    /// [`ConnectedCodec`]'s doc comment for why `core` treats
    /// `word`/`nominal_bitrate_bps` as opaque, already-decided display
    /// data rather than deriving them from codec identity itself.
    pub fn set_connected_codec(&mut self, codec: ConnectedCodec) {
        {
            let mut model = self.model.borrow_mut();
            // A renegotiation away from LDAC (or a fresh connect that isn't
            // LDAC at all) must drop the previous stream's live figure --
            // `ldac_live_kbps` (bead pico-link-7jol.5) is only ever meaningful
            // for the codec it was measured on, and a stale reading surviving
            // a codec change would show under the wrong hero word.
            if codec.word != "LDAC" {
                model.ldac_live_kbps = None;
            }
            model.connected_codec = Some(codec);
        }
        self.refresh_stack();
    }

    /// Folds one [`Event::VolumeChanged`] reading into [`BtModel::volume`]
    /// (bead pico-link-4v2.5, VT5, design section 7). No screen reads this
    /// yet -- `rebuild_root` is called anyway, matching every other
    /// `BtModel`-mutating fold in this file, so a future screen can rely
    /// on that convention rather than each one deciding for itself whether
    /// a redraw is warranted.
    pub fn on_volume_changed(&mut self, level: u8, muted: bool, source: VolumeSource) {
        self.model.borrow_mut().volume = Some(VolumeState { level, muted, source });
        self.refresh_stack();
    }

    /// Folds one [`Event::LdacBitrateChanged`] reading into
    /// [`BtModel::ldac_live_kbps`] (bead pico-link-7jol.5) -- see that
    /// field's doc comment for the "never snapped to the ladder" rule.
    /// Refreshes the stack so Home's bitrate line and the device page's
    /// `QUALITY` row (its Adaptive trailing note) both pick up the new
    /// figure the same frame it arrives.
    pub fn on_ldac_bitrate_changed(&mut self, kbps: u32) {
        self.model.borrow_mut().ldac_live_kbps = Some(kbps);
        self.refresh_stack();
    }

    /// Folds one [`Event::FaultRaised`] reading into
    /// [`BtModel::fault_log`] (design `.planning/design/2026-09-07-audio-
    /// fault-model.md` §7.4). Uses `self.now_us` (the FFI seam's own
    /// stored clock, see [`App::tick`]'s doc comment) rather than taking a
    /// clock parameter -- the same convention [`App::on_levels_changed`]'s
    /// peak-hold already uses. Calls [`App::refresh_stack`] like every
    /// other `BtModel`-mutating fold, so a future screen (S3,
    /// `pico-link-9eq2.3.3`) can rely on that convention rather than each
    /// one deciding for itself whether a redraw is warranted -- this bead
    /// builds no screen that actually reads `fault_log` yet.
    pub fn on_fault_raised(&mut self, key: FaultKey, value: Option<FaultValue>, count: u16) {
        let now = Instant::from_micros(self.now_us);
        self.model.borrow_mut().fault_log.record(key, now, value, count);
        self.refresh_stack();
    }

    /// Folds one [`Event::LevelsChanged`] reading into
    /// [`BtModel::out_level`] and refreshes the Home hero widget's meter
    /// (bead pico-link-du0). Also updates the per-channel peak-hold cap:
    /// a channel's hold value tracks the highest peak seen, and only
    /// drops back down once [`OUT_LEVEL_HOLD_DURATION`] has passed since
    /// it was last set to a new maximum -- the conventional VU-meter
    /// "peak stays pinned briefly, then releases" behaviour, decided once
    /// here (at fold time, using `self.now_us`) rather than re-computed
    /// every render frame (see [`OutLevelSample`]'s doc comment for why
    /// render-time decay isn't an option for a `&self`-rendered widget).
    // `l`/`r` channel-suffixed bindings are the domain vocabulary this
    // whole feature uses (matches `OutLevelSample`'s own field names) --
    // clippy::similar_names' false positive on stereo L/R naming.
    #[allow(clippy::similar_names)]
    pub fn on_levels_changed(&mut self, peak_l: u8, peak_r: u8, rms_l: u8, rms_r: u8) {
        let now = Instant::from_micros(self.now_us);
        let prev_out_level = self.model.borrow().out_level;
        let (prev_hold_l, prev_hold_l_at, prev_hold_r, prev_hold_r_at) = match prev_out_level {
            Some(sample) => (sample.hold_l, sample.hold_l_at, sample.hold_r, sample.hold_r_at),
            None => (0, now, 0, now),
        };
        let (hold_l, hold_l_at) = if peak_l >= prev_hold_l || now.saturating_duration_since(prev_hold_l_at) >= OUT_LEVEL_HOLD_DURATION {
            (peak_l, now)
        } else {
            (prev_hold_l, prev_hold_l_at)
        };
        let (hold_r, hold_r_at) = if peak_r >= prev_hold_r || now.saturating_duration_since(prev_hold_r_at) >= OUT_LEVEL_HOLD_DURATION {
            (peak_r, now)
        } else {
            (prev_hold_r, prev_hold_r_at)
        };
        // Release-ballistic attack anchor (bead pico-link-ajj, design
        // requirement C; anchors on peak, not rms, as of bead pico-link-53c
        // -- the bar itself now draws peak, so the ballistic must decay the
        // same quantity it draws or the displayed bar and its decay drift
        // apart): decay the previous anchor to "now" and compare against
        // the fresh peak sample. If the fresh sample is at or above that
        // decayed value, this is a rise -- attack is instantaneous, so the
        // anchor jumps straight to the new sample. Otherwise the anchor is
        // left exactly as it was, so the render-time decay in
        // `crate::render::hero` continues gliding down from the same
        // point instead of re-anchoring (and thus flattening the release
        // curve) on every quieter sample.
        let (prev_anchor_l, prev_anchor_l_at, prev_anchor_r, prev_anchor_r_at) = match prev_out_level {
            Some(sample) => (sample.attack_peak_l, sample.attack_peak_l_at, sample.attack_peak_r, sample.attack_peak_r_at),
            None => (0, now, 0, now),
        };
        let decayed_l = decay_peak(prev_anchor_l, now.saturating_duration_since(prev_anchor_l_at));
        let (attack_peak_l, attack_peak_l_at) =
            if peak_l >= decayed_l { (peak_l, now) } else { (prev_anchor_l, prev_anchor_l_at) };
        let decayed_r = decay_peak(prev_anchor_r, now.saturating_duration_since(prev_anchor_r_at));
        let (attack_peak_r, attack_peak_r_at) =
            if peak_r >= decayed_r { (peak_r, now) } else { (prev_anchor_r, prev_anchor_r_at) };
        self.model.borrow_mut().out_level = Some(OutLevelSample {
            peak_l,
            peak_r,
            rms_l,
            rms_r,
            hold_l,
            hold_r,
            hold_l_at,
            hold_r_at,
            received_at: now,
            attack_peak_l,
            attack_peak_r,
            attack_peak_l_at,
            attack_peak_r_at,
        });
        self.refresh_stack();
    }

    /// Adds (or, if `addr` is already known, updates the name/rssi of) one
    /// discovered device and refreshes the devices screen. Update-in-place
    /// rather than appending a duplicate row: BTstack's inquiry reports the
    /// same device repeatedly as its RSSI/name resolve.
    pub fn add_device(&mut self, addr: [u8; 6], name: String, rssi: i8, class_of_device: u32) {
        {
            let mut model = self.model.borrow_mut();
            if let Some(existing) = model.discovered.iter_mut().find(|d| d.addr == addr) {
                existing.name = name;
                existing.rssi = rssi;
                existing.class_of_device = class_of_device;
            } else {
                model.discovered.push(DeviceEntry { addr, name, rssi, class_of_device });
            }
        }
        // Kept in lockstep with `model.discovered` -- see `wizard_devices`'s
        // doc comment on why the wizard widget needs its own mirror
        // rather than a borrow into `self.model`.
        self.wizard_devices.borrow_mut().clone_from(&self.model.borrow().discovered);
        self.refresh_stack();
    }

    /// Clears the discovered-device list, e.g. at the start of a fresh
    /// scan.
    pub fn clear_devices(&mut self) {
        self.model.borrow_mut().discovered.clear();
        self.wizard_devices.borrow_mut().clear();
        self.refresh_stack();
    }

    /// Records a failed connect attempt with its [`ConnectFailureReason`]
    /// and returns the link to [`LinkState::Idle`] -- the attempt is over
    /// either way, retryable or not; a future screen deciding whether to
    /// offer a retry reads `reason.retryable()` off
    /// `BtModel::last_connect_failure`, not the link state.
    pub fn record_connect_failure(&mut self, addr: [u8; 6], reason: ConnectFailureReason) {
        self.model.borrow_mut().last_connect_failure = Some((addr, reason));
        // Bead pico-link-88xs, INVARIANT L2: `link_state` has exactly one
        // writer. Routed through `set_link_state` rather than assigning
        // the field directly (as this used to) -- correct today only by
        // accident, since the preceding `Connecting` push had already
        // cleared the four connected-model fields; going through the real
        // setter makes that true by construction instead. The
        // `refresh_stack()` call below is therefore redundant with the one
        // inside `set_link_state`, but harmless -- `refresh_stack` is
        // idempotent.
        self.set_link_state(LinkState::Idle);
        // Phase 4/5 -> phase 6 (failure outcome). Unconditional (not
        // gated on the wizard currently being open/mid-connect): a stray
        // `ConnectFailed` with the wizard closed or already past this
        // attempt just sets a phase nothing is currently rendering, which
        // is harmless and gets overwritten the next time the wizard opens
        // (`build_devices_screen`'s "Pair new headphones" row activation
        // resets it to `WizardPhase::scanning_pending`).
        *self.wizard_phase.borrow_mut() = WizardPhase::Failed { addr, reason };
        self.refresh_stack();
    }
}
