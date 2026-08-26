// Library exports for the desktop emulator binary and its example tools
// (which need types below but can't depend on a `[[bin]]` target).
//
// `pico_link_core::App` + `pico_link_core::run`, driven through `platform`'s
// `HostPlatform`/surfaces/inputs, is the unified app loop shared by both
// run modes — see `main.rs`.

pub mod desktop;
pub mod platform;
