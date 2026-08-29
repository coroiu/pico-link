cmake_minimum_required(VERSION 3.13)

# Board must be set before the SDK import.
set(PICO_BOARD pimoroni_pico_plus2_w_rp2350 CACHE STRING "Board type")

include(pico_sdk_import.cmake)

project(pico_link_firmware C CXX ASM)

set(CMAKE_C_STANDARD 11)
set(CMAKE_CXX_STANDARD 17)

pico_sdk_init()

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

add_executable(pico_link
    src/main.c
    src/st7789.c
    src/input.c
    src/bt.c
    src/a2dp.c
    src/codec_table.c
    src/codec_sbc.c
    src/panic_recorder.c
    src/usb_descriptors.c
    src/usb_audio.c
    src/usb_pump.c
    src/pcm_ring.c
    src/usb_reset.c
)

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
    # floor. Core1 is never launched (see main.c/bt.c's module docs --
    # matching USBPods' own working reference, everything runs on core0),
    # so PICO_CORE1_STACK_SIZE is left at the SDK default.
    PICO_STACK_SIZE=0xC000
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

target_link_libraries(pico_link
    pico_stdlib
    hardware_spi
    hardware_dma
    hardware_gpio
    hardware_watchdog
    ui_ffi
    # M2: the ready-made HCI transport this C-first pivot exists to use
    # (see bt.c's module doc). pico_btstack_cyw43 pulls in
    # pico_btstack_hci_transport_cyw43 and pico_btstack_base transitively.
    pico_btstack_classic
    pico_btstack_cyw43
    # M4 S1 (bead pico-link-cz0.5.2): BTstack's own bluedroid-derived SBC
    # encoder, ships in the pico-sdk (rp2_common/pico_btstack/CMakeLists.txt)
    # -- no new fetch, no new submodule (design sec 11.6).
    pico_btstack_sbc_encoder
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

pico_add_extra_outputs(pico_link)
