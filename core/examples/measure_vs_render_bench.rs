//! M-2 (`pico-link-7h5.2`, design section 11 of
//! `.planning/design/2026-09-06-damage-rect-render-and-partial-blit.md`):
//! host-side measurement of what fraction of a Home-screen render is
//! **layout** (`Widget::measure`) versus **rasterisation**
//! (`Widget::render`), on the exact scenario the damage-rect design is
//! built around -- the status face, connected, with the OUT meter live.
//!
//! This is a measurement tool, not production code -- it does not modify
//! any file under `core/src/render` or `core/src/app.rs`.
//!
//! Two things are measured:
//!
//! 1. **The `HeroStatusView` widget in isolation.** `Screen::render`'s
//!    per-widget loop (`core/src/render/screen.rs:472-476`) is exactly
//!    `widget.measure(..)` then `widget.render(..)`, and on Home's status
//!    face `HomeView::{measure,render}` (`core/src/render/home.rs:234-236,
//!    366-368`) is a bare passthrough to `HeroStatusView::{measure,
//!    render}` -- so timing the public `HeroStatusView` directly is a
//!    faithful, unmodified proxy for what `Screen::render`'s loop actually
//!    calls for Home, with no need to reach `HomeView` (private).
//! 2. **The whole-frame `App::render()`** for the same scenario, so the
//!    widget-level split can be related back to the ~25ms figure the
//!    design cites (title bar/status dot/link glyph/rail chrome drawing
//!    lives in `Screen::render` itself, outside the widget loop, and is
//!    included here but not decomposed further -- out of scope for M-2).
//!
//! Run with: `cargo run --release -p pico-link-core --example measure_vs_render_bench`
//!
//! Host absolute milliseconds are NOT representative of the RP2350 target
//! (a desktop CPU is vastly faster and has caches/branch prediction the
//! target doesn't) -- only the *ratio* between measure and render time is
//! expected to transfer, and even that only approximately (font shaping
//! is data/cache-bound work that may scale differently across
//! architectures). Report the ratio, not the absolute host ms, as the
//! answer to M-2.

use std::hint::black_box;
use std::time::Instant as StdInstant;

use embedded_graphics::prelude::Size;

use pico_link_core::app::{App, ConnectedCodec, Event, PairedDevice};
use pico_link_core::render::hero::{BitrateStatus, CodecStatus, HeroStatusView, OutLevelDisplay};
use pico_link_core::render::{compute_chrome, FrameBuffer565, RenderCtx, Widget};
use pico_link_core::platform::Instant;

const ITERS: u32 = 20_000;

/// Builds the exact scenario `core/src/app.rs`'s
/// `home_connected_with_out_level` test helper builds (that helper is
/// private to `app.rs`'s `#[cfg(test)] mod tests`, so this duplicates its
/// four `handle_event` calls rather than reaching into it) -- connected,
/// LDAC, a live `LevelsChanged` reading, which is the scenario the
/// design's ~25ms figure and the OUT-meter damage-hint narrowing (design
/// section 8) are both about.
fn home_connected_with_out_level() -> App {
    let mut app = App::new(240, 240);
    let addr = [7u8; 6];
    let seq_for_test_1008 = app.seed_connect_attempt_for_test(addr);
    app.handle_event(Event::ConnectSucceeded { addr, degraded: false, seq: seq_for_test_1008 });
    app.handle_event(Event::PairedDeviceUpserted(PairedDevice { addr, name: String::from("Cans"), mru_seq: 1, ldac_quality: 0, preset_id: 0 }));
    app.handle_event(Event::CodecChanged(ConnectedCodec { addr, word: String::from("LDAC"), nominal_bitrate_bps: 990_000 }));
    app.tick(1);
    app.handle_event(Event::LevelsChanged { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 });
    app
}

/// The `HeroStatusView` state `render::home`'s `HomeView::new` would build
/// for the same model state as `home_connected_with_out_level` above (see
/// that module's status-face construction, `core/src/render/home.rs`
/// around line 160) -- device name "Cans", LDAC connected, no fallback,
/// a live OUT-level reading just received.
fn matching_hero_view() -> HeroStatusView {
    HeroStatusView::new(
        "Cans",
        CodecStatus::Connected { word: String::from("LDAC"), fallback: None, bitrate: BitrateStatus::Kbps { kbps: 990, adaptive: false } },
    )
    .with_out_level(Some(OutLevelDisplay {
        peak_l: 200,
        peak_r: 180,
        rms_l: 120,
        rms_r: 100,
        hold_l: 200,
        hold_r: 180,
        received_at: Instant::from_micros(1),
        attack_peak_l: 120,
        attack_peak_r: 100,
        attack_peak_l_at: Instant::from_micros(1),
        attack_peak_r_at: Instant::from_micros(1),
    }))
}

fn time_iters<F: FnMut()>(iters: u32, mut f: F) -> std::time::Duration {
    let start = StdInstant::now();
    for _ in 0..iters {
        f();
    }
    start.elapsed()
}

fn main() {
    let chrome = compute_chrome(Size::new(240, 240));
    let ctx = RenderCtx::at(Instant::from_micros(2));
    let view = matching_hero_view();
    let content_area = chrome.content;
    let mut fb = FrameBuffer565::new(240, 240);

    // Warm up (fills caches, lets the branch predictor settle) before any
    // timed section -- untimed, discarded.
    for _ in 0..200 {
        black_box(view.measure(black_box(content_area.size), &ctx));
        view.render(content_area, &ctx, &mut fb).expect("core DrawTarget is Infallible");
    }

    // --- 1. HeroStatusView in isolation: measure vs render. ---
    let measure_elapsed = time_iters(ITERS, || {
        black_box(view.measure(black_box(content_area.size), &ctx));
    });
    let render_elapsed = time_iters(ITERS, || {
        view.render(content_area, &ctx, &mut fb).expect("core DrawTarget is Infallible");
    });

    // --- 2. Whole-frame App::render() for the same scenario, for scale. ---
    let mut app = home_connected_with_out_level();
    for _ in 0..200 {
        black_box(app.render());
    }
    let app_render_elapsed = time_iters(ITERS, || {
        black_box(app.render());
    });

    let measure_ns_per_iter = measure_elapsed.as_nanos() as f64 / f64::from(ITERS);
    let render_ns_per_iter = render_elapsed.as_nanos() as f64 / f64::from(ITERS);
    let app_render_ns_per_iter = app_render_elapsed.as_nanos() as f64 / f64::from(ITERS);
    let widget_total_ns = measure_ns_per_iter + render_ns_per_iter;
    let measure_fraction = measure_ns_per_iter / widget_total_ns;
    let render_fraction = render_ns_per_iter / widget_total_ns;

    println!("invocation: cargo run --release -p pico-link-core --example measure_vs_render_bench");
    println!("iterations per section: {ITERS}");
    println!();
    println!("HeroStatusView::measure  : {measure_ns_per_iter:>10.1} ns/iter  ({:.3} ms)", measure_ns_per_iter / 1e6);
    println!("HeroStatusView::render   : {render_ns_per_iter:>10.1} ns/iter  ({:.3} ms)", render_ns_per_iter / 1e6);
    println!(
        "measure fraction of (measure+render): {:.3}%  |  render fraction: {:.3}%",
        measure_fraction * 100.0,
        render_fraction * 100.0
    );
    println!();
    println!(
        "App::render() whole frame (chrome+rail+widget), same scenario: {app_render_ns_per_iter:>10.1} ns/iter ({:.3} ms)",
        app_render_ns_per_iter / 1e6
    );
    println!(
        "HeroStatusView::render as a fraction of whole-frame App::render(): {:.3}%",
        render_ns_per_iter / app_render_ns_per_iter * 100.0
    );
    println!(
        "HeroStatusView::measure as a fraction of whole-frame App::render(): {:.5}%",
        measure_ns_per_iter / app_render_ns_per_iter * 100.0
    );
}
