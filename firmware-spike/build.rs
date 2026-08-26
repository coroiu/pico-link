use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    std::fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");

    // --- BTstack cross-compile probe (pico-link-8v3.1 gate 3/4) -----------
    //
    // Goal: prove BTstack (classic core + embedded run loop) cross-compiles
    // for thumbv8m.main-none-eabihf and links into a Rust-owned binary,
    // with NO pico-sdk runtime underneath it. `cc::Build` compiles each
    // source to its own object file and archives them incrementally, which
    // sidesteps the "argument list too long" failure the previous attempt
    // hit from a giant single gcc command line.
    let pico_sdk = env::var("PICO_SDK_PATH").expect(
        "PICO_SDK_PATH must be set (export PICO_SDK_PATH=~/pico-sdk) - BTstack source lives at $PICO_SDK_PATH/lib/btstack",
    );
    let btstack = Path::new(&pico_sdk).join("lib/btstack");
    assert!(
        btstack.join("src/hci.c").exists(),
        "BTstack source not found at {:?} - check PICO_SDK_PATH",
        btstack
    );

    let probe_dir = Path::new("btstack-probe");

    let core_sources = [
        "src/btstack_util.c",
        "src/btstack_linked_list.c",
        "src/btstack_linked_queue.c",
        "src/btstack_run_loop.c",
        "src/btstack_run_loop_base.c",
        "src/btstack_memory.c",
        "src/btstack_memory_pool.c",
        "src/hci.c",
        "src/hci_cmd.c",
        "src/hci_event.c",
        "src/hci_event_builder.c",
        "src/hci_dump.c",
        "src/l2cap.c",
        "src/l2cap_signaling.c",
        "src/ad_parser.c",
        "src/btstack_hid.c",
        "src/btstack_hid_parser.c",
        "src/btstack_crypto.c",
        "platform/embedded/btstack_run_loop_embedded.c",
    ];

    // The Homebrew arm-none-eabi-gcc formula ships a bare compiler with no
    // newlib headers, so plain "arm-none-eabi-gcc" fails on `#include
    // <stdint.h>` before it even reaches BTstack. Use the full ARM GNU
    // Toolchain installed at /Applications/ArmGNUToolchain instead, which
    // bundles newlib (needed for standard headers even though nothing here
    // links libc - we only compile, never link against newlib's runtime).
    let arm_toolchain_bin = "/Applications/ArmGNUToolchain/15.2.rel1/arm-none-eabi/bin";
    let gcc = format!("{arm_toolchain_bin}/arm-none-eabi-gcc");
    let ar = format!("{arm_toolchain_bin}/arm-none-eabi-ar");
    assert!(
        Path::new(&gcc).exists(),
        "expected ARM GNU Toolchain gcc at {gcc}"
    );

    let mut build = cc::Build::new();
    build
        .compiler(&gcc)
        .archiver(&ar)
        .flag("-mcpu=cortex-m33")
        .flag("-mthumb")
        .flag("-mfloat-abi=hard")
        .flag("-mfpu=fpv5-sp-d16")
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        .flag("-std=gnu11")
        .warnings(false)
        .define("BTSTACK_FILE__", "\"probe\"")
        .include(probe_dir)
        .include(btstack.join("src"))
        .include(btstack.join("src/classic"))
        .include(btstack.join("src/ble"))
        .include(btstack.join("platform/embedded"));

    for src in core_sources {
        build.file(btstack.join(src));
    }
    build.file(probe_dir.join("hal_shim.c"));
    build.file(probe_dir.join("hci_transport_dummy.c"));

    build.compile("btstack_probe");

    // --- TinyUSB cross-compile probe (composite-device coexistence) -------
    //
    // The device-independent core (tusb.c, device/*.c, common/*.c, the CDC
    // class driver) has zero pico-sdk includes, same shape as BTstack's
    // core. Only the RP2040/RP2350 dcd (portable/raspberrypi/rp2040/*.c)
    // touches pico.h + hardware/* registers - that's the actual hurdle,
    // deliberately NOT probed here (gate 1 CDC bring-up, tracked
    // separately). This only proves the portable core builds standalone.
    let tinyusb = Path::new(&pico_sdk).join("lib/tinyusb/src");
    assert!(
        tinyusb.join("tusb.c").exists(),
        "TinyUSB source not found at {:?}",
        tinyusb
    );

    let tinyusb_sources = [
        "tusb.c",
        "common/tusb_fifo.c",
        "device/usbd.c",
        "device/usbd_control.c",
        "class/cdc/cdc_device.c",
    ];

    let mut tusb_build = cc::Build::new();
    tusb_build
        .compiler(&gcc)
        .archiver(&ar)
        .flag("-mcpu=cortex-m33")
        .flag("-mthumb")
        .flag("-mfloat-abi=hard")
        .flag("-mfpu=fpv5-sp-d16")
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        .flag("-std=gnu11")
        .warnings(false)
        .define("CFG_TUSB_MCU", "OPT_MCU_RP2040")
        .define("CFG_TUSB_OS", "OPT_OS_NONE")
        .include(probe_dir)
        .include(&tinyusb)
        .include(tinyusb.parent().unwrap());

    for src in tinyusb_sources {
        tusb_build.file(tinyusb.join(src));
    }
    tusb_build.compile("tinyusb_core_probe");

    println!("cargo:rerun-if-changed=btstack-probe");
    println!("cargo:rerun-if-env-changed=PICO_SDK_PATH");
}
