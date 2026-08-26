//! Host `Clock`: a thin wrapper over `std::time::Instant`, which is
//! already available on both targets (see the rationale in
//! `pico_link_core::platform`), so there is nothing host-specific to do beyond
//! satisfying the trait.

use pico_link_core::platform::Clock;
use std::time::Instant;

#[derive(Debug, Default, Clone, Copy)]
pub struct HostClock;

impl HostClock {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for HostClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic_across_two_calls() {
        let clock = HostClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }
}
