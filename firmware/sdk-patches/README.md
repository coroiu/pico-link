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
