cmake_minimum_required(VERSION 3.13)

# Board must be set before the SDK import.
set(PICO_BOARD pimoroni_pico_plus2_w_rp2350 CACHE STRING "Board type")

include(pico_sdk_import.cmake)

project(pico_link_firmware C CXX ASM)

set(CMAKE_C_STANDARD 11)
set(CMAKE_CXX_STANDARD 17)

pico_sdk_init()

# --- Vendored SDK patch enforcement (bead pico-link-06m) -------------------
# The SDK lives OUTSIDE this repo, so a patch we depend on is untracked and
# machine-local. Silent divergence between "the SDK on one laptop" and "the
# SDK in version control" is exactly the trap this check exists to prevent, so
# this is a FATAL_ERROR and not a warning: an unpatched build boots, runs, and
# then hard-locks in an IRQ path on the first arm/complete race, which is
# indistinguishable from any other wedge and costs a physical BOOTSEL press.
# See firmware/sdk-patches/README.md.
set(_pl_rp2040_usb "${PICO_SDK_PATH}/lib/tinyusb/src/portable/raspberrypi/rp2040/rp2040_usb.c")
if(NOT EXISTS "${_pl_rp2040_usb}")
    message(FATAL_ERROR "Cannot find ${_pl_rp2040_usb} -- is PICO_SDK_PATH right?")
endif()
file(READ "${_pl_rp2040_usb}" _pl_rp2040_usb_src)
foreach(_pl_mark pl_ep_double_arm_count pl_ep_inactive_xfer_count)
string(FIND "${_pl_rp2040_usb_src}" "${_pl_mark}" _pl_patch_pos)
if(_pl_patch_pos EQUAL -1)
    message(FATAL_ERROR
        "pico-sdk is MISSING a vendored patch (marker ${_pl_mark}).\n"
        "  Run:  tools/apply-sdk-patches.sh\n"
        "Without it, TinyUSB panic()s from an IRQ path on an endpoint\n"
        "double-arm and the board hard-locks, needing a physical BOOTSEL press.\n"
        "See firmware/sdk-patches/README.md.")
endif()
endforeach()
message(STATUS "pico-link: vendored SDK patches 01+02 (rp2040_usb.c) are applied")

# Bead pico-link-wbq: patch 03 (usbd.c) -- samples the ISO-OUT AVAIL bit in
# true ISR context instead of the worker-dispatched tud_sof_cb, which
# measured AVAIL at the wrong phase (see firmware/sdk-patches/README.md).
# Missing this patch does not crash the build or the board -- it silently
# reverts pico-link-2ap's hw_unavail discriminator to the mis-phased
# reading, which is exactly the ambiguity this bead exists to remove, so it
# is still a FATAL_ERROR rather than a warning.
set(_pl_usbd "${PICO_SDK_PATH}/lib/tinyusb/src/device/usbd.c")
if(NOT EXISTS "${_pl_usbd}")
    message(FATAL_ERROR "Cannot find ${_pl_usbd} -- is PICO_SDK_PATH right?")
endif()
file(READ "${_pl_usbd}" _pl_usbd_src)
string(FIND "${_pl_usbd_src}" "pl_usb_sof_isr_sample" _pl_patch3_pos)
if(_pl_patch3_pos EQUAL -1)
    message(FATAL_ERROR
        "pico-sdk is MISSING a vendored patch (marker pl_usb_sof_isr_sample).\n"
        "  Run:  tools/apply-sdk-patches.sh\n"
        "Without it, the pico-link-2ap ISO-OUT discriminator is sampled from\n"
        "worker context (wrong phase), not true SOF ISR context.\n"
        "See firmware/sdk-patches/README.md.")
endif()
message(STATUS "pico-link: vendored SDK patch 03 (usbd.c) is applied")

# Bead pico-link-2ap.6 (EXPERIMENT): patches 04 (usbd_pvt.h/usbd.c/
# audio_device.h/audio_device.c) and 05 (dcd_rp2040.c) -- the ISO-OUT
# xfer_isr backport and its required RP2040 reorder fix (upstream PR
# #3203). Missing either does not crash the build -- PL_USB_ISO_XFER_ISR
# defaults to 0 (firmware/src/tusb_config.h) so audiod_xfer_isr() is a
# no-op either way -- but a silent revert here would mean this bead's A/B
# test was never actually built, so it is FATAL_ERROR like 01-03.
set(_pl_dcd_rp2040 "${PICO_SDK_PATH}/lib/tinyusb/src/portable/raspberrypi/rp2040/dcd_rp2040.c")
if(NOT EXISTS "${_pl_dcd_rp2040}")
    message(FATAL_ERROR "Cannot find ${_pl_dcd_rp2040} -- is PICO_SDK_PATH right?")
endif()
file(READ "${_pl_usbd}" _pl_usbd_src2)
file(READ "${_pl_dcd_rp2040}" _pl_dcd_rp2040_src)
foreach(_pl_mark4 "audiod_xfer_isr")
string(FIND "${_pl_usbd_src2}" "${_pl_mark4}" _pl_patch4_pos)
if(_pl_patch4_pos EQUAL -1)
    message(FATAL_ERROR
        "pico-sdk is MISSING a vendored patch (marker ${_pl_mark4} in usbd.c).\n"
        "  Run:  tools/apply-sdk-patches.sh\n"
        "See firmware/sdk-patches/README.md (patch 04).")
endif()
endforeach()
string(FIND "${_pl_dcd_rp2040_src}" "const uint16_t xferred_len = ep->xferred_len" _pl_patch5_pos)
if(_pl_patch5_pos EQUAL -1)
    message(FATAL_ERROR
        "pico-sdk is MISSING a vendored patch (marker in dcd_rp2040.c).\n"
        "  Run:  tools/apply-sdk-patches.sh\n"
        "See firmware/sdk-patches/README.md (patch 05).")
endif()
message(STATUS "pico-link: vendored SDK patches 04+05 (ISO xfer_isr backport) are applied")

# Bead pico-link-9ziq (F2): patch 06 (audio_device.c) -- counts bytes
# tu_fifo_write_n() silently drops in audiod_xfer_isr (a partial write, FIFO
# had room for some but not all of xferred_bytes, makes the old
# `if (!tu_fifo_write_n(...))` test alone miss it). Measurement only, but a
# silent revert here would make this bead's usb_lost_bytes/shortfall
# counters read 0 even when loss is real -- see zero-readings-are-not-passes
# in project memory -- so FATAL_ERROR like the others.
set(_pl_audio_device_c "${PICO_SDK_PATH}/lib/tinyusb/src/class/audio/audio_device.c")
if(NOT EXISTS "${_pl_audio_device_c}")
    message(FATAL_ERROR "Cannot find ${_pl_audio_device_c} -- is PICO_SDK_PATH right?")
endif()
file(READ "${_pl_audio_device_c}" _pl_audio_device_c_src)
string(FIND "${_pl_audio_device_c_src}" "pl_usb_fifo_shortfall_bytes" _pl_patch6_pos)
if(_pl_patch6_pos EQUAL -1)
    message(FATAL_ERROR
        "pico-sdk is MISSING a vendored patch (marker pl_usb_fifo_shortfall_bytes in audio_device.c).\n"
        "  Run:  tools/apply-sdk-patches.sh\n"
        "See firmware/sdk-patches/README.md (patch 06).")
endif()
message(STATUS "pico-link: vendored SDK patch 06 (audio_device.c FIFO shortfall counter) is applied")
# ---------------------------------------------------------------------------

# Bead pico-link-cd3: debug-only NavIntent injection over the CDC console,
# so an agent can drive scan/connect without a human pressing the d-pad.
# OFF by default -- must never be on in a shipping build. Enable with
# `cmake -B build -DPL_DEBUG_REMOTE=ON`. See src/debug_remote.h.
option(PL_DEBUG_REMOTE "Enable debug NavIntent injection over the CDC console (unattended hardware testing only)" OFF)

# Bead pico-link-2ap.6 (EXPERIMENT): moves ISO-OUT re-arm from tud_task()
# context into true USB ISR context (sdk-patches/04+05, gated no-op when
# OFF). Default OFF -- this A/B tests a candidate mechanism for the
# pico-link-2ap halving, not a shipping behaviour change. See
# firmware/src/tusb_config.h and firmware/sdk-patches/README.md.
option(PL_USB_ISO_XFER_ISR "Re-arm ISO-OUT from true USB ISR context instead of tud_task() -- the pico-link-2ap halving fix, ON by default since bead pico-link-2ap.6" ON)

# Bead pico-link-ufh: software+hardware watchdog supervisor. Armed by
# default in EVERY build, including debug -- a watchdog only armed in
# release is a watchdog whose behaviour is never exercised before it ships
# (see .planning/design/2026-08-30-watchdog.md, "Debug vs release").
# Deliberately NOT gated on PL_DEBUG_REMOTE -- that is a flash-tooling flag
# and coupling watchdog arming to it would be exactly the accidental
# coupling this seam should not acquire.
option(PL_WATCHDOG "Arm the hardware watchdog + software supervisor" ON)

# Bead pico-link-ukk: reusable diagnostics gated in main.c behind
# PL_DIAG_* #ifdefs. All previously documented as "pass -DPL_DIAG_FOO via
# CMAKE_C_FLAGS/CMAKE_CXX_FLAGS" -- that recipe is broken, because passing
# -DCMAKE_C_FLAGS=... bare on the cmake command line REPLACES the pico-sdk
# toolchain file's seeded -mcpu/-march flags instead of appending to them,
# so the build either fails outright or silently compiles for the wrong
# architecture. Real option()s + target_compile_definitions below instead,
# same pattern as PL_WATCHDOG/PL_WDT_OBSERVE_ONLY above -- enable with e.g.
# `cmake -B build -DPL_DIAG_LDAC_BENCH=ON`, never via CMAKE_C_FLAGS.
option(PL_DIAG_COLOR_TEST "Halt-forever colour exposure test (pico-link-14l)" OFF)
option(PL_DIAG_MADCTL_TEST "Halt-forever MADCTL rotation/mirroring test (pico-link-zzq)" OFF)
option(PL_DIAG_LDAC_BENCH "Run the LDAC encoder CPU/heap benchmark at boot, then continue (pico-link-cz0.5.4)" OFF)
option(PL_DIAG_SKIP_BT "Skip cyw43_arch_init/pl_bt_init at boot (BT-free diagnostic builds)" OFF)
option(PL_DIAG_WINDOW_TEST "Halt-forever CASET/RASET window transform test (pico-link-7h5.1, M-1)" OFF)

# --- The Rust FFI seam (M1b, C-first pivot) ---
#
# `ui-ffi` is a `no_std` + `alloc` Rust staticlib (../ui-ffi) depending on
# the platform-free `pico-link-core` render/navigation crate (../core).
# Built here via `add_custom_command` + `add_library(... IMPORTED)` rather
# than Corrosion: zero new CMake dependencies, ~30 legible lines, failure
# modes are ours to read directly. `firmware/include/pico_link_ui.h` is
# cbindgen-generated from `ui-ffi/src/lib.rs` as its own build step, so it
# can never hand-drift from the real FFI surface -- a hand-written header is
# the most common source of silent FFI ABI bugs.
set(PICO_LINK_WORKSPACE_ROOT ${CMAKE_SOURCE_DIR}/..)
set(UI_FFI_DIR ${PICO_LINK_WORKSPACE_ROOT}/ui-ffi)
# NOT the "hf" (hard-float, VFP-register-argument ABI) variant: pico-sdk
# builds this board's C code softfp (integer-register float arguments,
# hardware FPU instructions still usable for the arithmetic itself -- see
# its `thumb/v8-m.main+fp/softfp` object path). Rust's plain
# `thumbv8m.main-none-eabi` target is soft-float (no hardware FPU
# instructions emitted at all), which is a strict subset of softfp's
# calling convention -- integer registers for float args either way, so the
# two link cleanly. Mixing eabihf Rust objects with this softfp C build
# fails link with "uses VFP register arguments, pico_link.elf does not".
set(UI_FFI_TARGET_TRIPLE thumbv8m.main-none-eabi)
set(UI_FFI_LIB ${PICO_LINK_WORKSPACE_ROOT}/target/${UI_FFI_TARGET_TRIPLE}/release/libui_ffi.a)
set(PICO_LINK_UI_HEADER_DIR ${CMAKE_SOURCE_DIR}/include)
set(PICO_LINK_UI_HEADER ${PICO_LINK_UI_HEADER_DIR}/pico_link_ui.h)

file(MAKE_DIRECTORY ${PICO_LINK_UI_HEADER_DIR})

file(GLOB_RECURSE UI_FFI_RUST_SOURCES CONFIGURE_DEPENDS
    ${UI_FFI_DIR}/src/*.rs
    ${PICO_LINK_WORKSPACE_ROOT}/core/src/*.rs
)

# `cargo build -p ui-ffi` (not a plain `cargo build`): this workspace's
# `default-members` deliberately excludes `ui-ffi` (see ../Cargo.toml) so a
# host-native `cargo build`/`cargo test` never touches this no_std cross
# target -- this is the one place that opts back in explicitly.
add_custom_command(
    OUTPUT ${UI_FFI_LIB}
    COMMAND rustup target add ${UI_FFI_TARGET_TRIPLE}
    COMMAND cargo build -p ui-ffi --release --target ${UI_FFI_TARGET_TRIPLE}
    WORKING_DIRECTORY ${PICO_LINK_WORKSPACE_ROOT}
    DEPENDS ${UI_FFI_RUST_SOURCES} ${UI_FFI_DIR}/Cargo.toml ${PICO_LINK_WORKSPACE_ROOT}/core/Cargo.toml ${PICO_LINK_WORKSPACE_ROOT}/Cargo.toml
    COMMENT "cargo build -p ui-ffi --release --target ${UI_FFI_TARGET_TRIPLE}"
    VERBATIM
)
add_custom_target(ui_ffi_build DEPENDS ${UI_FFI_LIB})

add_custom_command(
    OUTPUT ${PICO_LINK_UI_HEADER}
    COMMAND cbindgen --config cbindgen.toml --crate ui-ffi --output ${PICO_LINK_UI_HEADER}
    WORKING_DIRECTORY ${UI_FFI_DIR}
    DEPENDS ${UI_FFI_DIR}/src/lib.rs ${UI_FFI_DIR}/cbindgen.toml
    COMMENT "cbindgen -> firmware/include/pico_link_ui.h"
    VERBATIM
)
add_custom_target(pico_link_ui_header DEPENDS ${PICO_LINK_UI_HEADER})

add_library(ui_ffi STATIC IMPORTED GLOBAL)
set_target_properties(ui_ffi PROPERTIES IMPORTED_LOCATION ${UI_FFI_LIB})
add_dependencies(ui_ffi ui_ffi_build)

# Bead pico-link-cz0.5.4 (LDAC L0): Sony/libldac (Apache-2.0), encoder only,
# vendored under firmware/vendor/libldac -- see that directory's
# PROVENANCE.md. Linked alongside pico_btstack_sbc_encoder below. NOT wired
# into the A2DP media path yet (design doc .planning/design/2026-08-30-ldac.md
# Q1 -- that is L3, pico-link-cz0.5.6).
add_subdirectory(vendor/libldac)

# Bead pico-link-nli.9 (P2, Ada's design on nli.8): force libldac's .text/
# .rodata out of flash (XIP) into RAM via the .time_critical.* section
# convention pico-sdk's own linker script already honors
# (memmap_default.ld places *(.time_critical*) in RAM .data). Ada's design:
# core1's LDAC encode stalls on XIP cache evictions caused by core0 running
# concurrently on the same 16KB shared XIP cache libldac's 28.4KB .text+
# .rodata is 1.8x the size of; objcopy renaming our own build OUTPUT is the
# least invasive seam available -- pico-sdk 2.1.1 has no linker-fragment
# hook, and vendor/libldac must not be forked (see
# vendor/libldac/PROVENANCE.md). Runs UNCONDITIONALLY, not gated on
# PL_ENCODER_ON_CORE1: it helps core0's own encode path too (fewer XIP
# evictions either way) and keeps one memory layout for both builds.
#
# Relies on libldac being built WITHOUT -ffunction-sections, i.e. exactly
# one plain .text and one plain .rodata section per object file (verified
# against the linker map -- see nli.8's design comment). If that assumption
# ever breaks (upstream changes, a toolchain flag change), --rename-section
# silently does nothing to the extra per-function sections it doesn't name,
# so those functions stay in flash -- hence the post-link check below on
# pico_link itself, which FAILS THE BUILD (not just a warning) if that
# happens instead of silently reverting to the slow path.
#
# NOTE: this is an OUTPUT-based custom command, not a TARGET POST_BUILD one
# -- CMake requires add_custom_command(TARGET ...) to run in the SAME
# directory scope the target was created in, and pl_ldac_enc is created in
# vendor/libldac/CMakeLists.txt (must not be edited, see above). An
# OUTPUT-based command plus a stamp file and add_dependencies() below has
# no such restriction and orders correctly against the archive build.
set(PL_LDAC_SRAM_STAMP ${CMAKE_CURRENT_BINARY_DIR}/pl_ldac_enc_sram.stamp)
add_custom_command(
    OUTPUT ${PL_LDAC_SRAM_STAMP}
    COMMAND ${CMAKE_OBJCOPY}
        --rename-section .text=.time_critical.pl_ldac_text
        --rename-section .rodata=.time_critical.pl_ldac_rodata
        $<TARGET_FILE:pl_ldac_enc>
    COMMAND ${CMAKE_COMMAND} -E touch ${PL_LDAC_SRAM_STAMP}
    DEPENDS pl_ldac_enc
    COMMENT "pico-link-nli.9: renaming libldac .text/.rodata into .time_critical (SRAM-resident)"
    VERBATIM
)
add_custom_target(pl_ldac_enc_sram ALL DEPENDS ${PL_LDAC_SRAM_STAMP})

add_executable(pico_link
    src/main.c
    src/st7789.c
    src/input.c
    src/bt.c
    src/a2dp.c
    src/codec_table.c
    src/codec_sbc.c
    src/codec_ldac.c
    src/panic_recorder.c
    src/usb_descriptors.c
    src/usb_audio.c
    src/usb_pump.c
    src/pl_log_ring.c
    src/pl_prio.c
    src/pl_loop_prof.c
    src/pcm_ring.c
    # Bead pico-link-9eq2.3.2, design .planning/design/2026-09-07-audio-
    # fault-model.md: the audio fault evaluator -- see fault.h's module doc.
    src/fault.c
    src/usb_reset.c
    src/watchdog_sup.c
    src/persist.c
    # Bead pico-link-nli.2 (G1): the multicore flash-write lockout -- see
    # flash_lockout.c's module doc. Compiled unconditionally: it must be
    # correct on every build, single-core or not, since it overrides
    # pico-sdk's weak get_flash_safety_helper() for ALL flash writes,
    # including BTstack's own link-key saves.
    src/flash_lockout.c
    # Bead pico-link-47z.2 (media keys T2): the HID consumer-control
    # ring/drain/safety-timeout. Compiled unconditionally (unlike
    # debug_remote.c below) because T3's AVRCP handler is a second producer
    # into this module's ring, independent of PL_DEBUG_REMOTE.
    src/media_keys.c
    # Bead pico-link-4v2.2 (VT2 of the volume-sync epic pico-link-4v2): the
    # canonical volume state, loop-breaking rule and circuit breaker -- see
    # volume.h's module doc. Compiled unconditionally (like media_keys.c
    # above), not gated on PL_DEBUG_REMOTE: T3/T4's real USB/AVRCP
    # producers will call into it independent of the debug console.
    src/volume.c
    # Bead pico-link-cz0.5.4 (LDAC L0): temporary on-target benchmark, see
    # ldac_bench.h's module doc. Always compiled in (like codec_sbc.c);
    # pl_ldac_bench_run() is only ever CALLED under PL_DIAG_LDAC_BENCH
    # (main.c), so it costs flash but changes no runtime behaviour when the
    # flag is off.
    src/ldac_bench.c
)

if(PL_DEBUG_REMOTE)
    target_sources(pico_link PRIVATE src/debug_remote.c)
endif()

if(PL_WATCHDOG)
    # Rollout step 6 (design doc): observe-only first flash. pl_wdt_service()
    # feeds unconditionally and only REPORTS staleness -- never trips a
    # reset -- until step 7 explicitly undefines this after a hardware
    # session has turned the deadline table from judgement into
    # measurement. Flip by removing this option or building with
    # -DPL_WDT_OBSERVE_ONLY=OFF once step 7 is ready (see watchdog_sup.c).
    option(PL_WDT_OBSERVE_ONLY "Watchdog supervisor reports staleness but never trips a reset" ON)
    target_compile_definitions(pico_link PRIVATE PL_WATCHDOG=1)
    if(PL_WDT_OBSERVE_ONLY)
        target_compile_definitions(pico_link PRIVATE PL_WDT_OBSERVE_ONLY=1)
    endif()
endif()

# Bead pico-link-nli.4 (G3, epic pico-link-nli, "LDAC encoder on core1"):
# moves the LDAC encoder + pcm-ring fill loop to core1. Default ON as of
# 2026-09-23 (pico-link-nli.10) -- this is the bead's whole point -- kept a
# real option per the epic's own design doc sec 8 ("Behind a compile-time
# PL_ENCODER_ON_CORE1 so a bad build is one flag from reverting"). It was
# OFF by default from its introduction (pico-link-gmy) until 2026-09-23 as a
# P0 mitigation while the root cause of core1's render-cost regression was
# unknown; that root cause is now understood (nli.8/nli.9: libldac's .text
# fighting core0's XIP cache -- see the objcopy-rename comment above) and
# fixed on the core1 side (quzf: core1-safe resync trim; 9ziq: USB drain
# moved outside pl_usb_mutex; rzqd: resume-edge credit reset + edge-counted
# underruns), so ON is now the default. OFF remains a real, demoted A/B
# fallback (-DPL_ENCODER_ON_CORE1=OFF) -- not deleted, per Ada's quzf
# design -- and reproduces the exact pre-G3 single-core behaviour:
# pl_a2dp_seal_head() calls
# a2dp_source_stream_endpoint_request_can_send_now() directly again, the
# media timer's credit-clock/dwell-budget/fill()/level-push call sites are
# all restored, and core1 is never launched (pl_a2dp_launch_core1 does not
# even exist in that build). pico_multicore is already linked below
# unconditionally (G1, pico-link-nli.2, for the flash lockout) regardless of
# this flag.
option(PL_ENCODER_ON_CORE1 "Run the LDAC encoder on core1 instead of core0's media timer" ON)
if(PL_ENCODER_ON_CORE1)
    target_compile_definitions(pico_link PRIVATE PL_ENCODER_ON_CORE1=1)
endif()

add_dependencies(pico_link pico_link_ui_header)
target_include_directories(pico_link PRIVATE ${PICO_LINK_UI_HEADER_DIR})

# btstack_config.h lives alongside bt.c/main.c -- BTstack's own headers
# #include "btstack_config.h" unqualified and expect it on the include
# path, same as any other project-supplied BTstack config.
target_include_directories(pico_link PRIVATE ${CMAKE_CURRENT_LIST_DIR}/src)

pico_set_program_name(pico_link "pico_link")
pico_set_program_version(pico_link "0.1.0")

# USB CDC stdio: gives us a console AND lets `picotool reboot -f -u` reset a
# running board into BOOTSEL with no button press, for free.
pico_enable_stdio_usb(pico_link 1)
pico_enable_stdio_uart(pico_link 0)
target_compile_definitions(pico_link PRIVATE
    # Bead pico-link-l60: pico-sdk's own reset_interface.c is now compiled
    # down to nothing (this flag guards its ENTIRE #if block, including the
    # usbd_app_driver_get_cb definition) -- this project owns the reset
    # class driver directly in src/usb_reset.c instead, unconditionally,
    # because reset_interface.c's BOOTSEL/FLASH branches were silently
    # compiled OUT by this project's own tusb_config.h replacing
    # pico_stdio_usb's (see usb_reset.h's module doc for the full
    # mechanism). Setting this to 0 also frees the strong
    # usbd_app_driver_get_cb symbol for usb_reset.c to provide -- two
    # definitions would be a link error.
    PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE=0
    # pico-sdk's stdio_usb gates ALL console output on DTR by default
    # (stdio_usb_connected() checks tud_cdc_connected(), which tracks DTR) --
    # stdio_usb_out_chars() silently drops everything while that's false.
    # tools/usb-console/cdc_reader.py, this project's standard capture path,
    # cannot assert DTR on macOS (claiming the CDC-Communication interface
    # fails with EACCES -- see bead pico-link-4mc), so without this flag a
    # perfectly healthy board is indistinguishable from a hung one: the
    # console just never speaks. This makes stdio_usb_connected() return
    # tud_ready() instead (device configured, no DTR needed) -- see
    # pico_stdio_usb/stdio_usb.c. Root-caused 2026-08-28 after a control-test
    # flash of the M1b baseline reproduced the exact same "zero CDC bytes"
    # symptom as M2 despite the panel visibly rendering, which is what
    # exposed this as a tooling gap rather than a firmware hang.
    PICO_STDIO_USB_CONNECTION_WITHOUT_DTR=1
    # Bead pico-link-tfj: pico_stdio_usb's own stdio_usb_out_chars() spins
    # for up to PICO_STDIO_USB_STDOUT_TIMEOUT_US (default 500000, i.e.
    # 500ms) waiting for tud_cdc_write_available() before giving up on one
    # printf/pl_log call. Combined with PICO_STDIO_USB_CONNECTION_WITHOUT_DTR
    # above (which makes stdio_usb_connected() true even with no reader
    # attached), a listening test with no capture running could otherwise
    # block the pl_usb_mutex -- and therefore the 0xC0 worker's
    # mutex_try_enter -- for half a second on every single log line. 2ms is
    # enough slack for a real reader without holding the mutex anywhere
    # near the worker's 1ms re-arm deadline.
    PICO_STDIO_USB_STDOUT_TIMEOUT_US=2000
    # M2: Bluetooth only, no Wi-Fi -- without this, cyw43.h unconditionally
    # pulls in lwip/netif.h (a build dependency we have no use for and
    # haven't wired up). Matches USBPods' own working reference.
    CYW43_LWIP=0
    # M2 (bd pico-link-cz0.3): whichever core loads the cyw43 Bluetooth
    # firmware needs at least 48KB of stack -- earned expensively on real
    # hardware in an earlier session, where an 8-16KB core0 stack
    # overflowed during PIO/SPI init with a badly misleading symptom (a
    # diagnostic store on the FIRST line of a task appeared never to
    # execute, depending on what code came LATER). Set well above that
    # floor. Core1 is STILL never launched as of this bead (pico-link-nli.2,
    # G1 of the "LDAC encoder on core1" epic) -- only the multicore flash
    # lockout (flash_lockout.c) is wired up, ready for G3
    # (pico-link-nli.4) to actually call multicore_launch_core1().
    PICO_STACK_SIZE=0xC000
    # Explicit rather than the SDK default (0x800/2KB) so G3 does not
    # inherit a silently-too-small core1 stack -- pico_multicore is now
    # linked (below) purely for multicore_lockout; nothing runs on core1
    # yet. core1's stack (`.stack1_dummy`) is placed in the SCRATCH_X SRAM
    # bank by the default linker script, which is a FIXED 4KB region --
    # 0x2000 (8KB) overflows it at link time (measured on this bead).
    # 0x1000 (4KB) is the practical ceiling without a linker-script change;
    # generous for the encoder loop G3 will add (libldac's working set +
    # local buffers) relative to core0's cyw43-driven 0xC000 above, since
    # core1 never touches BTstack/cyw43.
    PICO_CORE1_STACK_SIZE=0x1000
    # bd pico-link-gap, DEV AFFORDANCE (see panic_recorder.h's ship-marker
    # note): routes pico-sdk's own panic()/assert path (the one that
    # produced the bare "*** PANIC ***" banner with zero context on
    # 2026-08-28) through our recorder instead of the stock
    # puts+vprintf+_exit(1). pico_platform_panic's CMakeLists.txt adds
    # panic.c as an INTERFACE source, i.e. it compiles as part of THIS
    # target and sees this definition -- confirmed by reading
    # pico_platform_panic/CMakeLists.txt rather than assumed.
    PICO_PANIC_FUNCTION=pl_panic_c_hook
)

# Bead pico-link-cz0.6 (M5 persistence): shift the BTstack flash-bank store
# DOWN by one extra 4KB sector from pico-sdk's own default
# (PICO_FLASH_BANK_STORAGE_OFFSET = PICO_FLASH_SIZE_BYTES -
# PICO_FLASH_BANK_TOTAL_SIZE, i.e. the literal last 8KB of flash --
# pico/btstack_flash_bank.h). Measured directly from this board's own
# pico_link.uf2 (`python3 -c` decoding its UF2 blocks): pico-sdk 2.x's
# `pico_add_extra_outputs` appends a 256-byte "absolute" family/metadata
# block (UF2 family id 0xe48bff57) at the FIXED address 0x10FFFF00 -- the
# very last 256 bytes of this 16MB part's flash -- on every build, and (per
# the RP2350 UF2/PICOBOOT convention this implements) that block is written
# to physical flash by the mass-storage BOOTSEL drag-and-drop path on every
# reflash, not just interpreted in transit. The stock last-8KB TLV window
# ([FLASH_SIZE-8KB, FLASH_SIZE)) has that block sitting INSIDE its second
# (higher-address) bank, so a reflash could silently clobber the tail of our
# own store, or a TLV write could clobber picotool/bootrom's own metadata --
# exactly the "reflash requirement not met" failure mode this bead's
# acceptance criterion warns against. Moving the whole 8KB window one more
# sector earlier leaves that block's sector entirely untouched by either
# side. PICO_FLASH_BANK_TOTAL_SIZE is left at its default (2 sectors, 8KB) --
# only the offset moves.
math(EXPR PL_FLASH_BANK_STORAGE_OFFSET "16 * 1024 * 1024 - 3 * 4096")
target_compile_definitions(pico_link PRIVATE
    PICO_FLASH_BANK_STORAGE_OFFSET=${PL_FLASH_BANK_STORAGE_OFFSET}u
)

if(PL_DEBUG_REMOTE)
    # Gates main.c's call into debug_remote.c (only compiled in above when
    # PL_DEBUG_REMOTE is ON) -- see src/debug_remote.h's module doc.
    target_compile_definitions(pico_link PRIVATE PL_DEBUG_REMOTE=1)
endif()

if(PL_USB_ISO_XFER_ISR)
    # Bead pico-link-2ap.6: overrides tusb_config.h's #ifndef-guarded
    # default (0). pico_link compiles TinyUSB's sources itself (see the
    # build output -- audio_device.c/usbd.c land under
    # CMakeFiles/pico_link.dir/..., not a separate library target), so this
    # -D reaches src/tusb_config.h for every translation unit that includes
    # it, tinyusb's own included.
    target_compile_definitions(pico_link PRIVATE PL_USB_ISO_XFER_ISR=1)
endif()

# Bead pico-link-ukk: PL_DIAG_* reusable diagnostics -- see the option()
# block near the top of this file for why these are real CMake options
# rather than CMAKE_C_FLAGS defines.
if(PL_DIAG_COLOR_TEST)
    target_compile_definitions(pico_link PRIVATE PL_DIAG_COLOR_TEST=1)
endif()
if(PL_DIAG_MADCTL_TEST)
    target_compile_definitions(pico_link PRIVATE PL_DIAG_MADCTL_TEST=1)
endif()
if(PL_DIAG_LDAC_BENCH)
    target_compile_definitions(pico_link PRIVATE PL_DIAG_LDAC_BENCH=1)
endif()
if(PL_DIAG_SKIP_BT)
    target_compile_definitions(pico_link PRIVATE PL_DIAG_SKIP_BT=1)
endif()
if(PL_DIAG_WINDOW_TEST)
    target_compile_definitions(pico_link PRIVATE PL_DIAG_WINDOW_TEST=1)
endif()

# Bead pico-link-vxc: the dirty-gate backstop. `pl_ui_dirty()` lets main.c
# skip a render+blit when nothing changed, but the dirty flag cannot see
# panel-side corruption (an SPI fault, a truncated DMA, a brownout) -- the
# unconditional blit it replaces has been silently self-healing that within
# 33ms for weeks. This bounds any such miss to at most this many
# milliseconds instead, forcing a repaint on a timer regardless of the
# dirty flag. 0 disables the backstop entirely (never force-repaint) -- see
# .planning/design/2026-09-02-dirty-gate-across-the-ffi-seam.md §5 for why
# 1000ms and why this lives in C, not core (it defends the ST7789's GRAM,
# a platform artifact core cannot know about and the emulator does not
# have).
# Not a boolean `option()` like the flags above -- a tunable period, so a
# plain cache variable (settable via `-DPL_FORCED_REPAINT_MS=0` the same
# way the booleans above take `-DPL_x=ON/OFF`) rather than `option()`,
# which CMake reserves for ON/OFF switches.
set(PL_FORCED_REPAINT_MS "1000" CACHE STRING "Repaint the panel at least this often, 0 = never")
target_compile_definitions(pico_link PRIVATE PL_FORCED_REPAINT_MS=${PL_FORCED_REPAINT_MS})

target_link_libraries(pico_link
    pico_stdlib
    hardware_spi
    hardware_dma
    hardware_gpio
    hardware_pwm
    hardware_clocks
    hardware_watchdog
    ui_ffi
    # Bead pico-link-nli.2 (G1): needed ONLY for multicore_lockout_* (see
    # flash_lockout.c) and to reserve core1's stack (PICO_CORE1_STACK_SIZE
    # above). Linking this flips on
    # PICO_FLASH_SAFE_EXECUTE_PICO_SUPPORT_MULTICORE_LOCKOUT for pico-sdk's
    # flash_safe_execute() tree-wide, which is exactly why flash_lockout.c
    # overrides get_flash_safety_helper() rather than relying on the SDK
    # default -- see that file's module doc. multicore_launch_core1() is
    # NOT called anywhere yet; that is G3 (pico-link-nli.4).
    pico_multicore
    # M2: the ready-made HCI transport this C-first pivot exists to use
    # (see bt.c's module doc). pico_btstack_cyw43 pulls in
    # pico_btstack_hci_transport_cyw43 and pico_btstack_base transitively.
    pico_btstack_classic
    pico_btstack_cyw43
    # Bead pico-link-cz0.6 (M5 persistence): pico_flash_bank_instance() --
    # links pico_flash transitively. btstack_tlv_flash_bank.c and
    # btstack_link_key_db_tlv.c are already compiled into
    # pico_btstack_base/pico_btstack_classic (verified by reading
    # src/rp2_common/pico_btstack/CMakeLists.txt); this target supplies only
    # the RP2350-specific hal_flash_bank_t implementation persist.c calls.
    pico_btstack_flash_bank
    # M4 S1 (bead pico-link-cz0.5.2): BTstack's own bluedroid-derived SBC
    # encoder, ships in the pico-sdk (rp2_common/pico_btstack/CMakeLists.txt)
    # -- no new fetch, no new submodule (design sec 11.6).
    pico_btstack_sbc_encoder
    # Bead pico-link-cz0.5.4 (LDAC L0): the vendored Sony/libldac encoder
    # (firmware/vendor/libldac). Originally linked for the offline
    # on-target benchmark only (PL_DIAG_LDAC_BENCH in main.c); bead
    # pico-link-cz0.5.6 (LDAC L3) adds the real A2DP codec row
    # (src/codec_ldac.c) on top of the same library.
    pl_ldac_enc
    pico_cyw43_arch_threadsafe_background
    # M3: link the MARKED tinyusb_device target (not just the
    # tinyusb_device_unmarked that pico_enable_stdio_usb pulls in on its
    # own) -- this is what sets the LIB_TINYUSB_DEVICE=1 compile definition
    # that tells pico_stdio_usb "the app owns tud_init()/descriptors now",
    # compiling OUT its own default tusb_config.h and descriptor callbacks
    # (see firmware/src/tusb_config.h's module doc) while KEEPING its
    # stdio_usb.c I/O glue linked in for the CDC console. The reset vendor-
    # interface driver is now src/usb_reset.c, this project's own (bead
    # pico-link-l60) -- pico-sdk's reset_interface.c is compiled to nothing
    # via PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE=0 above.
    tinyusb_device
)

# Bead pico-link-nli.9 (P2): pico_link must not link pl_ldac_enc's archive
# before the objcopy rename above has run on it -- target_link_libraries
# above only orders against pl_ldac_enc's own build, not this extra step.
add_dependencies(pico_link pl_ldac_enc_sram)

pico_add_extra_outputs(pico_link)

# Bead pico-link-nli.9 (P2): fail the BUILD (not just warn) if any libldac
# object file (ldacBT.c.o / ldaclib.c.o) still has code or data placed in
# flash (0x10xxxxxx) in the final link, i.e. the POST_BUILD rename above
# didn't take for some symbol. Checks the linked .elf directly with nm --
# no dependency on a .map file existing. See that rename's own comment for
# why this can silently fail to catch everything (-ffunction-sections) and
# why a hard build failure, not a warning, is the right response.
add_custom_command(TARGET pico_link POST_BUILD
    COMMAND ${CMAKE_COMMAND}
        -DELF_FILE=$<TARGET_FILE:pico_link>
        -DNM=${CMAKE_NM}
        -DOBJCOPY=${CMAKE_OBJCOPY}
        -DLDAC_ARCHIVE=$<TARGET_FILE:pl_ldac_enc>
        -P ${CMAKE_CURRENT_SOURCE_DIR}/cmake/check_ldac_not_in_flash.cmake
    COMMENT "pico-link-nli.9: verifying libldac is not resident in flash"
    VERBATIM
)
