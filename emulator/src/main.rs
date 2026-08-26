//! The `desktop` binary: the emulator entry point for the two host run
//! modes (windowed, headless). Both modes share the exact same
//! `pico_link_core::App` + `pico_link_core::run` loop; they differ only in which
//! concrete `DisplaySurface`/`InputSource` they hand to a
//! `platform::HostPlatform`.
//!
//! # Usage
//!
//! Windowed (default): `cargo run --bin desktop --target <host-triple>`
//!
//! Headless: `cargo run --bin desktop --target <host-triple> -- --headless
//! [--dump-png PATH] [--frames N]`. `--dump-png` writes the framebuffer as
//! a PNG after `N` frames (default 1) and exits — the fast path for
//! automated/agent verification. Without `--dump-png`, headless mode runs
//! the loop until an HTTP shutdown, driven entirely over HTTP: `POST
//! /api/input` injects a `NavIntent` (drained every frame by
//! `platform::HttpInput`, the `InputSource` for this mode) and `GET
//! /api/screenshot` returns a PNG of whatever `platform::
//! SharedHeadlessSurface` most recently had flushed to it — the same
//! `HeadlessSurface` PNG path `--dump-png` uses, just shared with the HTTP
//! server thread instead of read once at loop exit. This is how an agent
//! drives and observes the shell with no window and no hardware.
//!
//! The HTTP server (`POST /api/input`, `GET /api/screenshot`, `POST
//! /api/shutdown`) runs in both modes. `/api/screenshot` only does
//! anything in headless mode (404 otherwise); `/api/input` is always
//! accepted, but windowed mode's `WindowedInput` never drains the queue it
//! feeds, so injecting there is a harmless no-op.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pico_link_core::{run, App, IdlePowerSetting};
use emulator::desktop::HttpServer;
use emulator::platform::{FileStorage, HeadlessSurface, HostPlatform, HttpInput, MinifbSurface, RecordingPowerControl, SharedHeadlessSurface, WindowedInput};
use minifb::{Window, WindowOptions};

/// Pico Plus 2 W + Waveshare Pico-LCD-1.3 panel geometry (Epic B2's
/// 240x240 retarget). Previously 320x170 (the T-Embed reference panel).
const WIDTH: u32 = 240;
const HEIGHT: u32 = 240;
const WINDOW_SCALE: u32 = 3;
/// ~30fps: light on CPU for a background/agent-driven headless run.
const FRAME_BUDGET: Duration = Duration::from_millis(33);

struct Args {
    headless: bool,
    dump_png: Option<String>,
    frames: u32,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    let headless = raw.iter().any(|a| a == "--headless");
    let dump_png = raw
        .iter()
        .position(|a| a == "--dump-png")
        .and_then(|i| raw.get(i + 1))
        .cloned();
    let frames = raw
        .iter()
        .position(|a| a == "--frames")
        .and_then(|i| raw.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    Args { headless, dump_png, frames }
}

fn main() {
    let args = parse_args();

    println!("Starting desktop emulator ({} mode)...", if args.headless { "headless" } else { "windowed" });

    let mut server = HttpServer::new("127.0.0.1:8080").expect("Failed to start HTTP server");
    let shutdown_signal = server.get_shutdown_signal();
    let input_queue = server.get_input_queue_ref();

    // The headless screenshot surface has to be created here (before the
    // server is moved into its request-loop thread below) so the same
    // `Arc<Mutex<HeadlessSurface>>` can be registered on `server` *and*
    // handed to `run_headless`'s `HostPlatform` — that shared handle is
    // what lets `GET /api/screenshot` (served on the HTTP thread) see
    // frames the render loop (on this thread) flushes. Windowed mode never
    // constructs one, so `/api/screenshot` there stays 404.
    let screenshot_surface = if args.headless {
        let surface = SharedHeadlessSurface::new();
        server.set_screenshot_surface(surface.handle());
        Some(surface)
    } else {
        None
    };

    let kv_storage = FileStorage::new_default().expect("Failed to open kv store");

    // The single persisted idle-power toggle covering both the
    // screensaver and (once armed -- see `pico_link_core::power::
    // DEEP_SLEEP_ARMED`) deep-sleep tiers, loaded once here before
    // `kv_storage` is moved into whichever `HostPlatform` gets built below.
    // Defaults to enabled (screensaver on) if never explicitly saved.
    //
    // Unlike the previous product layer (which let a live Settings screen
    // toggle this without a restart), this minimal shell has no settings
    // UI yet, so the setting is only read once, at boot, and baked into
    // `run`'s two `Option<Duration>` parameters for the whole process
    // lifetime -- a future settings screen can reintroduce live gating the
    // same way the old `App::idle_power`/`take_settings_dirty` seam did.
    let idle_power = IdlePowerSetting::load(&kv_storage);
    let idle_timeout = idle_power.idle_timeout();
    let deep_sleep_timeout = idle_power.deep_sleep_timeout();

    std::thread::spawn(move || {
        println!("HTTP server running on http://127.0.0.1:8080");
        println!("Endpoints:");
        println!("  POST /api/input - Inject a NavIntent (JSON; headless mode only takes effect)");
        println!("  GET  /api/screenshot - PNG of the current framebuffer (headless mode only)");
        println!("  POST /api/shutdown - Shutdown emulator");
        loop {
            if let Err(e) = server.handle_request() {
                eprintln!("HTTP server error: {e}");
            }
        }
    });

    let mut app = App::new(WIDTH, HEIGHT);

    // The `PowerControl` capability, shared by both run modes the same way
    // any other capability field would be (only one branch below actually
    // moves it into a `HostPlatform`).
    let power = RecordingPowerControl::new();

    if args.headless {
        let surface = screenshot_surface.expect("headless mode always constructs a screenshot surface above");
        // Keep a second handle to the same `HeadlessSurface` around:
        // `surface` itself is about to be moved into `platform`, but
        // `--dump-png` still needs to read the final frame back out after
        // the loop stops.
        let surface_handle = surface.handle();
        let mut platform = HostPlatform::new(surface, HttpInput::new(input_queue), kv_storage, power);
        run_headless(&mut platform, &mut app, &shutdown_signal, &surface_handle, &args, idle_timeout, deep_sleep_timeout);
    } else {
        run_windowed(&mut app, kv_storage, &shutdown_signal, power, idle_timeout, deep_sleep_timeout);
    }

    println!("Emulator closed.");
}

fn run_headless(
    platform: &mut HostPlatform<SharedHeadlessSurface, HttpInput>,
    app: &mut App,
    shutdown_signal: &Arc<std::sync::atomic::AtomicBool>,
    surface_handle: &Arc<Mutex<HeadlessSurface>>,
    args: &Args,
    idle_timeout: Option<Duration>,
    deep_sleep_timeout: Option<Duration>,
) {
    if let Some(path) = &args.dump_png {
        // Bounded run for automated/agent verification: N frames, then dump
        // and exit.
        let mut frame = 0u32;
        run(platform, app, FRAME_BUDGET, idle_timeout, deep_sleep_timeout, || {
            frame += 1;
            frame <= args.frames
        });
        surface_handle.lock().unwrap().save_png(path).expect("failed to save headless PNG");
        println!("Wrote headless screenshot to {path} ({WIDTH}x{HEIGHT}, {} frame(s))", args.frames);
    } else {
        println!("Headless mode running. Drive it over HTTP: POST /api/input (NavIntent JSON), GET /api/screenshot (PNG).");
        run(platform, app, FRAME_BUDGET, idle_timeout, deep_sleep_timeout, || !shutdown_signal.load(Ordering::Relaxed));
    }
}

fn run_windowed(
    app: &mut App,
    storage: FileStorage,
    shutdown_signal: &Arc<std::sync::atomic::AtomicBool>,
    power: RecordingPowerControl,
    idle_timeout: Option<Duration>,
    deep_sleep_timeout: Option<Duration>,
) {
    println!("Controls: Arrow Up/Down (Prev/Next), Enter (Activate), Backspace/Esc (Back)");
    println!("Window size: {}x{} ({WINDOW_SCALE}x scale)", WIDTH * WINDOW_SCALE, HEIGHT * WINDOW_SCALE);

    let mut window = Window::new(
        "Pico Link - Desktop Emulator",
        (WIDTH * WINDOW_SCALE) as usize,
        (HEIGHT * WINDOW_SCALE) as usize,
        WindowOptions::default(),
    )
    .unwrap_or_else(|e| panic!("Unable to create window: {e}"));
    window.set_target_fps(60);
    let window = Rc::new(RefCell::new(window));

    let display = MinifbSurface::new(Rc::clone(&window), WIDTH, HEIGHT, WINDOW_SCALE);
    let input = WindowedInput::new(Rc::clone(&window));
    let mut platform = HostPlatform::new(display, input, storage, power);

    println!("Emulator started!");

    let window_for_should_continue = Rc::clone(&window);
    run(&mut platform, app, FRAME_BUDGET, idle_timeout, deep_sleep_timeout, || {
        window_for_should_continue.borrow().is_open() && !shutdown_signal.load(Ordering::Relaxed)
    });
}
