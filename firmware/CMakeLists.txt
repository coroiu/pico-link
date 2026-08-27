cmake_minimum_required(VERSION 3.13)

# Board must be set before the SDK import.
set(PICO_BOARD pimoroni_pico_plus2_w_rp2350 CACHE STRING "Board type")

include(pico_sdk_import.cmake)

project(pico_link_firmware C CXX ASM)

set(CMAKE_C_STANDARD 11)
set(CMAKE_CXX_STANDARD 17)

pico_sdk_init()

add_executable(pico_link
    src/main.c
)

pico_set_program_name(pico_link "pico_link")
pico_set_program_version(pico_link "0.1.0")

# USB CDC stdio: gives us a console AND lets `picotool reboot -f -u` reset a
# running board into BOOTSEL with no button press, for free.
pico_enable_stdio_usb(pico_link 1)
pico_enable_stdio_uart(pico_link 0)
target_compile_definitions(pico_link PRIVATE
    PICO_STDIO_USB_ENABLE_RESET_VIA_VENDOR_INTERFACE=1
)

target_link_libraries(pico_link
    pico_stdlib
)

pico_add_extra_outputs(pico_link)
