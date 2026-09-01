#!/bin/bash
# Idempotent applier for the vendored pico-sdk patches (bead pico-link-06m).
#
# Uses exact-string replacement rather than patch(1) on purpose: it is
# idempotent, it can distinguish "already applied" from "unknown SDK version"
# (patch(1) reports both as a failed hunk), and it needs no fuzz. The .patch
# files in firmware/sdk-patches/ remain the human-readable source of truth.
set -euo pipefail
SDK="${PICO_SDK_PATH:-$HOME/.pico-sdk/sdk/2.1.1}"
python3 - "$SDK" <<'PY'
import sys, pathlib
sdk = pathlib.Path(sys.argv[1])
f = sdk / "lib/tinyusb/src/portable/raspberrypi/rp2040/rp2040_usb.c"
if not f.exists():
    sys.exit(f"FAIL: not found: {f}\nIs PICO_SDK_PATH correct? (got {sdk})")
src = f.read_text()

STOCK = '        panic("ep %02X was already available", ep->ep_addr);'
MARK  = "pl_ep_double_arm_count"
PATCHED = """        // pico-link (bead pico-link-06m): was
        //   panic("ep %02X was already available", ep->ep_addr);
        // panic() is a pico-sdk function -- NDEBUG does NOT elide it -- called
        // from an IRQ path and ending in __breakpoint(), i.e. a hard lockup
        // needing a physical BOOTSEL press. USBPods comments this out; we
        // COUNT it instead so the arm/complete race stays measurable.
        // Defined in firmware/src/usb_pump.c. See firmware/sdk-patches/README.md.
        { extern volatile unsigned int pl_ep_double_arm_count[32];
          pl_ep_double_arm_count[((ep->ep_addr & 0x0fu) << 1) | ((ep->ep_addr & 0x80u) ? 1u : 0u)]++; }"""

if MARK in src:
    print("ok: 01-tinyusb-rp2040-double-arm already applied")
elif STOCK in src:
    src = src.replace(STOCK, PATCHED, 1)
    f.write_text(src)
    print(f"APPLIED: 01-tinyusb-rp2040-double-arm -> {f}")
else:
    sys.exit("FAIL: rp2040_usb.c matches neither the stock nor the patched form.\n"
             "Unknown SDK version -- inspect it by hand before proceeding.")

# --- 02: stale completion on an inactive endpoint ------------------------
# hw_endpoint_xfer_continue() panics if a buffer-status completion arrives for
# an endpoint with ep->active == 0 -- exactly what an abrupt alt0 teardown
# produces. Same arm/complete disorder as 01, different fatal exit.
#
# Recovery is NOT "count and fall through": _hw_endpoint_xfer_sync() would run
# against torn-down state, and returning true would hand the stack a bogus
# dcd_event_xfer_complete() with a stale xferred_len. The correct action is to
# treat it as a stale completion -- release the lock taken on entry (the panic
# never had to, we do) and return false, so hw_handle_buff_status() neither
# completes nor resets the transfer. The buf_status bit is already cleared by
# the caller before this call (dcd_rp2040.c:157), so returning changes nothing
# about interrupt acknowledgement and cannot cause a storm.
STOCK2 = '  // Part way through a transfer\n  if (!ep->active) {\n    panic("Can\'t continue xfer on inactive ep %02X", ep->ep_addr);\n  }'
MARK2 = "pl_ep_inactive_xfer_count"
PATCHED2 = """  // Part way through a transfer
  if (!ep->active) {
    // pico-link (bead pico-link-06m): was
    //   panic("Can't continue xfer on inactive ep %02X", ep->ep_addr);
    // A stale completion for an endpoint torn down mid-transfer (abrupt alt0).
    // Counting and RETURNING is the correct recovery -- falling through would
    // sync against torn-down state, and returning true would deliver a bogus
    // xfer_complete with a stale xferred_len. Must release the lock taken on
    // entry above, which the panic never needed to. See sdk-patches/README.md.
    { extern volatile unsigned int pl_ep_inactive_xfer_count[32];
      pl_ep_inactive_xfer_count[((ep->ep_addr & 0x0fu) << 1) | ((ep->ep_addr & 0x80u) ? 1u : 0u)]++; }
    hw_endpoint_lock_update(ep, -1);
    return false;
  }"""

src = f.read_text()
if MARK2 in src:
    print("ok: 02-tinyusb-rp2040-inactive-xfer already applied")
elif STOCK2 in src:
    f.write_text(src.replace(STOCK2, PATCHED2, 1))
    print(f"APPLIED: 02-tinyusb-rp2040-inactive-xfer -> {f}")
else:
    sys.exit("FAIL: hw_endpoint_xfer_continue matches neither stock nor patched form.")

# --- 03: sample the ISO-OUT AVAIL bit in TRUE ISR context (bead pico-link-wbq) ---
# usbd.c's DCD_EVENT_SOF case in dcd_event_handler() runs in real ISR context,
# but only calls the app's tud_sof_cb() by RE-QUEUING an event that is later
# drained by tud_task() -- on this firmware, from inside the 0xC0
# pl_usb_pump_worker_irq. Sampling from there measures AVAIL up to ~1ms after
# the real start of frame, at a phase set by our own 1ms timer, not the bus.
# This patch calls pl_usb_sof_isr_sample() (firmware/src/usb_pump.c) directly
# from inside the ISR case, before any re-queuing happens.
f3 = sdk / "lib/tinyusb/src/device/usbd.c"
if not f3.exists():
    sys.exit(f"FAIL: not found: {f3}\nIs PICO_SDK_PATH correct? (got {sdk})")
src3 = f3.read_text()

STOCK3 = """    case DCD_EVENT_SOF:
      // SOF driver handler in ISR context
      for (uint8_t i = 0; i < TOTAL_DRIVER_COUNT; i++) {"""
MARK3 = "pl_usb_sof_isr_sample"
PATCHED3 = """    case DCD_EVENT_SOF:
      // SOF driver handler in ISR context
      // pico-link (bead pico-link-wbq): sample the raw ISO-OUT AVAIL bit
      // HERE, in true ISR context, before this event is (maybe) re-queued
      // for tud_task() below. Defined in firmware/src/usb_pump.c. See
      // firmware/sdk-patches/README.md.
      { extern void pl_usb_sof_isr_sample(uint32_t frame_count);
        pl_usb_sof_isr_sample(event->sof.frame_count); }
      for (uint8_t i = 0; i < TOTAL_DRIVER_COUNT; i++) {"""

if MARK3 in src3:
    print("ok: 03-tinyusb-usbd-sof-isr-sample already applied")
elif STOCK3 in src3:
    f3.write_text(src3.replace(STOCK3, PATCHED3, 1))
    print(f"APPLIED: 03-tinyusb-usbd-sof-isr-sample -> {f3}")
else:
    sys.exit("FAIL: usbd.c's DCD_EVENT_SOF case matches neither stock nor patched form.\n"
             "Unknown SDK version -- inspect it by hand before proceeding.")

# --- 04: ISO-OUT re-arm in TRUE ISR context (bead pico-link-2ap.6, EXPERIMENT) ---
# Backports the mechanism of upstream TinyUSB PR #3150 ("Move ISO transfers
# into xfer_isr", 0.19.0) without the surrounding 0.18->0.19 refactor: a new
# optional xfer_isr class-driver hook, called directly from dcd_event_handler
# (true USB ISR context) instead of only via tud_task()'s queued
# DCD_EVENT_XFER_COMPLETE. Gated end-to-end by PL_USB_ISO_XFER_ISR (default
# 0, see firmware/src/tusb_config.h) -- with the flag off this patch is a
# no-op: audiod_xfer_isr() always exists (wired into usbd's driver table and
# called from dcd_event_handler unconditionally, like upstream) but always
# returns false when the flag is off, deferring every completion to the
# existing audiod_xfer_cb() task-context path unchanged.
#
# 04a: usbd_pvt.h -- add the xfer_isr field to usbd_class_driver_t.
f4a = sdk / "lib/tinyusb/src/device/usbd_pvt.h"
if not f4a.exists():
    sys.exit(f"FAIL: not found: {f4a}\nIs PICO_SDK_PATH correct? (got {sdk})")
src4a = f4a.read_text()
STOCK4A = """  bool     (* xfer_cb          ) (uint8_t rhport, uint8_t ep_addr, xfer_result_t result, uint32_t xferred_bytes);
  void     (* sof              ) (uint8_t rhport, uint32_t frame_count); // optional
} usbd_class_driver_t;"""
MARK4A = "xfer_isr"
PATCHED4A = """  bool     (* xfer_cb          ) (uint8_t rhport, uint8_t ep_addr, xfer_result_t result, uint32_t xferred_bytes);
  // pico-link (bead pico-link-2ap.6): OPTIONAL. Called from true USB ISR
  // context (dcd_event_handler) before the completion would otherwise be
  // queued for xfer_cb() -- return true to mean "fully handled, do not also
  // queue", false to defer to the normal xfer_cb() task-context path. NULL
  // (the default -- omitted fields are zero-initialized) means every
  // completion on that driver's endpoints goes through xfer_cb() as before.
  // See firmware/sdk-patches/README.md.
  bool     (* xfer_isr         ) (uint8_t rhport, uint8_t ep_addr, xfer_result_t result, uint32_t xferred_bytes); // optional
  void     (* sof              ) (uint8_t rhport, uint32_t frame_count); // optional
} usbd_class_driver_t;"""

if MARK4A in src4a:
    print("ok: 04a-tinyusb-usbd-pvt-xfer-isr already applied")
elif STOCK4A in src4a:
    f4a.write_text(src4a.replace(STOCK4A, PATCHED4A, 1))
    print(f"APPLIED: 04a-tinyusb-usbd-pvt-xfer-isr -> {f4a}")
else:
    sys.exit("FAIL: usbd_pvt.h's usbd_class_driver_t matches neither stock nor patched form.")

# 04b: usbd.c -- wire audiod_xfer_isr into the AUDIO driver table entry, and
# dispatch to it from dcd_event_handler's DCD_EVENT_XFER_COMPLETE (a case
# that does not exist in stock 0.18.0 -- that event always falls to
# `default: send = true;` and is unconditionally queued).
src3_now = f3.read_text()
STOCK4B_TABLE = """        .control_xfer_cb  = audiod_control_xfer_cb,
        .xfer_cb          = audiod_xfer_cb,
        .sof              = audiod_sof_isr"""
MARK4B_TABLE = "audiod_xfer_isr"
PATCHED4B_TABLE = """        .control_xfer_cb  = audiod_control_xfer_cb,
        .xfer_cb          = audiod_xfer_cb,
        // pico-link (bead pico-link-2ap.6): always wired (like upstream PR
        // #3150) -- audiod_xfer_isr() itself is a no-op deferring to
        // xfer_cb() unless PL_USB_ISO_XFER_ISR is 1. See
        // firmware/src/tusb_config.h and sdk-patches/README.md.
        .xfer_isr         = audiod_xfer_isr,
        .sof              = audiod_sof_isr"""

STOCK4B_HANDLER = """    case DCD_EVENT_SETUP_RECEIVED:
      TU_ASSERT(_usbd_queued_setup > 0,);
      _usbd_queued_setup--;"""

if MARK4B_TABLE in src3_now:
    print("ok: 04b-tinyusb-usbd-xfer-isr-wiring already applied")
else:
    if STOCK4B_TABLE not in src3_now:
        sys.exit("FAIL: usbd.c's AUDIO driver table entry matches neither stock nor patched form.")
    src3_now = src3_now.replace(STOCK4B_TABLE, PATCHED4B_TABLE, 1)

    STOCK4B_XFER = """      case DCD_EVENT_XFER_COMPLETE: {
        // Invoke the class callback associated with the endpoint address
        uint8_t const ep_addr = event.xfer_complete.ep_addr;
        uint8_t const epnum = tu_edpt_number(ep_addr);
        uint8_t const ep_dir = tu_edpt_dir(ep_addr);

        TU_LOG_USBD("on EP %02X with %u bytes\\r\\n", ep_addr, (unsigned int) event.xfer_complete.len);

        _usbd_dev.ep_status[epnum][ep_dir].busy = 0;
        _usbd_dev.ep_status[epnum][ep_dir].claimed = 0;

        if (0 == epnum) {"""
    if STOCK4B_XFER not in src3_now:
        sys.exit("FAIL: usbd.c's tud_task_ext DCD_EVENT_XFER_COMPLETE case matches neither stock nor patched form (04b xfer).")
    # tud_task_ext's DCD_EVENT_XFER_COMPLETE case is left untouched -- it is
    # only reached when xfer_isr declined (or the driver has none), in which
    # case ep_status was already reverted to busy/claimed=1 below in
    # dcd_event_handler, so re-clearing it here is correct and matches the
    # unconditional-queue path's existing behaviour exactly.
    STOCK4B_HANDLER_FULL = """    case DCD_EVENT_SETUP_RECEIVED:
      _usbd_queued_setup++;
      send = true;
      break;

    default:
      send = true;
      break;
  }"""
    PATCHED4B_HANDLER = """    case DCD_EVENT_SETUP_RECEIVED:
      _usbd_queued_setup++;
      send = true;
      break;

    // pico-link (bead pico-link-2ap.6): if the owning class driver offers an
    // xfer_isr hook, give it first refusal on the completion HERE, in true
    // ISR context, instead of unconditionally queuing for tud_task(). NULL
    // (every driver but AUDIO, and AUDIO itself unless PL_USB_ISO_XFER_ISR
    // is 1) falls straight through to the unconditional queue below,
    // identical to stock. xfer_isr() returning true means it fully handled
    // the completion (including any re-arm) and this event must NOT also be
    // queued for xfer_cb; false defers to the existing task-context path.
    // ep_status housekeeping mirrors tud_task_ext()'s DCD_EVENT_XFER_COMPLETE
    // case so xfer_cb() sees identical state on whichever path ran.
    case DCD_EVENT_XFER_COMPLETE: {
      uint8_t const ep_addr = event->xfer_complete.ep_addr;
      uint8_t const epnum = tu_edpt_number(ep_addr);
      uint8_t const ep_dir = tu_edpt_dir(ep_addr);

      send = true;
      if (epnum > 0) {
        usbd_class_driver_t const* driver = get_driver(_usbd_dev.ep2drv[epnum][ep_dir]);
        if (driver && driver->xfer_isr) {
          _usbd_dev.ep_status[epnum][ep_dir].busy = 0;
          _usbd_dev.ep_status[epnum][ep_dir].claimed = 0;

          send = !driver->xfer_isr(event->rhport, ep_addr, (xfer_result_t) event->xfer_complete.result, event->xfer_complete.len);

          // xfer_isr() deferred to xfer_cb() -- revert busy/claimed status
          if (send) {
            _usbd_dev.ep_status[epnum][ep_dir].busy = 1;
            _usbd_dev.ep_status[epnum][ep_dir].claimed = 1;
          }
        }
      }
      break;
    }

    default:
      send = true;
      break;
  }"""
    if STOCK4B_HANDLER_FULL not in src3_now:
        sys.exit("FAIL: usbd.c's dcd_event_handler tail (SETUP_RECEIVED..default) matches neither stock nor patched form.")
    src3_now = src3_now.replace(STOCK4B_HANDLER_FULL, PATCHED4B_HANDLER, 1)
    f3.write_text(src3_now)
    print(f"APPLIED: 04b-tinyusb-usbd-xfer-isr-wiring -> {f3}")

# 04c: audio_device.h -- declare audiod_xfer_isr.
f4c = sdk / "lib/tinyusb/src/class/audio/audio_device.h"
if not f4c.exists():
    sys.exit(f"FAIL: not found: {f4c}\nIs PICO_SDK_PATH correct? (got {sdk})")
src4c = f4c.read_text()
STOCK4C = """bool     audiod_xfer_cb        (uint8_t rhport, uint8_t edpt_addr, xfer_result_t result, uint32_t xferred_bytes);
void     audiod_sof_isr        (uint8_t rhport, uint32_t frame_count);"""
MARK4C = "audiod_xfer_isr"
PATCHED4C = """bool     audiod_xfer_cb        (uint8_t rhport, uint8_t edpt_addr, xfer_result_t result, uint32_t xferred_bytes);
// pico-link (bead pico-link-2ap.6): see sdk-patches/README.md and usbd_pvt.h's
// xfer_isr doc comment.
bool     audiod_xfer_isr       (uint8_t rhport, uint8_t edpt_addr, xfer_result_t result, uint32_t xferred_bytes);
void     audiod_sof_isr        (uint8_t rhport, uint32_t frame_count);"""

if MARK4C in src4c:
    print("ok: 04c-tinyusb-audio-device-h-decl already applied")
elif STOCK4C in src4c:
    f4c.write_text(src4c.replace(STOCK4C, PATCHED4C, 1))
    print(f"APPLIED: 04c-tinyusb-audio-device-h-decl -> {f4c}")
else:
    sys.exit("FAIL: audio_device.h's audiod_xfer_cb/audiod_sof_isr declarations match neither stock nor patched form.")

# 04d: audio_device.c -- define audiod_xfer_isr. Real work only when
# PL_USB_ISO_XFER_ISR is 1 AND CFG_TUD_AUDIO_ENABLE_EP_OUT (always true in
# this project) AND !CFG_TUD_AUDIO_ENABLE_DECODING (this project's config --
# see tusb_config.h). Any other combination returns false unconditionally,
# deferring to audiod_xfer_cb() exactly as stock 0.18.0 always did.
f4d = sdk / "lib/tinyusb/src/class/audio/audio_device.c"
if not f4d.exists():
    sys.exit(f"FAIL: not found: {f4d}\nIs PICO_SDK_PATH correct? (got {sdk})")
src4d = f4d.read_text()
STOCK4D = "#endif//CFG_TUD_AUDIO_ENABLE_EP_OUT\n\n// The following functions are used in case CFG_TUD_AUDIO_ENABLE_DECODING"
MARK4D = "audiod_xfer_isr(uint8_t rhport"
PATCHED4D = '''// pico-link (bead pico-link-2ap.6): ISR-context ISO-OUT re-arm, gated by
// PL_USB_ISO_XFER_ISR (see tusb_config.h; default 0, a no-op). Called
// directly from dcd_event_handler in TRUE USB ISR context (rp2040_usb.c's
// USBCTRL_IRQ via patch 04b), NOT from tud_task() -- backports the
// mechanism of upstream TinyUSB PR #3150 without the surrounding 0.18->0.19
// refactor. Only claims our ISO-OUT endpoint (audio->ep_out); every other
// endpoint (ep_in, ep_fb, ep_int) and every other configuration this
// project does not use (CFG_TUD_AUDIO_ENABLE_DECODING, which is 0 here)
// returns false so usbd.c falls back to the unchanged audiod_xfer_cb()
// task-context path. Body replicates audiod_rx_done_cb()'s
// !CFG_TUD_AUDIO_ENABLE_DECODING / USE_LINEAR_BUFFER_RX branch (this
// project's config): copy the just-received packet out of the linear
// buffer into the OUT FIFO, THEN re-arm -- re-arming first would let
// TinyUSB overwrite lin_buf_out before this copy runs.
bool audiod_xfer_isr(uint8_t rhport, uint8_t ep_addr, xfer_result_t result, uint32_t xferred_bytes) {
  (void) result;
#if PL_USB_ISO_XFER_ISR && CFG_TUD_AUDIO_ENABLE_EP_OUT && !CFG_TUD_AUDIO_ENABLE_DECODING && USE_LINEAR_BUFFER_RX
  for (uint8_t func_id = 0; func_id < CFG_TUD_AUDIO; func_id++) {
    audiod_function_t *audio = &_audiod_fct[func_id];
    if (audio->ep_out != ep_addr) {
      continue;
    }

    uint8_t idxItf = 0;
    uint8_t const *dummy2;
    uint8_t idx_audio_fct = audiod_get_audio_fct_idx(audio);
    if (!audiod_get_AS_interface_index(audio->ep_out_as_intf_num, audio, &idxItf, &dummy2)) {
      return false;
    }

    // Weak callback: informs the app a packet arrived, before FIFO write.
    if (!tud_audio_rx_done_pre_read_cb(rhport, (uint16_t) xferred_bytes, idx_audio_fct, audio->ep_out, audio->alt_setting[idxItf])) {
      return false;
    }

    // Data currently is in the linear buffer -- copy into the EP OUT FIFO
    // BEFORE re-arming, since re-arming lets TinyUSB overwrite lin_buf_out.
    if (!tu_fifo_write_n(&audio->ep_out_ff, audio->lin_buf_out, (uint16_t) xferred_bytes)) {
      return false;
    }

    // Re-arm for next receive -- THE point of this patch: this call now
    // happens from true ISR context instead of tud_task().
    if (!usbd_edpt_xfer(rhport, audio->ep_out, audio->lin_buf_out, audio->ep_out_sz)) {
      return false;
    }

  #if CFG_TUD_AUDIO_ENABLE_FEEDBACK_EP
    if (audio->feedback.compute_method == AUDIO_FEEDBACK_METHOD_FIFO_COUNT) {
      audiod_fb_fifo_count_update(audio, tu_fifo_count(&audio->ep_out_ff));
    }
  #endif

    // Weak callback: informs the app decoding/FIFO-write completed.
    if (!tud_audio_rx_done_post_read_cb(rhport, (uint16_t) xferred_bytes, idx_audio_fct, audio->ep_out, audio->alt_setting[idxItf])) {
      return false;
    }

    return true;
  }
#else
  (void) rhport;
  (void) ep_addr;
  (void) xferred_bytes;
#endif
  return false;
}

#endif//CFG_TUD_AUDIO_ENABLE_EP_OUT

// The following functions are used in case CFG_TUD_AUDIO_ENABLE_DECODING'''

if MARK4D in src4d:
    print("ok: 04d-tinyusb-audio-device-c-isr already applied")
elif STOCK4D in src4d:
    f4d.write_text(src4d.replace(STOCK4D, PATCHED4D, 1))
    print(f"APPLIED: 04d-tinyusb-audio-device-c-isr -> {f4d}")
else:
    sys.exit("FAIL: audio_device.c's CFG_TUD_AUDIO_ENABLE_EP_OUT closing #endif matches neither stock nor patched form.")

# --- 05: reset transfer state BEFORE notifying the stack (upstream TinyUSB
# PR #3203, "fix rp2 iso transfer with new audio driver") ------------------
# HARD DEPENDENCY of patch 04, not optional and not gated by
# PL_USB_ISO_XFER_ISR: stock 0.18.0's hw_handle_buff_status() calls
# dcd_event_xfer_complete() (which patch 04b can now dispatch SYNCHRONOUSLY
# into audiod_xfer_isr() from true ISR context) BEFORE hw_endpoint_reset_transfer(ep)
# clears ep->active and the endpoint's transfer bookkeeping. With patch 04
# enabled, xfer_isr's re-arm (usbd_edpt_xfer) would then run against a
# still-active endpoint struct -- exactly the arm/complete disorder patches
# 01/02 exist to survive, except here it is self-inflicted by this patch set
# rather than a host/device race, and reordering removes the cause instead
# of just counting the symptom. Applied unconditionally (not gated by
# PL_USB_ISO_XFER_ISR) because it is a pure reordering with no observable
# effect on the existing queued xfer_cb path: dcd_event_xfer_complete()
# still queues (or, with 04b + the flag off, xfer_isr defers) exactly the
# same event with exactly the same xferred_len, just after hw state is
# already clean instead of before.
f5 = sdk / "lib/tinyusb/src/portable/raspberrypi/rp2040/dcd_rp2040.c"
if not f5.exists():
    sys.exit(f"FAIL: not found: {f5}\nIs PICO_SDK_PATH correct? (got {sdk})")
src5 = f5.read_text()

STOCK5 = """      bool done = hw_endpoint_xfer_continue(ep);
      if (done) {
        // Notify
        dcd_event_xfer_complete(0, ep->ep_addr, ep->xferred_len, XFER_RESULT_SUCCESS, true);
        hw_endpoint_reset_transfer(ep);
      }"""
MARK5 = "const uint16_t xferred_len = ep->xferred_len"
PATCHED5 = """      bool done = hw_endpoint_xfer_continue(ep);
      if (done) {
        // pico-link (bead pico-link-2ap.6, upstream TinyUSB PR #3203 "fix rp2
        // iso transfer with new audio driver"): reset transfer state BEFORE
        // notifying the stack, not after. dcd_event_xfer_complete() below can
        // now (patch 04) synchronously call into a class driver's xfer_isr()
        // from THIS ISR, which may re-arm the SAME endpoint via
        // usbd_edpt_xfer() before returning -- that re-arm must not race
        // hw_endpoint_reset_transfer() clearing ep->active and friends.
        // Capture xferred_len first since reset_transfer() clears it.
        const uint16_t xferred_len = ep->xferred_len;
        hw_endpoint_reset_transfer(ep);
        dcd_event_xfer_complete(0, ep->ep_addr, xferred_len, XFER_RESULT_SUCCESS, true);
      }"""

if MARK5 in src5:
    print("ok: 05-tinyusb-rp2040-reset-before-notify already applied")
elif STOCK5 in src5:
    f5.write_text(src5.replace(STOCK5, PATCHED5, 1))
    print(f"APPLIED: 05-tinyusb-rp2040-reset-before-notify -> {f5}")
else:
    sys.exit("FAIL: dcd_rp2040.c's hw_handle_buff_status done-branch matches neither stock nor patched form.")
PY
