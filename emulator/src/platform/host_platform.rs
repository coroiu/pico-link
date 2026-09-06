//! `HostPlatform<D, I>`: the minimal wiring needed to instantiate a
//! `pico_link_core::platform::Platform` on the host, generic over which
//! `DisplaySurface` (`D`) and `InputSource` (`I`) back it — the headless
//! and windowed run modes plug in `HeadlessSurface`/`NoopInput` or
//! `MinifbSurface`/`WindowedInput` respectively, sharing the same `Clock`,
//! `Storage`, and `PowerControl` implementations either way.

use pico_link_core::platform::{DisplaySurface, InputSource, Platform};

use super::clock::HostClock;
use super::power::RecordingPowerControl;
use super::storage::FileStorage;

pub struct HostPlatform<D: DisplaySurface, I: InputSource> {
    display: D,
    input: I,
    clock: HostClock,
    storage: FileStorage,
    /// The `PowerControl` capability. Concrete, mirroring `clock`/`storage`
    /// above — see [`RecordingPowerControl`]'s doc comment for why the
    /// host's implementation is a recording/no-op stub rather than
    /// anything that actually sleeps the process.
    power: RecordingPowerControl,
}

impl<D: DisplaySurface, I: InputSource> HostPlatform<D, I> {
    #[must_use]
    pub fn new(display: D, input: I, storage: FileStorage, power: RecordingPowerControl) -> Self {
        Self { display, input, clock: HostClock::new(), storage, power }
    }
}

impl<D: DisplaySurface, I: InputSource> Platform for HostPlatform<D, I> {
    type Display = D;
    type Input = I;
    type Clock = HostClock;
    type Storage = FileStorage;
    type Power = RecordingPowerControl;

    fn display(&mut self) -> &mut Self::Display {
        &mut self.display
    }

    fn input(&mut self) -> &mut Self::Input {
        &mut self.input
    }

    fn clock(&self) -> &Self::Clock {
        &self.clock
    }

    fn storage(&mut self) -> &mut Self::Storage {
        &mut self.storage
    }

    fn power(&mut self) -> &mut Self::Power {
        &mut self.power
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{HeadlessSurface, NoopInput};
    use pico_link_core::platform::{Clock, Instant, PowerControl, Storage};
    use pico_link_core::render::{FrameBuffer565, Navigator, RenderCtx, Screen};
    use uuid::Uuid;

    fn temp_storage_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("pico-link-emulator-host-platform-test-{name}-{}.json", Uuid::new_v4()))
    }

    #[test]
    fn a_headless_host_platform_can_be_assembled_and_used_through_the_platform_trait() {
        let storage = FileStorage::new(temp_storage_path("storage")).unwrap();
        let mut platform = HostPlatform::new(HeadlessSurface::new(), NoopInput::new(), storage, RecordingPowerControl::new());

        // Exercise every accessor through the `Platform` trait object,
        // proving the bundle actually satisfies the trait, not just that
        // the struct compiles standalone.
        assert!(platform.input().poll().is_empty());
        let _now = platform.clock().now();
        assert_eq!(platform.storage().get("missing"), None);
        platform.storage().set("probe", vec![1, 2, 3]).unwrap();
        assert_eq!(platform.storage().get("probe"), Some(vec![1, 2, 3]));
        assert!(!platform.power().on_external_power());
        platform.power().enter_deep_sleep();
        assert_eq!(platform.power().deep_sleep_call_count(), 1);

        let mut navigator = Navigator::new(Screen::new("Test", vec![]));
        let mut framebuffer = FrameBuffer565::new(10, 10);
        let ctx = RenderCtx::at(Instant::from_micros(0));
        navigator.render(&ctx, &mut framebuffer).unwrap();
        platform.display().flush(&framebuffer).unwrap();
    }
}
