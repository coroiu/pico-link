#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m_rt::entry;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_time::Delay;
use embedded_hal::delay::DelayNs;
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
    const INVON: u8 = 0x21;

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

        // A single CS-low window covers the command byte AND its parameter
        // bytes (DC toggled low-then-high in between, CS held throughout).
        // The ST7789 datasheet's write sequences show CSX staying asserted
        // for the whole command+parameter transaction; raising CS between
        // the command and its data (the original bug here) makes many
        // panels discard the command or ignore the parameters that follow.
        fn command(&mut self, cmd: u8, params: &[u8]) {
            self.cs.set_low();
            self.dc.set_low();
            let _ = self.spi.blocking_write(&[cmd]);
            if !params.is_empty() {
                self.dc.set_high();
                let _ = self.spi.blocking_write(params);
            }
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

            self.command(SWRESET, &[]);
            delay.delay_ms(150);

            self.command(SLPOUT, &[]);
            delay.delay_ms(120);

            self.command(COLMOD, &[0x55]); // 16 bits/pixel, RGB565
            self.command(MADCTL, &[0x00]);
            // The Waveshare Pico-LCD-1.3 panel needs display inversion on,
            // or colours render photo-negative. Wrong colour alone wouldn't
            // explain an all-white screen, but it will bite the moment the
            // fill itself is fixed, so it goes in now.
            self.command(INVON, &[]);

            // CASET/RASET addresses are INCLUSIVE start/end column and row,
            // so the end value for a 240px axis is 239 (0x00EF), not 240 -
            // 240 (0x00F0) programs a 241px window on a 240px panel.
            let x_end = WIDTH - 1;
            let y_end = HEIGHT - 1;
            self.command(CASET, &[0x00, 0x00, (x_end >> 8) as u8, (x_end & 0xff) as u8]);
            self.command(RASET, &[0x00, 0x00, (y_end >> 8) as u8, (y_end & 0xff) as u8]);

            // RAMWR's pixel stream is itself parameter data to the command,
            // so it must stay inside the same CS-low window as the command
            // byte too - same fix as above, just with a big params buffer
            // instead of a few register bytes. Written directly rather than
            // through `command()` to avoid building a 115200-byte buffer.
            self.cs.set_low();
            self.dc.set_low();
            let _ = self.spi.blocking_write(&[RAMWR]);
            self.dc.set_high();
            let px = [(color >> 8) as u8, (color & 0xff) as u8];
            for _ in 0..(WIDTH as u32 * HEIGHT as u32) {
                let _ = self.spi.blocking_write(&px);
            }
            self.cs.set_high();

            self.command(DISPON, &[]);
            delay.delay_ms(20);

            // Backlight on. The panel is otherwise driven correctly with the
            // backlight off, but nothing would be visible to Andreas.
            self.bl.set_high();
        }
    }
}

// --- Core1 stack + dual-core auto-BOOTSEL escape hatch (pico-link-8v3.2.6) ---
//
// The board has no USB reset interface yet (that lands with pico-link-
// 8v3.2.2's picotool reboot support), so every flash normally needs a
// manual BOOTSEL hold. For unattended overnight iteration that is fatal:
// the first hypothesis that hangs or crashes ends the whole run. The fix,
// per Andreas's explicit approval of the dual-core design over a simpler
// single-core timer: core0 does nothing risky at all. It spawns core1 to
// run the still-unverified display bring-up, waits a fixed window, and
// unconditionally calls the RP2350 bootrom's reset_to_usb_boot - a watchdog
// scratch-register reboot (see embassy-rp 0.10.0's
// src/rom_data/rp235x.rs:770, ported from pico-sdk's bootrom.c) that does
// not require core1, or anything else, to cooperate or even still be
// running.
//
// Independence claim, stated plainly rather than assumed: the RP2350's two
// Cortex-M33 cores have separate NVICs and separate fault handling: a
// HardFault or an infinite loop on core1 halts (or loops on) core1 only: it
// does not touch core0's instruction stream, and core0's poll loop below
// touches no core1-owned peripheral and no shared mutex/spinlock, only a
// plain atomic flag and the hardware timer. I have NOT verified this on
// real silicon by deliberately hard-faulting core1 - that is a fair
// residual gap, and I'm flagging it rather than quietly asserting more
// confidence than I have. The one theoretical shared-resource risk I can
// think of: both cores fetch code over the same XIP flash interface, so if
// core1's fault happened to wedge that shared controller mid-transaction
// (not just loop in already-fetched code), it could in principle stall
// core0's fetches too. I have no evidence this occurs for the SPI/GPIO code
// running on core1 here, and it is not something core0 can insure itself
// against without moving code into RAM, which I did not do tonight.
static mut CORE1_STACK: Stack<16384> = Stack::new();

/// Set by core1 once the ST7789 bring-up call has RETURNED (not merely
/// started) - i.e. it ran to completion without hanging or hard-faulting
/// before reaching this point. This is deliberately the ONLY thing core0
/// waits on, and only up to a bounded deadline: if core1 never sets it,
/// core0's poll loop below still terminates on its own timer and reboots
/// anyway. The flag is a best-effort "did the risky code get through init"
/// signal, not a display-correctness signal - it says nothing about
/// red-vs-white, only about whether init_and_fill returned.
static CORE1_DISPLAY_INIT_DONE: AtomicU32 = AtomicU32::new(0);

/// Runs on core1: the ST7789 bring-up plus the pre-existing BTstack/TinyUSB
/// link-seam probes. Everything here is allowed to hang or fault - that is
/// the entire point of running it off core0.
fn run_core1(
    spi1: embassy_rp::Peri<'static, embassy_rp::peripherals::SPI1>,
    sck: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_10>,
    mosi: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_11>,
    dc: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_8>,
    cs: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_9>,
    rst: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_12>,
    bl: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_13>,
) -> ! {
    let mut delay = Delay;

    let dc = Output::new(dc, Level::Low);
    let cs = Output::new(cs, Level::High);
    let rst = Output::new(rst, Level::Low);
    let bl = Output::new(bl, Level::Low);

    let mut spi_config = SpiConfig::default();
    // 20MHz on the first attempt produced a solid white screen (backlight
    // on, panel never actually initialised - see the CS-framing fix above,
    // the most likely cause). Dropping this further removes clock speed as
    // a variable while that fix is being verified; 1MHz is nowhere near
    // ST7789's ~62.5MHz ceiling. Bump back up once the fill is confirmed.
    spi_config.frequency = 1_000_000;
    let spi = Spi::new_blocking_txonly(spi1, sck, mosi, spi_config);

    let mut display = st7789::St7789::new(spi, dc, cs, rst, bl);
    // Solid red fill, RGB565 0xF800 - the sign of life Andreas is asked to
    // look for.
    display.init_and_fill(&mut delay, 0xF800);

    // Signal core0: init_and_fill returned. See CORE1_DISPLAY_INIT_DONE's
    // doc comment for exactly what this does and does not claim.
    CORE1_DISPLAY_INIT_DONE.store(1, Ordering::SeqCst);

    unsafe {
        btstack_ffi::btstack_memory_init();
        let run_loop = btstack_ffi::btstack_run_loop_embedded_get_instance();
        btstack_ffi::btstack_run_loop_init(run_loop);
        let transport = btstack_ffi::hci_transport_dummy_instance();
        btstack_ffi::hci_init(transport, core::ptr::null());
        // Never actually reached with a dummy transport (hci_power_control
        // is not wired up yet - gate 5/6), but proves gap_inquiry_start
        // resolves at link time against the cross-compiled BTstack archive.
        // Deliberately AFTER the done-flag store above: this probe is not
        // known to return, and must not be able to block the hatch signal.
        let _ = btstack_ffi::gap_inquiry_start(5);
        let _ = tinyusb_ffi::tusb_inited();
    }

    loop {
        cortex_m::asm::wfe();
    }
}

#[entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());

    let core1_stack = unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) };
    let spi1 = p.SPI1;
    let sck = p.PIN_10;
    let mosi = p.PIN_11;
    let dc = p.PIN_8;
    let cs = p.PIN_9;
    let rst = p.PIN_12;
    let bl = p.PIN_13;

    spawn_core1(p.CORE1, core1_stack, move || {
        run_core1(spi1, sck, mosi, dc, cs, rst, bl)
    });

    // --- Core0: nothing but the auto-BOOTSEL escape hatch below ---
    //
    // Timing: a ~10s / ~25s split, chosen so the two outcomes are trivially
    // distinguishable from the host by timing when the RP2350 volume
    // reappears under /Volumes (per the coordinator's overnight telemetry
    // request), while both numbers stay in the "a human can read the
    // screen, an unattended loop isn't painfully slow" 15-30s ballpark this
    // was scoped to land in:
    //   - up to SUCCESS_DEADLINE_MS (10s): poll CORE1_DISPLAY_INIT_DONE
    //     every 100ms. Display init in this spike is a few hundred
    //     RAMWR-loop milliseconds at 1MHz, so 10s is generous slack for a
    //     genuine success signal to arrive.
    //   - if not signalled by then: keep waiting, WITHOUT continuing to
    //     depend on the flag, up to HARD_DEADLINE_MS (25s), then reboot
    //     unconditionally regardless of core1's state. This is the
    //     independence guarantee: a hung or hard-faulted core1 still cannot
    //     prevent core0 from reaching reset_to_usb_boot, it only pushes the
    //     timing from the ~10s success bucket to the ~25s fallback bucket.
    const POLL_INTERVAL_MS: u32 = 100;
    const SUCCESS_DEADLINE_MS: u32 = 10_000;
    const HARD_DEADLINE_MS: u32 = 25_000;

    let mut delay = Delay;
    let mut elapsed_ms: u32 = 0;
    let mut signalled = false;
    while elapsed_ms < SUCCESS_DEADLINE_MS {
        if CORE1_DISPLAY_INIT_DONE.load(Ordering::SeqCst) != 0 {
            signalled = true;
            break;
        }
        delay.delay_ms(POLL_INTERVAL_MS);
        elapsed_ms += POLL_INTERVAL_MS;
    }
    let deadline_ms = if signalled { SUCCESS_DEADLINE_MS } else { HARD_DEADLINE_MS };
    while elapsed_ms < deadline_ms {
        delay.delay_ms(POLL_INTERVAL_MS);
        elapsed_ms += POLL_INTERVAL_MS;
    }

    // Drops the chip into BOOTSEL; re-enumerates as the RP2350 mass-storage
    // drive with no human involvement. Signature verified against embassy-rp
    // 0.10.0's vendored source (src/rom_data/rp235x.rs:770): both arguments
    // are bitmasks - 0 for usb_activity_gpio_pin_mask means no activity-LED
    // pin, 0 for disable_interface_mask means both the mass-storage and
    // PICOBOOT USB interfaces stay enabled, matching a cold boot into
    // BOOTSEL.
    embassy_rp::rom_data::reset_to_usb_boot(0, 0);

    // reset_to_usb_boot does not return on success (REBOOT2_FLAG_NO_RETURN_ON_SUCCESS).
    // This only executes if the reboot call itself failed.
    loop {
        cortex_m::asm::nop();
    }
}
