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
use pico_link_core::platform::DisplayPower;
use pico_link_core::run::IdlePolicy;
use pico_link_core::{
    App, Command, ConnectFailureReason, ConnectStep, ConnectedCodec, DeviceEntry, Event, LinkState, NavIntent, PairedDevice,
    StoreStatus, DEFAULT_IDLE_TIMEOUT,
};

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
// `#[cfg(all(not(test), target_os = "none"))]`: under `cargo test -p ui-ffi`
// this crate builds with `std` linked in (see the crate root's
// `cfg_attr(not(test), no_std)`), and the heap arena below (the only thing
// that ever calls into `critical_section`) is itself `#[cfg(not(test))]`'d
// away in favour of std's own allocator -- so no `critical_section::Impl`
// is ever needed, or registered, under test. Gated further on
// `target_os = "none"` (true only for the bare-metal thumbv8m target, see
// the panic handler's doc comment above for why `not(test)` alone is not
// enough): the inline `asm!` blocks below are Armv8-M-only mnemonics
// (`mrs`/`cpsid`/`cpsie`) that do not assemble for a host-native
// `cargo build --workspace` (aarch64/x86_64), and are not needed there
// either -- a plain host build never links this staticlib into a running
// binary, so no `critical_section::Impl` ever needs to be resolved for it.
#[cfg(all(not(test), target_os = "none"))]
struct SingleCoreCriticalSection;
#[cfg(all(not(test), target_os = "none"))]
critical_section::set_impl!(SingleCoreCriticalSection);

// SAFETY: `acquire`/`release` correctly save and restore the interrupt
// mask (PRIMASK) around the critical section, per `critical_section::Impl`'s
// contract -- interrupts are disabled for the duration and restored to
// exactly their prior state afterward, and these two calls are never
// reordered or elided (`acquire` returns the token `release` consumes).
#[cfg(all(not(test), target_os = "none"))]
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
// no_std panic handler can safely do. Gated on `target_os = "none"` (true
// only for the bare-metal thumbv8m target), not merely `not(test)`: a plain
// host-native `cargo build --workspace` (no `--target`, `default-members`
// notwithstanding -- `--workspace` overrides it) also satisfies
// `not(test)`, but that build links against host `std` transitively (via
// this crate's dependency graph), and `std` already defines the
// `panic_impl` lang item. `#![no_std]` on this crate does not stop a
// dependency elsewhere in the graph from pulling `std` in when targeting a
// real OS -- only building for a target with no OS (`target_os = "none"`)
// removes `std` from the graph structurally. A crate linked into a `std`
// binary (host build or `#[cfg(test)]`) must not define its own
// `#[panic_handler]` -- std already provides one (ordinary unwinding
// panics, which `#[test]` relies on for `#[should_panic]` and for
// reporting a failing assertion).
#[cfg(all(not(test), target_os = "none"))]
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
    /// The idle-screensaver/deep-sleep decision, shared with the
    /// emulator's `Runner::step` via `pico_link_core::run::IdlePolicy` --
    /// see pico-link-i3e and
    /// `.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`.
    /// Deep sleep stays unreachable here: constructed with
    /// `deep_sleep_timeout: None` (see [`pl_ui_create`]'s doc comment),
    /// matching `crate::power::DEEP_SLEEP_ARMED == false` today.
    idle: IdlePolicy,
    /// Set by [`pl_ui_input`] whenever a call carries at least one intent
    /// (`count > 0`, valid or malformed), and consumed (reset to `false`)
    /// by the next [`pl_ui_tick`] call. `pl_ui_input` has no clock
    /// available -- only `pl_ui_tick`'s `now_us` does -- so this flag is
    /// how "input arrived this frame" crosses from one FFI call to the
    /// next, mirroring `Runner::step`'s single-call `had_input` local
    /// (there, input poll and idle tick happen in the same call; here they
    /// are necessarily two separate C calls per frame).
    input_since_last_tick: bool,
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
/// Constructs its [`IdlePolicy`] with `idle_timeout: Some(DEFAULT_IDLE_TIMEOUT)`
/// and `deep_sleep_timeout: None` -- the screensaver tier is on, matching
/// `crate::power::IdlePowerSetting::default()`'s always-on behavior (the
/// persisted toggle itself is not yet read from `Storage` here -- see
/// pico-link-i3e's follow-up beads); deep sleep stays unreachable, matching
/// `crate::power::DEEP_SLEEP_ARMED == false`. Deliberately does **not**
/// seed `IdlePolicy`'s idle clock from any timestamp here: this function
/// has no clock (see the FFI direction rule -- C owns the clock, only
/// [`pl_ui_tick`]'s `now_us` ever supplies one), and `IdlePolicy` lazily
/// initializes `last_input` on its first `tick` call for exactly this
/// reason (see that type's doc comment) -- a UI created 61s after boot
/// must not blank on its very first rendered frame.
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

    let ui = PlUi {
        app: App::new(width, height),
        idle: IdlePolicy::new(Some(DEFAULT_IDLE_TIMEOUT), None),
        input_since_last_tick: false,
        malformed_tag_count: 0,
    };
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
/// **Idle/wake:** if the display is currently blanked (see
/// [`pl_ui_display_power`]), this call only wakes it -- the batch is
/// swallowed, never forwarded to `app.handle_input`, matching
/// `pico_link_core::run::Runner::step`'s wake contract exactly (see
/// [`IdlePolicy::on_input`]'s doc comment: the wake-triggering input must
/// never itself navigate). Otherwise forwarded normally. Either way, this
/// call flags that input arrived so the following [`pl_ui_tick`] resets
/// the idle clock -- see [`PlUi::input_since_last_tick`]'s doc comment for
/// why that update is deferred to `pl_ui_tick` rather than done here.
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
    ui.input_since_last_tick = true;

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

    if ui.idle.on_input() {
        // Was Asleep, now woken: drop this batch entirely (even any
        // successfully-mapped intents) and mark the app dirty instead, so
        // the just-woken display gets a fresh flush -- see the doc comment
        // above.
        ui.app.mark_dirty();
        return;
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
/// Also arms/evaluates the idle-screensaver tier for this frame (see
/// [`IdlePolicy::tick`]): consumes [`PlUi::input_since_last_tick`] and
/// updates the idle clock accordingly. `on_external_power` is hardcoded to
/// `true` -- the firmware has no external-power sense yet (see the design
/// doc's §5.2) and this instance's `deep_sleep_timeout` is always `None`
/// (set in [`pl_ui_create`]), so the deep-sleep tier can never actually
/// fire regardless of this value; it exists only so `IdlePolicy::tick`'s
/// signature doesn't need a second, firmware-only variant. The resulting
/// power level is read separately via [`pl_ui_display_power`] -- this
/// function does not report it.
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
    let had_input = core::mem::take(&mut ui.input_since_last_tick);
    let now = pico_link_core::platform::Instant::from_micros(now_us);
    // `enter_deep_sleep` is deliberately ignored -- see the doc comment
    // above for why it can never be `true` here.
    let _decision = ui.idle.tick(now, had_input, ui.app.is_at_home_root(), true);
    ui.app.tick(now_us);
}

/// Requested panel power level, mirroring
/// [`pico_link_core::platform::DisplayPower`] for the C side of the FFI --
/// see [`pl_ui_display_power`]'s doc comment for the full contract. Pinned
/// discriminants (part of the wire ABI, same rationale as
/// [`PlLinkState`]'s).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlDisplayPower {
    On = 0,
    Off = 1,
}

impl From<DisplayPower> for PlDisplayPower {
    fn from(power: DisplayPower) -> Self {
        match power {
            DisplayPower::On => PlDisplayPower::On,
            DisplayPower::Off => PlDisplayPower::Off,
        }
    }
}

/// Current requested panel power. **A level, not an edge**: intended to be
/// read once per superloop iteration (after [`pl_ui_tick`]) and applied
/// idempotently to the backlight GPIO -- see
/// `.planning/design/2026-09-01-idle-policy-across-the-ffi-seam.md`'s §2.4
/// for why a pull-based level was chosen over a command or a callback (a
/// dropped edge would leave the backlight permanently wrong; a level
/// re-applied every frame is self-healing, including across a watchdog
/// reboot). Safe to call at any time; returns [`PlDisplayPower::On`] if
/// `ui` is null (never leaves a null `ui` looking blanked).
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_display_power(ui: *mut PlUi) -> PlDisplayPower {
    if ui.is_null() {
        return PlDisplayPower::On;
    }
    // SAFETY: caller contract above.
    let ui = &*ui;
    ui.idle.display_power().into()
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
///
/// `class_of_device` added by bead pico-link-znb.11 (E9, design section 21
/// Tier 1) -- [`PL_EVENT_ABI_VERSION`] bumped 3 -> 4 for it, same class of
/// change as [`PlConnectSucceededPayload::addr`]'s 1 -> 2 bump: a
/// non-additive payload shape change to an *existing* tag, not a new tag.
/// This is BTstack's raw 24-bit Class-of-Device field
/// (`gap_event_inquiry_result_get_class_of_device`), reported as a `u32`
/// with the top byte always zero -- carried through uninterpreted, exactly
/// like `addr` (see [`pico_link_core::app::DeviceEntry::addr`]'s doc
/// comment for why `core` never blindly interprets BTstack internals):
/// `core` decodes the major-device-class bits itself
/// (`pico_link_core::app::is_audio_sink`) rather than C pre-deciding
/// "is this an audio sink" and losing the raw value. A value of `0` means
/// BTstack did not report one (unlike `name`/`rssi`, the inquiry-result
/// accessor has no "available" flag of its own, so `0` is the only
/// distinguishable "absent" sentinel) and is treated as *unknown*, not
/// *non-audio* -- see `is_audio_sink`'s doc comment for why an unknown
/// class must never be silently excluded from the scan list.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDeviceDiscoveredPayload {
    pub addr: [u8; 6],
    pub name: *const u8,
    pub name_len: usize,
    pub rssi: i8,
    pub class_of_device: u32,
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
///
/// `addr` added by bead pico-link-cz0.6 (M5 persistence, [`PL_EVENT_ABI_VERSION`]
/// bumped 1 -> 2 for it -- a non-additive payload shape change, unlike every
/// new *tag* this module has added before): previously `core` could only
/// learn which device just succeeded by reading it back off `WizardPhase`
/// (`Connecting`/`NotResponding` both carry `addr`), which is silently
/// `None` for a connection C initiated OUTSIDE the wizard (the
/// `PL_DEBUG_REMOTE` bypass, `firmware/src/bt.c`'s `pl_bt_debug_connect` --
/// exactly the path unattended hardware testing uses). Carrying `addr`
/// directly here means auto-reconnect persistence (design point 7) works
/// identically whether the connection was driven by a real d-pad press or
/// the debug bypass.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectSucceededPayload {
    pub addr: [u8; 6],
    pub degraded: u8,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::CodecChanged`.
///
/// `name`/`name_len` carry the codec's display name as a bounded,
/// fixed-size buffer copied by value -- unlike
/// [`PlDeviceDiscoveredPayload::name`] (a borrowed pointer+len, sound
/// because the whole event is consumed synchronously within one
/// [`pl_ui_push_event`] call), this payload also lives inside
/// [`PlEventPayload`], a `Copy` union with no `Drop`; a fixed buffer keeps
/// every [`PlEventPayload`] member a plain, `Copy`, no-lifetime value with
/// the same safety story as [`PlLinkStateChangedPayload`]'s `u32` field --
/// `unsafe` review at the [`pl_ui_push_event`] call site only ever needs to
/// check `name_len as usize <= name.len()`, never a pointer's validity
/// window. Bytes past `name_len` are unspecified; `name_len` above
/// `name.len()` is treated as an empty name (see [`pl_ui_push_event`]'s
/// `CodecChanged` arm) rather than panicking or truncating silently.
///
/// `nominal_bitrate_bps` is exactly `firmware/src/codec_table.h`'s
/// `pl_codec_frame_info_t::nominal_bitrate_bps` for whichever row just
/// negotiated -- the codec table's own declared *nominal* figure, never a
/// live/adaptive one (design section 13; see
/// [`pico_link_core::app::ConnectedCodec`]'s doc comment).
///
/// `addr` identifies which device this codec applies to -- carried for
/// forward compatibility though today's `core`-side handling clears codec
/// state on any non-`Connected` `LinkStateChanged` instead of keying off
/// it (see [`pico_link_core::app::App::set_link_state`]'s doc comment).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlCodecChangedPayload {
    pub addr: [u8; 6],
    /// Fixed at 16 bytes -- headroom past the longest name
    /// `firmware/src/codec_table.h`'s table carries today ("aptX HD", 7
    /// bytes). A literal, not a named const, matching this struct's
    /// `addr` field (and `PlDeviceDiscoveredPayload::addr`) using a bare
    /// `6` -- cbindgen does not inline a module-local const into an
    /// emitted C array's size, so a named const here would fail to
    /// compile on the C side (measured while building this payload).
    pub name: [u8; 16],
    pub name_len: u8,
    pub nominal_bitrate_bps: u32,
}

/// Mirrors [`pico_link_core::StoreStatus`]'s four variants 1:1 (bead
/// pico-link-cz0.6, M5 persistence). Explicit discriminants pinned for the
/// same reason as [`PlLinkState`]'s -- see [`PlStoreLoadedPayload::status`]'s
/// doc comment. These numeric values also match `firmware/src/persist.h`'s
/// `pl_persist_status_t` C enum exactly -- both sides are pinned
/// independently rather than one generating the other (persist.h predates
/// and is not itself part of this FFI surface), so a change to either must
/// update the other by hand.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlStoreStatus {
    FirstBoot = 0,
    Loaded = 1,
    RecordCorrupt = 2,
    VersionMismatch = 3,
}

impl core::convert::TryFrom<u8> for PlStoreStatus {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlLinkState`]'s
    /// `TryFrom` impl for the full rationale (pico-link-ptu).
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlStoreStatus::FirstBoot),
            1 => Ok(PlStoreStatus::Loaded),
            2 => Ok(PlStoreStatus::RecordCorrupt),
            3 => Ok(PlStoreStatus::VersionMismatch),
            _ => Err(()),
        }
    }
}

impl From<PlStoreStatus> for StoreStatus {
    fn from(status: PlStoreStatus) -> Self {
        match status {
            PlStoreStatus::FirstBoot => StoreStatus::FirstBoot,
            PlStoreStatus::Loaded => StoreStatus::Loaded,
            PlStoreStatus::RecordCorrupt => StoreStatus::RecordCorrupt,
            PlStoreStatus::VersionMismatch => StoreStatus::VersionMismatch,
        }
    }
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::StoreLoaded` (bead
/// pico-link-cz0.6, M5 persistence). `status` is a plain `u8`, not
/// [`PlStoreStatus`] -- same reason as every other union-member
/// tag/state/reason field in this module (see
/// [`PlLinkStateChangedPayload::state`]'s doc comment); convert via
/// [`PlStoreStatus::try_from`], never by transmuting.
///
/// Reshaped by bead pico-link-4vb.6 (T2), design section 5.2 -- `has_device`
/// and `addr` are REMOVED; `count` is new. This is the load-bearing seam
/// call in that design: `has_device`/`addr` were C DECIDING which device to
/// auto-reconnect to (`pl_persist_boot_has_device`/`pl_persist_boot_device_addr`,
/// "the one slot" -- not a policy with one slot, but a real one with eight).
/// That policy belongs on `core`'s side of the seam (design point 7), so
/// this event no longer carries an opinion about it: C's boot sequence is
/// now `count` x [`PlEventTag::PairedDeviceUpserted`] (one per surviving
/// record), THEN this event as the terminator -- `core` computes the
/// reconnect target itself (`paired.iter().max_by_key(|d| d.mru_seq)`,
/// design section 5.2) once it has folded every upserted record into its
/// own model.
///
/// **`count` is not yet populated by any real producer.** `firmware/src/bt.c`
/// still calls `pl_persist_boot_has_device`/`pl_persist_boot_device_addr`
/// and constructs the OLD `{status, has_device, addr}` shape this struct no
/// longer has (T3, deferred out of this bead's scope -- see
/// `persist.h`'s doc comment on those two getters). Until T3 lands and bt.c
/// is updated to push `count` x `PairedDeviceUpserted` instead, this event
/// arriving with any `count` means nothing has actually been folded into
/// `core`'s `paired` list yet (that list doesn't even exist in `core` until
/// T4) -- `pl_ui_push_event`'s `StoreLoaded` arm below always queues
/// `device_addr: None`, deliberately NOT reading `count` to synthesize a
/// reconnect target, exactly because there is no way to sound-ly derive one
/// from a bare count.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlStoreLoadedPayload {
    pub status: u8,
    /// How many [`PlEventTag::PairedDeviceUpserted`] events preceded this
    /// one in C's boot push sequence (design section 5.2). See this
    /// struct's doc comment for why nothing reads this yet.
    pub count: u8,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::PairedDeviceUpserted`
/// (bead pico-link-4vb.6, T2, design section 5.1). One remembered (paired)
/// device the flash store holds, pushed either at boot (C's `count` x this
/// event, design section 5.2) or after a device is newly persisted /
/// updated.
///
/// `name` is an inline fixed buffer copied by value, following
/// [`PlCodecChangedPayload::name`]'s convention -- NOT
/// [`PlDeviceDiscoveredPayload::name`]'s borrowed-pointer style -- so every
/// [`PlEventPayload`] member stays `Copy` with no lifetime (design section
/// 5.1's explicit "inline fixed buffer copied by value" call-out).
///
/// **Not yet producible from C.** `firmware/src/bt.c` does not push this
/// tag yet -- that's T3 (deferred out of this bead's scope, see
/// `PlStoreLoadedPayload`'s doc comment). This tag/payload shape exists now,
/// purely additive, so the header and the rest of T2's surface land
/// together; `pl_ui_push_event` accepts it (counts as legal, not malformed)
/// but does not yet fold it into `core`'s model -- `pico_link_core::Event`
/// has no matching variant yet (T4, also deferred). See this tag's arm in
/// [`pl_ui_push_event`] for the explicit "accepted, not yet wired" handling.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlPairedDeviceUpsertedPayload {
    pub addr: [u8; 6],
    /// Fixed at 32 bytes, matching `firmware/src/persist.c`'s
    /// `pl_persist_device_record_t::name` on-flash field exactly (bead
    /// pico-link-cz0.6, M5 persistence) -- a literal, not a named const, for
    /// the same cbindgen-array-size reason as
    /// [`PlDeviceDiscoveredPayload::addr`]'s bare `6` (see that field's doc
    /// comment).
    pub name: [u8; 32],
    /// `> 32` is treated as an empty name, same convention as
    /// [`PlCodecChangedPayload::name_len`].
    pub name_len: u8,
    /// Monotonic use-sequence (design section 3's [`PairedDevice::mru_seq`]
    /// doc comment in `core` -- never a wall clock, this board has no RTC).
    /// Ordering key for the eventual Devices screen (design section 4).
    pub mru_seq: u32,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::PairedDeviceForgotten`
/// (bead pico-link-4vb.6, T2, design section 5.1). Not yet producible from
/// C -- see [`PlPairedDeviceUpsertedPayload`]'s doc comment for the same
/// "accepted, not yet wired" story (T3/T4, both deferred).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlPairedDeviceForgottenPayload {
    pub addr: [u8; 6],
}

/// Which variant of [`PlEventPayload`] is active in a given [`PlEvent`].
/// `DevicesCleared`/`WizardAutoDismiss` carry no data -- the payload union
/// is simply unread for those tags (see [`PlEventPayload`]'s doc comment).
/// Explicit discriminants (pinned, not compiler-assigned) for the same
/// reason as [`PlIntentTag`]'s: [`PlEvent::tag`] carries this value as a
/// plain `u32`, and these numbers are the wire ABI. See
/// [`PlEvent::tag`]'s doc comment.
///
/// The four variants after `LinkStateChanged`..`ConnectFailed` were added
/// by pico-link-znb.7 (E5, the pairing wizard); `CodecChanged` was added by
/// pico-link-1v5 (the Home hero's live codec/bitrate). All purely
/// additive, so [`PL_EVENT_ABI_VERSION`] is unchanged; see
/// [`pico_link_core::Event`]'s doc comment for the design-doc rationale
/// each one closes.
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
    CodecChanged = 8,
    /// Bead pico-link-cz0.6 (M5 persistence). Purely additive like the rest
    /// of this enum -- [`PL_EVENT_ABI_VERSION`] was unchanged by this tag's
    /// own addition (it moved 1->2 the same bead for an unrelated,
    /// non-additive reason -- see that constant's doc comment); its own
    /// *payload shape* was later reshaped non-additively by bead
    /// pico-link-4vb.6 (see [`PlStoreLoadedPayload`]'s doc comment), which
    /// is what bumped [`PL_EVENT_ABI_VERSION`] to 3.
    StoreLoaded = 9,
    /// Bead pico-link-4vb.6 (T2), design section 5.1. Purely additive --
    /// see [`PlPairedDeviceUpsertedPayload`]'s doc comment for the payload
    /// shape and its "not yet producible from C" status.
    PairedDeviceUpserted = 10,
    /// Bead pico-link-4vb.6 (T2), design section 5.1. Purely additive --
    /// see [`PlPairedDeviceForgottenPayload`]'s doc comment.
    PairedDeviceForgotten = 11,
    /// Bead pico-link-4vb.6 (T2), design section 5.1: the store refused a
    /// save because every slot was occupied by a different address (no
    /// eviction, ever -- design section 3's "must never silently evict").
    /// Carries no payload -- like
    /// [`DevicesCleared`](Self::DevicesCleared)/[`WizardAutoDismiss`](Self::WizardAutoDismiss),
    /// the union simply isn't read for this tag. Purely additive. Not yet
    /// producible from C (T3, deferred) -- `firmware/src/persist.c`'s
    /// `pl_persist_do_write` already returns a distinguishable
    /// `PL_PERSIST_WRITE_STORE_FULL` result for this (bead pico-link-4vb.6's
    /// T1), but nothing in `bt.c` reads that result and pushes this tag yet.
    PairedStoreFull = 12,
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
            8 => Ok(PlEventTag::CodecChanged),
            9 => Ok(PlEventTag::StoreLoaded),
            10 => Ok(PlEventTag::PairedDeviceUpserted),
            11 => Ok(PlEventTag::PairedDeviceForgotten),
            12 => Ok(PlEventTag::PairedStoreFull),
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
    pub codec_changed: PlCodecChangedPayload,
    pub store_loaded: PlStoreLoadedPayload,
    /// Bead pico-link-4vb.6 (T2). See [`PlPairedDeviceUpsertedPayload`]'s
    /// doc comment.
    pub paired_device_upserted: PlPairedDeviceUpsertedPayload,
    /// Bead pico-link-4vb.6 (T2). See [`PlPairedDeviceForgottenPayload`]'s
    /// doc comment.
    pub paired_device_forgotten: PlPairedDeviceForgottenPayload,
    // PlEventTag::PairedStoreFull has no payload of its own -- like
    // DevicesCleared/WizardAutoDismiss above, the union simply isn't read
    // for that tag, so no placeholder member is needed.
}

/// ABI version [`PlEvent`] producers (C call sites) must set on every
/// value. Bumped whenever an existing tag's payload shape changes in a way
/// that isn't purely additive (a new tag/payload variant does not need a
/// bump -- old tags are unaffected); see the module section doc for the
/// version-guard rationale.
// Bead pico-link-cz0.6 (M5 persistence): bumped 1 -> 2.
// PlConnectSucceededPayload gained `addr` -- a non-additive shape change to
// an existing tag's payload (see that struct's doc comment) -- so every
// producer/consumer in this one coordinated build picks up the new field
// together; nothing outside this repo speaks this ABI.
//
// Bead pico-link-4vb.6 (T2), design section 5.2: bumped 2 -> 3.
// PlStoreLoadedPayload dropped `has_device`/`addr` and gained `count` -- a
// non-additive shape change to an existing tag's payload, same class of
// bump as the 1->2 one above (see that struct's doc comment for the full
// rationale -- moving the auto-reconnect policy decision to `core`'s side
// of the seam).
//
// Bead pico-link-znb.11 (E9), design section 21 Tier 1 row E9 / section 9
// phase 2 rule 3: bumped 3 -> 4. PlDeviceDiscoveredPayload gained
// `class_of_device` -- a non-additive shape change to an existing tag's
// payload, same class of bump as both above (see that field's doc comment).
pub const PL_EVENT_ABI_VERSION: u32 = 4;

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
            Event::DeviceDiscovered(DeviceEntry { addr, name, rssi: payload.rssi, class_of_device: payload.class_of_device })
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
            Event::ConnectSucceeded { addr: payload.addr, degraded: payload.degraded != 0 }
        }
        PlEventTag::WizardAutoDismiss => Event::WizardAutoDismiss,
        PlEventTag::CodecChanged => {
            // SAFETY: `tag` says this union currently holds `codec_changed`.
            // Reading it is sound regardless of field values -- every
            // field is a plain integer/byte-array type with no validity
            // invariant to violate (see `PlCodecChangedPayload`'s doc
            // comment).
            let payload = unsafe { event.payload.codec_changed };
            let name_len = usize::from(payload.name_len).min(payload.name.len());
            let word = String::from_utf8_lossy(&payload.name[..name_len]).into_owned();
            Event::CodecChanged(ConnectedCodec {
                addr: payload.addr,
                word,
                nominal_bitrate_bps: payload.nominal_bitrate_bps,
            })
        }
        PlEventTag::StoreLoaded => {
            // SAFETY: `tag` says this union currently holds `store_loaded`.
            // Reading it is sound regardless of field values -- every
            // field is a plain integer/byte-array type with no validity
            // invariant to violate (see `PlStoreLoadedPayload`'s doc
            // comment).
            let payload = unsafe { event.payload.store_loaded };
            let status = match PlStoreStatus::try_from(payload.status) {
                Ok(status) => status,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            // Bead pico-link-4vb.6 (T2): `PlStoreLoadedPayload` no longer
            // carries an address -- see that struct's doc comment for why
            // (the auto-reconnect policy moved to `core`'s side of the
            // seam, design section 5.2). `payload.count` is deliberately
            // NOT read here either -- as of bead pico-link-4vb.4 (T4),
            // `core::App::on_store_loaded` computes the reconnect target
            // itself from whatever `PairedDeviceUpserted` events already
            // folded into `BtModel::paired` by the time this arrives (C's
            // own boot sequence pushes those first, this event last); this
            // crate's only job is to hand the bare `status` through.
            Event::StoreLoaded { status: status.into() }
        }
        // Bead pico-link-4vb.4 (T4): these three tags now fold into real
        // `Event::*` variants, closing the "accepted but not yet wired"
        // gap bead pico-link-4vb.6 (T2) deliberately left open pending
        // `core/src/app.rs` gaining `BtModel::paired` and the matching
        // `Event` variants.
        PlEventTag::PairedDeviceUpserted => {
            // SAFETY: `tag` says this union currently holds
            // `paired_device_upserted`. Reading it is sound regardless of
            // field values -- every field is a plain integer/byte-array
            // type with no validity invariant to violate (see
            // `PlPairedDeviceUpsertedPayload`'s doc comment).
            let payload = unsafe { event.payload.paired_device_upserted };
            let name_len = usize::from(payload.name_len).min(payload.name.len());
            let name = String::from_utf8_lossy(&payload.name[..name_len]).into_owned();
            Event::PairedDeviceUpserted(PairedDevice { addr: payload.addr, name, mru_seq: payload.mru_seq })
        }
        PlEventTag::PairedDeviceForgotten => {
            // SAFETY: same as the `PairedDeviceUpserted` arm above.
            let payload = unsafe { event.payload.paired_device_forgotten };
            Event::PairedDeviceForgotten { addr: payload.addr }
        }
        PlEventTag::PairedStoreFull => Event::PairedStoreFull,
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
    /// Bead pico-link-cz0.6 (M5 persistence), design point 7: `core`'s
    /// auto-reconnect/remember-this-device POLICY output. Carries the
    /// target `addr` via the [`PlAddrPayload`] union member (bead
    /// pico-link-4vb.6 moved this and [`CancelConnect`](Self::CancelConnect)
    /// off `.connect` onto `.addr` -- see [`PlCommandPayload`]'s doc
    /// comment; a *source*-only change, no wire byte moves for either).
    PersistDevice = 5,
    /// Bead pico-link-4vb.6 (T2), design
    /// `.planning/design/2026-09-01-remembered-devices.md` section 5.3:
    /// user-initiated "forget this remembered device" (the Devices screen's
    /// X action, or the pick-one-to-forget flow when the store is full).
    /// Carries the target `addr` via [`PlAddrPayload`], same as
    /// [`CancelConnect`](Self::CancelConnect)/[`PersistDevice`](Self::PersistDevice).
    ///
    /// **Not yet producible from `core`** -- `pico_link_core::Command` has
    /// no `ForgetDevice` variant yet (that's T4/T5, bead pico-link-4vb.4,
    /// which depends on `core/src/app.rs` changes deliberately deferred out
    /// of this bead's scope). This tag and its wire shape exist now so the
    /// C-side header (`firmware/include/pico_link_ui.h`) and the ABI
    /// version bump land together with the rest of T2's additive surface,
    /// per the same "define the shape now, wire the producer later"
    /// precedent [`CancelConnect`](Self::CancelConnect) itself set
    /// (pico-link-znb.7's completion report). [`pl_command_from`] has no
    /// match arm for it yet -- unreachable until T4 lands.
    ForgetDevice = 6,
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::Connect`.
///
/// `name`/`name_len` added by bead pico-link-4vb.6 (T2), design section
/// 5.3's "Why the name rides on `Connect`" (alternative A, chosen over a
/// separate `SetDeviceName` command -- seeing that alternative's cost
/// analysis is the point: an unenforceable "send this first" ordering rule
/// whose failure mode is a silently nameless record, exactly the defect
/// class this whole line of work exists to kill). Fixed inline buffer
/// copied by value, following [`PlCodecChangedPayload::name`]'s convention
/// -- `core` is expected to have already truncated on a UTF-8 *character*
/// boundary before setting `name_len` (design section 5.3, "Rust owns
/// text; C owns bytes"); this crate does not re-validate that here since
/// `core` is the only producer of `PlCommand` values.
///
/// **`name`/`name_len` are not yet populated by any real caller** --
/// `pico_link_core::Command::Connect` doesn't carry a name yet (T4,
/// deferred out of this bead's scope, per the same reasoning as
/// [`PlCommandTag::ForgetDevice`]'s doc comment); [`pl_command_from`]
/// zero-fills both today. Added now so the wire shape and the
/// [`PL_COMMAND_ABI_VERSION`] bump land together with the rest of T2.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectPayload {
    pub addr: [u8; 6],
    pub name: [u8; 32],
    pub name_len: u8,
}

/// [`PlCommand`]'s payload for every tag that carries nothing but a target
/// address -- [`PlCommandTag::CancelConnect`], [`PlCommandTag::PersistDevice`]
/// and [`PlCommandTag::ForgetDevice`] (bead pico-link-4vb.6, T2, design
/// section 5.3). Introduced as its own type, rather than continuing to
/// reuse [`PlConnectPayload`] the way those first two tags did before this
/// bead, because `PlConnectPayload` now also carries `name`/`name_len` --
/// fields meaningless for a cancel/persist/forget command. Both structs
/// still start with `addr: [u8; 6]` at the same offset, so this is a
/// *source*-only change for the two pre-existing tags: no wire byte moves
/// for either (see [`PlCommandPayload`]'s doc comment).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlAddrPayload {
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
    /// See [`PlAddrPayload`]'s doc comment. Bead pico-link-4vb.6 (T2).
    pub addr: PlAddrPayload,
}

/// ABI version [`PlCommand`] consumers (C call sites, i.e. `bt.c`'s poll
/// loop) must check against before reading `payload`. Rust is the sole
/// producer of `PlCommand` values (see [`pl_ui_poll_command`]) and always
/// sets this correctly; the check exists on the C side as the same
/// defensive belt-and-suspenders guard [`PL_EVENT_ABI_VERSION`] is for
/// events -- see the module section doc.
// Bead pico-link-4vb.6 (T2): bumped 1 -> 2. `PlConnectPayload` gained
// `name`/`name_len` -- a non-additive shape change to an existing tag's
// payload (same class of change `PL_EVENT_ABI_VERSION`'s 1->2 bump was for
// `PlConnectSucceededPayload` gaining `addr`, bead pico-link-cz0.6) -- so
// every producer/consumer in this one coordinated build picks up the new
// field together.
pub const PL_COMMAND_ABI_VERSION: u32 = 2;

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
    PlCommand {
        version: PL_COMMAND_ABI_VERSION,
        tag: PlCommandTag::None,
        payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0 } },
    }
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
        Command::StartScan => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::StartScan,
            payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0 } },
        },
        Command::Connect { addr, name } => {
            // `core` is the sole producer of `Command` values and already
            // truncated `name` to at most 32 bytes on a UTF-8 character
            // boundary before constructing this variant (bead
            // pico-link-4vb.4, T4/design section 5.3: "Rust owns text; C
            // owns bytes") -- the `.min(32)` here is defensive only, not a
            // second truncation this crate is expected to perform.
            let mut name_buf = [0u8; 32];
            let len = name.len().min(name_buf.len());
            name_buf[..len].copy_from_slice(&name.as_bytes()[..len]);
            // `len <= name_buf.len() == 32`, so this always fits in a `u8`.
            let name_len = u8::try_from(len).unwrap_or(u8::MAX);
            PlCommand {
                version: PL_COMMAND_ABI_VERSION,
                tag: PlCommandTag::Connect,
                payload: PlCommandPayload { connect: PlConnectPayload { addr, name: name_buf, name_len } },
            }
        }
        Command::CancelScan => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::CancelScan,
            payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0 } },
        },
        // Bead pico-link-4vb.6 (T2): moved off `.connect` onto `.addr`
        // (`PlAddrPayload`) -- see that struct's doc comment. Source-only
        // change: both payload shapes start with `addr: [u8; 6]` at offset
        // 0, so no wire byte moves.
        Command::CancelConnect { addr } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::CancelConnect,
            payload: PlCommandPayload { addr: PlAddrPayload { addr } },
        },
        Command::PersistDevice { addr } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::PersistDevice,
            payload: PlCommandPayload { addr: PlAddrPayload { addr } },
        },
        // Bead pico-link-4vb.4 (T4): `Command::ForgetDevice` now exists on
        // `pico_link_core`'s side -- wired to the wire shape T2 already
        // pinned (see `PlCommandTag::ForgetDevice`'s doc comment).
        Command::ForgetDevice { addr } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::ForgetDevice,
            payload: PlCommandPayload { addr: PlAddrPayload { addr } },
        },
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
    fn pl_ui_display_power_defaults_to_on_and_is_on_for_a_null_ui() {
        let ui = new_ui();
        assert!(unsafe { pl_ui_display_power(ui) } == PlDisplayPower::On, "a fresh PlUi must not start blanked");
        assert!(unsafe { pl_ui_display_power(core::ptr::null_mut()) } == PlDisplayPower::On, "a null ui must read as On, never blanked");
        unsafe { pl_ui_destroy(ui) };
    }

    #[test]
    fn pl_ui_tick_blanks_after_the_idle_timeout_at_home_root() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe { pl_ui_tick(ui, 0) };
        assert!(unsafe { pl_ui_display_power(ui) } == PlDisplayPower::On, "must not blank on the very first tick");

        unsafe { pl_ui_tick(ui, idle_timeout_us - 1) };
        assert!(unsafe { pl_ui_display_power(ui) } == PlDisplayPower::On, "must not blank one microsecond early");

        unsafe { pl_ui_tick(ui, idle_timeout_us) };
        assert!(unsafe { pl_ui_display_power(ui) } == PlDisplayPower::Off, "must blank once the idle timeout has elapsed at Home root");

        unsafe { pl_ui_destroy(ui) };
    }

    #[test]
    fn pl_ui_tick_baseline_is_lazily_established_not_seeded_to_zero() {
        // Mirrors `IdlePolicy`'s own `last_input_is_lazily_initialized_not_seeded_to_zero`
        // test, but through the real FFI entry points: a UI whose first
        // `pl_ui_tick` call already carries a large `now_us` (i.e. created
        // long after device boot) must not read as already-idle.
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;
        let far_future_us = idle_timeout_us + 1_000_000_000;

        unsafe { pl_ui_tick(ui, far_future_us) };
        assert!(
            unsafe { pl_ui_display_power(ui) } == PlDisplayPower::On,
            "the first tick must establish the baseline, not read as already idle past the timeout"
        );

        unsafe { pl_ui_destroy(ui) };
    }

    #[test]
    fn pl_ui_input_while_asleep_wakes_and_swallows_the_batch() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe { pl_ui_tick(ui, 0) };
        unsafe { pl_ui_tick(ui, idle_timeout_us) };
        assert!(unsafe { pl_ui_display_power(ui) } == PlDisplayPower::Off, "sanity: asleep");

        // Render once to reach a clean (non-dirty) baseline, then capture
        // the current selection state as a proxy for "was this intent
        // forwarded to the navigator" -- same pattern as
        // `pico_link_core::run::tests::waking_input_is_swallowed_but_the_
        // next_input_reaches_the_app`.
        let mut out_px = core::ptr::null();
        let mut out_len = 0usize;
        unsafe { pl_ui_render(ui, &mut out_px, &mut out_len) };
        assert!(!unsafe { (*ui).app.dirty() }, "PlUi should be clean immediately after a render");

        let down = [PlIntent { tag: PlIntentTag::Down as u32, jump_by: 0 }];
        unsafe { pl_ui_input(ui, down.as_ptr(), down.len()) };

        assert!(unsafe { pl_ui_display_power(ui) } == PlDisplayPower::On, "the waking input must turn the display back on immediately");
        assert!(unsafe { (*ui).app.dirty() }, "waking must mark the app dirty so the just-woken display gets a fresh flush");

        unsafe { pl_ui_destroy(ui) };
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_tag_without_panicking() {
        let ui = new_ui();
        let bogus_payload = PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Idle as u32 } };
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            // One past PairedStoreFull = 12, the highest legal PlEventTag
            // as of bead pico-link-4vb.6 (T2) -- moved from 10 (one past
            // the old highest, StoreLoaded = 9) when this bead added tags
            // 10-12.
            tag: 13,
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
                payload: PlEventPayload { connect_succeeded: PlConnectSucceededPayload { addr: [0; 6], degraded: 1 } },
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
    fn pl_ui_push_event_codec_changed_populates_connected_codec() {
        // bead pico-link-1v5: a `CodecChanged` event's fixed-size `name`
        // buffer round-trips into `BtModel::connected_codec` with the
        // right name/bitrate, and a later non-`Connected`
        // `LinkStateChanged` clears it back to `None` -- see
        // `pico_link_core::App::set_link_state`'s doc comment for why
        // clearing keys off the link state rather than a dedicated
        // disconnect tag.
        let ui = new_ui();
        let mut name = [0u8; 16];
        name[..4].copy_from_slice(b"LDAC");
        let addr = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let codec_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::CodecChanged as u32,
            payload: PlEventPayload {
                codec_changed: PlCodecChangedPayload { addr, name, name_len: 4, nominal_bitrate_bps: 990_000 },
            },
        };
        let link_connected = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Connected as u32 } },
        };
        let link_idle = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Idle as u32 } },
        };
        unsafe {
            pl_ui_push_event(ui, link_connected);
            pl_ui_push_event(ui, codec_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            let codec = (*ui).app.model().connected_codec.clone().expect("codec should be populated");
            assert_eq!(codec.word, "LDAC");
            assert_eq!(codec.nominal_bitrate_bps, 990_000);
            assert_eq!(codec.addr, addr);

            pl_ui_push_event(ui, link_idle);
            assert!(
                (*ui).app.model().connected_codec.is_none(),
                "disconnecting must clear the codec, never leave it stale (design section 15)"
            );
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_store_loaded_with_no_preceding_upserts_records_status_but_queues_nothing() {
        // Bead pico-link-4vb.6 (T2) reshaped PlStoreLoadedPayload -- it no
        // longer carries an address (see that struct's doc comment), so
        // StoreLoaded alone can no longer queue a Command::Connect the way
        // it did pre-reshape (bead pico-link-cz0.6's original test this one
        // replaces). As of bead pico-link-4vb.4 (T4), the real auto-reconnect
        // mechanism is `core` computing `paired.iter().max_by_key(mru_seq)`
        // from whatever `PairedDeviceUpserted` events already folded into
        // `BtModel::paired` -- `payload.count` itself is still never read to
        // synthesize a target (there is no sound way to derive one from a
        // bare count). This event alone, with no preceding upserts pushed
        // through this same `ui`, must queue nothing. `store_status` is
        // still recorded either way -- design point 5's "never renders
        // identically to a new one" still holds.
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::StoreLoaded as u32,
            payload: PlEventPayload { store_loaded: PlStoreLoadedPayload { status: PlStoreStatus::Loaded as u8, count: 2 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            assert_eq!((*ui).app.model().store_status, Some(pico_link_core::StoreStatus::Loaded));

            let cmd = pl_ui_poll_command(ui);
            assert!(matches!(cmd.tag, PlCommandTag::None), "count alone must not synthesize a Connect target");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_store_loaded_after_real_upserts_auto_reconnects_to_the_mru_max() {
        // The real boot mechanism (design section 5.2): C pushes `count` x
        // `PairedDeviceUpserted` THEN `StoreLoaded` as the terminator.
        let ui = new_ui();
        let addr_old = [1u8; 6];
        let addr_new = [2u8; 6];
        let mut name = [0u8; 32];
        name[..3].copy_from_slice(b"New");
        let upsert_old = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PairedDeviceUpserted as u32,
            payload: PlEventPayload {
                paired_device_upserted: PlPairedDeviceUpsertedPayload { addr: addr_old, name: [0u8; 32], name_len: 0, mru_seq: 1 },
            },
        };
        let upsert_new = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PairedDeviceUpserted as u32,
            payload: PlEventPayload {
                paired_device_upserted: PlPairedDeviceUpsertedPayload { addr: addr_new, name, name_len: 3, mru_seq: 2 },
            },
        };
        let store_loaded = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::StoreLoaded as u32,
            payload: PlEventPayload { store_loaded: PlStoreLoadedPayload { status: PlStoreStatus::Loaded as u8, count: 2 } },
        };
        unsafe {
            pl_ui_push_event(ui, upsert_old);
            pl_ui_push_event(ui, upsert_new);
            pl_ui_push_event(ui, store_loaded);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);

            let cmd = pl_ui_poll_command(ui);
            assert!(matches!(cmd.tag, PlCommandTag::Connect), "the highest mru_seq record must auto-reconnect");
            let payload = cmd.payload.connect;
            assert_eq!(payload.addr, addr_new);
            assert_eq!(&payload.name[..3], b"New");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_store_loaded_with_zero_count_queues_nothing() {
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::StoreLoaded as u32,
            payload: PlEventPayload { store_loaded: PlStoreLoadedPayload { status: PlStoreStatus::FirstBoot as u8, count: 0 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            assert_eq!((*ui).app.model().store_status, Some(pico_link_core::StoreStatus::FirstBoot));

            let cmd = pl_ui_poll_command(ui);
            assert!(matches!(cmd.tag, PlCommandTag::None), "no device -- nothing to auto-reconnect to");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_nested_store_status() {
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::StoreLoaded as u32,
            payload: PlEventPayload { store_loaded: PlStoreLoadedPayload { status: 99, count: 0 } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range nested store status should be counted, not matched-on");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_folds_the_new_paired_device_tags_into_the_model() {
        // Bead pico-link-4vb.4 (T4): PairedDeviceUpserted/PairedDeviceForgotten
        // now fold into `core`'s `BtModel::paired` -- the single-writer rule
        // design section 3 requires (`paired` is mutated by exactly these
        // two events and nothing else). PairedStoreFull is legal but carries
        // no payload to misread.
        let ui = new_ui();
        let addr = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let mut name = [0u8; 32];
        name[..2].copy_from_slice(b"XM");
        let upsert = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PairedDeviceUpserted as u32,
            payload: PlEventPayload { paired_device_upserted: PlPairedDeviceUpsertedPayload { addr, name, name_len: 2, mru_seq: 7 } },
        };
        let forget = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PairedDeviceForgotten as u32,
            payload: PlEventPayload { paired_device_forgotten: PlPairedDeviceForgottenPayload { addr } },
        };
        let store_full = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PairedStoreFull as u32,
            payload: PlEventPayload { paired_device_forgotten: PlPairedDeviceForgottenPayload { addr: [0; 6] } },
        };
        unsafe {
            pl_ui_push_event(ui, upsert);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            {
                let paired = &(*ui).app.model().paired;
                assert_eq!(paired.len(), 1);
                assert_eq!(paired[0].addr, addr);
                assert_eq!(paired[0].name, "XM");
                assert_eq!(paired[0].mru_seq, 7);
            }

            pl_ui_push_event(ui, forget);
            assert!((*ui).app.model().paired.is_empty(), "forgetting must remove the record");

            pl_ui_push_event(ui, store_full);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "PairedStoreFull is legal and carries no payload to misread");
            assert_eq!((*ui).app.model().link_state, LinkState::Idle, "none of these tags should touch link_state");
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
            PlEventTag::CodecChanged,
            PlEventTag::StoreLoaded,
            PlEventTag::PairedDeviceUpserted,
            PlEventTag::PairedDeviceForgotten,
            PlEventTag::PairedStoreFull,
        ];
        for tag in legal {
            assert!(PlEventTag::try_from(tag as u32).is_ok());
        }
        assert!(PlEventTag::try_from(13u32).is_err());
        assert!(PlEventTag::try_from(u32::MAX).is_err());
    }

    #[test]
    fn pl_ui_push_event_carries_class_of_device_through_to_the_model() {
        // Bead pico-link-znb.11 (E9): PlDeviceDiscoveredPayload::class_of_device
        // must round-trip all the way from the FFI event into
        // BtModel::discovered's DeviceEntry -- the whole point of carrying
        // it across the seam at all is that `core`, not C, decides which
        // devices are audio sinks.
        let ui = new_ui();
        let addr = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let name = b"Cans";
        // 0x240404: a real headphones-shaped Class-of-Device (major
        // service "Audio" bit set, major device class 0x04 Audio/Video,
        // minor device class 0x04 wearable headset) -- not just a bare
        // major-class nibble, so this also proves the full 24-bit value
        // survives, not merely whichever bits a lossy carry might keep.
        let class_of_device: u32 = 0x24_04_04;
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::DeviceDiscovered as u32,
            payload: PlEventPayload {
                device_discovered: PlDeviceDiscoveredPayload {
                    addr,
                    name: name.as_ptr(),
                    name_len: name.len(),
                    rssi: -40,
                    class_of_device,
                },
            },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            let model = (*ui).app.model();
            assert_eq!(model.discovered.len(), 1, "the device must have been folded into BtModel::discovered");
            assert_eq!(
                model.discovered[0].class_of_device, class_of_device,
                "class_of_device must round-trip byte-for-byte, not just its top bits"
            );
            pl_ui_destroy(ui);
        }
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
        //
        // Bead pico-link-4vb.6 (T2): reads `.addr.addr` now, not
        // `.connect.addr` -- `pl_command_from` moved this tag onto
        // `PlAddrPayload` (see that struct's doc comment). Source-only
        // change: both members start with `addr: [u8; 6]` at offset 0.
        let addr = [0xAA; 6];
        let wire = pl_command_from(Command::CancelConnect { addr });
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::CancelConnect as u32);
        // SAFETY: `wire.tag` above confirms the union currently holds `addr`.
        assert_eq!(unsafe { wire.payload.addr.addr }, addr);
    }

    #[test]
    fn persist_device_command_maps_to_the_persist_device_tag_and_carries_the_addr() {
        // Bead pico-link-4vb.6 (T2): same `.connect` -> `.addr` move as
        // `CancelConnect` above, covering `PersistDevice` too -- not
        // previously covered by its own dedicated test.
        let addr = [0x55; 6];
        let wire = pl_command_from(Command::PersistDevice { addr });
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::PersistDevice as u32);
        // SAFETY: `wire.tag` above confirms the union currently holds `addr`.
        assert_eq!(unsafe { wire.payload.addr.addr }, addr);
    }

    #[test]
    fn start_scan_and_connect_still_map_to_their_own_tags() {
        // Regression guard: extracting `pl_command_from` out of
        // `pl_ui_poll_command` (pico-link-znb.2) must not change the
        // existing StartScan/Connect mappings.
        let start = pl_command_from(Command::StartScan);
        assert_eq!(start.tag as u32, PlCommandTag::StartScan as u32);

        let addr = [1, 2, 3, 4, 5, 6];
        let connect = pl_command_from(Command::Connect { addr, name: String::new() });
        assert_eq!(connect.tag as u32, PlCommandTag::Connect as u32);
        // SAFETY: `connect.tag` above confirms the union currently holds `connect`.
        let payload = unsafe { connect.payload.connect };
        assert_eq!(payload.addr, addr);
        // An empty `core`-side name still zero-fills the wire buffer.
        assert_eq!(payload.name_len, 0);
        assert_eq!(payload.name, [0u8; 32]);
    }

    #[test]
    fn connect_command_carries_the_name_bead_pico_link_4vb_4() {
        // Bead pico-link-4vb.4 (T4): `Command::Connect` now carries a name,
        // already truncated by `core` -- this crate copies it byte-for-byte
        // into the fixed wire buffer.
        let addr = [9, 9, 9, 9, 9, 9];
        let connect = pl_command_from(Command::Connect { addr, name: String::from("Sony WH-1000XM5") });
        assert_eq!(connect.tag as u32, PlCommandTag::Connect as u32);
        // SAFETY: `connect.tag` above confirms the union currently holds `connect`.
        let payload = unsafe { connect.payload.connect };
        assert_eq!(payload.addr, addr);
        assert_eq!(payload.name_len, 15);
        assert_eq!(&payload.name[..15], b"Sony WH-1000XM5");
    }

    #[test]
    fn forget_device_command_maps_to_the_forget_device_tag_and_carries_the_addr() {
        // Bead pico-link-4vb.4 (T4): `Command::ForgetDevice` now maps to the
        // wire shape T2 (bead pico-link-4vb.6) already pinned.
        assert_eq!(PlCommandTag::ForgetDevice as u32, 6);
        let addr = [0x77; 6];
        let wire = pl_command_from(Command::ForgetDevice { addr });
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::ForgetDevice as u32);
        // SAFETY: `wire.tag` above confirms the union currently holds `addr`.
        assert_eq!(unsafe { wire.payload.addr.addr }, addr);
    }
}
