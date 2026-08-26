//! Host `PowerControl`: a recording/no-op stub for the deep-sleep
//! mechanism.
//!
//! There is no real hardware to power down on the host, so this
//! implementation just records what `pico_link_core::run::run`'s deep-sleep
//! policy asked it to do — `enter_deep_sleep` increments a call counter
//! instead of ever actually sleeping the process, and `on_external_power`
//! returns whatever a test/caller last set via [`RecordingPowerControl::
//! set_external_power`] (defaulting to `false`, i.e. "on battery" —
//! matching the conservative default a real board would report if it
//! couldn't determine its power source). This makes `crate::run::run`'s
//! deep-sleep policy provably testable with no device.
//!
//! In practice, today's default builds never actually call
//! `enter_deep_sleep` in a way that matters: `pico_link_core::power::
//! DEEP_SLEEP_ARMED` is `false` (see that constant's doc comment), so
//! `emulator::main`'s wiring always passes `deep_sleep_timeout: None` into
//! `run`. This type exists so the seam is real end to end (a concrete
//! `Platform::Power` the emulator can construct) even while the tier
//! itself stays dormant.

use std::sync::{Arc, Mutex};

use pico_link_core::platform::PowerControl;

#[derive(Debug, Default)]
struct PowerControlState {
    external_power: bool,
    deep_sleep_calls: u32,
}

/// Shared via `Arc<Mutex<_>>` (cloning shares the same underlying state) —
/// not because anything here needs cross-thread sharing today (nothing
/// here is exposed over HTTP), but so a test can hold a clone for
/// inspection/control after the original has been moved into a
/// `HostPlatform`, exactly like `RecordingPower`'s pattern in
/// `pico_link_core::run`'s own tests.
#[derive(Clone, Default)]
pub struct RecordingPowerControl(Arc<Mutex<PowerControlState>>);

impl RecordingPowerControl {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets what the next `on_external_power` call (and every one after,
    /// until changed again) will report.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `Mutex` is poisoned (a prior holder of the
    /// lock panicked while holding it) -- not expected in practice, since
    /// nothing here does fallible work while holding the lock.
    pub fn set_external_power(&self, value: bool) {
        self.0.lock().unwrap().external_power = value;
    }

    /// How many times `enter_deep_sleep` has been called so far.
    ///
    /// # Panics
    ///
    /// Panics if the underlying `Mutex` is poisoned -- see
    /// [`Self::set_external_power`]'s doc comment.
    #[must_use]
    pub fn deep_sleep_call_count(&self) -> u32 {
        self.0.lock().unwrap().deep_sleep_calls
    }
}

impl PowerControl for RecordingPowerControl {
    fn on_external_power(&self) -> bool {
        self.0.lock().unwrap().external_power
    }

    fn enter_deep_sleep(&mut self) {
        self.0.lock().unwrap().deep_sleep_calls += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_not_on_external_power_and_no_deep_sleep_calls() {
        let power = RecordingPowerControl::new();
        assert!(!power.on_external_power());
        assert_eq!(power.deep_sleep_call_count(), 0);
    }

    #[test]
    fn set_external_power_is_reflected_immediately() {
        let power = RecordingPowerControl::new();
        power.set_external_power(true);
        assert!(power.on_external_power());
        power.set_external_power(false);
        assert!(!power.on_external_power());
    }

    #[test]
    fn enter_deep_sleep_increments_the_call_count_every_time() {
        let mut power = RecordingPowerControl::new();
        power.enter_deep_sleep();
        power.enter_deep_sleep();
        assert_eq!(power.deep_sleep_call_count(), 2);
    }

    #[test]
    fn cloning_shares_the_same_underlying_state() {
        let mut power = RecordingPowerControl::new();
        let clone = power.clone();

        power.enter_deep_sleep();
        clone.set_external_power(true);

        assert_eq!(power.deep_sleep_call_count(), 1);
        assert!(power.on_external_power());
    }
}
