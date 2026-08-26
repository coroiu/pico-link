#![no_std]
#![no_main]

use cortex_m_rt::entry;
use panic_halt as _;

// Link-seam probe for pico-link-8v3.1 gate 3/4: force the linker to pull in
// BTstack's HCI/L2CAP core + the embedded run loop from the Rust-owned
// binary, using a dummy hci_transport_t (btstack-probe/hci_transport_dummy.c)
// in place of the real cyw43 bridge. If this links, BTstack itself is not a
// wall for the Cargo-owns-the-binary architecture.
mod btstack_ffi {
    #[allow(non_camel_case_types)]
    pub type hci_transport_t = core::ffi::c_void;

    unsafe extern "C" {
        pub fn btstack_memory_init();
        pub fn btstack_run_loop_init(run_loop: *const core::ffi::c_void);
        pub fn btstack_run_loop_embedded_get_instance() -> *const core::ffi::c_void;
        pub fn hci_init(transport: *const hci_transport_t, config: *const core::ffi::c_void);
        pub fn hci_transport_dummy_instance() -> *const hci_transport_t;
        pub fn gap_inquiry_start(duration: u8) -> u8;
    }
}

// Same probe, for TinyUSB's device-independent core (proves it links
// alongside BTstack in one Cargo-owned binary - Andreas's composite-device
// coexistence requirement). tud_init's rhport-specific dcd_init is NOT
// linked here (that needs the RP2350 dcd, gate 1's remaining hurdle) - this
// only proves usbd.c + cdc_device.c resolve against tusb.c.
mod tinyusb_ffi {
    unsafe extern "C" {
        pub fn tusb_inited() -> bool;
    }
}

#[entry]
fn main() -> ! {
    let _p = embassy_rp::init(Default::default());

    unsafe {
        btstack_ffi::btstack_memory_init();
        let run_loop = btstack_ffi::btstack_run_loop_embedded_get_instance();
        btstack_ffi::btstack_run_loop_init(run_loop);
        let transport = btstack_ffi::hci_transport_dummy_instance();
        btstack_ffi::hci_init(transport, core::ptr::null());
        // Never actually reached with a dummy transport (hci_power_control
        // is not wired up yet - gate 5/6), but proves gap_inquiry_start
        // resolves at link time against the cross-compiled BTstack archive.
        let _ = btstack_ffi::gap_inquiry_start(5);
        let _ = tinyusb_ffi::tusb_inited();
    }

    loop {
        cortex_m::asm::nop();
    }
}
