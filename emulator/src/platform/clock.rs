//! Host `Clock`: wraps `std::time::Instant`, converting to/from
//! `pico_link_core`'s own `platform::Instant` at the trait boundary --
//! `core` is `no_std` (see `pico_link_core::lib`'s doc comment) and has no
//! `Instant` type of its own, so every `Clock` impl, including this host
//! one, is responsible for that conversion. `HostClock` records a fixed
//! `std::time::Instant` reference point at construction and reports every
//! `now()` call as the elapsed microseconds since that point -- an
//! arbitrary but stable reference, exactly matching the "implementation-
//! chosen reference point" contract `platform::Instant`'s doc comment
//! documents.
//!
//! Also implements `Clock::sleep` via `std::thread::sleep` -- the only
//! `std`-specific primitive `pico_link_core::run::run`'s loop needs (see
//! that module's "Why sleeping is a `Clock` method" doc comment).

use pico_link_core::platform::{Clock, Instant as CoreInstant};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct HostClock {
    epoch: Instant,
}

impl HostClock {
    #[must_use]
    pub fn new() -> Self {
        Self { epoch: Instant::now() }
    }
}

impl Default for HostClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for HostClock {
    fn now(&self) -> CoreInstant {
        let micros = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        CoreInstant::from_micros(micros)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
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
