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

// `no_std` everywhere except this crate's own `#[cfg(test)]` unit tests,
// which run on the host under `cargo test -p ui-ffi` -- mirrors
// `pico_link_core`'s own `#[cfg_attr(not(test), no_std)]` escape hatch (see
// `core/src/lib.rs`) for exactly the same reason: `cargo test` needs `std`'s
// panic-unwind machinery and a working host allocator, neither of which a
// bare-metal `no_std` build provides. The heap arena, the hand-rolled
// critical-section impl, and the panic handler below are all `#[cfg(not(test))]`
// for the same reason -- see each section's own comment.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(not(test))]
use embedded_alloc::LlffHeap as Heap;
use pico_link_core::{App, Command, ConnectFailureReason, ConnectStep, DeviceEntry, Event, LinkState, NavIntent};

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
// `#[cfg(not(test))]`: under `cargo test -p ui-ffi` this crate builds with
// `std` linked in (see the crate root's `cfg_attr(not(test), no_std)`), and
// the heap arena below (the only thing that ever calls into
// `critical_section`) is itself `#[cfg(not(test))]`'d away in favour of
// std's own allocator -- so no `critical_section::Impl` is ever needed, or
// registered, under test.
#[cfg(not(test))]
struct SingleCoreCriticalSection;
#[cfg(not(test))]
critical_section::set_impl!(SingleCoreCriticalSection);

// SAFETY: `acquire`/`release` correctly save and restore the interrupt
// mask (PRIMASK) around the critical section, per `critical_section::Impl`'s
// contract -- interrupts are disabled for the duration and restored to
// exactly their prior state afterward, and these two calls are never
// reordered or elided (`acquire` returns the token `release` consumes).
#[cfg(not(test))]
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
// rather than silently overridden. `#[cfg(not(test))]`: under `cargo test`
// this crate links `std`, which already provides a working host allocator --
// declaring a second `#[global_allocator]` here would hijack the *entire*
// test binary (including the test harness itself, before any of our test
// code runs) into using an uninitialized arena. See the crate root's
// `cfg_attr(not(test), no_std)` comment.
#[cfg(not(test))]
#[global_allocator]
static HEAP: Heap = Heap::empty();

#[cfg(not(test))]
const HEAP_SIZE: usize = 192 * 1024;
#[cfg(not(test))]
static mut HEAP_MEM: [core::mem::MaybeUninit<u8>; HEAP_SIZE] = [core::mem::MaybeUninit::uninit(); HEAP_SIZE];

/// Initializes the global allocator's arena. Called exactly once, from the
/// first [`pl_ui_create`] -- `LlffHeap::init` itself is `unsafe` because
/// calling it twice (or handing it overlapping memory) would corrupt heap
/// bookkeeping; the `HEAP_INITIALIZED` latch below is what makes a second
/// `pl_ui_create` call safe instead of relying on the C caller never doing
/// that. A no-op under `cargo test` (see the `HEAP` static's doc comment) --
/// std's own allocator is used instead.
#[cfg(not(test))]
static HEAP_INITIALIZED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[cfg(not(test))]
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

#[cfg(test)]
fn ensure_heap_initialized() {
    // No-op under test -- std's allocator is already live, see the `HEAP`
    // static's doc comment above.
}

// --- Panic handling ---
//
// `no_std` + no unwinder on this target (thumbv8m.main-none-eabihf) + this
// workspace's `panic = "abort"` release profile: a panic here can only ever
// report and halt, never unwind across the `extern "C"` boundary into C
// (which would be UB). Reports via the one call Rust is allowed to make
// back into C, then loops forever -- there is nothing else a bare-metal
// no_std panic handler can safely do. `#[cfg(not(test))]`: a crate linked
// into a `std` test binary must not define its own `#[panic_handler]` --
// std already provides one (ordinary unwinding panics, which `#[test]`
// relies on for `#[should_panic]` and for reporting a failing assertion).
#[cfg(not(test))]
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

// `#[cfg(not(test))]`: the only caller is `panic` above, itself
// `#[cfg(not(test))]`'d away (see its doc comment) -- under `cargo test`
// nothing calls this, and an unused `extern "C"` declaration triggers
// `dead_code`.
#[cfg(not(test))]
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
    /// Count of `pl_ui_input`/`pl_ui_push_event` calls that carried a tag
    /// value with no corresponding `PlIntentTag`/`PlEventTag` variant --
    /// i.e. a malformed or garbage discriminant, most plausibly arriving via
    /// a corrupted C-owned ring buffer entry (see pico-link-6o2) or a
    /// version-skewed caller. Incremented instead of matched-on-and-panicking
    /// (see [`pl_ui_input`]/[`pl_ui_push_event`]'s doc comments) so a bad tag
    /// is graceful *and* observable rather than silently swallowed --
    /// mirrors `firmware/src/input.c`'s own `s_ring_drop_count` diagnostic
    /// pattern for "things dropped that a healthy system should chase."
    /// Read via [`pl_ui_malformed_tag_count`].
    malformed_tag_count: u32,
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

    let ui = PlUi { app: App::new(width, height), malformed_tag_count: 0 };
    Box::into_raw(Box::new(ui))
}

/// Returns how many `pl_ui_input`/`pl_ui_push_event` calls have carried a
/// tag value with no corresponding enum variant since this instance was
/// created -- see [`PlUi::malformed_tag_count`]'s doc comment. Never
/// resets. Returns 0 if `ui` is null.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_malformed_tag_count(ui: *mut PlUi) -> u32 {
    if ui.is_null() {
        return 0;
    }
    // SAFETY: caller contract above.
    let ui = &*ui;
    ui.malformed_tag_count
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
/// Explicit discriminants (pinned, not compiler-assigned) because these
/// exact integers are now part of the wire ABI: [`PlIntent::tag`] carries
/// this value as a plain `u32`, not as a `PlIntentTag`-typed field -- see
/// that field's doc comment for why. `PlIntentTag` itself stays a real Rust
/// enum purely so the cbindgen header keeps emitting named `PL_INTENT_TAG_*`
/// C constants for firmware source to use.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlIntentTag {
    Up = 0,
    Down = 1,
    Left = 2,
    Right = 3,
    JumpBy = 4,
    Select = 5,
    Back = 6,
    ShortcutX = 7,
    ShortcutY = 8,
}

impl core::convert::TryFrom<u32> for PlIntentTag {
    type Error = ();

    /// Checked conversion from the raw wire value -- the only sound way to
    /// turn a C-supplied integer into this enum. Matching directly on a
    /// `PlIntentTag`-typed field read straight off the wire (the pre-hardening
    /// shape) is undefined behaviour the instant a garbage discriminant is
    /// loaded, before any Rust code even runs a check -- see pico-link-ptu.
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlIntentTag::Up),
            1 => Ok(PlIntentTag::Down),
            2 => Ok(PlIntentTag::Left),
            3 => Ok(PlIntentTag::Right),
            4 => Ok(PlIntentTag::JumpBy),
            5 => Ok(PlIntentTag::Select),
            6 => Ok(PlIntentTag::Back),
            7 => Ok(PlIntentTag::ShortcutX),
            8 => Ok(PlIntentTag::ShortcutY),
            _ => Err(()),
        }
    }
}

/// One input event as C constructs it. See [`PlIntentTag`]'s doc comment
/// for the `jump_by` field's contract.
///
/// `tag` is a plain `u32`, not [`PlIntentTag`] -- deliberately, so that
/// reading a `PlIntent` off the wire (this struct is passed by value, e.g.
/// via [`pl_ui_input`]'s `intents` slice) can never itself be undefined
/// behaviour no matter what bit pattern C supplies. The numeric values
/// match [`PlIntentTag`]'s pinned discriminants exactly, so the wire layout
/// and every existing tag's numeric value are unchanged from before this
/// hardening (see pico-link-ptu). Convert via
/// `NavIntent::try_from`/[`PlIntentTag::try_from`] rather than transmuting.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlIntent {
    pub tag: u32,
    pub jump_by: i16,
}

impl core::convert::TryFrom<PlIntent> for NavIntent {
    type Error = ();

    fn try_from(intent: PlIntent) -> Result<Self, Self::Error> {
        let tag = PlIntentTag::try_from(intent.tag)?;
        Ok(match tag {
            PlIntentTag::Up => NavIntent::Up,
            PlIntentTag::Down => NavIntent::Down,
            PlIntentTag::Left => NavIntent::Left,
            PlIntentTag::Right => NavIntent::Right,
            PlIntentTag::JumpBy => NavIntent::JumpBy(intent.jump_by),
            PlIntentTag::Select => NavIntent::Select,
            PlIntentTag::Back => NavIntent::Back,
            PlIntentTag::ShortcutX => NavIntent::ShortcutX,
            PlIntentTag::ShortcutY => NavIntent::ShortcutY,
        })
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
    // initialized `PlIntent` values for the duration of this call. Reading
    // them is sound regardless of `tag`'s value because `PlIntent::tag` is a
    // plain `u32` -- see that field's doc comment.
    let slice = core::slice::from_raw_parts(intents, count);
    let mut mapped: Vec<NavIntent> = Vec::with_capacity(count);
    for raw in slice.iter().copied() {
        match NavIntent::try_from(raw) {
            Ok(intent) => mapped.push(intent),
            // A malformed tag drops just this one intent, not the whole
            // batch -- graceful, and counted rather than silent (see
            // `PlUi::malformed_tag_count`'s doc comment).
            Err(()) => ui.malformed_tag_count += 1,
        }
    }
    if !mapped.is_empty() {
        ui.app.handle_input(mapped);
    }
}

/// One tick of the app core's clock, with `now_us` C's own timestamp (C
/// owns the clock under the FFI direction rule -- Rust never reads a
/// hardware timer itself). Previously discarded `now_us` entirely (see
/// pico-link-a67) -- now recorded via [`App::tick`] so the core actually has
/// a clock available, for future time-driven repaint sources (e.g. a live
/// link-status/liveness indicator during multi-second waits, per the
/// approved on-device UI design). A no-op if `ui` is null.
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
    let ui = &mut *ui;
    ui.app.tick(now_us);
}

/// Renders the current screen -- unconditionally, every call, regardless of
/// `App::dirty()` (unlike `pico_link_core::run::Runner::step`'s dirty gate,
/// which this FFI surface does NOT mirror: `App::render` itself has no
/// dirty check, only `Runner`/`run` do, and neither is in the M1 FFI
/// surface). Idempotent -- calling it twice with no intervening
/// `pl_ui_input`/`pl_ui_tick` produces the identical frame both times -- but
/// C should not assume a cheap early-out here; skipping a redundant blit
/// when nothing changed is C's own call to make, not something this
/// function does for it. Hands back a borrowed pointer to the raw RGB565
/// pixel data plus its length in pixels
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

// --- The Bluetooth link surface: one PlEvent union in, one PlCommand union out ---
//
// Restructured in pico-link-a67 from three per-field setters
// (`pl_ui_set_link_state`/`pl_ui_add_device`/`pl_ui_clear_devices`) into a
// single tagged-union event surface, per Ada's sustainable-path design
// (bead pico-link-aii.1's comments). `core` itself has no idea BTstack
// exists -- it only knows `Event`/`LinkState`/`DeviceEntry`/`Command`/
// `ConnectFailureReason` (see `pico_link_core::app`'s doc comments) -- this
// is the seam that adapts those to a C-friendly ABI, same pattern as
// `PlIntent`/`NavIntent` above.
//
// # Why a union, not more flat setters
//
// The old shape's failure mode was structural: every new field the
// approved on-device UI design needs (codec, bitrate, per-device codec
// availability + reason, volume, ...) would have been another setter, each
// one a new call site in C *and* Rust, with no way to express "this failed,
// here's why." A tagged union scales by adding a payload struct + a tag
// variant -- existing tags/payloads are untouched, so old C call sites
// don't need to change when a new event kind is added later (only new call
// sites do).
//
// # ABI version guard
//
// [`PlEvent`]/[`PlCommand`] each carry a `version` field, checked by the
// consumer (`pl_ui_push_event` here for events; `bt.c`'s own poll loop for
// commands) against [`PL_EVENT_ABI_VERSION`]/[`PL_COMMAND_ABI_VERSION`]
// before the `payload` union is read at all. In this repo the cbindgen
// header is regenerated from this file on every firmware build (see
// `firmware/CMakeLists.txt`), so producer and consumer can't drift within
// one build -- but a version field costs nothing and is exactly the kind of
// guard that turns "add a variant, forget to update every call site" from a
// silent wrong-union-arm read into a defensive no-op/ignore instead. C
// literals that zero-initialize `version` (e.g. `PlEvent event = {0};` with
// the field never set) are caught by this the same way a version *mismatch*
// would be.
//
// # The IRQ-context note (pico-link-6o2, NOT fixed here)
//
// `bt.c` currently calls what is now `pl_ui_push_event` directly from
// BTstack's background IRQ context, which violates the "nothing calls into
// Rust from interrupt context" rule pico-link-5am established -- tracked
// separately as pico-link-6o2 and deliberately not fixed in this bead. This
// one-event-at-a-time, pass-by-value `pl_ui_push_event(ui, PlEvent)` shape
// is exactly what that fix needs, though: C growing its own ring buffer of
// `PlEvent` values (filled from IRQ context, drained by the superloop in
// thread context, each drained event handed to this same function) is a
// change entirely on the C side of this seam -- this function's signature
// doesn't need to change for that fix to land.

/// Mirrors [`pico_link_core::LinkState`]'s four variants 1:1. Explicit
/// discriminants (pinned, not compiler-assigned) for the same reason as
/// [`PlIntentTag`]'s: [`PlLinkStateChangedPayload::state`] carries this
/// value as a plain `u32`, not as a `PlLinkState`-typed field -- see that
/// field's doc comment. `PlLinkState` itself stays a real Rust enum purely
/// so the cbindgen header keeps emitting named `PL_LINK_STATE_*` C
/// constants for firmware source to use.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlLinkState {
    Idle = 0,
    Scanning = 1,
    Connecting = 2,
    Connected = 3,
}

impl core::convert::TryFrom<u32> for PlLinkState {
    type Error = ();

    /// Checked conversion from the raw wire value -- same hazard, same fix
    /// as [`PlIntentTag`]'s `TryFrom` impl (pico-link-ptu): this payload
    /// sits inside [`PlEventPayload`], a union C constructs and
    /// [`pl_ui_push_event`] receives by value, one layer beneath the outer
    /// tag this bead originally hardened.
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlLinkState::Idle),
            1 => Ok(PlLinkState::Scanning),
            2 => Ok(PlLinkState::Connecting),
            3 => Ok(PlLinkState::Connected),
            _ => Err(()),
        }
    }
}

impl From<PlLinkState> for LinkState {
    fn from(state: PlLinkState) -> Self {
        match state {
            PlLinkState::Idle => LinkState::Idle,
            PlLinkState::Scanning => LinkState::Scanning,
            PlLinkState::Connecting => LinkState::Connecting,
            PlLinkState::Connected => LinkState::Connected,
        }
    }
}

/// Mirrors [`pico_link_core::ConnectFailureReason`]'s five variants 1:1.
/// See that type's doc comment for what each means and which two are
/// structurally non-retryable. Explicit discriminants pinned for the same
/// reason as [`PlLinkState`]'s -- see [`PlConnectFailedPayload::reason`]'s
/// doc comment.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlFailureReason {
    Timeout = 0,
    Rejected = 1,
    NoA2dpSink = 2,
    NeedsPin = 3,
    RadioError = 4,
}

impl core::convert::TryFrom<u32> for PlFailureReason {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlLinkState`]'s
    /// `TryFrom` impl for the full rationale (pico-link-ptu).
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlFailureReason::Timeout),
            1 => Ok(PlFailureReason::Rejected),
            2 => Ok(PlFailureReason::NoA2dpSink),
            3 => Ok(PlFailureReason::NeedsPin),
            4 => Ok(PlFailureReason::RadioError),
            _ => Err(()),
        }
    }
}

impl From<PlFailureReason> for ConnectFailureReason {
    fn from(reason: PlFailureReason) -> Self {
        match reason {
            PlFailureReason::Timeout => ConnectFailureReason::Timeout,
            PlFailureReason::Rejected => ConnectFailureReason::Rejected,
            PlFailureReason::NoA2dpSink => ConnectFailureReason::NoA2dpSink,
            PlFailureReason::NeedsPin => ConnectFailureReason::NeedsPin,
            PlFailureReason::RadioError => ConnectFailureReason::RadioError,
        }
    }
}

/// Mirrors [`pico_link_core::ConnectStep`]'s four variants 1:1 (added by
/// pico-link-znb.7 / E5, the pairing wizard's phase-4 named sub-steps).
/// Explicit discriminants pinned for the same reason as [`PlLinkState`]'s.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlConnectStep {
    Connecting = 0,
    Pairing = 1,
    SettingUpAudio = 2,
    NegotiatingCodec = 3,
}

impl core::convert::TryFrom<u32> for PlConnectStep {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlLinkState`]'s
    /// `TryFrom` impl for the full rationale (pico-link-ptu).
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlConnectStep::Connecting),
            1 => Ok(PlConnectStep::Pairing),
            2 => Ok(PlConnectStep::SettingUpAudio),
            3 => Ok(PlConnectStep::NegotiatingCodec),
            _ => Err(()),
        }
    }
}

impl From<PlConnectStep> for ConnectStep {
    fn from(step: PlConnectStep) -> Self {
        match step {
            PlConnectStep::Connecting => ConnectStep::Connecting,
            PlConnectStep::Pairing => ConnectStep::Pairing,
            PlConnectStep::SettingUpAudio => ConnectStep::SettingUpAudio,
            PlConnectStep::NegotiatingCodec => ConnectStep::NegotiatingCodec,
        }
    }
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::LinkStateChanged`.
///
/// `state` is a plain `u32`, not [`PlLinkState`] -- deliberately, for the
/// same reason as [`PlEvent::tag`]: this payload sits inside the
/// [`PlEventPayload`] union, which [`pl_ui_push_event`] receives by value,
/// so a `PlLinkState`-typed field here would already be undefined behaviour
/// to read the moment C hands in a garbage discriminant. The numeric values
/// match [`PlLinkState`]'s pinned discriminants exactly, so the wire layout
/// is unchanged (see pico-link-ptu). Convert via [`PlLinkState::try_from`]
/// rather than transmuting.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlLinkStateChangedPayload {
    pub state: u32,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::DeviceDiscovered`. `name`
/// points to `name_len` bytes of UTF-8 text, not necessarily
/// NUL-terminated; invalid UTF-8 is replaced lossily rather than rejected
/// (see the module doc's memory rules). A null `name` (regardless of
/// `name_len`) is treated as an empty device name. `name`/`addr` are
/// borrowed for the duration of the [`pl_ui_push_event`] call only.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDeviceDiscoveredPayload {
    pub addr: [u8; 6],
    pub name: *const u8,
    pub name_len: usize,
    pub rssi: i8,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::ConnectFailed`.
///
/// `reason` is a plain `u32`, not [`PlFailureReason`] -- same reason and
/// same fix as [`PlLinkStateChangedPayload::state`]; see that field's doc
/// comment (pico-link-ptu).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectFailedPayload {
    pub addr: [u8; 6],
    pub reason: u32,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::ConnectStepChanged`.
/// `step` is a plain `u32`, not [`PlConnectStep`] -- same reason as
/// [`PlLinkStateChangedPayload::state`]; see that field's doc comment.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectStepChangedPayload {
    pub step: u32,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::ConnectRetrying`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectRetryingPayload {
    pub attempt: u16,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::ConnectSucceeded`.
/// `degraded` is a plain `u8` (0/1), not `bool` -- this project's existing
/// convention throughout this module is to keep every union-member field a
/// plain integer type with no validity invariant of its own (a `bool` read
/// from a garbage byte is already undefined behaviour in Rust; a `u8`
/// isn't), same rationale as every `u32` tag/state/reason field above. Any
/// nonzero value is treated as `true`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectSucceededPayload {
    pub degraded: u8,
}

/// Which variant of [`PlEventPayload`] is active in a given [`PlEvent`].
/// `DevicesCleared`/`WizardAutoDismiss` carry no data -- the payload union
/// is simply unread for those tags (see [`PlEventPayload`]'s doc comment).
/// Explicit discriminants (pinned, not compiler-assigned) for the same
/// reason as [`PlIntentTag`]'s: [`PlEvent::tag`] carries this value as a
/// plain `u32`, and these numbers are the wire ABI. See
/// [`PlEvent::tag`]'s doc comment.
///
/// The last four variants were added by pico-link-znb.7 (E5, the pairing
/// wizard) -- purely additive, so [`PL_EVENT_ABI_VERSION`] is unchanged;
/// see [`pico_link_core::Event`]'s doc comment for the design-doc
/// rationale each one closes.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlEventTag {
    LinkStateChanged = 0,
    DeviceDiscovered = 1,
    DevicesCleared = 2,
    ConnectFailed = 3,
    ConnectStepChanged = 4,
    ConnectRetrying = 5,
    ConnectSucceeded = 6,
    WizardAutoDismiss = 7,
}

impl core::convert::TryFrom<u32> for PlEventTag {
    type Error = ();

    /// Checked conversion from the raw wire value -- see
    /// [`PlIntentTag`]'s `TryFrom` impl for the full rationale (same
    /// hazard, same fix, pico-link-ptu).
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlEventTag::LinkStateChanged),
            1 => Ok(PlEventTag::DeviceDiscovered),
            2 => Ok(PlEventTag::DevicesCleared),
            3 => Ok(PlEventTag::ConnectFailed),
            4 => Ok(PlEventTag::ConnectStepChanged),
            5 => Ok(PlEventTag::ConnectRetrying),
            6 => Ok(PlEventTag::ConnectSucceeded),
            7 => Ok(PlEventTag::WizardAutoDismiss),
            _ => Err(()),
        }
    }
}

/// The union of every [`PlEvent`] payload shape. Which field is valid to
/// read is determined entirely by the sibling `tag` field on [`PlEvent`] --
/// reading the wrong field is a logic bug, not a memory-safety one (every
/// member is a plain, `Copy`, no-`Drop` payload struct), but is still
/// meaningless data. `PlEventTag::DevicesCleared`/`WizardAutoDismiss` have
/// no payload of their own; the union simply isn't read for those tags, so
/// no placeholder member is needed for either.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PlEventPayload {
    pub link_state_changed: PlLinkStateChangedPayload,
    pub device_discovered: PlDeviceDiscoveredPayload,
    pub connect_failed: PlConnectFailedPayload,
    pub connect_step_changed: PlConnectStepChangedPayload,
    pub connect_retrying: PlConnectRetryingPayload,
    pub connect_succeeded: PlConnectSucceededPayload,
}

/// ABI version [`PlEvent`] producers (C call sites) must set on every
/// value. Bumped whenever an existing tag's payload shape changes in a way
/// that isn't purely additive (a new tag/payload variant does not need a
/// bump -- old tags are unaffected); see the module section doc for the
/// version-guard rationale.
pub const PL_EVENT_ABI_VERSION: u32 = 1;

/// One inbound Bluetooth-domain event, C -> Rust -- the single entry point
/// replacing the old `pl_ui_set_link_state`/`pl_ui_add_device`/
/// `pl_ui_clear_devices` setter trio. See the module section doc above for
/// the union-vs-setters rationale and the ABI version guard.
/// `tag` is a plain `u32`, not [`PlEventTag`] -- deliberately, for the same
/// reason as [`PlIntent::tag`]: [`PlEvent`] is passed by value across the
/// FFI boundary (see [`pl_ui_push_event`]), so a `PlEventTag`-typed field
/// would already be undefined behaviour to read the moment C hands in a
/// garbage discriminant, before [`pl_ui_push_event`]'s body runs at all. The
/// numeric values match [`PlEventTag`]'s pinned discriminants exactly, so
/// the wire layout and every existing tag's numeric value are unchanged
/// from before this hardening (see pico-link-ptu). Convert via
/// [`PlEventTag::try_from`] rather than transmuting.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlEvent {
    pub version: u32,
    pub tag: u32,
    pub payload: PlEventPayload,
}

/// Pushes one [`PlEvent`] into the app core, folding it into the live
/// Bluetooth model and refreshing the root screen
/// (`App::handle_event`'s FFI entry point). A no-op if `ui` is null,
/// `event.version` doesn't match [`PL_EVENT_ABI_VERSION`] (see the module
/// section doc's ABI version guard -- a mismatch means the `payload` union
/// must not be read under this build's variant shapes), or `event.tag`
/// doesn't correspond to any [`PlEventTag`] variant -- the latter case
/// increments [`PlUi::malformed_tag_count`] before returning.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. For `PlEventTag::DeviceDiscovered`, if `payload.device_discovered.name`
/// is non-null it must point to at least `name_len` valid bytes, borrowed
/// for the duration of this call only.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_push_event(ui: *mut PlUi, event: PlEvent) {
    if ui.is_null() || event.version != PL_EVENT_ABI_VERSION {
        return;
    }
    // SAFETY: caller contract above.
    let ui = &mut *ui;

    // Checked conversion from the raw wire tag -- matches the shape of the
    // `event.version` guard above (early return, no panic) rather than
    // matching directly on a `PlEventTag`-typed field, which would already
    // be UB for a garbage discriminant before this line ever ran. Counted,
    // not silent -- see `PlUi::malformed_tag_count`'s doc comment.
    let tag = match PlEventTag::try_from(event.tag) {
        Ok(tag) => tag,
        Err(()) => {
            ui.malformed_tag_count += 1;
            return;
        }
    };

    let core_event = match tag {
        PlEventTag::LinkStateChanged => {
            // SAFETY: `tag` says this union currently holds `link_state_changed`.
            // Reading `payload` itself is sound regardless of `state`'s
            // value because `PlLinkStateChangedPayload::state` is a plain
            // `u32` -- see that field's doc comment.
            let payload = unsafe { event.payload.link_state_changed };
            let state = match PlLinkState::try_from(payload.state) {
                Ok(state) => state,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            Event::LinkStateChanged(state.into())
        }
        PlEventTag::DeviceDiscovered => {
            // SAFETY: `tag` says this union currently holds `device_discovered`.
            let payload = unsafe { event.payload.device_discovered };
            let addr = payload.addr;
            let name = if payload.name.is_null() || payload.name_len == 0 {
                String::new()
            } else {
                // SAFETY: caller contract above -- `name` points to at
                // least `name_len` valid bytes for the duration of this call.
                let bytes = unsafe { core::slice::from_raw_parts(payload.name, payload.name_len) };
                String::from_utf8_lossy(bytes).into_owned()
            };
            Event::DeviceDiscovered(DeviceEntry { addr, name, rssi: payload.rssi })
        }
        PlEventTag::DevicesCleared => Event::DevicesCleared,
        PlEventTag::ConnectFailed => {
            // SAFETY: `tag` says this union currently holds `connect_failed`.
            // Reading `payload` itself is sound regardless of `reason`'s
            // value because `PlConnectFailedPayload::reason` is a plain
            // `u32` -- see that field's doc comment.
            let payload = unsafe { event.payload.connect_failed };
            let reason = match PlFailureReason::try_from(payload.reason) {
                Ok(reason) => reason,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            Event::ConnectFailed { addr: payload.addr, reason: reason.into() }
        }
        PlEventTag::ConnectStepChanged => {
            // SAFETY: `tag` says this union currently holds `connect_step_changed`.
            let payload = unsafe { event.payload.connect_step_changed };
            let step = match PlConnectStep::try_from(payload.step) {
                Ok(step) => step,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            Event::ConnectStepChanged(step.into())
        }
        PlEventTag::ConnectRetrying => {
            // SAFETY: `tag` says this union currently holds `connect_retrying`.
            // Reading it is sound regardless of `attempt`'s value -- it's a
            // plain `u16` with no validity invariant to violate.
            let payload = unsafe { event.payload.connect_retrying };
            Event::ConnectRetrying { attempt: payload.attempt }
        }
        PlEventTag::ConnectSucceeded => {
            // SAFETY: `tag` says this union currently holds `connect_succeeded`.
            // Reading it is sound regardless of `degraded`'s value -- see
            // `PlConnectSucceededPayload`'s doc comment.
            let payload = unsafe { event.payload.connect_succeeded };
            Event::ConnectSucceeded { degraded: payload.degraded != 0 }
        }
        PlEventTag::WizardAutoDismiss => Event::WizardAutoDismiss,
    };
    ui.app.handle_event(core_event);
}

/// Which variant of [`PlCommand`] this value is. `None` is not one of
/// [`pico_link_core::Command`]'s variants; it exists purely so
/// [`pl_ui_poll_command`] has a value to return when nothing is queued (or
/// `ui` is null, or `ui`'s version check fails), since this function
/// returns by value rather than an `Option`-shaped pointer.
///
/// Unlike [`PlIntentTag`]/[`PlEventTag`], this tag never needs a checked
/// conversion: Rust is the sole producer of every [`PlCommand`] (see
/// [`pl_ui_poll_command`]) and never constructs one from a raw integer, so
/// there is no C-supplied bit pattern this type is ever read from -- see
/// pico-link-ptu. Discriminants are still pinned explicitly (not
/// compiler-assigned) purely so [`PL_COMMAND_ABI_VERSION`]'s "wire ABI"
/// framing applies uniformly to all three tag enums in this module.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlCommandTag {
    None = 0,
    StartScan = 1,
    Connect = 2,
    /// pico-link-znb.2 (E1, MVP-blocking): user-initiated cancel of an
    /// in-flight inquiry scan. Carries no payload -- see
    /// [`PlCommandPayload`]'s doc comment; `pl_ui_poll_command` reuses the
    /// zeroed `connect` payload for it, same as `StartScan`.
    CancelScan = 3,
    /// pico-link-znb.7 code-review fix: user-initiated abort of an
    /// in-flight connect attempt (the wizard's phase 4/5, design section
    /// 9 -- "B genuinely aborts", not just leaves the screen). Carries
    /// the target `addr` via the same `connect` payload member
    /// `PlCommandTag::Connect` uses. **Plumbed through this FFI surface
    /// but deliberately left unhandled on the C side by this bead** --
    /// real abort semantics (tearing down an in-flight ACL/SSP/AVDTP
    /// attempt) is real BT work beyond this bead's scope, the same way
    /// `CancelScan`'s C-side handling needed its own follow-up bead
    /// (pico-link-znb.2). See this bead's completion report for the
    /// explicit callout; file the C-side bead against this tag.
    CancelConnect = 4,
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::Connect`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectPayload {
    pub addr: [u8; 6],
}

/// The union of every [`PlCommand`] payload shape -- mirrors
/// [`PlEventPayload`]'s shape (one member per tag that carries data;
/// `StartScan`/`None` carry none). Kept as a real union rather than a flat
/// `addr` field specifically so this scales the same way `PlEvent` does:
/// planned future commands (per-device codec set, LDAC quality set, AVRCP
/// volume set -- see `.planning/design/2026-08-28-on-device-ui.md` section
/// 14's "Ada" handoff) have payload shapes `addr` alone can't carry, and
/// adding them means adding a payload struct + tag variant here, not
/// reshaping this one.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PlCommandPayload {
    pub connect: PlConnectPayload,
}

/// ABI version [`PlCommand`] consumers (C call sites, i.e. `bt.c`'s poll
/// loop) must check against before reading `payload`. Rust is the sole
/// producer of `PlCommand` values (see [`pl_ui_poll_command`]) and always
/// sets this correctly; the check exists on the C side as the same
/// defensive belt-and-suspenders guard [`PL_EVENT_ABI_VERSION`] is for
/// events -- see the module section doc.
pub const PL_COMMAND_ABI_VERSION: u32 = 1;

/// One user-initiated command, Rust -> C. See [`PlCommandPayload`]'s doc
/// comment for the extensibility rationale and [`PL_COMMAND_ABI_VERSION`]
/// for the version-guard contract.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlCommand {
    pub version: u32,
    pub tag: PlCommandTag,
    pub payload: PlCommandPayload,
}

/// A `PlCommand` with `tag == PlCommandTag::None`, `version` already set
/// correctly -- the "nothing queued" value returned by [`pl_ui_poll_command`]
/// in every case that isn't `Some(Command::{StartScan,Connect})`.
fn pl_command_none() -> PlCommand {
    PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::None, payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6] } } }
}

/// Pops the oldest user-initiated command queued by the devices screen
/// (selecting "Scan" or a discovered device row), or a
/// Maps one core-side [`Command`] to its wire [`PlCommand`] shape. A pure
/// function (no `PlUi`/FFI involved) purely so this mapping -- the actual
/// risk in this bead, per pico-link-znb.2 -- is unit-testable directly,
/// without needing to drive a queued command through `App`'s private
/// mailbox first (`CancelScan` has no UI binding yet to do that with; the
/// wizard screen in pico-link-znb.7 adds one).
fn pl_command_from(command: Command) -> PlCommand {
    match command {
        Command::StartScan => {
            PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::StartScan, payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6] } } }
        }
        Command::Connect { addr } => {
            PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::Connect, payload: PlCommandPayload { connect: PlConnectPayload { addr } } }
        }
        Command::CancelScan => {
            PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::CancelScan, payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6] } } }
        }
        Command::CancelConnect { addr } => {
            PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::CancelConnect, payload: PlCommandPayload { connect: PlConnectPayload { addr } } }
        }
    }
}

/// Pops the oldest user-initiated command queued by the devices screen
/// (selecting "Scan" or a discovered device row), or a
/// `PlCommandTag::None` command if none is pending -- callers should poll
/// this once per main-loop iteration and drain it in a loop if more than
/// one command might be queued between polls. Also returns
/// `PlCommandTag::None` if `ui` is null.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_poll_command(ui: *mut PlUi) -> PlCommand {
    if ui.is_null() {
        return pl_command_none();
    }
    // SAFETY: caller contract above.
    let ui = &mut *ui;
    match ui.app.poll_command() {
        Some(command) => pl_command_from(command),
        None => pl_command_none(),
    }
}

// --- Tests: pico-link-ptu, checked tag conversion ---
//
// Host-only (`#[cfg(test)]`, run via `cargo test -p ui-ffi` -- this crate is
// deliberately excluded from the workspace's `default-members`, see
// ../Cargo.toml, so these do not appear in a bare `cargo test` at the repo
// root). Exercises the actual `pl_ui_*` entry points, not the `TryFrom`
// impls directly in isolation, per pico-link-ptu's ask: "a test that only
// exercises legal tags does not test this bug." Every test here feeds at
// least one out-of-range tag value through `pl_ui_input`/`pl_ui_push_event`
// and asserts it is rejected gracefully (no panic, no crash, and observably
// counted) rather than merely "not observed to crash."
#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a fresh `PlUi` for one test. Freed by the caller via
    /// `pl_ui_destroy` -- these tests exercise real FFI entry points, so
    /// they follow the real ownership contract rather than reaching into
    /// `PlUi` by constructing it directly.
    fn new_ui() -> *mut PlUi {
        let ui = pl_ui_create(16, 16);
        assert!(!ui.is_null(), "pl_ui_create(16, 16) unexpectedly returned null");
        ui
    }

    #[test]
    fn pl_ui_input_rejects_out_of_range_tag_without_panicking() {
        let ui = new_ui();
        // 9 is one past PlIntentTag's highest legal discriminant (ShortcutY
        // = 8); 0xDEAD_BEEF stands in for fully garbage memory (e.g. an
        // uninitialized or corrupted ring-buffer slot per pico-link-6o2).
        let bad_intents = [
            PlIntent { tag: 9, jump_by: 0 },
            PlIntent { tag: 0xDEAD_BEEF, jump_by: 0 },
        ];
        unsafe {
            pl_ui_input(ui, bad_intents.as_ptr(), bad_intents.len());
            assert_eq!(
                pl_ui_malformed_tag_count(ui),
                2,
                "both out-of-range PlIntent tags should have been counted, not matched-on"
            );
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_input_still_applies_the_valid_intents_in_a_mixed_batch() {
        let ui = new_ui();
        // `App::handle_input` only sets `dirty` when it actually receives a
        // non-empty intent list (see its doc comment) -- a clean way to
        // prove the good intents in this batch reached the app core despite
        // the malformed one, without depending on which screen happens to
        // be root today. A fresh `App` starts dirty (it hasn't rendered its
        // initial screen yet), so render once through the real FFI entry
        // point first to get to a known-clean baseline.
        let mut out_px = core::ptr::null();
        let mut out_len = 0usize;
        unsafe { pl_ui_render(ui, &mut out_px, &mut out_len) };
        assert!(!unsafe { (*ui).app.dirty() }, "PlUi should be clean immediately after a render");
        let mixed = [
            PlIntent { tag: PlIntentTag::Down as u32, jump_by: 0 },
            PlIntent { tag: 123, jump_by: 0 },
            PlIntent { tag: PlIntentTag::Down as u32, jump_by: 0 },
        ];
        unsafe {
            pl_ui_input(ui, mixed.as_ptr(), mixed.len());
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "exactly one malformed tag in this batch");
            assert!((*ui).app.dirty(), "the two legal Down intents should still have reached App::handle_input");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_input_all_malformed_tags_leaves_app_untouched() {
        let ui = new_ui();
        let dirty_before = unsafe { (*ui).app.dirty() };
        let bad_intents = [PlIntent { tag: 255, jump_by: 0 }];
        unsafe {
            pl_ui_input(ui, bad_intents.as_ptr(), bad_intents.len());
            assert_eq!(pl_ui_malformed_tag_count(ui), 1);
            // `handle_input` is skipped entirely when nothing decoded, so a
            // batch that is *only* malformed tags must not perturb dirty
            // state at all.
            assert_eq!((*ui).app.dirty(), dirty_before);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_tag_without_panicking() {
        let ui = new_ui();
        let bogus_payload = PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Idle as u32 } };
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: 8, // one past WizardAutoDismiss = 7, the highest legal PlEventTag
            payload: bogus_payload,
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range PlEvent tag should be counted, not matched-on");
            // Must not have folded into the model -- link state stays the
            // default (Idle) since the malformed event was rejected before
            // `App::handle_event` ever ran.
            assert_eq!((*ui).app.model().link_state, LinkState::Idle);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_garbage_tag_distinct_from_version_mismatch() {
        let ui = new_ui();
        let payload = PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Connected as u32 } };
        // A wildly out-of-range tag (e.g. an uninitialized-memory pattern)
        // with an otherwise-correct version -- this must be caught by the
        // tag check, not slip through because only `version` is validated.
        let bad_event = PlEvent { version: PL_EVENT_ABI_VERSION, tag: 0xFFFF_FFFF, payload };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1);
            assert_eq!((*ui).app.model().link_state, LinkState::Idle, "rejected event must not reach App::handle_event");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_accepts_every_legal_tag() {
        let ui = new_ui();
        let events = [
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::LinkStateChanged as u32,
                payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Connected as u32 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::DevicesCleared as u32,
                payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Idle as u32 } },
            },
        ];
        unsafe {
            for event in events {
                pl_ui_push_event(ui, event);
            }
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "every tag above is legal -- none should be counted as malformed");
            assert_eq!((*ui).app.model().link_state, LinkState::Connected, "the LinkStateChanged event should have taken effect");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_accepts_the_znb7_wizard_tags() {
        // pico-link-znb.7 (E5): the four tags this bead added. Pushed
        // through `pl_ui_push_event` with the wizard closed (no way to
        // observe `WizardPhase` from this crate -- `pico_link_core::App`
        // doesn't expose it publicly), so this proves what this crate
        // *can* prove: every new tag/payload shape round-trips through the
        // checked-conversion path without being rejected as malformed and
        // without panicking, matching `pl_ui_push_event_accepts_every_legal_tag`'s
        // existing shape for the pre-existing tags.
        let ui = new_ui();
        let events = [
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::ConnectStepChanged as u32,
                payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: PlConnectStep::Pairing as u32 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::ConnectRetrying as u32,
                payload: PlEventPayload { connect_retrying: PlConnectRetryingPayload { attempt: 3 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::ConnectSucceeded as u32,
                payload: PlEventPayload { connect_succeeded: PlConnectSucceededPayload { degraded: 1 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::WizardAutoDismiss as u32,
                payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: 0 } },
            },
        ];
        unsafe {
            for event in events {
                pl_ui_push_event(ui, event);
            }
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "every tag above is legal -- none should be counted as malformed");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_nested_connect_step() {
        // Same gap as the nested link-state/failure-reason tests above,
        // one layer deeper for the new `ConnectStepChanged` payload: the
        // outer tag is legal, only the nested `step` value is garbage.
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::ConnectStepChanged as u32,
            payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: 99 } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range nested ConnectStep should be counted, not matched-on");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_connect_step_try_from_round_trips_every_legal_discriminant() {
        let legal = [PlConnectStep::Connecting, PlConnectStep::Pairing, PlConnectStep::SettingUpAudio, PlConnectStep::NegotiatingCodec];
        for step in legal {
            assert!(PlConnectStep::try_from(step as u32).is_ok());
        }
        assert!(PlConnectStep::try_from(4u32).is_err());
        assert!(PlConnectStep::try_from(u32::MAX).is_err());
    }

    #[test]
    fn pl_intent_tag_try_from_round_trips_every_legal_discriminant() {
        let legal = [
            PlIntentTag::Up,
            PlIntentTag::Down,
            PlIntentTag::Left,
            PlIntentTag::Right,
            PlIntentTag::JumpBy,
            PlIntentTag::Select,
            PlIntentTag::Back,
            PlIntentTag::ShortcutX,
            PlIntentTag::ShortcutY,
        ];
        for tag in legal {
            assert!(PlIntentTag::try_from(tag as u32).is_ok());
        }
        assert!(PlIntentTag::try_from(9u32).is_err());
        assert!(PlIntentTag::try_from(u32::MAX).is_err());
    }

    #[test]
    fn pl_event_tag_try_from_round_trips_every_legal_discriminant() {
        let legal = [
            PlEventTag::LinkStateChanged,
            PlEventTag::DeviceDiscovered,
            PlEventTag::DevicesCleared,
            PlEventTag::ConnectFailed,
            PlEventTag::ConnectStepChanged,
            PlEventTag::ConnectRetrying,
            PlEventTag::ConnectSucceeded,
            PlEventTag::WizardAutoDismiss,
        ];
        for tag in legal {
            assert!(PlEventTag::try_from(tag as u32).is_ok());
        }
        assert!(PlEventTag::try_from(8u32).is_err());
        assert!(PlEventTag::try_from(u32::MAX).is_err());
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_nested_link_state() {
        // The outer tag (LinkStateChanged) is legal -- only the *nested*
        // `state` field, one layer deeper in the union, is garbage. This is
        // the exact gap the tag-only hardening left open: `PlEventTag`
        // alone being valid says nothing about `PlLinkStateChangedPayload`.
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: 99 } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range nested PlLinkState should be counted, not matched-on");
            assert_eq!((*ui).app.model().link_state, LinkState::Idle, "rejected event must not reach App::handle_event");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_garbage_nested_link_state() {
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: 0xFFFF_FFFF } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1);
            assert_eq!((*ui).app.model().link_state, LinkState::Idle);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_nested_failure_reason() {
        // Same gap, other nested payload: `PlEventTag::ConnectFailed` is
        // legal, `PlConnectFailedPayload::reason` is garbage.
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::ConnectFailed as u32,
            payload: PlEventPayload { connect_failed: PlConnectFailedPayload { addr: [0xAA; 6], reason: 255 } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range nested PlFailureReason should be counted, not matched-on");
            assert_eq!((*ui).app.model().last_connect_failure, None, "rejected event must not reach App::handle_event");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_garbage_nested_failure_reason() {
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::ConnectFailed as u32,
            payload: PlEventPayload { connect_failed: PlConnectFailedPayload { addr: [0xAA; 6], reason: 0xDEAD_BEEF } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1);
            assert_eq!((*ui).app.model().last_connect_failure, None);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_accepts_every_legal_nested_link_state_and_failure_reason() {
        let ui = new_ui();
        unsafe {
            for state in [PlLinkState::Idle, PlLinkState::Scanning, PlLinkState::Connecting, PlLinkState::Connected] {
                let event = PlEvent {
                    version: PL_EVENT_ABI_VERSION,
                    tag: PlEventTag::LinkStateChanged as u32,
                    payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: state as u32 } },
                };
                pl_ui_push_event(ui, event);
            }
            for reason in [
                PlFailureReason::Timeout,
                PlFailureReason::Rejected,
                PlFailureReason::NoA2dpSink,
                PlFailureReason::NeedsPin,
                PlFailureReason::RadioError,
            ] {
                let event = PlEvent {
                    version: PL_EVENT_ABI_VERSION,
                    tag: PlEventTag::ConnectFailed as u32,
                    payload: PlEventPayload { connect_failed: PlConnectFailedPayload { addr: [0; 6], reason: reason as u32 } },
                };
                pl_ui_push_event(ui, event);
            }
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "every nested state/reason above is legal -- none should be counted as malformed");
            assert_eq!(
                (*ui).app.model().last_connect_failure,
                Some(([0; 6], ConnectFailureReason::RadioError)),
                "the last legal ConnectFailed event should have taken effect"
            );
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_link_state_try_from_round_trips_every_legal_discriminant() {
        let legal = [PlLinkState::Idle, PlLinkState::Scanning, PlLinkState::Connecting, PlLinkState::Connected];
        for state in legal {
            assert!(PlLinkState::try_from(state as u32).is_ok());
        }
        assert!(PlLinkState::try_from(4u32).is_err());
        assert!(PlLinkState::try_from(u32::MAX).is_err());
    }

    #[test]
    fn pl_failure_reason_try_from_round_trips_every_legal_discriminant() {
        let legal =
            [PlFailureReason::Timeout, PlFailureReason::Rejected, PlFailureReason::NoA2dpSink, PlFailureReason::NeedsPin, PlFailureReason::RadioError];
        for reason in legal {
            assert!(PlFailureReason::try_from(reason as u32).is_ok());
        }
        assert!(PlFailureReason::try_from(5u32).is_err());
        assert!(PlFailureReason::try_from(u32::MAX).is_err());
    }

    #[test]
    fn cancel_scan_command_maps_to_the_cancel_scan_tag_with_the_current_abi_version() {
        // pico-link-znb.2 (E1): the wire-mapping half of the round trip
        // core::app's `cancel_scan_command_round_trips_through_poll_command`
        // proves on the `App`/`poll_command` side. `CancelScan` has no UI
        // binding yet (the wizard screen in pico-link-znb.7 adds one), so
        // this drives `pl_command_from` directly rather than through
        // `pl_ui_poll_command` end to end.
        let wire = pl_command_from(Command::CancelScan);
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::CancelScan as u32);
    }

    #[test]
    fn cancel_connect_command_maps_to_the_cancel_connect_tag_and_carries_the_addr() {
        // pico-link-znb.7's code-review fix: B during the wizard's
        // connecting/not-responding phases now queues this instead of
        // silently leaving the abandoned attempt running in C. Plumbed
        // through the FFI surface by this bead; left unhandled C-side
        // (see `PlCommandTag::CancelConnect`'s doc comment).
        let addr = [0xAA; 6];
        let wire = pl_command_from(Command::CancelConnect { addr });
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::CancelConnect as u32);
        // SAFETY: `wire.tag` above confirms the union currently holds `connect`.
        assert_eq!(unsafe { wire.payload.connect.addr }, addr);
    }

    #[test]
    fn start_scan_and_connect_still_map_to_their_own_tags() {
        // Regression guard: extracting `pl_command_from` out of
        // `pl_ui_poll_command` (pico-link-znb.2) must not change the
        // existing StartScan/Connect mappings.
        let start = pl_command_from(Command::StartScan);
        assert_eq!(start.tag as u32, PlCommandTag::StartScan as u32);

        let addr = [1, 2, 3, 4, 5, 6];
        let connect = pl_command_from(Command::Connect { addr });
        assert_eq!(connect.tag as u32, PlCommandTag::Connect as u32);
        assert_eq!(unsafe { connect.payload.connect.addr }, addr);
    }
}
