#![no_std]
#![no_main]

use core::fmt::Write as _;
use core::sync::atomic::{AtomicU32, Ordering};

use cortex_m_rt::entry;
use embassy_executor::Executor;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::peripherals::USB;
use embassy_rp::spi::{Config as SpiConfig, Spi};
use embassy_rp::usb::{Driver as UsbDriver, InterruptHandler as UsbInterruptHandler};
use embassy_rp::watchdog::{ResetReason, Watchdog};
use embassy_time::{Delay, Duration, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State as CdcState};
use embedded_hal::delay::DelayNs;
use embassy_usb::{Builder as UsbBuilder, Config as UsbConfig};
use static_cell::StaticCell;

// pico-link-8v3.2.7: embassy-usb owns the USB peripheral tonight (THROWAWAY -
// see the bead. TinyUSB must take the peripheral back for UAC2 audio later;
// this CDC console does not coexist with that, it is a debug-only detour).
bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => UsbInterruptHandler<USB>;
});

/// Custom panic handler, replacing `panic-halt`'s bare `loop {}`.
///
/// WHY: that silent halt is what masked pico-link-8v3.2.7's real bug (a
/// too-small CONTROL_BUF, see its comment above) for an entire session. The
/// panic fired inside usb_task's poll - i.e. inside a normal async task on
/// core0's cooperative executor, not inside the USBCTRL_IRQ ISR - and
/// `loop {}` never unwound back to `Executor::run()`, so it never got the
/// chance to poll ANY other task again either. That froze
/// `watchdog_feed_task` and (in the since-removed async-Timer design) the
/// old safety_net_task right along with USB, which read on the bench as a
/// total, USB-independent hang rather than what it actually was: a panic in
/// one task with everything else collaterally starved.
///
/// CHOICE: jump straight to `reset_to_usb_boot()` rather than looping and
/// waiting on the hardware watchdog armed in `fn main`. Two reasons. First,
/// there is no RTT/probe attached on this bench, so `PanicInfo` has nowhere
/// to go - printing it and then halting buys nothing a plain halt doesn't
/// already give (once again: nothing distinguishable). Second, the thing we
/// actually need for the unattended dev loop is recovery, and this gets it
/// ~8s faster than letting the watchdog time out. Cost: a panic is no
/// longer distinguishable on-screen from a clean reboot - the USB_STAGE
/// colour mechanism already answers "how far did enumeration get" for
/// panics on that specific path, which was this session's actual open
/// question; a panic anywhere else now just self-recovers rather than
/// wedging, which is the trade this project needs more than a diagnosis
/// display can't show anyway.
///
/// Calling a synchronous ROM function from panic context is safe here: it
/// only pokes WATCHDOG scratch/control registers and does not touch
/// whatever state was live when the panic fired, so it doesn't matter that
/// that state may be inconsistent.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    embassy_rp::rom_data::reset_to_usb_boot(0, 0);
    loop {
        cortex_m::asm::nop();
    }
}

/// Small `core::fmt::Write` sink over a fixed buffer - avoids pulling in
/// `heapless` as a direct dependency (it's already a transitive dep of
/// embassy-usb, but not usable without declaring it ourselves) just to
/// format one heartbeat line. `no_std`, no `alloc` in this crate.
struct FixedBuf<const N: usize> {
    data: [u8; N],
    len: usize,
}

impl<const N: usize> FixedBuf<N> {
    const fn new() -> Self {
        Self { data: [0; N], len: 0 }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

impl<const N: usize> core::fmt::Write for FixedBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let end = (self.len + bytes.len()).min(N);
        let take = end - self.len;
        self.data[self.len..end].copy_from_slice(&bytes[..take]);
        self.len = end;
        Ok(())
    }
}

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

        /// Full-screen 16bpp RGB565 fill, assuming init already ran. Used to
        /// repaint the USB enumeration stage without re-initialising.
        pub fn fill(&mut self, color: u16) {
            self.cs.set_low();
            self.dc.set_low();
            let _ = self.spi.blocking_write(&[RAMWR]);
            self.dc.set_high();
            let px = [(color >> 8) as u8, (color & 0xff) as u8];
            for _ in 0..(WIDTH as u32 * HEIGHT as u32) {
                let _ = self.spi.blocking_write(&px);
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

// --- picotool reset interface (pico-link-b4o) ---
//
// Reimplementation, from the wire protocol only, of the vendor interface
// pico-sdk exposes as `pico_stdio_usb`'s reset interface. The constants below
// are protocol facts (they are what picotool sends); none of pico-sdk's code
// is copied here - this is ~40 lines of embassy-usb `Handler` written against
// the same three descriptor bytes and one control request.
//
// What it buys: `picotool reboot -f -u` puts a RUNNING board into BOOTSEL, so
// flashing no longer needs a human holding the button, and the blunt timed
// reboot on core0 can drop back to being a safety net instead of the primary
// mechanism.
//
// LIMIT, stated plainly: this only works while the USB stack on core1 is
// alive. If core1 hangs before enumeration, or the USB task itself is what is
// broken, picotool cannot see the device at all and the core0 timer is the
// only way back. That is exactly why the timer stays.
mod reset_iface {
    use embassy_usb::control::{OutResponse, Recipient, Request, RequestType};
    use embassy_usb::Handler;

    /// USB vendor-specific class, with the subclass/protocol pair picotool
    /// matches on when it looks for a rebootable device.
    pub const CLASS: u8 = 0xFF;
    pub const SUBCLASS: u8 = 0x00;
    pub const PROTOCOL: u8 = 0x01;

    /// bRequest for "reset into BOOTSEL". wValue carries picotool's flags:
    /// bit 8 set means bits 15..9 are an activity-LED GPIO number, and the
    /// low 7 bits are the bootrom's disable-interface mask (0 = leave both
    /// mass-storage and PICOBOOT enabled).
    const REQUEST_BOOTSEL: u8 = 0x01;

    pub struct ResetHandler {
        itf_num: u8,
    }

    impl ResetHandler {
        pub const fn new(itf_num: u8) -> Self {
            Self { itf_num }
        }
    }

    impl Handler for ResetHandler {
        fn control_out(&mut self, req: Request, _data: &[u8]) -> Option<OutResponse> {
            // embassy-usb hands every non-standard control request to EVERY
            // registered handler, so the wIndex check is what keeps this from
            // hijacking requests aimed at the CDC interfaces. Both Class and
            // Vendor request types are accepted: TinyUSB routes by interface
            // number without checking the type field, so picotool's exact
            // choice of type bits is not something to depend on.
            if req.recipient != Recipient::Interface
                || req.index != self.itf_num as u16
                || !matches!(req.request_type, RequestType::Class | RequestType::Vendor)
                || req.request != REQUEST_BOOTSEL
            {
                return None;
            }

            let gpio_mask = if req.value & 0x100 != 0 {
                1u32 << (req.value >> 9)
            } else {
                0
            };
            let disable_mask = (req.value & 0x7f) as u32;
            embassy_rp::rom_data::reset_to_usb_boot(gpio_mask, disable_mask);

            // Unreachable in practice - reset_to_usb_boot does not return on
            // success. Rejecting (rather than accepting) is the honest answer
            // if it ever does: nothing was reset.
            Some(OutResponse::Rejected)
        }
    }
}

// --- embassy-usb CDC-ACM debug console (pico-link-8v3.2.7) ---
//
// THROWAWAY: embassy-usb and TinyUSB cannot both own the USB peripheral.
// When UAC2 audio lands in TinyUSB (Andreas's composite-device requirement)
// this console moves to TinyUSB CDC or gets reworked. Chosen over waiting
// for pico-link-8v3.2.2's TinyUSB dcd because embassy-rp already has a
// working RP2350 USB device driver and embassy-usb already has a CDC-ACM
// class in pure Rust - no dcd linking, no pico-sdk headers needed tonight.
mod usb_console {
    use super::*;
    use embassy_usb::Handler;

    pub type Cdc = CdcAcmClass<'static, UsbDriver<'static, USB>>;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    // 256, matching CONFIG_DESC/BOS_DESC below - NOT 64 (pico-link-8v3.2.7
    // root cause, found by a parallel investigation and confirmed by
    // arithmetic against the vendored source: embassy-usb 0.6.0 writes each
    // string descriptor into this same buffer as it answers
    // GET_DESCRIPTOR(STRING) requests, and asserts `pos + 2 < buf.len()`
    // (embassy-usb-0.6.0 src/lib.rs:753). config.product below is 38 chars -
    // UTF-16LE plus the 2-byte header needs 78 bytes, which blew a 64-byte
    // buffer. manufacturer (17 chars) and serial_number (7 chars) both fit
    // under 64 and would not have triggered this, which is why enumeration
    // got as far as it did before dying on the product string specifically.
    // The panic happened inside usb_task's poll (device.run().await), not
    // inside the USBCTRL_IRQ ISR - see the panic handler below for why that
    // made it look like a total hang rather than a panic.
    static CONTROL_BUF: StaticCell<[u8; 256]> = StaticCell::new();
    static CDC_STATE: StaticCell<CdcState<'static>> = StaticCell::new();
    static RESET_HANDLER: StaticCell<reset_iface::ResetHandler> = StaticCell::new();
    static STAGE_HANDLER: StaticCell<StageHandler> = StaticCell::new();
    static EXECUTOR: StaticCell<Executor> = StaticCell::new();

    /// Builds the USB device + CDC-ACM class from the USB peripheral and
    /// spawns both onto a fresh embassy executor, alongside the hardware-
    /// watchdog feed task (see `fn main` for why the watchdog, not an async
    /// Timer, is what has to survive a lockup). Runs forever - this is
    /// meant to be the last thing core0 does.
    pub fn run(usb: embassy_rp::Peri<'static, USB>, watchdog: Watchdog) -> ! {
        let driver = UsbDriver::new(usb, Irqs);

        // VID/PID: the Raspberry Pi vendor ID with pico-sdk's stdio_usb PID.
        // NOT cosmetic and NOT a claim of identity we are entitled to ship -
        // picotool 2.x only scans devices whose idVendor is 0x2e8a, so the
        // picotool reset interface below is unreachable under any other VID.
        // This is a dev-loop affordance on Raspberry Pi silicon, exactly as
        // pico-sdk's own default does it; it must get a real VID/PID (and
        // this whole embassy-usb console must move to TinyUSB) before ship.
        let mut config = UsbConfig::new(0x2e8a, 0x000a);
        config.manufacturer = Some("pico-link (spike)");
        config.product = Some("debug console (embassy-usb, throwaway)");
        config.serial_number = Some("8v3.2.7");
        config.max_power = 100;
        config.max_packet_size_0 = 64;
        // Required for CDC-ACM's IAD (interface association descriptor) so
        // Windows enumerates it as a single composite device - harmless on
        // macOS/Linux either way.
        config.device_class = 0xEF;
        config.device_sub_class = 0x02;
        config.device_protocol = 0x01;
        config.composite_with_iads = true;

        let mut builder = UsbBuilder::new(
            driver,
            config,
            CONFIG_DESC.init([0; 256]),
            BOS_DESC.init([0; 256]),
            &mut [][..],
            CONTROL_BUF.init([0; 256]),
        );

        let class = CdcAcmClass::new(&mut builder, CDC_STATE.init(CdcState::new()), 64);

        // picotool reset interface (pico-link-b4o): a zero-endpoint vendor
        // interface whose only job is to answer one control request by
        // dropping into BOOTSEL, so `picotool reboot -f -u` can re-flash the
        // board with no human and no manual BOOTSEL hold. Scoped into a block
        // so the function/interface builders release their &mut borrow of
        // `builder` before `builder.handler()` needs it.
        let reset_itf = {
            let mut func = builder.function(
                reset_iface::CLASS,
                reset_iface::SUBCLASS,
                reset_iface::PROTOCOL,
            );
            let mut iface = func.interface();
            let num = iface.interface_number();
            iface.alt_setting(
                reset_iface::CLASS,
                reset_iface::SUBCLASS,
                reset_iface::PROTOCOL,
                None,
            );
            num
        };
        builder.handler(RESET_HANDLER.init(reset_iface::ResetHandler::new(reset_itf.0)));
        builder.handler(STAGE_HANDLER.init(StageHandler));

        let device = builder.build();

        let executor = EXECUTOR.init(Executor::new());
        executor.run(|spawner| {
            spawner.spawn(usb_task(device)).unwrap();
            spawner.spawn(console_task(class)).unwrap();
            spawner.spawn(watchdog_feed_task(watchdog)).unwrap();
        })
    }

    /// Reports how far enumeration got, into `USB_STAGE`, for core1 to paint.
    /// Purely diagnostic - it answers no control requests.
    pub struct StageHandler;

    impl Handler for StageHandler {
        fn enabled(&mut self, enabled: bool) {
            if enabled {
                bump(super::stage::USB_ENABLED);
            }
        }
        fn reset(&mut self) {
            bump(super::stage::RESET);
        }
        fn addressed(&mut self, _addr: u8) {
            bump(super::stage::ADDRESSED);
        }
        fn configured(&mut self, configured: bool) {
            if configured {
                bump(super::stage::CONFIGURED);
            }
        }
    }

    fn bump(to: u32) {
        let _ = USB_STAGE.fetch_max(to, Ordering::SeqCst);
    }

    /// Feeds the hardware watchdog every 3s so it does not fire under normal
    /// operation. This REPLACES pico-link-8v3.2.6's original async-Timer
    /// safety net (an identically-shaped task that awaited
    /// `Timer::after(Duration::from_secs(180))` then called
    /// `reset_to_usb_boot` directly) after that mechanism was proven, on
    /// real hardware chasing pico-link-8v3.2.7, not to survive the very
    /// lockup it existed to recover from: the board hung during USB
    /// enumeration and the safety net never fired even after ~15 minutes -
    /// 5x its 180s deadline - meaning whatever hung also stopped this
    /// executor from making progress on ANY task, including one with no
    /// dependency on USB at all.
    ///
    /// The fix is to make the thing that survives a lockup be hardware, not
    /// software: `fn main` arms the RP2350's watchdog peripheral (a counter
    /// clocked independently of the CPU, not affected by masked interrupts
    /// or a frozen executor) before doing anything risky, and this task's
    /// only job is to keep telling it "still alive" every 3s. If the
    /// executor genuinely locks up, THIS task stops running and stops
    /// feeding right along with everything else - by design, since it is
    /// the watchdog hardware that has to survive the lockup, not this task.
    /// The watchdog then fires within 8s of the last feed, resets the chip,
    /// and the check at the top of `fn main` (BEFORE `embassy_rp::init`,
    /// deliberately not trusting anything after it) sees the timeout reason
    /// and jumps straight into BOOTSEL via a synchronous ROM call with its
    /// own from-scratch USB stack, independent of whatever hung.
    ///
    /// UNVERIFIED as of this writing - the same lockup that motivated this
    /// change also means the board hasn't reached BOOTSEL since to flash it.
    /// Development affordance only regardless. Remove or feature-gate before
    /// ship.
    #[embassy_executor::task]
    async fn watchdog_feed_task(mut watchdog: Watchdog) -> ! {
        loop {
            watchdog.feed(Duration::from_secs(8));
            Timer::after(Duration::from_secs(3)).await;
        }
    }

    #[embassy_executor::task]
    async fn usb_task(mut device: embassy_usb::UsbDevice<'static, UsbDriver<'static, USB>>) -> ! {
        device.run().await
    }

    /// Prints a boot line the instant the host opens the port (DTR), then a
    /// heartbeat once a second for as long as the port stays open. The
    /// heartbeat is driven by `embassy_time` (embassy-rp's own hardware
    /// timer, already live via the `time-driver` feature) - it is NOT proof
    /// that BTstack's `hal_time_ms` works, that is pico-link-8v3.2.3's gate,
    /// still open.
    #[embassy_executor::task]
    async fn console_task(mut class: Cdc) -> ! {
        let mut n: u32 = 0;
        loop {
            class.wait_connection().await;
            let _ = class
                .write_packet(b"PICO-LINK boot: embassy-usb CDC console up\r\n")
                .await;
            loop {
                Timer::after(Duration::from_millis(1000)).await;
                let mut buf: FixedBuf<32> = FixedBuf::new();
                buf.clear();
                let _ = write!(buf, "heartbeat {n}\r\n");
                n = n.wrapping_add(1);
                if class.write_packet(buf.as_bytes()).await.is_err() {
                    // Host closed the port - go back to waiting for a fresh
                    // connection rather than erroring out.
                    break;
                }
            }
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

/// USB enumeration stage, written by core0's USB handler and painted onto the
/// panel by core1. This exists because the CDC console is exactly the thing
/// that is broken: the host sees the right VID/PID and then nothing - no
/// config descriptor, no strings, no tty node - so the usual debug channel
/// cannot report on its own failure. The display already works (pico-link-
/// 8v3.2.1), so it becomes the instrument.
///
/// Values are stages, monotonically increasing; core1 paints the highest one
/// reached.
mod stage {
    pub const BOOT: u32 = 0; // red    - core1 alive, USB not started
    pub const USB_ENABLED: u32 = 1; // yellow - bus power/enable seen
    pub const RESET: u32 = 2; // magenta - host issued a bus reset
    pub const ADDRESSED: u32 = 3; // blue   - SET_ADDRESS completed
    pub const CONFIGURED: u32 = 4; // green  - SET_CONFIGURATION completed

    pub const COLORS: [u16; 5] = [
        0xF800, // red
        0xFFE0, // yellow
        0xF81F, // magenta
        0x001F, // blue
        0x07E0, // green
    ];
}

static USB_STAGE: AtomicU32 = AtomicU32::new(stage::BOOT);

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

    // Core1's remaining job: paint whatever enumeration stage core0's USB
    // handler has reached. This is the debug channel for a broken debug
    // channel - see USB_STAGE. Repaints only on change; a full 240x240 fill
    // at 1MHz SPI is roughly a second, so polling faster than the stage can
    // change would just smear the screen.
    let mut painted = stage::BOOT;
    loop {
        let now = USB_STAGE.load(Ordering::SeqCst).min(stage::CONFIGURED);
        if now != painted {
            display.fill(stage::COLORS[now as usize]);
            painted = now;
        }
        delay.delay_ms(50);
    }
}

#[entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());

    // Hardware-watchdog recovery, checked immediately after `init()` and
    // before anything else - core1 spawn, USB setup, all of it. `pac` is
    // `pub(crate)` inside embassy-rp, so a pre-`init()` raw register read
    // (which would cover `init()` itself too) isn't available from outside
    // the crate; `Watchdog::reset_reason()` is the earliest hook this crate
    // exposes. `init()`'s own risk is judged low - it's `ClockConfig::
    // crystal(12_000_000)`, embassy-rp's own default and the standard
    // Pico/Pico 2 crystal, not the bespoke code that actually hung.
    //
    // If the watchdog reset us because nothing fed it in time, jump straight
    // to the ROM bootloader's BOOTSEL mode. `reset_to_usb_boot` is a
    // synchronous ROM call with its own from-scratch USB stack - it does not
    // depend on our USB driver or the executor, so it is not subject to
    // whatever hung on the previous boot. See `watchdog_feed_task`'s doc
    // comment for the full story of why this replaces pico-link-8v3.2.6's
    // async-Timer safety net.
    let mut watchdog = Watchdog::new(p.WATCHDOG);
    if watchdog.reset_reason() == Some(ResetReason::TimedOut) {
        embassy_rp::rom_data::reset_to_usb_boot(0, 0);
        loop {
            cortex_m::asm::nop();
        }
    }

    // Arm the watchdog as early as possible, before core1 spawn or any USB
    // setup - the whole point is to cover as much of the risky code as
    // possible, including setup that runs before the executor exists to
    // host a feed task. 8s is the RP2350 watchdog's own ceiling (its load
    // register holds a 24-bit microsecond count, 0xFFFFFF us = ~16.777s;
    // `Watchdog::feed` panics above that), not a product choice -
    // `watchdog_feed_task` re-arms it for another 8s every 3s once the
    // executor is up, so under normal operation it never fires. Under a
    // total lockup it fires within 8s of the last feed.
    watchdog.start(Duration::from_secs(8));

    let core1_stack = unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) };
    let spi1 = p.SPI1;
    let sck = p.PIN_10;
    let mosi = p.PIN_11;
    let dc = p.PIN_8;
    let cs = p.PIN_9;
    let rst = p.PIN_12;
    let bl = p.PIN_13;
    let usb = p.USB;

    spawn_core1(p.CORE1, core1_stack, move || {
        run_core1(spi1, sck, mosi, dc, cs, rst, bl)
    });

    // --- Core0: the embassy executor - USB console + BOOTSEL safety net ---
    //
    // WHY CORE0 AND NOT CORE1 (measured, not preferred): with the executor and
    // the USB peripheral on core1 the device answered GET_DESCRIPTOR(device) -
    // the host saw the right VID/PID - and then went no further. No config
    // descriptor, no strings, no /dev/cu.usbmodem node, and picotool could not
    // see the reset interface either. Reproduced with the display and BTstack
    // probes removed, so the cause was the placement itself, not the risky
    // code sharing the core. Moving USB to core0 is the fix.
    //
    // COST, stated plainly: the recovery path is now a hardware watchdog fed
    // from a task on the same executor as USB, rather than a bare loop on an
    // otherwise-idle core, so it is no longer independent of core0's other
    // work in the way pico-link-8v3.2.6 originally intended. What makes this
    // still work as a recovery mechanism despite that: the watchdog COUNTER
    // is hardware, clocked independently of the CPU, so it does not need
    // core0 to be healthy to fire - only the feed task needs to be healthy
    // to STOP it from firing. See `watchdog_feed_task`'s doc comment for why
    // the previous async-Timer design (which did need core0 to stay healthy
    // to fire) was replaced: it was proven not to survive the exact lockup
    // it existed to catch.
    usb_console::run(usb, watchdog)
}
