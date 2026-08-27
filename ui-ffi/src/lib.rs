//! The FFI seam for the C-first pivot: a `no_std` + `alloc` staticlib that
//! pico-sdk's C `main()` links in and drives over the narrow `extern "C"`
//! surface below. This crate owns nothing beyond that surface -- no
//! `main()`, no boot, no scheduling (all C's job, per
//! `.planning/decisions/2026-08-27-c-first-pico-sdk-owns-main.md`); it
//! exists purely to let C call into `pico_link_core`'s render/navigation
//! logic and get RGB565 pixels back.
//!
//! # FFI direction rule
//!
//! C calls Rust; Rust calls nothing back except [`pl_ui_panic_hook`], the
//! **one** call in the other direction, implemented by C. This is
//! deliberate and load-bearing: keeping the boundary one-directional (bar
//! that single hook) is what keeps `pico_link_core` itself emulator-testable
//! with zero FFI awareness -- see the M1b bead for the full rationale (three
//! earlier sessions got expensive partly by losing that property).
//!
//! # Memory rules
//!
//! - Rust owns everything behind [`PlUi`] (an opaque pointer from C's
//!   perspective); C never dereferences it, only passes it back.
//! - Strings Rust receives from C (none in the M1 surface below) would be
//!   borrowed for the call only and copied immediately, with invalid UTF-8
//!   replaced lossily rather than panicking -- see [`pl_ui_panic_hook`] for
//!   the one place this crate currently receives a string.
//! - The pixel pointer [`pl_ui_render`] hands back is borrowed until the
//!   next mutating call (`pl_ui_input`/`pl_ui_tick`/`pl_ui_render` again) --
//!   C must copy out (e.g. into a DMA source) before calling back in.
//! - No Rust allocation is ever freed by C, and no C allocation is ever
//!   freed by Rust.
//! - [`PlUi`] is not `Sync`: every call for one instance must come from one
//!   core (the bead's "Superloop on core0 only").
//! - Every entry point checks for a null pointer first and returns quietly
//!   rather than panicking -- a null-pointer bug from the C side must not
//!   crash the one thing (the display) a developer needs to debug it.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use embedded_alloc::LlffHeap as Heap;
use pico_link_core::{App, NavIntent};

// --- critical-section implementation ---
//
// `embedded_alloc::LlffHeap` guards its internal free-list with a
// `critical_section::Mutex`, which needs *some*
// `critical_section::Impl` registered at link time (otherwise
// `_critical_section_1_0_acquire`/`_release` are left undefined -- this is
// exactly what the M1b bead's day-one `nm` check caught). The bead forbids
// pulling in `cortex-m-rt` (a full runtime crate) for this, so this is a
// small hand-rolled implementation instead: plain PRIMASK save/disable/
// restore via inline `cpsid`/`msr`/`cpsie`, the same three instructions
// `cortex-m`'s own critical-section impl uses under the hood, with no
// framework attached. Sound for this crate's actual concurrency model too
// (see the module doc's "Superloop on core0 only" -- everything here runs
// on one core with no preemption other than interrupts, which is exactly
// what disabling PRIMASK excludes for the critical section's duration).
struct SingleCoreCriticalSection;
critical_section::set_impl!(SingleCoreCriticalSection);

// SAFETY: `acquire`/`release` correctly save and restore the interrupt
// mask (PRIMASK) around the critical section, per `critical_section::Impl`'s
// contract -- interrupts are disabled for the duration and restored to
// exactly their prior state afterward, and these two calls are never
// reordered or elided (`acquire` returns the token `release` consumes).
unsafe impl critical_section::Impl for SingleCoreCriticalSection {
    unsafe fn acquire() -> critical_section::RawRestoreState {
        let primask: u32;
        // SAFETY: reads PRIMASK (no side effect) then disables interrupts;
        // both are single, uninterruptible instructions valid on any
        // Armv8-M target, which is all this crate ever builds for.
        core::arch::asm!(
            "mrs {0}, PRIMASK",
            "cpsid i",
            out(reg) primask,
            options(nomem, nostack, preserves_flags),
        );
        // Bit 0 of PRIMASK is 1 when interrupts were already disabled --
        // that's the exact bit `release` needs to decide whether to
        // re-enable them.
        primask & 1
    }

    unsafe fn release(token: critical_section::RawRestoreState) {
        // Only re-enable interrupts if they were enabled before `acquire`
        // (token == 0) -- a nested critical section must not re-enable
        // interrupts an outer one is still relying on being off.
        if token == 0 {
            // SAFETY: single, uninterruptible instruction valid on any
            // Armv8-M target.
            core::arch::asm!("cpsie i", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Global allocator ---
//
// A single static arena in Rust `.bss`. The M1b bead's design specified 64KB
// ("sized from the M1 linker map"), but that is smaller than a single
// 240x240 RGB565 `FrameBuffer565` on its own (240*240*2 = 115,200 bytes) --
// `App::new`'s very first allocation would fail outright. Measured on real
// hardware (bd pico-link-cz0.2): flashing with the 64KB arena produced total
// silence over CDC, including no boot banner, consistent with a HardFault/
// alloc-error trap during `pl_ui_create` before the firmware got anywhere
// near its first `printf`. Raised to 192KB here -- comfortably over the
// 115KB floor with headroom for `Navigator`/`Screen`/`ListItem` allocations
// -- as an implementation-level correction to a design number that turned
// out not to match reality, not an architecture change (RP2350 has 520KB
// SRAM total; 192KB leaves well over half for BTstack/TinyUSB/cyw43 once
// those land in M2/M3). Flagged to the architect via a LEARNED bead comment
// rather than silently overridden.
#[global_allocator]
static HEAP: Heap = Heap::empty();

const HEAP_SIZE: usize = 192 * 1024;
static mut HEAP_MEM: [core::mem::MaybeUninit<u8>; HEAP_SIZE] = [core::mem::MaybeUninit::uninit(); HEAP_SIZE];

/// Initializes the global allocator's arena. Called exactly once, from the
/// first [`pl_ui_create`] -- `LlffHeap::init` itself is `unsafe` because
/// calling it twice (or handing it overlapping memory) would corrupt heap
/// bookkeeping; the `HEAP_INITIALIZED` latch below is what makes a second
/// `pl_ui_create` call safe instead of relying on the C caller never doing
/// that.
static HEAP_INITIALIZED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn ensure_heap_initialized() {
    use core::sync::atomic::Ordering;
    if HEAP_INITIALIZED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        // SAFETY: the `compare_exchange` above guarantees this branch runs
        // at most once across the process lifetime (single-core superloop,
        // see the module doc's "Superloop on core0 only"), so `HEAP_MEM` is
        // handed to the allocator exactly once and never aliased.
        unsafe {
            let start = core::ptr::addr_of_mut!(HEAP_MEM).cast::<u8>();
            HEAP.init(start as usize, HEAP_SIZE);
        }
    }
}

// --- Panic handling ---
//
// `no_std` + no unwinder on this target (thumbv8m.main-none-eabihf) + this
// workspace's `panic = "abort"` release profile: a panic here can only ever
// report and halt, never unwind across the `extern "C"` boundary into C
// (which would be UB). Reports via the one call Rust is allowed to make
// back into C, then loops forever -- there is nothing else a bare-metal
// no_std panic handler can safely do.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // A tiny fixed-size stack buffer, not a heap `alloc::format!` -- a
    // panic is exactly the moment the allocator itself might be the thing
    // that's broken, so this path must not depend on it.
    struct FixedBuf {
        data: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for FixedBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let remaining = self.data.len() - self.len;
            let n = s.len().min(remaining);
            self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    let mut buf = FixedBuf { data: [0; 256], len: 0 };
    // Deliberately ignore the `Result`: if the message got truncated by
    // `FixedBuf`'s fixed capacity, there is nothing more useful to do here
    // than report the truncated prefix we do have.
    let _ = core::fmt::write(&mut buf, format_args!("{info}"));

    // SAFETY: `pl_ui_panic_hook` is C's own responsibility to implement
    // soundly (documented in this crate's public FFI contract); Rust holds
    // up its side by passing a valid pointer + exact length into memory it
    // owns for the duration of this call, per the module doc's memory
    // rules.
    unsafe {
        pl_ui_panic_hook(buf.data.as_ptr(), buf.len);
    }

    loop {
        core::hint::spin_loop();
    }
}

extern "C" {
    /// The one call in the Rust -> C direction: reports a Rust panic
    /// message so C can log it (and, on real hardware, likely reset).
    /// `msg` points to `len` bytes of UTF-8 text, valid only for the
    /// duration of this call -- C must copy out anything it needs to keep.
    fn pl_ui_panic_hook(msg: *const u8, len: usize);
}

// --- The opaque handle C holds ---

/// Opaque from C's perspective (forward-declared as `struct pl_ui_t;` in
/// the cbindgen-generated header) -- C only ever holds a pointer to this
/// and passes it back into the functions below; it never dereferences it
/// (see the module doc's memory rules).
pub struct PlUi {
    app: App,
}

/// Creates a new UI instance rendering into a `width`x`height` framebuffer,
/// returning an owning pointer for C to hold and pass back into every other
/// `pl_ui_*` call. Initializes the global allocator's arena on the first
/// call (see [`ensure_heap_initialized`]).
///
/// Returns a null pointer if `width`/`height` would overflow the internal
/// pixel-count arithmetic (mirrors [`pico_link_core::render::FrameBuffer565::new`]'s
/// panic contract, but this boundary never panics across FFI -- see the
/// module doc).
///
/// # Safety
///
/// The returned pointer must eventually be passed to exactly one
/// [`pl_ui_destroy`] call and no other `pl_ui_*` call after that.
#[no_mangle]
pub extern "C" fn pl_ui_create(width: u32, height: u32) -> *mut PlUi {
    ensure_heap_initialized();

    if width == 0 || height == 0 {
        return core::ptr::null_mut();
    }
    // `width as usize * height as usize` overflow check, mirroring
    // `FrameBuffer565::new`'s own panic contract but turned into a null
    // return instead -- this boundary must never panic into C.
    if (width as usize).checked_mul(height as usize).is_none() {
        return core::ptr::null_mut();
    }

    let ui = PlUi { app: App::new(width, height) };
    Box::into_raw(Box::new(ui))
}

/// Destroys a UI instance created by [`pl_ui_create`], freeing every Rust
/// allocation behind it. A null `ui` is a no-op (see the module doc's
/// null-check rule).
///
/// # Safety
///
/// `ui` must be either null or a pointer previously returned by
/// [`pl_ui_create`] and not yet destroyed. After this call `ui` is
/// dangling and must not be passed to any other `pl_ui_*` function.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_destroy(ui: *mut PlUi) {
    if ui.is_null() {
        return;
    }
    // SAFETY: caller contract above -- `ui` is a live `Box::into_raw`
    // pointer from `pl_ui_create`, not yet destroyed.
    unsafe {
        drop(Box::from_raw(ui));
    }
}

/// Maps 1:1 to [`pico_link_core::NavIntent`]'s variants; `jump_by` is only
/// meaningful when `tag == PL_INTENT_JUMP_BY` (mirrors `NavIntent::JumpBy`'s
/// single `i16` payload -- every other variant carries no data, matching
/// `NavIntent`'s C-unrepresentable per-variant payload constraint, which is
/// exactly why this is a flat tag+payload struct instead of a `#[repr(C)]`
/// enum with data).
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlIntentTag {
    Up,
    Down,
    Left,
    Right,
    JumpBy,
    Select,
    Back,
    ShortcutX,
    ShortcutY,
}

/// One input event as C constructs it. See [`PlIntentTag`]'s doc comment
/// for the `jump_by` field's contract.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlIntent {
    pub tag: PlIntentTag,
    pub jump_by: i16,
}

impl From<PlIntent> for NavIntent {
    fn from(intent: PlIntent) -> Self {
        match intent.tag {
            PlIntentTag::Up => NavIntent::Up,
            PlIntentTag::Down => NavIntent::Down,
            PlIntentTag::Left => NavIntent::Left,
            PlIntentTag::Right => NavIntent::Right,
            PlIntentTag::JumpBy => NavIntent::JumpBy(intent.jump_by),
            PlIntentTag::Select => NavIntent::Select,
            PlIntentTag::Back => NavIntent::Back,
            PlIntentTag::ShortcutX => NavIntent::ShortcutX,
            PlIntentTag::ShortcutY => NavIntent::ShortcutY,
        }
    }
}

/// Forwards `count` polled input events (C's debounced GPIO edges, already
/// mapped to `pl_intent_t`) into the app core. A no-op if `ui` is null,
/// `intents` is null, or `count` is zero.
///
/// # Safety
///
/// If non-null, `intents` must point to at least `count` valid,
/// initialized `pl_intent_t` values, and `ui` must be a live pointer from
/// [`pl_ui_create`] not yet destroyed. The pointed-to memory is borrowed
/// for the duration of this call only (see the module doc's memory rules).
#[no_mangle]
pub unsafe extern "C" fn pl_ui_input(ui: *mut PlUi, intents: *const PlIntent, count: usize) {
    if ui.is_null() || intents.is_null() || count == 0 {
        return;
    }
    // SAFETY: caller contract above.
    let ui = &mut *ui;
    // SAFETY: caller contract above -- `intents` points to `count` valid,
    // initialized `PlIntent` values for the duration of this call.
    let slice = core::slice::from_raw_parts(intents, count);
    let mapped: Vec<NavIntent> = slice.iter().copied().map(NavIntent::from).collect();
    ui.app.handle_input(mapped);
}

/// One tick of the app core's clock, with `now_us` C's own timestamp (C
/// owns the clock under the FFI direction rule -- Rust never reads a
/// hardware timer itself). Reserved for future time-driven repaint sources
/// (e.g. a live link-status indicator); today's placeholder root screen has
/// no such source, so this is currently a deliberate no-op beyond accepting
/// the call. A no-op (including the "deliberate no-op" above) if `ui` is
/// null.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_tick(ui: *mut PlUi, now_us: u64) {
    if ui.is_null() {
        return;
    }
    // SAFETY: caller contract above.
    let _ui = &mut *ui;
    // `now_us` is intentionally unused today -- see the doc comment above.
    let _ = now_us;
}

/// Renders the current screen (only if dirty -- mirrors
/// `pico_link_core::run::Runner::step`'s own dirty gate) and hands back a
/// borrowed pointer to the raw RGB565 pixel data plus its length in pixels
/// (not bytes). Native CPU (little-endian) `u16` values, one per pixel, row
/// major -- **not** the panel's big-endian wire format; C's DMA blit is
/// expected to byte-swap in hardware on the way out (see
/// `FrameBuffer565::as_raw_u16`'s doc comment).
///
/// Sets `*out_px`/`*out_len` to null/0 if `ui` is null. Always sets both
/// (never leaves them uninitialized) when `ui`, `out_px`, and `out_len` are
/// all non-null.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `out_px`/`out_len`, if non-null, must point to valid,
/// writable `*const u16`/`usize` storage. The pixel pointer written to
/// `*out_px` is borrowed until the next call that mutates `ui`
/// (`pl_ui_input`/`pl_ui_tick`/`pl_ui_render` again) -- C must have copied
/// out (e.g. into a DMA source) before making that next call.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_render(ui: *mut PlUi, out_px: *mut *const u16, out_len: *mut usize) {
    if out_px.is_null() || out_len.is_null() {
        return;
    }
    if ui.is_null() {
        // SAFETY: caller contract -- both pointers are valid, writable
        // storage per the null checks above.
        unsafe {
            *out_px = core::ptr::null();
            *out_len = 0;
        }
        return;
    }
    // SAFETY: caller contract above.
    let ui = &mut *ui;
    let framebuffer = ui.app.render();
    let raw = framebuffer.as_raw_u16();
    // SAFETY: caller contract -- both pointers are valid, writable storage.
    unsafe {
        *out_px = raw.as_ptr();
        *out_len = raw.len();
    }
}
