#![no_std]
#![no_main]

use cortex_m_rt::entry;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_time::Delay;
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

// Minimal ST7789 bring-up for pico-link-8v3.2.1's proof-of-life gate: a
// solid-colour full-screen fill over SPI, hand-rolled against raw ST7789
// commands rather than pulling in a driver crate, since all we need here is
// proof that Rust owns this path end to end (SPI + GPIO, no C involved) -
// this is not the product's eventual display driver.
//
// Pins per CLAUDE.md (Waveshare Pico-LCD-1.3, 240x240 ST7789, 4-wire SPI):
// GP8 DC, GP9 CS, GP10 SCK, GP11 MOSI, GP12 RST, GP13 BL.
mod st7789 {
    use embassy_rp::gpio::Output;
    use embassy_rp::spi::{Blocking, Spi};
    use embassy_rp::spi::Instance as SpiInstance;
    use embassy_time::Delay;
    use embedded_hal::delay::DelayNs;

    const CASET: u8 = 0x2A;
    const RASET: u8 = 0x2B;
    const RAMWR: u8 = 0x2C;
    const MADCTL: u8 = 0x36;
    const COLMOD: u8 = 0x3A;
    const SLPOUT: u8 = 0x11;
    const DISPON: u8 = 0x29;
    const SWRESET: u8 = 0x01;

    pub const WIDTH: u16 = 240;
    pub const HEIGHT: u16 = 240;

    pub struct St7789<'d, T: SpiInstance> {
        spi: Spi<'d, T, Blocking>,
        dc: Output<'d>,
        cs: Output<'d>,
        rst: Output<'d>,
        bl: Output<'d>,
    }

    impl<'d, T: SpiInstance> St7789<'d, T> {
        pub fn new(
            spi: Spi<'d, T, Blocking>,
            dc: Output<'d>,
            cs: Output<'d>,
            rst: Output<'d>,
            bl: Output<'d>,
        ) -> Self {
            Self { spi, dc, cs, rst, bl }
        }

        fn write_cmd(&mut self, cmd: u8) {
            self.dc.set_low();
            self.cs.set_low();
            let _ = self.spi.blocking_write(&[cmd]);
            self.cs.set_high();
        }

        fn write_data(&mut self, data: &[u8]) {
            self.dc.set_high();
            self.cs.set_low();
            let _ = self.spi.blocking_write(data);
            self.cs.set_high();
        }

        /// Hardware reset, register init, and full-screen 16bpp RGB565 fill.
        pub fn init_and_fill(&mut self, delay: &mut Delay, color: u16) {
            // Hardware reset: active low.
            self.rst.set_high();
            delay.delay_ms(10);
            self.rst.set_low();
            delay.delay_ms(10);
            self.rst.set_high();
            delay.delay_ms(120);

            self.write_cmd(SWRESET);
            delay.delay_ms(150);

            self.write_cmd(SLPOUT);
            delay.delay_ms(120);

            self.write_cmd(COLMOD);
            self.write_data(&[0x55]); // 16 bits/pixel, RGB565

            self.write_cmd(MADCTL);
            self.write_data(&[0x00]);

            self.write_cmd(CASET);
            self.write_data(&[0x00, 0x00, (WIDTH >> 8) as u8, (WIDTH & 0xff) as u8]);

            self.write_cmd(RASET);
            self.write_data(&[0x00, 0x00, (HEIGHT >> 8) as u8, (HEIGHT & 0xff) as u8]);

            self.write_cmd(RAMWR);
            let px = [(color >> 8) as u8, (color & 0xff) as u8];
            self.dc.set_high();
            self.cs.set_low();
            for _ in 0..(WIDTH as u32 * HEIGHT as u32) {
                let _ = self.spi.blocking_write(&px);
            }
            self.cs.set_high();

            self.write_cmd(DISPON);
            delay.delay_ms(20);

            // Backlight on. The panel is otherwise driven correctly with the
            // backlight off, but nothing would be visible to Andreas.
            self.bl.set_high();
        }
    }
}

#[entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());
    let mut delay = Delay;

    let dc = Output::new(p.PIN_8, Level::Low);
    let cs = Output::new(p.PIN_9, Level::High);
    let rst = Output::new(p.PIN_12, Level::Low);
    let bl = Output::new(p.PIN_13, Level::Low);

    let mut spi_config = SpiConfig::default();
    // Conservative first-bring-up clock; ST7789 tops out far higher, but
    // this is unverified hardware and there is nothing to lose by starting
    // slow. Bump once the fill is confirmed on screen.
    spi_config.frequency = 20_000_000;
    let spi = Spi::new_blocking_txonly(p.SPI1, p.PIN_10, p.PIN_11, spi_config);

    let mut display = st7789::St7789::new(spi, dc, cs, rst, bl);
    // Solid red fill, RGB565 0xF800 - the sign of life Andreas is asked to
    // look for.
    display.init_and_fill(&mut delay, 0xF800);

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
