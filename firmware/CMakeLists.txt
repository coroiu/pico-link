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
    PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE=1
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
)

target_link_libraries(pico_link
    pico_stdlib
    hardware_spi
    hardware_dma
    hardware_gpio
    ui_ffi
    # M2: the ready-made HCI transport this C-first pivot exists to use
    # (see bt.c's module doc). pico_btstack_cyw43 pulls in
    # pico_btstack_hci_transport_cyw43 and pico_btstack_base transitively.
    pico_btstack_classic
    pico_btstack_cyw43
    pico_cyw43_arch_threadsafe_background
)

pico_add_extra_outputs(pico_link)
