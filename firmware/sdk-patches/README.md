# Vendored pico-sdk patches

The SDK lives OUTSIDE this repo (`$PICO_SDK_PATH`, normally
`~/.pico-sdk/sdk/2.1.1`). Anything we change there is untracked, machine-local,
and silently lost on an SDK reinstall or on a second machine. So every SDK
change we depend on is vendored here, applied by a script, and **enforced at
CMake configure time** -- `firmware/CMakeLists.txt` reads the SDK source and
calls `message(FATAL_ERROR ...)` if the patch is missing. A build that silently
diverges from version control is the specific trap this directory exists to
prevent, so the check fails the build; it does not warn.

Apply with:

    tools/apply-sdk-patches.sh          # idempotent, safe to re-run

## 01-tinyusb-rp2040-double-arm.patch

`rp2040_usb.c:108`, `_hw_endpoint_buffer_control_update32()`. Stock TinyUSB
calls `panic("ep %02X was already available")` when an endpoint buffer is
re-armed while `USB_BUF_CTRL_AVAIL` is still set -- the arm/complete race.

Two things make this fatal for us rather than merely noisy:

  * `panic()` is a **pico-sdk** function, not `assert()`. `NDEBUG` does NOT
    elide it, and pico-sdk forces `CMAKE_BUILD_TYPE=Release` when the caller
    sets none -- so it is live in our default build.
  * It fires from an **IRQ path** (`__tusb_irq_path_func`) and ends in
    `__breakpoint()`. With no debugger attached that is a hard lockup: the USB
    device stays enumerated but completely unresponsive, which is
    indistinguishable from any other wedge and costs a physical BOOTSEL press.

USBPods (the known-good reference on this exact silicon) ships precisely this
one-line patch and comments the panic out. Andreas confirmed on 2026-08-30 that
the bare `*** PANIC ***` seen previously on this project is this same panic.

**We do not merely silence it -- we COUNT it.** Commenting the panic out the
way USBPods does would remove the symptom and leave us blind to a race that may
or may not be corrupting the ISO-OUT stream, which is what bead pico-link-okx
is actually about. Instead the panic is replaced by an increment of
`pl_ep_double_arm_count[]` (defined in `firmware/src/usb_pump.c`, reported in
the `usb-pump-race:` line), indexed by endpoint number and direction. That
turns a fatal, unmeasurable event into a per-endpoint rate we can watch.

Beads: pico-link-06m (the panic), rides with pico-link-okx.

## 02-tinyusb-rp2040-inactive-xfer.patch

`rp2040_usb.c:315`, `hw_endpoint_xfer_continue()`. Stock TinyUSB calls
`panic("Can't continue xfer on inactive ep %02X")` when a buffer-status
completion arrives for an endpoint whose `ep->active` is 0 -- a stale
completion for an endpoint torn down mid-transfer, which is exactly what an
abrupt alt0 teardown produces. Same arm/complete disorder as patch 01, a
different fatal exit, same IRQ path, same hard lockup.

This is arguably the MORE likely door the historical teardown wedges went
through, because the double-arm site (01) fires at stream START while this one
fires at stream TEARDOWN.

Recovery is deliberately **not** "count and fall through":

  * falling through runs `_hw_endpoint_xfer_sync(ep)` against torn-down state;
  * returning `true` would hand the stack a bogus `dcd_event_xfer_complete()`
    carrying a stale `xferred_len`.

So it counts into `pl_ep_inactive_xfer_count[]` (a SEPARATE counter from patch
01's, so the two sites are distinguishable in one report line -- see
`usb-pump-inactxfer:`), releases the entry lock, and returns `false`. The
caller then neither completes nor resets the transfer, which is the correct
handling of "there is no transfer here". The `buf_status` bit is already
cleared by `dcd_rp2040.c:157` before the call, so returning cannot cause an
interrupt storm.

Verified in the linked binary: no `bl panic` remains in the function; the
counter increment and `return false` are compiled in at `0x20001078`.

## Deliberately NOT patched: dcd_rp2040.c:333 `panic("Unhandled IRQ")`

Assessed 2026-08-30 and left FATAL on purpose. `if (status ^ handled) panic(...)`
fires when a USB interrupt bit is asserted that the handler has no code for --
and therefore **does not clear**. Counting and returning would leave the
interrupt asserted, so the ISR would immediately re-enter: an unbounded
interrupt storm that starves the main loop and wedges the board anyway, but
silently and with no diagnosis. A loud death is strictly more diagnosable than
a silent livelock.

This is the structural difference from patches 01 and 02: in both of those the
interrupt source is already acknowledged (01 proceeds to write the buffer
control as normal; 02's `buf_status` bit is cleared by the caller), so
continuing is genuinely recoverable. Here it is not.

If this ever fires, the right change is a BREADCRUMB -- record `status ^ handled`
to the watchdog scratch registers before dying -- not a counter. Do not silence
it.
