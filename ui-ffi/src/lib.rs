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
//! - The pixel pointer [`pl_ui_render_ex`] hands back is borrowed until the
//!   next mutating call (`pl_ui_input`/`pl_ui_tick`/`pl_ui_render_ex` again)
//!   -- C must copy out (e.g. into a DMA source) before calling back in.
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
    StoreStatus, VolumeSource, DEFAULT_IDLE_TIMEOUT,
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
    /// Set by [`pl_ui_push_event`] whenever a `VolumeChanged` event arrives
    /// whose [`pico_link_core::app::VolumeState::wakes_idle`] is `true`
    /// (design `.planning/design/2026-09-07-volume-on-display.md` section
    /// 5.3: a non-host source that isn't itself muted/zero) -- consumed
    /// (reset to `false`, OR'd into `had_input`) by the next [`pl_ui_tick`]
    /// call, the same deferred-flag shape [`PlUi::input_since_last_tick`]
    /// already uses and for the identical reason: `pl_ui_push_event` has
    /// no clock, only `pl_ui_tick`'s `now_us` does, so "extend the idle
    /// timer" can only take effect at the next tick. The immediate
    /// Asleep -> Active wake itself (`IdlePolicy::on_input`) is applied
    /// synchronously in `pl_ui_push_event`, not deferred -- only the
    /// clock-dependent `last_input` reset waits for `pl_ui_tick`.
    volume_wake_since_last_tick: bool,
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
    /// Backing storage for the `rects` array [`pl_ui_render_ex`] hands back
    /// -- `PlRenderOut.rects` borrows this, so it must outlive the call and
    /// live somewhere with the same lifetime contract as the framebuffer
    /// pointer (valid until the next `pl_ui_input`/`pl_ui_tick`/
    /// `pl_ui_render*`). Phase 1 (`pico-link-7h5.6`) only ever needs one
    /// slot -- `App::render` reports a single bounding `Rectangle` -- but
    /// this is a fixed-size array rather than a scalar field so a future
    /// core-side move to multiple damage rects (design section 5) only
    /// grows how many of these slots get filled, not the FFI shape.
    last_damage_rects: [PlDamageRect; 1],
    /// The last [`pico_link_core::dsp::Program`] handed to C by
    /// [`pl_ui_take_dsp_program`], if any -- the "newest wins, pull only
    /// on change" seqlock-style state design sec 3.2 describes ("the
    /// PROGRAM IS STATE, NOT AN EVENT"). Compared by value against a
    /// freshly recomputed [`App::dsp_program`] on every call; `None`
    /// until the first successful take, which always counts as a change
    /// (the initial Off program still needs to reach C's engine once, so
    /// its own bypass path is armed correctly at boot).
    last_dsp_program: Option<pico_link_core::dsp::Program>,
    /// The `library_rev` [`pl_ui_library`] last returned a nonzero-length
    /// encode for, or `0` if it has never yet published one -- bead
    /// `pico-link-jyhk.20`'s "returns 0 when unchanged" contract (design
    /// section 11 Task 3). [`pico_link_core::app::App::library_snapshot`]
    /// always encodes a fresh full snapshot on every call (it has no
    /// "don't bother, nothing changed" signal of its own -- see that
    /// method's doc comment), so the unchanged-detection this FFI layer
    /// promises C lives here, one call above core, compared against the
    /// `library_rev` field already inside the freshly encoded bytes.
    /// `Cell`, not a plain field, because [`pl_ui_library`] takes `*const
    /// PlUi` (mirrors [`pl_ui_telemetry`]'s "read-only from the caller's
    /// point of view" contract) -- same interior-mutability shape `App`
    /// itself uses for `library_rev`/`telemetry_snap_seq`. `0` is a safe
    /// "never published" sentinel: `App::refresh_library_rev`'s own doc
    /// comment guarantees a real rev is never `0`.
    last_library_rev: core::cell::Cell<u16>,
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
/// and `deep_sleep_timeout: None`, `mode: Off` -- reproducing
/// `pico_link_core::power::DisplaySettings::default()`'s exact behavior
/// (Off/1 min, Andreas's Q3 ruling on pico-link-qivj.2) until C's persisted
/// [`PlEventTag::DisplaySettingsLoaded`] event lands (`main.c` pushes it
/// right after `pl_bt_init`, before the superloop starts, so it always
/// arrives ahead of the first real [`pl_ui_tick`]) and
/// `App::take_display_settings_to_apply` configures the live policy from
/// it. Deep sleep stays unreachable, matching `crate::power::
/// DEEP_SLEEP_ARMED == false`. Deliberately does **not**
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
        volume_wake_since_last_tick: false,
        malformed_tag_count: 0,
        last_damage_rects: [PlDamageRect { x: 0, y: 0, w: 0, h: 0 }; 1],
        last_dsp_program: None,
        last_library_rev: core::cell::Cell::new(0),
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
/// [`PlUi::volume_wake_since_last_tick`] (OR'd together into `had_input` --
/// design `.planning/design/2026-09-07-volume-on-display.md` section
/// 5.1/5.3: a sink/device volume change extends the idle timer exactly
/// like ordinary input does) and updates the idle clock accordingly.
/// `mute_or_zero` reads [`pico_link_core::app::App::volume_requires_dim_floor`]
/// fresh every tick -- this is what makes design section 5.4's "never
/// fully blank while muted/zero" rule apply on the real target, not just
/// the emulator (see [`IdlePolicy::tick`]'s doc comment for the mechanism;
/// this is the ONE call site that matters for that guarantee, since the
/// firmware never runs `pico_link_core::run::Runner`). `on_external_power`
/// is hardcoded to `true` -- the firmware has no external-power sense yet
/// (see the design doc's §5.2) and this instance's `deep_sleep_timeout` is
/// always `None` (set in [`pl_ui_create`]), so the deep-sleep tier can
/// never actually fire regardless of this value; it exists only so
/// `IdlePolicy::tick`'s signature doesn't need a second, firmware-only
/// variant. The resulting power level is read separately via
/// [`pl_ui_display_power`] -- this function does not report it.
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
    // A Settings picker's pick, applied to the live `IdlePolicy` before
    // this tick's own arm decision -- same "apply latch, drained here"
    // shape `Runner::step` uses (pico-link-qivj.2, S4/S8).
    if let Some(settings) = ui.app.take_display_settings_to_apply() {
        ui.idle.configure(settings.idle_timeout(), settings.deep_sleep_timeout(), settings.mode);
    }
    let had_input = core::mem::take(&mut ui.input_since_last_tick) || core::mem::take(&mut ui.volume_wake_since_last_tick);
    let now = pico_link_core::platform::Instant::from_micros(now_us);
    let mute_or_zero = ui.app.volume_requires_dim_floor();
    // `enter_deep_sleep` is deliberately ignored -- see the doc comment
    // above for why it can never be `true` here.
    let _decision = ui.idle.tick(now, had_input, ui.app.is_at_home_root(), true, mute_or_zero);
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
    /// Backlight reduced to `pl_ui_backlight_permille`'s value; content
    /// still rendered and visible. Added by pico-link-qivj.2 -- additive,
    /// no ABI version bump (same discipline as event tags 13-17 / command
    /// tag 8).
    Dim = 2,
}

impl From<DisplayPower> for PlDisplayPower {
    fn from(power: DisplayPower) -> Self {
        match power {
            DisplayPower::On => PlDisplayPower::On,
            DisplayPower::Dim => PlDisplayPower::Dim,
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

/// Backlight level, out of 1000 (permille), for [`pl_ui_display_power`]'s
/// current level -- `On` = 1000, `Dim` = the single
/// `pico_link_core::power::DIM_BACKLIGHT_PERMILLE` constant, `Off` = 0.
/// `core` owns this policy entirely; C never hardcodes a level, it only
/// ever asks this function and applies the result to the backlight PWM
/// (`st7789_set_backlight_permille`). Same self-healing "fail bright"
/// polarity as [`pl_ui_display_power`]: returns `1000` for a null `ui`.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_backlight_permille(ui: *const PlUi) -> u16 {
    if ui.is_null() {
        return 1000;
    }
    // SAFETY: caller contract above.
    let ui = &*ui;
    ui.idle.display_power().backlight_permille()
}

/// Whether [`pl_ui_render_ex`] would currently draw something different from
/// the last time it was called -- `pico_link_core::app::App::dirty()`
/// exposed across the seam (pico-link-vxc). **A level, not an edge**, read
/// once per superloop iteration and, like [`pl_ui_display_power`],
/// deliberately after [`pl_ui_tick`] -- `tick` is what turns a due
/// `Widget::redraw_after` into a dirty flag, so reading this before the
/// tick would delay every time-driven repaint by one frame.
///
/// **Cleared only by [`pl_ui_render_ex`].** If C decides to skip a render
/// this iteration, the flag simply persists to the next one -- there is no
/// other way to clear it, so a skipped render can never be silently lost.
///
/// Returns `true` if `ui` is null -- every degenerate case here fails
/// toward *painting*, never toward a screen that looks clean when it is
/// actually just unobserved. Same self-healing polarity as
/// [`pl_ui_display_power`] returning `On` for a null `ui`.
///
/// This is a pure query: it does not consume, latch, or reset any state,
/// which is why it takes `*const` rather than `*mut`.
///
/// C is expected to gate the render+blit on `display_on && pl_ui_dirty(ui)`,
/// in that order -- the dirty check must sit *inside* the display-power
/// gate, never beside it, because calling [`pl_ui_render_ex`] while the panel
/// is blanked would clear the dirty flag for a frame nobody saw. See
/// `.planning/design/2026-09-02-dirty-gate-across-the-ffi-seam.md` §3.3.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_dirty(ui: *const PlUi) -> bool {
    if ui.is_null() {
        return true;
    }
    // SAFETY: caller contract above.
    let ui = &*ui;
    ui.app.dirty()
}

// --- The versioned render-out seam (pico-link-7h5.6) ---
//
// `pl_ui_render_ex` carries the frame damage rect `App::render`'s
// `RenderOutput` (`pico-link-7h5.4`) computes. It replaced the older
// unversioned `pl_ui_render` (bare `out_px`/`out_len` pair, no damage rect),
// which was kept alive alongside it only until the partial-blit path
// (`pico-link-7h5.8`) was proven on hardware, then deleted in
// `pico-link-7h5.10`. `pl_ui_render_ex` is now the single render entry
// point. Same ABI-version-guard discipline as
// `PlEvent`/`PL_EVENT_ABI_VERSION` above (module section doc,
// `pl_ui_push_event`'s doc comment) -- a `version` field the consumer
// checks before reading anything else in the struct.

/// One damage rectangle, in framebuffer pixel coordinates (`x`/`y` are the
/// top-left corner, `w`/`h` the extent) -- the sub-rectangle of
/// [`PlRenderOut::px`] that actually changed this frame.
///
/// Deliberately **not** a packed copy of the damaged pixels: `px` always
/// points at the whole framebuffer, and a rect only describes which part of
/// it to read, at [`PlRenderOut::stride`]. Copying pixels out to pack them
/// would spend on the CPU exactly what this design exists to save (design
/// doc section 6).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDamageRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// ABI version for [`PlRenderOut`], checked by the consumer the same way
/// [`PL_EVENT_ABI_VERSION`]/[`PL_COMMAND_ABI_VERSION`] are: C reads
/// `out.version` before trusting anything else in the struct and, on a
/// mismatch, must fall back to blitting the **whole** framebuffer rather
/// than trusting a `rects` shape it wasn't built to read -- a version
/// mismatch degrades to slow-and-correct, never fast-and-wrong (design doc
/// section 6).
pub const PL_RENDER_ABI_VERSION: u32 = 1;

/// The versioned render-out payload [`pl_ui_render_ex`] fills in. `px`
/// always points at the **whole** framebuffer -- `rects` narrows which part
/// of it is worth transferring to the panel, it never repackages the pixels
/// themselves.
///
/// `rect_count == 0` means "nothing to paint this frame" and C must not
/// blit at all. In practice this is reachable only if C calls
/// [`pl_ui_render_ex`] without first checking [`pl_ui_dirty`] -- callers
/// are expected to keep gating on that as today; this is a belt, not the
/// primary mechanism.
///
/// `px`/`rects` are both valid only until the next call that mutates `ui`
/// (`pl_ui_input`/`pl_ui_tick`/`pl_ui_render_ex` again). C must copy out
/// (e.g. into a DMA source) before making that next call.
#[repr(C)]
pub struct PlRenderOut {
    pub version: u32,
    pub px: *const u16,
    pub px_len: usize,
    pub stride: u16,
    pub rect_count: u16,
    pub rects: *const PlDamageRect,
}

/// Renders the current screen -- unconditionally, every call, regardless of
/// `App::dirty()` (unlike `pico_link_core::run::Runner::step`'s dirty gate,
/// which this FFI surface does NOT mirror). Idempotent -- calling it twice
/// with no intervening `pl_ui_input`/`pl_ui_tick` produces the identical
/// frame both times -- but C should not assume a cheap early-out here;
/// skipping a redundant render (and the blit that would follow it) when
/// nothing changed is C's own call to make, not something this function
/// does for it -- see [`pl_ui_dirty`], added for exactly that call
/// (pico-link-vxc). This function is what clears the dirty flag
/// [`pl_ui_dirty`] reports, so C must not call this while the panel is
/// blanked (it would clear dirty for a frame nobody saw). Additionally
/// reports the frame damage rect via `out`.
///
/// Writes `*out` unconditionally when `ui` and `out` are both non-null:
/// on a null `ui`, `out->version` is still set to [`PL_RENDER_ABI_VERSION`]
/// but `px` is null, `px_len`/`rect_count` are `0`, and `rects` is null --
/// never leaving the struct uninitialized. A no-op (does not touch `*out`
/// at all) if `out` itself is null.
///
/// `out->rect_count` is `0` when the just-rendered frame changed nothing
/// visible (`App::render`'s reported damage rect was empty) -- C must treat
/// that as "do not blit," matching [`pl_ui_dirty`]'s gating contract.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `out`, if non-null, must point to valid, writable
/// [`PlRenderOut`] storage. The `px`/`rects` pointers written into `*out`
/// are borrowed until the next call that mutates `ui`
/// (`pl_ui_input`/`pl_ui_tick`/`pl_ui_render_ex` again) -- C must have
/// copied out (e.g. into a DMA source) before making that next call.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_render_ex(ui: *mut PlUi, out: *mut PlRenderOut) {
    if out.is_null() {
        return;
    }
    if ui.is_null() {
        // SAFETY: caller contract -- `out` is valid, writable storage per
        // the null check above.
        unsafe {
            (*out).version = PL_RENDER_ABI_VERSION;
            (*out).px = core::ptr::null();
            (*out).px_len = 0;
            (*out).stride = 0;
            (*out).rect_count = 0;
            (*out).rects = core::ptr::null();
        }
        return;
    }
    // SAFETY: caller contract above.
    let ui = &mut *ui;
    let output = ui.app.render();
    // The framebuffer/panel are 240x240 today and any plausible future
    // panel this project targets stays well under `u16::MAX` -- saturating
    // rather than panicking or wrapping keeps this boundary infallible like
    // every other FFI entry point (module doc's memory rules), at the cost
    // of a (currently unreachable) clamp instead of UB on a pathological
    // framebuffer size.
    let stride = u16::try_from(output.width()).unwrap_or(u16::MAX);
    let raw = output.as_raw_u16();
    let damage = output.damage;

    let rect_count = if damage.size.width == 0 || damage.size.height == 0 {
        0u16
    } else {
        ui.last_damage_rects[0] = PlDamageRect {
            x: u16::try_from(damage.top_left.x).unwrap_or(0),
            y: u16::try_from(damage.top_left.y).unwrap_or(0),
            w: u16::try_from(damage.size.width).unwrap_or(u16::MAX),
            h: u16::try_from(damage.size.height).unwrap_or(u16::MAX),
        };
        1u16
    };

    // SAFETY: caller contract -- `out` is valid, writable storage.
    unsafe {
        (*out).version = PL_RENDER_ABI_VERSION;
        (*out).px = raw.as_ptr();
        (*out).px_len = raw.len();
        (*out).stride = stride;
        (*out).rect_count = rect_count;
        (*out).rects = if rect_count == 0 { core::ptr::null() } else { ui.last_damage_rects.as_ptr() };
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

/// Mirrors [`pico_link_core::LinkState`]'s three variants 1:1. Explicit
/// discriminants (pinned, not compiler-assigned) for the same reason as
/// [`PlIntentTag`]'s: [`PlLinkStateChangedPayload::state`] carries this
/// value as a plain `u32`, not as a `PlLinkState`-typed field -- see that
/// field's doc comment. `PlLinkState` itself stays a real Rust enum purely
/// so the cbindgen header keeps emitting named `PL_LINK_STATE_*` C
/// constants for firmware source to use.
///
/// Bead `pico-link-88xs`, design `.planning/design/2026-09-08-link-state-
/// vs-discovery-axis.md` section 2.5: `Scanning = 1` is **removed**, not
/// renumbered -- `1` is reserved and rejected by `TryFrom` below rather
/// than reused, and [`PL_EVENT_ABI_VERSION`] does NOT move for this
/// change. This is a narrowing of a nested enum's accepted value space,
/// not a layout change, and the version guard is the wrong instrument for
/// it: on a version mismatch [`pl_ui_push_event`] returns silently and
/// EVERY event in the stream vanishes with no counter moved, whereas
/// leaving the version alone means a stale producer of `state == 1` is
/// rejected AND counted in [`PlUi::malformed_tag_count`] -- one bad event,
/// observably, which given this project's history with silent event drops
/// is strictly the better failure mode. Scanning now travels on its own
/// wire tag -- see [`PlDiscoveryState`].
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlLinkState {
    Idle = 0,
    // 1 was `Scanning` -- deliberately not reused, see the doc comment
    // above and `TryFrom`'s match below.
    Connecting = 2,
    Connected = 3,
}

impl core::convert::TryFrom<u32> for PlLinkState {
    type Error = ();

    /// Checked conversion from the raw wire value -- same hazard, same fix
    /// as [`PlIntentTag`]'s `TryFrom` impl (pico-link-ptu): this payload
    /// sits inside [`PlEventPayload`], a union C constructs and
    /// [`pl_ui_push_event`] receives by value, one layer beneath the outer
    /// tag this bead originally hardened. `1` (the former `Scanning`) is
    /// deliberately absent -- a stale producer sending it is rejected and
    /// counted (bead `pico-link-88xs`), not silently accepted.
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlLinkState::Idle),
            2 => Ok(PlLinkState::Connecting),
            3 => Ok(PlLinkState::Connected),
            _ => Err(()),
        }
    }
}

/// Decodes a wire [`PlLinkState`] into the [`Event`] it means -- NOT a
/// blanket `From<PlLinkState> for LinkState`, because since bead
/// `pico-link-0cq2` the three wire values no longer map 1:1 onto
/// `core::LinkState`'s two variants (see that type's doc comment):
/// `PlLinkState::Connecting` (wire value `2`, the former
/// `PL_LINK_STATE_CONNECTING`) now means [`Event::ConnectAttemptStarted`]
/// instead of a `LinkStateChanged` payload -- no C ABI change, no wire byte
/// change, only where in `core`'s event vocabulary that byte lands.
fn decode_link_state_event(state: PlLinkState) -> Event {
    match state {
        PlLinkState::Idle => Event::LinkStateChanged(LinkState::Idle),
        PlLinkState::Connecting => Event::ConnectAttemptStarted,
        PlLinkState::Connected => Event::LinkStateChanged(LinkState::Connected),
    }
}

/// Mirrors [`pico_link_core::BtModel::discovering`]'s two observable
/// states 1:1 (bead `pico-link-88xs`, design section 2.3). Unlike
/// [`PlLinkState`], the wire carries a nested enum rather than a raw
/// `bool` byte -- deliberate room to grow (e.g. a future
/// `PL_DISCOVERY_STATE_STARTING` or an LE-scan variant) without an ABI
/// event, matching the shape every other nested enum on this wire already
/// uses.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlDiscoveryState {
    Idle = 0,
    Scanning = 1,
}

impl core::convert::TryFrom<u32> for PlDiscoveryState {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlLinkState`]'s
    /// `TryFrom` impl for the full rationale (pico-link-ptu).
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlDiscoveryState::Idle),
            1 => Ok(PlDiscoveryState::Scanning),
            _ => Err(()),
        }
    }
}

impl PlDiscoveryState {
    /// Whether this wire value means "a GAP inquiry is running" -- the
    /// exact `bool` [`pico_link_core::app::App::set_discovering`] takes.
    #[must_use]
    pub fn is_scanning(self) -> bool {
        matches!(self, PlDiscoveryState::Scanning)
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

/// Mirrors [`pico_link_core::ConnectStep`]'s five variants 1:1 (four added
/// by pico-link-znb.7 / E5, the pairing wizard's phase-4 named sub-steps;
/// `Disconnecting` added by pico-link-sfw6, design `.planning/design/2026-
/// 09-25-device-switch-break-before-make.md` sec 3 -- A's teardown during a
/// break-before-make device switch). Explicit discriminants pinned for the
/// same reason as [`PlLinkState`]'s. Additive: existing discriminants 0-3
/// are unchanged, so this is not an ABI version bump (pico-link-ptu).
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlConnectStep {
    Connecting = 0,
    Pairing = 1,
    SettingUpAudio = 2,
    NegotiatingCodec = 3,
    Disconnecting = 4,
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
            4 => Ok(PlConnectStep::Disconnecting),
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
            PlConnectStep::Disconnecting => ConnectStep::Disconnecting,
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

/// [`PlEvent`]'s payload when `tag == PlEventTag::DiscoveryStateChanged`
/// (bead `pico-link-88xs`). Shape copied exactly from
/// [`PlLinkStateChangedPayload`] -- `state` is a plain `u32`, not
/// [`PlDiscoveryState`], for the same reason: this sits inside
/// [`PlEventPayload`], a union [`pl_ui_push_event`] receives by value, so a
/// typed field here would already be UB to read on a garbage discriminant.
/// Convert via [`PlDiscoveryState::try_from`] rather than transmuting.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDiscoveryStateChangedPayload {
    pub state: u32,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::DisplaySettingsLoaded`,
/// and also [`PlCommand`]'s payload when `tag ==
/// PlCommandTag::SetDisplaySettings` -- the same wire shape serves both
/// directions (bead pico-link-qivj.2), matching
/// `pico_link_core::power::DisplaySettings::to_wire`/`from_wire` exactly:
/// `mode` (1=Off, 2=Dim; any other value falls back to Off) and
/// `timeout_s` (0=Never; `{0,30,60,120,300}`; any other value falls back
/// to 60). `core` decodes with `DisplaySettings::from_wire`, never a raw
/// match here -- same "typed field would already be UB on a garbage
/// discriminant" reasoning as every other payload struct in this union.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDisplaySettingsPayload {
    pub mode: u8,
    pub timeout_s: u16,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::CushionPolicyLoaded`,
/// and also [`PlCommand`]'s payload when `tag ==
/// PlCommandTag::SetCushionPolicy` -- the same wire shape serves both
/// directions (bead pico-link-8pp1.4), matching
/// `pico_link_core::audio::CushionPolicy::to_wire`/`from_wire` exactly:
/// `policy` (`0` = unset -> `Low`, `1` = `Low`, `2` = `Stable`; any other
/// value, including the reserved `3`, falls back to `Low`). `core` decodes
/// with `CushionPolicy::from_wire`, never a raw match here -- same
/// "typed field would already be UB on a garbage discriminant" reasoning
/// as every other payload struct in this union.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlCushionPolicyPayload {
    pub policy: u8,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::AbrFloorLoaded`, and also
/// [`PlCommand`]'s payload when `tag == PlCommandTag::SetAbrFloor` -- the
/// same wire shape serves both directions (bead pico-link-d42g.3), matching
/// `pico_link_core::audio::AbrFloor::to_wire`/`from_wire` exactly: `floor`
/// (`0` = unset -> `Kbps330`, `1` = `Kbps330`, `2` = `Kbps246`, `3` =
/// `Kbps198`; any other value falls back to `Kbps330`). `core` decodes with
/// `AbrFloor::from_wire`, never a raw match here -- same "typed field would
/// already be UB on a garbage discriminant" reasoning as every other
/// payload struct in this union.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlAbrFloorPayload {
    pub floor: u8,
}

/// `PL_DSP_PRESET_BLOB_LEN` -- the fixed on-wire preset blob buffer width
/// shared by [`PlPresetLoadedPayload`]/[`PlSavePresetPayload`] (bead
/// `pico-link-ryw.5`, design sec 2.2/3.2). A literal, not
/// `pico_link_core::dsp::preset::BLOB_LEN` (`70`): cbindgen needs a
/// concrete integer for a C array size, and this width is deliberately
/// C's own on-flash reservation (design sec 2.2: "C's on-flash record
/// reserves `blob[80]`... for headroom past" `core`'s current wire length)
/// -- `core`'s own `BLOB_LEN` may grow within this budget without an ABI
/// bump, same "`name[32]`, not a `core`-side constant" convention
/// [`PlPairedDeviceUpsertedPayload::name`] already uses.
pub const PL_DSP_PRESET_BLOB_LEN: usize = 80;

/// The page-0 Home telemetry snapshot's fixed wire length, in bytes --
/// mirrors [`pico_link_core::app::HOME_SNAPSHOT_LEN`] (bead `pico-link-5adh`)
/// so cbindgen emits it into `pico_link_ui.h`'s `PL_HOME_SNAPSHOT_LEN`,
/// replacing the hand-copied `#define PL_CFG_HOME_SNAPSHOT_LEN 169` that
/// used to live in `firmware/src/usb_config_itf.h` with no link back to
/// `core` at all.
///
/// A literal, not `pico_link_core::app::HOME_SNAPSHOT_LEN` directly: that
/// constant is itself `OFF_CODEC_FALLBACK_REASON + 1`, a chain of
/// crate-private offset constants, and cbindgen (0.29) cannot evaluate a
/// path expression into another crate -- confirmed by running it against
/// this exact item, which it silently dropped from the header rather than
/// erroring (both with and without `parse.parse_deps`). The
/// [`HOME_SNAPSHOT_LEN_MATCHES_CORE`] assertion below is what makes this
/// literal safe: it is a real `const` expression checked by `rustc` on
/// every build, so any future append to `core`'s layout that doesn't also
/// update this literal fails `cargo build`/`cargo test` for this crate,
/// not silently at the C header.
pub const PL_HOME_SNAPSHOT_LEN: usize = 169;

/// Compile-time proof that [`PL_HOME_SNAPSHOT_LEN`] has not drifted from
/// [`pico_link_core::app::HOME_SNAPSHOT_LEN`] -- see that constant's doc
/// comment for why the value can't be derived directly.
#[allow(dead_code)]
const HOME_SNAPSHOT_LEN_MATCHES_CORE: () =
    assert!(PL_HOME_SNAPSHOT_LEN == pico_link_core::app::HOME_SNAPSHOT_LEN);

/// [`PlEvent`]'s payload when `tag == PlEventTag::PresetLoaded` (bead
/// `pico-link-ryw.5`, design sec 2.2/3.2). Pushed either at boot (C's
/// `count` x this event ahead of [`PlEventTag::PresetStoreLoaded`]) or as
/// the [`PlCommandTag::SavePreset`] echo -- `id` is C's allocated id
/// either way and is never `0` (`0` is [`pico_link_core::dsp::store::
/// NO_PRESET_ID`], reserved and never assigned to a real preset). `blob`
/// is an inline fixed buffer copied by value, following
/// [`PlPairedDeviceUpsertedPayload::name`]'s convention -- `blob_len`
/// bytes are meaningful, the rest is unspecified padding; `core` treats
/// `blob` as entirely opaque (design sec 2.2: "C NEVER parses the blob"),
/// decoding only the `blob_len`-byte prefix with
/// [`pico_link_core::dsp::preset::Preset::from_wire`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlPresetLoadedPayload {
    pub id: u16,
    pub blob_len: u8,
    pub blob: [u8; PL_DSP_PRESET_BLOB_LEN],
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::PresetDeleted` (bead
/// `pico-link-ryw.5`, design sec 2.4) -- the [`PlCommandTag::DeletePreset`]
/// echo, a real deletion C's flash store performed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlPresetDeletedPayload {
    pub id: u16,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::PresetStoreLoaded` (bead
/// `pico-link-ryw.5`, design sec 2.2) -- the terminator of C's boot-time
/// `count` x [`PlEventTag::PresetLoaded`] push sequence, same shape
/// [`PlStoreLoadedPayload`] is for [`PlEventTag::PairedDeviceUpserted`].
/// `status` is a plain `u8`, not [`PlStoreStatus`] -- same reason as every
/// other union-member tag/state/status field in this module; convert via
/// [`PlStoreStatus::try_from`], never by transmuting.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlPresetStoreLoadedPayload {
    pub count: u16,
    pub status: u8,
    /// C's own preset-id high-water mark -- bead `pico-link-ryw.14`, Ada's
    /// preset-id-allocation contract. [`PL_EVENT_ABI_VERSION`] bumped 6 ->
    /// 7 for this field: a non-additive shape change to an existing tag's
    /// payload, same class of bump every prior `PL_EVENT_ABI_VERSION` bump
    /// documents. `core` now allocates every preset id itself
    /// ([`pico_link_core::dsp::PresetStore::create`]) rather than C, so it
    /// must be told C's own counter (via
    /// [`pico_link_core::dsp::PresetStore::raise_next_id`]) before
    /// allocating anything itself, or a fresh id could alias one C already
    /// holds for a deleted-then-reused slot.
    pub next_id: u16,
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
///
/// `seq` added by ADA DESIGN v2 (bead `pico-link-chc3`), [`PL_EVENT_ABI_
/// VERSION`] bumped 7 -> 8: the `seq` of the attempt this failure belongs
/// to (`0` if C has none to attribute -- see [`pico_link_core::app::
/// Event::ConnectFailed`]'s doc comment for the full seq-scoping rule
/// `core`'s fold applies).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectFailedPayload {
    pub addr: [u8; 6],
    pub reason: u32,
    pub seq: u16,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::ConnectStepChanged`.
/// `step` is a plain `u32`, not [`PlConnectStep`] -- same reason as
/// [`PlLinkStateChangedPayload::state`]; see that field's doc comment.
///
/// `seq` added by ADA DESIGN v2 (bead `pico-link-chc3`), [`PL_EVENT_ABI_
/// VERSION`] bumped 7 -> 8 -- see [`PlConnectFailedPayload::seq`]'s doc
/// comment for the shared rule.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectStepChangedPayload {
    pub step: u32,
    pub seq: u16,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::ConnectRetrying`.
///
/// `seq` added by ADA DESIGN v2 (bead `pico-link-chc3`), [`PL_EVENT_ABI_
/// VERSION`] bumped 7 -> 8 -- see [`PlConnectFailedPayload::seq`]'s doc
/// comment for the shared rule.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectRetryingPayload {
    pub attempt: u16,
    pub seq: u16,
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
///
/// `seq` added by ADA DESIGN v2 (bead `pico-link-chc3`), [`PL_EVENT_ABI_
/// VERSION`] bumped 7 -> 8: `0` means the session genuinely APPEARED (a
/// headset-initiated reconnect, or the `PL_DEBUG_REMOTE` bypass) -- see
/// [`pico_link_core::app::Event::ConnectSucceeded`]'s doc comment for the
/// full rule `core`'s fold applies to it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectSucceededPayload {
    pub addr: [u8; 6],
    pub degraded: u8,
    pub seq: u16,
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

/// [`PlEvent`]'s payload when `tag == PlEventTag::LevelsChanged`. Bead
/// pico-link-du0, design section 21 E17/C8: one ~4Hz stereo OUT-meter
/// reading, sampled cheaply off the real-time encode path in
/// `firmware/src/a2dp.c` (a running peak/sum-of-squares accumulator,
/// reduced once per push interval) and pushed through the same `bt.c`
/// MPSC ring every other Bluetooth-domain event uses (`pl_bt_push_levels_
/// changed`, `bt.c`) -- never a direct call into this crate from IRQ
/// context (pico-link-6o2).
///
/// Every field is a plain `u8`, linear 0-255 (255 == full-scale PCM /
/// clipping) -- no validity invariant to violate, so
/// [`pl_ui_push_event`]'s `LevelsChanged` arm reads this union member
/// unconditionally once `tag` says it's live, same as
/// [`PlConnectRetryingPayload`]'s `attempt`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlLevelsChangedPayload {
    pub peak_l: u8,
    pub peak_r: u8,
    pub rms_l: u8,
    pub rms_r: u8,
}

/// Mirrors `firmware/src/volume.h`'s `PlVolumeSource` enum values 0/1/2
/// exactly (design `.planning/design/2026-09-02-volume-sync.md` section 7)
/// -- both sides are pinned independently rather than one generating the
/// other, same convention as [`PlStoreStatus`]'s doc comment explains for
/// `persist.h`. `volume.h`'s fourth value, `PL_VOLUME_SOURCE_CONSOLE = 3`
/// (T2's debug-only origin), is deliberately NOT a member here -- design
/// section 7 excludes it from this event, and `firmware/src/volume.c`'s
/// `apply_and_propagate` only calls `pl_bt_push_volume_changed` on the two
/// real edges (`emit == true`), never on the debug-console path.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlVolumeSource {
    Host = 0,
    Sink = 1,
    Device = 2,
}

impl core::convert::TryFrom<u8> for PlVolumeSource {
    type Error = ();

    /// Checked conversion from the raw wire value -- same hazard/fix as
    /// [`PlStoreStatus`]'s `TryFrom` impl (pico-link-ptu): a garbage
    /// `source` byte must be rejected, never matched-on or transmuted.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlVolumeSource::Host),
            1 => Ok(PlVolumeSource::Sink),
            2 => Ok(PlVolumeSource::Device),
            _ => Err(()),
        }
    }
}

impl From<PlVolumeSource> for VolumeSource {
    fn from(source: PlVolumeSource) -> Self {
        match source {
            PlVolumeSource::Host => VolumeSource::Host,
            PlVolumeSource::Sink => VolumeSource::Sink,
            PlVolumeSource::Device => VolumeSource::Device,
        }
    }
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::VolumeChanged`. Bead
/// pico-link-4v2.5 (VT5), design section 7: `level` is always in the
/// AVRCP absolute-volume domain (0..127) -- `firmware/src/volume.c`'s
/// canonical state's native domain, no mapping needed here. `muted` is a
/// plain `u8` boolean (0/1), not `bool`, matching this crate's C-repr
/// convention elsewhere (cbindgen emits `bool` fine, but the wider fields
/// around it here are already byte-sized, so this keeps the struct's
/// layout obviously flat). `source` is [`PlVolumeSource`]'s raw wire
/// value -- checked via `TryFrom` in [`pl_ui_push_event`], never
/// transmuted, same discipline as `PlStoreLoadedPayload::status`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlVolumeChangedPayload {
    pub level: u8,
    pub muted: u8,
    pub source: u8,
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::LdacBitrateChanged`
/// (bead pico-link-7jol.5, design `.planning/design/2026-09-07-ldac-
/// quality-selector.md` §6). Pushed from `firmware/src/a2dp.c`'s
/// `pl_a2dp_poll_ldac_bitrate` -- same once-per-superloop-iteration,
/// push-only-on-change cadence as `PlLevelsChangedPayload`. Deliberately
/// carries no `adaptive` flag: Home's `ADAPTIVE` tag is driven by the
/// connected device's *stored* `ldac_quality` (the `PairedDeviceUpserted`
/// echo), not a fact this event needs to duplicate.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlLdacBitrateChangedPayload {
    pub kbps: u32,
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
    /// The persisted LDAC quality pick, 1-based -- [`PairedDevice::
    /// ldac_quality`]'s doc comment. Added by bead pico-link-7jol.5
    /// (`.planning/design/2026-09-07-device-page-and-single-select-
    /// picker.md` §2.2's option (a)): a non-additive shape change to an
    /// EXISTING tag's payload (same class of change
    /// [`PlDeviceDiscoveredPayload::class_of_device`]'s own addition was),
    /// hence the [`PL_EVENT_ABI_VERSION`] bump 4 -> 5. This is the wire
    /// shape the `QUALITY` row/picker's "check follows the stored echo,
    /// never the press" rule depends on.
    pub ldac_quality: u8,
    /// The assigned DSP effects preset's id, `0`
    /// ([`pico_link_core::dsp::store::NO_PRESET_ID`]) meaning "none" --
    /// [`pico_link_core::app::PairedDevice::preset_id`]'s doc comment.
    /// Added by bead `pico-link-ryw.5`, design sec 3.2: a non-additive
    /// shape change to this EXISTING tag's payload (same class of change
    /// `ldac_quality`'s own addition was), hence the
    /// [`PL_EVENT_ABI_VERSION`] bump 5 -> 6.
    pub preset_id: u16,
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

/// Mirrors [`pico_link_core::app::FaultKey`]'s six ordinals exactly (design
/// `.planning/design/2026-09-07-audio-fault-model.md` §3.1's catalogue
/// table). Ordinals are **append-only forever** -- reordering this
/// catalogue is a wire break (§7.3). Only the ordinal itself is part of
/// the wire contract; the display name lives on the `core`-side type (rule
/// 3, §2).
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlFaultKey {
    BufStarved = 0,
    BufOverflow = 1,
    UsbSupplyLow = 2,
    AirCongested = 3,
    AirLinkLost = 4,
    EncResync = 5,
}

impl core::convert::TryFrom<u8> for PlFaultKey {
    type Error = ();

    /// Checked conversion from the raw wire value -- same hazard/fix as
    /// [`PlVolumeSource`]'s `TryFrom` impl (pico-link-ptu): an unknown
    /// ordinal must be rejected, never matched-on or transmuted (design
    /// §7.3: "an unknown ordinal increments `malformed_tag_count` and is
    /// dropped, never matched on as a discriminant").
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlFaultKey::BufStarved),
            1 => Ok(PlFaultKey::BufOverflow),
            2 => Ok(PlFaultKey::UsbSupplyLow),
            3 => Ok(PlFaultKey::AirCongested),
            4 => Ok(PlFaultKey::AirLinkLost),
            5 => Ok(PlFaultKey::EncResync),
            _ => Err(()),
        }
    }
}

impl From<PlFaultKey> for pico_link_core::app::FaultKey {
    fn from(key: PlFaultKey) -> Self {
        match key {
            PlFaultKey::BufStarved => pico_link_core::app::FaultKey::BufStarved,
            PlFaultKey::BufOverflow => pico_link_core::app::FaultKey::BufOverflow,
            PlFaultKey::UsbSupplyLow => pico_link_core::app::FaultKey::UsbSupplyLow,
            PlFaultKey::AirCongested => pico_link_core::app::FaultKey::AirCongested,
            PlFaultKey::AirLinkLost => pico_link_core::app::FaultKey::AirLinkLost,
            PlFaultKey::EncResync => pico_link_core::app::FaultKey::EncResync,
        }
    }
}

/// Wire-authoritative severity (design §7.3: "`glyph` and `severity` are
/// on the wire, not derived in Rust"). C decides this per raise, including
/// `AIR CONGESTED`'s dynamic escalation to `Audible` on co-occurrence with
/// `BUF OVERFLOW` (§7.3, §3.1) -- a computation that depends on window
/// counter deltas which never cross the seam, so `core`/`ui-ffi` could not
/// reproduce it even if they wanted to.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PlFaultSeverity {
    Concealed = 0,
    Audible = 1,
}

impl core::convert::TryFrom<u8> for PlFaultSeverity {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlFaultKey`]'s
    /// `TryFrom` impl for the identical rationale.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlFaultSeverity::Concealed),
            1 => Ok(PlFaultSeverity::Audible),
            _ => Err(()),
        }
    }
}

/// Wire-authoritative glyph class (design §3.1, §7.3) -- declared by C,
/// never inferred at render time (Uma's §5.2 requirement, satisfied here
/// with one table rather than two). Not consumed by anything in this bead
/// (S1 is Rust-only foundation, no rendering) -- carried through so S3
/// (`pico-link-9eq2.3.3`) has it without a follow-up wire change.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlFaultGlyph {
    Neutral = 0,
    Filled = 1,
    Starved = 2,
}

impl core::convert::TryFrom<u8> for PlFaultGlyph {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlFaultKey`]'s
    /// `TryFrom` impl for the identical rationale.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlFaultGlyph::Neutral),
            1 => Ok(PlFaultGlyph::Filled),
            2 => Ok(PlFaultGlyph::Starved),
            _ => Err(()),
        }
    }
}

/// Which of [`PlAudioFaultPayload::value`]'s interpretations applies
/// (design §3.1's "value kind" column) -- mirrors
/// [`pico_link_core::app::FaultValue`]'s three variants plus `None` (no
/// value at all, e.g. `USB SUPPLY LOW` before `pl_usb_supply_q8()` exists,
/// design §6.3: "ship that row absent, never faked").
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlFaultValueKind {
    None = 0,
    Ratio = 1,
    Count = 2,
    Millis = 3,
}

impl core::convert::TryFrom<u8> for PlFaultValueKind {
    type Error = ();

    /// Checked conversion from the raw wire value -- see [`PlFaultKey`]'s
    /// `TryFrom` impl for the identical rationale.
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PlFaultValueKind::None),
            1 => Ok(PlFaultValueKind::Ratio),
            2 => Ok(PlFaultValueKind::Count),
            3 => Ok(PlFaultValueKind::Millis),
            _ => Err(()),
        }
    }
}

/// [`PlEvent`]'s payload when `tag == PlEventTag::AudioFault` (bead
/// pico-link-9eq2.3.1, design `.planning/design/2026-09-07-audio-fault-
/// model.md` §7.3 exactly). Eight bytes, POD, `Copy`, no `Drop`, no
/// pointers -- unlike `DeviceDiscovered` it borrows nothing, so it has no
/// lifetime contract to violate and nothing to memcpy.
///
/// `key`/`severity`/`glyph`/`value_kind` are all **checked** on the Rust
/// side with the existing `TryFrom` idiom in [`pl_ui_push_event`]; an
/// unknown ordinal for any of them increments [`PlUi::malformed_tag_count`]
/// and the event is dropped rather than matched on as a discriminant
/// (which would be UB).
///
/// `count` is C's ABSOLUTE running count for `key`, not an increment --
/// see [`pico_link_core::app::FaultLog::record`]'s doc comment for the
/// "assigned, never added" rule (design §5.4).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlAudioFaultPayload {
    pub key: u8,
    pub severity: u8,
    pub glyph: u8,
    pub value_kind: u8,
    pub value: u16,
    pub count: u16,
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
    /// Bead pico-link-du0, design section 21 E17/C8: one ~4Hz stereo
    /// OUT-meter reading. Purely additive -- see [`PlLevelsChangedPayload`]'s
    /// doc comment; [`PL_EVENT_ABI_VERSION`] is unchanged by this tag's
    /// addition, same as [`Self::CodecChanged`]'s own addition was.
    LevelsChanged = 13,
    /// Bead pico-link-4v2.5 (VT5), design section 7: one canonical volume
    /// reading. Purely additive -- see [`PlVolumeChangedPayload`]'s doc
    /// comment; [`PL_EVENT_ABI_VERSION`] is unchanged by this tag's
    /// addition, same discipline `LevelsChanged` (tag 13, bead
    /// pico-link-du0) used for its own additive tag.
    VolumeChanged = 14,
    /// Bead pico-link-7jol.5, design `.planning/design/2026-09-07-ldac-
    /// quality-selector.md` §6: the LDAC encoder's live effective rate.
    /// Purely additive -- see [`PlLdacBitrateChangedPayload`]'s doc
    /// comment; [`PL_EVENT_ABI_VERSION`] is unchanged by THIS tag's own
    /// addition (same discipline as `LevelsChanged`/`VolumeChanged`
    /// above) -- the version bump this bead needed was for
    /// `PlPairedDeviceUpsertedPayload` gaining `ldac_quality`, an existing
    /// tag's payload, not this new one.
    LdacBitrateChanged = 15,
    /// Bead pico-link-9eq2.3.1, design `.planning/design/2026-09-07-audio-
    /// fault-model.md` §7.3: one audio fault raised or refreshed by C's 1Hz
    /// fault evaluator (firmware half is `pico-link-9eq2.3.2`, not yet
    /// producible from C as of this bead). Purely additive, same discipline
    /// as `LevelsChanged`/`VolumeChanged`/`LdacBitrateChanged` above --
    /// [`PL_EVENT_ABI_VERSION`] is unchanged by this tag's own addition --
    /// see [`PlAudioFaultPayload`]'s doc comment.
    AudioFault = 16,
    /// Bead `pico-link-88xs`, design `.planning/design/2026-09-08-link-
    /// state-vs-discovery-axis.md` §2.3/§2.4: whether the radio is
    /// currently running a GAP inquiry -- the second, independent axis
    /// from [`Self::LinkStateChanged`]. Purely additive, same discipline
    /// as `LevelsChanged`/`VolumeChanged`/`LdacBitrateChanged`/
    /// `AudioFault` above -- [`PL_EVENT_ABI_VERSION`] is unchanged by this
    /// tag's own addition. `17`, not reusing `PlLinkState::Scanning`'s old
    /// `1` -- see [`PlLinkState`]'s doc comment for why that ordinal is
    /// reserved-and-rejected rather than renumbered.
    DiscoveryStateChanged = 17,
    /// Bead pico-link-qivj.2: C's persisted screensaver dim/off + timeout
    /// setting finished loading at boot (`main.c` pushes this right after
    /// `pl_bt_init`, thread context, before the superloop starts -- same
    /// context `a2dp.c:1064` uses). Purely additive, same discipline as
    /// every tag from `LevelsChanged` (13) on -- [`PL_EVENT_ABI_VERSION`]
    /// is unchanged by this tag's own addition. See
    /// [`PlDisplaySettingsPayload`]'s doc comment.
    DisplaySettingsLoaded = 18,
    /// Bead pico-link-8pp1.4 (S3), design `.planning/design/2026-09-24-
    /// congestion-cushion.md` sec 4: C's persisted global congestion-
    /// cushion policy (`PL:S:1`) finished loading at boot (`main.c` pushes
    /// this right after `pl_bt_init`, same context/timing as
    /// `DisplaySettingsLoaded` above). Purely additive, same discipline as
    /// every tag from `LevelsChanged` (13) on -- [`PL_EVENT_ABI_VERSION`]
    /// is unchanged by this tag's own addition. See
    /// [`PlCushionPolicyPayload`]'s doc comment.
    CushionPolicyLoaded = 19,
    /// Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-25-
    /// adaptive-floor.md` sec 2: C's persisted global LDAC Adaptive floor
    /// (`PL:S:2`) finished loading at boot (`main.c` pushes this right
    /// after `pl_bt_init`, same context/timing as `CushionPolicyLoaded`
    /// above). Purely additive, same discipline as every tag from
    /// `LevelsChanged` (13) on -- [`PL_EVENT_ABI_VERSION`] is unchanged by
    /// this tag's own addition. See [`PlAbrFloorPayload`]'s doc comment.
    AbrFloorLoaded = 20,
    /// Bead `pico-link-ryw.5`, design sec 2.2/3.2: one DSP effects preset
    /// the flash store holds (boot push, or a [`PlCommandTag::SavePreset`]
    /// echo). Purely additive, same discipline as every tag from
    /// `LevelsChanged` (13) on -- [`PL_EVENT_ABI_VERSION`] is unchanged by
    /// THIS tag's own addition (the bump this bead needed was for
    /// `PlPairedDeviceUpsertedPayload` gaining `preset_id`, an existing
    /// tag's payload, not this new one). See [`PlPresetLoadedPayload`]'s
    /// doc comment.
    PresetLoaded = 21,
    /// Bead `pico-link-ryw.5`, design sec 2.4: a DSP effects preset was
    /// deleted (the [`PlCommandTag::DeletePreset`] echo). Purely
    /// additive, same discipline as [`Self::PresetLoaded`] above. See
    /// [`PlPresetDeletedPayload`]'s doc comment.
    PresetDeleted = 22,
    /// Bead `pico-link-ryw.5`, design sec 2.2: C's flash-backed DSP
    /// preset store (`PL:P:<slot>`) finished loading at boot -- the
    /// terminator of C's `count` x [`Self::PresetLoaded`] boot push
    /// sequence, same shape [`Self::StoreLoaded`] is for
    /// [`Self::PairedDeviceUpserted`]. Purely additive, same discipline
    /// as [`Self::PresetLoaded`] above. See [`PlPresetStoreLoadedPayload`]'s
    /// doc comment.
    PresetStoreLoaded = 23,
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
            13 => Ok(PlEventTag::LevelsChanged),
            14 => Ok(PlEventTag::VolumeChanged),
            15 => Ok(PlEventTag::LdacBitrateChanged),
            16 => Ok(PlEventTag::AudioFault),
            17 => Ok(PlEventTag::DiscoveryStateChanged),
            18 => Ok(PlEventTag::DisplaySettingsLoaded),
            19 => Ok(PlEventTag::CushionPolicyLoaded),
            20 => Ok(PlEventTag::AbrFloorLoaded),
            21 => Ok(PlEventTag::PresetLoaded),
            22 => Ok(PlEventTag::PresetDeleted),
            23 => Ok(PlEventTag::PresetStoreLoaded),
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
    /// Bead pico-link-du0. See [`PlLevelsChangedPayload`]'s doc comment.
    pub levels_changed: PlLevelsChangedPayload,
    /// Bead pico-link-4v2.5 (VT5). See [`PlVolumeChangedPayload`]'s doc
    /// comment.
    pub volume_changed: PlVolumeChangedPayload,
    /// Bead pico-link-7jol.5. See [`PlLdacBitrateChangedPayload`]'s doc
    /// comment.
    pub ldac_bitrate_changed: PlLdacBitrateChangedPayload,
    /// Bead pico-link-9eq2.3.1. See [`PlAudioFaultPayload`]'s doc comment.
    pub audio_fault: PlAudioFaultPayload,
    /// Bead pico-link-88xs. See [`PlDiscoveryStateChangedPayload`]'s doc
    /// comment.
    pub discovery_state_changed: PlDiscoveryStateChangedPayload,
    /// Bead pico-link-qivj.2. See [`PlDisplaySettingsPayload`]'s doc
    /// comment.
    pub display_settings: PlDisplaySettingsPayload,
    /// Bead pico-link-8pp1.4. See [`PlCushionPolicyPayload`]'s doc
    /// comment.
    pub cushion_policy: PlCushionPolicyPayload,
    /// Bead pico-link-d42g.3. See [`PlAbrFloorPayload`]'s doc comment.
    pub abr_floor: PlAbrFloorPayload,
    /// Bead pico-link-ryw.5. See [`PlPresetLoadedPayload`]'s doc comment.
    pub preset_loaded: PlPresetLoadedPayload,
    /// Bead pico-link-ryw.5. See [`PlPresetDeletedPayload`]'s doc comment.
    pub preset_deleted: PlPresetDeletedPayload,
    /// Bead pico-link-ryw.5. See [`PlPresetStoreLoadedPayload`]'s doc
    /// comment.
    pub preset_store_loaded: PlPresetStoreLoadedPayload,
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
//
// Bead pico-link-7jol.5, design `.planning/design/2026-09-07-device-page-
// and-single-select-picker.md` §2.2 option (a): bumped 4 -> 5.
// PlPairedDeviceUpsertedPayload gained `ldac_quality` -- a non-additive
// shape change to an existing tag's payload, same class of bump as all
// three above (see that field's doc comment).
//
// Bead pico-link-ryw.5, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 3.2: bumped 5 -> 6. PlPairedDeviceUpsertedPayload gained
// `preset_id` -- a non-additive shape change to an existing tag's payload,
// same class of bump as the 4->5 one above (see that field's doc comment).
// The three new tags this bead adds (PresetLoaded/PresetDeleted/
// PresetStoreLoaded) are purely additive on their own and would not have
// required a bump by themselves -- same discipline every tag from
// `LevelsChanged` (13) on documents.
//
// Bead pico-link-ryw.14, Ada's preset-id-allocation contract: bumped 6 ->
// 7. `PlPresetStoreLoadedPayload` gained `next_id` -- a non-additive shape
// change to an existing tag's payload, same class of bump as every one
// above.
//
// ADA DESIGN v2 (bead `pico-link-chc3`): bumped 7 -> 8.
// `PlConnectStepChangedPayload`/`PlConnectRetryingPayload`/
// `PlConnectSucceededPayload`/`PlConnectFailedPayload` all gained `seq` --
// a non-additive shape change to four existing tags' payloads at once, same
// class of bump as every one above. `seq` is the attempt identity `core`
// allocates ([`pico_link_core::app::ConnectAttempt::seq`]) and C echoes back
// on every connect-lifecycle event, so a late echo of a cancelled or
// superseded attempt can be told apart from the attempt it actually belongs
// to (see `.planning/design/2026-08-30-cancel-connect.md`'s "v2" section).
pub const PL_EVENT_ABI_VERSION: u32 = 8;

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
            decode_link_state_event(state)
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
            Event::ConnectFailed { addr: payload.addr, reason: reason.into(), seq: payload.seq }
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
            Event::ConnectStepChanged(step.into(), payload.seq)
        }
        PlEventTag::ConnectRetrying => {
            // SAFETY: `tag` says this union currently holds `connect_retrying`.
            // Reading it is sound regardless of `attempt`'s value -- it's a
            // plain `u16` with no validity invariant to violate.
            let payload = unsafe { event.payload.connect_retrying };
            Event::ConnectRetrying { attempt: payload.attempt, seq: payload.seq }
        }
        PlEventTag::ConnectSucceeded => {
            // SAFETY: `tag` says this union currently holds `connect_succeeded`.
            // Reading it is sound regardless of `degraded`'s value -- see
            // `PlConnectSucceededPayload`'s doc comment.
            let payload = unsafe { event.payload.connect_succeeded };
            Event::ConnectSucceeded { addr: payload.addr, degraded: payload.degraded != 0, seq: payload.seq }
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
            Event::PairedDeviceUpserted(PairedDevice {
                addr: payload.addr,
                name,
                mru_seq: payload.mru_seq,
                ldac_quality: payload.ldac_quality,
                preset_id: payload.preset_id,
            })
        }
        PlEventTag::PairedDeviceForgotten => {
            // SAFETY: same as the `PairedDeviceUpserted` arm above.
            let payload = unsafe { event.payload.paired_device_forgotten };
            Event::PairedDeviceForgotten { addr: payload.addr }
        }
        PlEventTag::PairedStoreFull => Event::PairedStoreFull,
        PlEventTag::LevelsChanged => {
            // SAFETY: `tag` says this union currently holds
            // `levels_changed`. Reading it is sound regardless of field
            // values -- every field is a plain `u8` with no validity
            // invariant to violate (see `PlLevelsChangedPayload`'s doc
            // comment).
            let payload = unsafe { event.payload.levels_changed };
            Event::LevelsChanged {
                peak_l: payload.peak_l,
                peak_r: payload.peak_r,
                rms_l: payload.rms_l,
                rms_r: payload.rms_r,
            }
        }
        PlEventTag::VolumeChanged => {
            // SAFETY: `tag` says this union currently holds
            // `volume_changed`. Reading it is sound regardless of field
            // values -- every field is a plain `u8` with no validity
            // invariant to violate (see `PlVolumeChangedPayload`'s doc
            // comment); `source` is range-checked below before use.
            let payload = unsafe { event.payload.volume_changed };
            let source = match PlVolumeSource::try_from(payload.source) {
                Ok(source) => source,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            let volume =
                pico_link_core::app::VolumeState { level: payload.level, muted: payload.muted != 0, source: source.into() };
            // Design `.planning/design/2026-09-07-volume-on-display.md`
            // section 5.1/5.3: a non-host source that isn't itself
            // muted/zero wakes the display to full and extends the idle
            // timer -- applied HERE (not deferred to `pl_ui_tick`)
            // because this is the site that actually observes the event,
            // and because `VolumeState::wakes_idle` needs the event's own
            // fields, not just the model's already-folded state. The
            // Asleep -> Active flip itself needs no clock (`on_input`),
            // but extending `last_input` does (`pl_ui_tick`'s `now_us`) --
            // see `PlUi::volume_wake_since_last_tick`'s doc comment for why
            // that half is deferred. A host-sourced or muted/zero reading
            // does neither: section 5.4 outranks 5.3, and a host event is
            // never evidence of a human at the device (section 5.2).
            if volume.wakes_idle() {
                ui.idle.on_input();
                ui.volume_wake_since_last_tick = true;
            } else if volume.muted || volume.level == 0 {
                // Section 5.4's floor, applied immediately rather than
                // waiting for the next `pl_ui_tick` -- an already-blanked
                // display must never sit dark once a mute/zero reading
                // arrives, from ANY source (including host). `on_input`
                // only flips the power level; it never touches
                // `last_input`, so this deliberately does NOT extend the
                // idle timer (no `volume_wake_since_last_tick` set here) --
                // `IdlePolicy::tick`'s own `mute_or_zero` check is what
                // then holds the display up for as long as the mute/zero
                // reading persists (see its doc comment), independent of
                // this one-off promotion.
                ui.idle.on_input();
            }
            Event::VolumeChanged { level: volume.level, muted: volume.muted, source: volume.source }
        }
        PlEventTag::LdacBitrateChanged => {
            // SAFETY: `tag` says this union currently holds
            // `ldac_bitrate_changed`. Reading it is sound regardless of
            // field values -- `kbps` is a plain `u32` with no validity
            // invariant to violate.
            let payload = unsafe { event.payload.ldac_bitrate_changed };
            Event::LdacBitrateChanged { kbps: payload.kbps }
        }
        PlEventTag::AudioFault => {
            // SAFETY: `tag` says this union currently holds `audio_fault`.
            // Reading it is sound regardless of field values -- every
            // field is a plain `u8`/`u16` with no validity invariant to
            // violate; `key`/`severity`/`glyph`/`value_kind` are range-
            // checked below before use (design §7.3).
            let payload = unsafe { event.payload.audio_fault };
            let key = match PlFaultKey::try_from(payload.key) {
                Ok(key) => key,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            let severity = match PlFaultSeverity::try_from(payload.severity) {
                Ok(severity) => severity,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            // `glyph` is validated even though this bead does no
            // rendering and never reads the checked value -- an unknown
            // ordinal must still be counted and dropped (design §7.3),
            // not silently accepted just because nothing consumes it yet.
            if PlFaultGlyph::try_from(payload.glyph).is_err() {
                ui.malformed_tag_count += 1;
                return;
            }
            let value_kind = match PlFaultValueKind::try_from(payload.value_kind) {
                Ok(value_kind) => value_kind,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            let value = match value_kind {
                PlFaultValueKind::None => None,
                PlFaultValueKind::Ratio => Some(pico_link_core::app::FaultValue::Ratio(payload.value)),
                PlFaultValueKind::Count => Some(pico_link_core::app::FaultValue::Count(payload.value)),
                PlFaultValueKind::Millis => Some(pico_link_core::app::FaultValue::Millis(payload.value)),
            };
            let core_key: pico_link_core::app::FaultKey = key.into();
            let now = pico_link_core::platform::Instant::from_micros(ui.app.now_us());
            // Design §7.2: only an `Audible` key, not already Live, whose
            // static `wakes_display` bit is set, wakes the display (§7.5:
            // ords 0/1/4 -- `AIR CONGESTED` keeps `wakes_display = false`
            // even when it escalates to `Audible`, because its co-live
            // `BUF OVERFLOW` already provides the wake). Computed BEFORE
            // folding this event below -- folding first would make
            // `is_live` trivially true for the very raise being checked.
            let already_live = ui.app.model().fault_log.is_live(core_key, now);
            let wants_wake = severity == PlFaultSeverity::Audible && fault_key_wakes_display(core_key) && !already_live;
            if wants_wake {
                // Storm-limited: `IdlePolicy::on_fault_wake` applies the
                // cooldown/session-cap gate and, if granted, both flips
                // the display Asleep -> Active and arms the expiring
                // hold. Applied HERE, at the event site (design §7.5),
                // using `ui.app.now_us()` -- unlike `VolumeChanged`'s
                // wake (which defers its clock-dependent half to the next
                // `pl_ui_tick`, see `PlUi::volume_wake_since_last_tick`'s
                // doc comment), this call site already has a clock
                // reading available, so no deferred flag is needed --
                // exactly the "nothing equivalent to
                // `volume_wake_since_last_tick`" instruction (design
                // §7.5, home-fault-strip §12 Ruby item 3).
                ui.idle.on_fault_wake(now);
            }
            Event::FaultRaised { key: core_key, value, count: payload.count }
        }
        PlEventTag::DiscoveryStateChanged => {
            // SAFETY: `tag` says this union currently holds
            // `discovery_state_changed`. Reading it is sound regardless of
            // `state`'s value because `PlDiscoveryStateChangedPayload::state`
            // is a plain `u32` -- see that field's doc comment.
            let payload = unsafe { event.payload.discovery_state_changed };
            let state = match PlDiscoveryState::try_from(payload.state) {
                Ok(state) => state,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            Event::DiscoveryStateChanged { scanning: state.is_scanning() }
        }
        PlEventTag::DisplaySettingsLoaded => {
            // SAFETY: `tag` says this union currently holds
            // `display_settings`. Reading it is sound regardless of its
            // field values -- `core`'s own `DisplaySettings::from_wire`
            // (called inside `App::handle_event`) falls back per-field on
            // any out-of-range value, same as every other malformed-but-
            // memory-safe payload in this union.
            let payload = unsafe { event.payload.display_settings };
            Event::DisplaySettingsLoaded { mode: payload.mode, timeout_s: payload.timeout_s }
        }
        PlEventTag::CushionPolicyLoaded => {
            // SAFETY: `tag` says this union currently holds
            // `cushion_policy`. Reading it is sound regardless of its
            // field value -- `core`'s own `CushionPolicy::from_wire`
            // (called inside `App::handle_event`) falls back on any
            // out-of-range value, same as `DisplaySettingsLoaded` above.
            let payload = unsafe { event.payload.cushion_policy };
            Event::CushionPolicyLoaded { policy: payload.policy }
        }
        PlEventTag::AbrFloorLoaded => {
            // SAFETY: `tag` says this union currently holds `abr_floor`.
            // Reading it is sound regardless of its field value -- `core`'s
            // own `AbrFloor::from_wire` (called inside `App::handle_event`)
            // falls back on any out-of-range value, same as
            // `CushionPolicyLoaded` above.
            let payload = unsafe { event.payload.abr_floor };
            Event::AbrFloorLoaded { floor: payload.floor }
        }
        PlEventTag::PresetLoaded => {
            // SAFETY: `tag` says this union currently holds
            // `preset_loaded`. Reading it is sound regardless of field
            // values -- every field is a plain integer/byte-array type
            // with no validity invariant to violate (see
            // `PlPresetLoadedPayload`'s doc comment); `core`'s own
            // `Preset::from_wire` degrades per-field on a malformed blob,
            // never panics.
            let payload = unsafe { event.payload.preset_loaded };
            let blob_len = usize::from(payload.blob_len).min(payload.blob.len());
            let blob = alloc::vec::Vec::from(&payload.blob[..blob_len]);
            Event::PresetLoaded { id: payload.id, blob }
        }
        PlEventTag::PresetDeleted => {
            // SAFETY: `tag` says this union currently holds
            // `preset_deleted`. `id` is a plain `u16` with no validity
            // invariant to violate.
            let payload = unsafe { event.payload.preset_deleted };
            Event::PresetDeleted { id: payload.id }
        }
        PlEventTag::PresetStoreLoaded => {
            // SAFETY: `tag` says this union currently holds
            // `preset_store_loaded`. Reading it is sound regardless of
            // field values; `status` is range-checked below before use,
            // same discipline as `PlEventTag::StoreLoaded` above.
            let payload = unsafe { event.payload.preset_store_loaded };
            let status = match PlStoreStatus::try_from(payload.status) {
                Ok(status) => status,
                Err(()) => {
                    ui.malformed_tag_count += 1;
                    return;
                }
            };
            Event::PresetStoreLoaded { count: payload.count, status: status.into(), next_id: payload.next_id }
        }
    };
    ui.app.handle_event(core_event);
}

/// Static wake-eligibility table (design §7.5: "Only `Audible` keys with
/// `wakes_display = true` request a wake -- three of six (ords 0, 1, 4)").
/// Deliberately NOT on the wire (§7.3: "a static per-key property... Rust
/// never needs it" -- from C's side; `ui-ffi` still needs its own copy to
/// gate the wake decision) and deliberately NOT part of
/// [`pico_link_core::app::FaultKey`]'s own table (this bead's scope item 2:
/// only ordinals and names live in `core`) -- this is wake *policy*, kept
/// beside the one FFI call site that applies it rather than the display
/// catalogue.
fn fault_key_wakes_display(key: pico_link_core::app::FaultKey) -> bool {
    use pico_link_core::app::FaultKey;
    matches!(key, FaultKey::BufStarved | FaultKey::BufOverflow | FaultKey::AirLinkLost)
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
    /// its own [`PlCancelConnectPayload`] (ADA DESIGN v2, bead
    /// `pico-link-chc3` -- moved off the shared [`PlAddrPayload`] the
    /// instant a second field, `seq`, was needed). The real abort
    /// semantics (tearing down an in-flight ACL/SSP/AVDTP attempt) are
    /// implemented on the C side as of `chc3`; see that bead's design
    /// (`.planning/design/2026-08-30-cancel-connect.md`) for the full
    /// cid-scoped teardown/reissue protocol.
    CancelConnect = 4,
    /// Bead pico-link-cz0.6 (M5 persistence), design point 7: `core`'s
    /// auto-reconnect/remember-this-device POLICY output. Carries the
    /// target `addr` via the [`PlAddrPayload`] union member (bead
    /// pico-link-4vb.6 moved this off `.connect` onto `.addr` -- see
    /// [`PlCommandPayload`]'s doc comment; a *source*-only change, no wire
    /// byte moves. `chc3` later gave [`CancelConnect`](Self::CancelConnect)
    /// its own payload instead, once it needed a `seq` field this one
    /// still doesn't.).
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
    /// Bead pico-link-44w: user-initiated "drop the current Bluetooth
    /// link" -- FFI surface only, no screen queues this yet (the
    /// design-of-record's manage-connected-device screen is the eventual
    /// caller; wiring it up now would be a labelled-but-dead UI
    /// affordance, which the design of record's rule 2 forbids). Carries
    /// no payload: `firmware/src/a2dp.c` tracks at most one active
    /// connection (`s_ctx.a2dp_cid`), and C's existing debug-only
    /// disconnect path (`pl_bt_debug_disconnect`, bead pico-link-nb6)
    /// already queues its `PL_BT_PENDING_DISCONNECT` pending-queue entry
    /// with a null address for the same reason -- see
    /// [`pl_command_from`]'s `Command::Disconnect` arm, which reuses the
    /// zeroed `connect` payload the same way `StartScan`/`CancelScan` do.
    /// Purely additive to the tag enum -- no existing payload shape
    /// changed -- so this does not bump [`PL_COMMAND_ABI_VERSION`] (see
    /// that constant's doc comment: the bump is reserved for non-additive
    /// changes to an *existing* tag's payload, which this is not).
    Disconnect = 7,
    /// Bead pico-link-7jol.5, design `.planning/design/2026-09-07-ldac-
    /// quality-selector.md` §5: user-initiated pin/Adaptive pick for one
    /// device's LDAC quality, from the `QUALITY` picker's `A` handler.
    /// Purely additive to the tag enum -- no existing payload shape
    /// changed -- so this does not bump [`PL_COMMAND_ABI_VERSION`], same
    /// reasoning as [`Disconnect`](Self::Disconnect)'s own addition.
    SetDeviceLdacQuality = 8,
    /// Bead pico-link-qivj.2: the screensaver dim/off + timeout setting
    /// changed (a Settings-picker pick), for C to persist under `PL:S:0`.
    /// Unlike every other command here, this one is NOT drained from
    /// `App`'s ordinary `commands` queue -- it comes from
    /// `App::take_display_settings_to_save`, checked FIRST by
    /// [`pl_ui_poll_command`], ahead of the ordinary queue (design D9: one
    /// save latch in `core`, two adapters -- this is the `ui-ffi` one, the
    /// emulator's `Runner::step` is the other). Purely additive to the tag
    /// enum -- no existing payload shape changed -- so this does not bump
    /// [`PL_COMMAND_ABI_VERSION`], same reasoning as
    /// [`SetDeviceLdacQuality`](Self::SetDeviceLdacQuality)'s own addition.
    SetDisplaySettings = 9,
    /// Bead pico-link-8pp1.4 (S3), design `.planning/design/2026-09-24-
    /// congestion-cushion.md` sec 4: the global congestion-cushion policy
    /// changed (a future Settings-picker pick, S4), for C to persist under
    /// `PL:S:1` and apply live via `pl_a2dp_set_trim_policy`. Like
    /// [`SetDisplaySettings`](Self::SetDisplaySettings), NOT drained from
    /// `App`'s ordinary `commands` queue -- it comes from `App::take_
    /// cushion_policy_to_save`, checked in [`pl_ui_poll_command`] the same
    /// way (design D9). Purely additive to the tag enum -- no existing
    /// payload shape changed -- so this does not bump
    /// [`PL_COMMAND_ABI_VERSION`], same reasoning as
    /// [`SetDisplaySettings`](Self::SetDisplaySettings)'s own addition.
    SetCushionPolicy = 10,
    /// Bead pico-link-d42g.3 (F3), design `.planning/design/2026-09-25-
    /// adaptive-floor.md` sec 2/4: the global LDAC Adaptive floor changed
    /// (a future Settings-picker pick, F4), for C to persist under `PL:S:2`
    /// and apply live via `pl_codec_ldac_set_floor`. Like
    /// [`SetCushionPolicy`](Self::SetCushionPolicy), NOT drained from
    /// `App`'s ordinary `commands` queue -- it comes from `App::take_
    /// abr_floor_to_save`, checked in [`pl_ui_poll_command`] the same way
    /// (design D9). Purely additive to the tag enum -- no existing payload
    /// shape changed -- so this does not bump [`PL_COMMAND_ABI_VERSION`],
    /// same reasoning as [`SetCushionPolicy`](Self::SetCushionPolicy)'s own
    /// addition.
    SetAbrFloor = 11,
    /// Bead `pico-link-ryw.5`, design sec 3.2: create (`preset_id == 0`)
    /// or overwrite a DSP effects preset. Drained from `App`'s ordinary
    /// `commands` queue, unlike `SetDisplaySettings`/`SetCushionPolicy`/
    /// `SetAbrFloor` above -- Andreas's "every value change in the editor
    /// saves immediately" ruling means each edit is its own queued
    /// command, not a single coalesced latch (see
    /// `pico_link_core::app::Command::SavePreset`'s doc comment). See
    /// [`PlSavePresetPayload`]'s doc comment.
    SavePreset = 12,
    /// Bead `pico-link-ryw.5`, design sec 2.4/3.2: delete a DSP effects
    /// preset. Drained from the ordinary `commands` queue, same shape as
    /// [`Self::SavePreset`]. See [`PlDeletePresetPayload`]'s doc comment.
    DeletePreset = 13,
    /// Bead `pico-link-ryw.5`, design sec 3.2: assign (or clear) a
    /// device's DSP effects preset. Drained from the ordinary `commands`
    /// queue, same shape as [`Self::SavePreset`]. See
    /// [`PlAssignPresetPayload`]'s doc comment.
    AssignPreset = 14,
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
///
/// `seq` added by ADA DESIGN v2 (bead `pico-link-chc3`), [`PL_COMMAND_ABI_
/// VERSION`] bumped 4 -> 5: the [`pico_link_core::app::ConnectAttempt::seq`]
/// `core` allocated for this attempt, echoed back by C on every
/// connect-lifecycle event -- see [`PlCancelConnectPayload`]'s doc comment
/// for the shared rationale.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectPayload {
    pub addr: [u8; 6],
    pub name: [u8; 32],
    pub name_len: u8,
    pub seq: u16,
}

/// [`PlCommand`]'s payload for every tag that carries nothing but a target
/// address -- [`PlCommandTag::PersistDevice`] and
/// [`PlCommandTag::ForgetDevice`] (bead pico-link-4vb.6, T2, design section
/// 5.3; [`PlCommandTag::CancelConnect`] moved off this shared payload onto
/// its own [`PlCancelConnectPayload`] with `chc3`, see that struct's doc
/// comment). Introduced as its own type, rather than continuing to reuse
/// [`PlConnectPayload`] the way those tags did before pico-link-4vb.6,
/// because `PlConnectPayload` also carries `name`/`name_len`/`seq` --
/// fields meaningless for a persist/forget command. Both structs still
/// start with `addr: [u8; 6]` at the same offset, so moving a tag onto this
/// payload is a *source*-only change: no wire byte moves (see
/// [`PlCommandPayload`]'s doc comment).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlAddrPayload {
    pub addr: [u8; 6],
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::CancelConnect`.
/// Introduced by ADA DESIGN v2 (bead `pico-link-chc3`), replacing the
/// shared [`PlAddrPayload`] this tag used before -- `seq` is the second
/// field a cancel needs and `PlAddrPayload` has no room to grow it without
/// also handing it, meaninglessly, to `PersistDevice`/`ForgetDevice`.
///
/// `seq` is the [`pico_link_core::app::ConnectAttempt::seq`] being
/// cancelled (`0` if there was no attempt in flight -- see
/// [`pico_link_core::app::Command::CancelConnect`]'s doc comment). C scopes
/// the abort to the cid owned by this `seq`, never globally; a debug-only
/// `PL_SEQ_ANY` ("cancel whatever is in flight") exists on the C side only
/// and is never produced by `core`/this crate.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlCancelConnectPayload {
    pub addr: [u8; 6],
    pub seq: u16,
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::SetDeviceLdacQuality`
/// (bead pico-link-7jol.5). `ldac_quality` is 1-based, mirroring
/// [`pico_link_core::app::PairedDevice::ldac_quality`]'s convention (`0` =
/// never chosen, `1`/`2`/`3` = pinned 990/660/330 kbps, `4` = Adaptive) --
/// `core` is the sole producer and always sends a value in `1..=4`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlSetDeviceLdacQualityPayload {
    pub addr: [u8; 6],
    pub ldac_quality: u8,
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::SavePreset` (bead
/// `pico-link-ryw.5`, design sec 3.2; reallocation contract updated by bead
/// `pico-link-ryw.14`, [`PL_COMMAND_ABI_VERSION`] 3 -> 4). `preset_id` is
/// ALWAYS a real, already-allocated id -- `core` is now the sole allocator
/// ([`pico_link_core::dsp::PresetStore::create`]); `0`
/// ([`pico_link_core::dsp::store::NO_PRESET_ID`]) is never sent and C must
/// refuse it (log only, no echo) if it ever sees one. C treats this as an
/// UPSERT: if a slot already holds `preset_id`, overwrite it; if no slot
/// holds it but it's at or past C's own high-water mark, claim a free slot
/// and raise the mark past it; otherwise (a stale or already-deleted id, or
/// no free slot) refuse. After every attempt, successful or refused, C
/// emits exactly one truth echo for `preset_id` -- [`PlEventTag::
/// PresetLoaded`] if a slot holds it (the new blob on success, the old one
/// unchanged on a same-id refusal), else [`PlEventTag::PresetDeleted`] (a
/// refused creation, which never had a slot to begin with). `blob` is an
/// inline fixed buffer copied by value, `blob_len` bytes meaningful -- same
/// shape as [`PlPresetLoadedPayload`], and C treats it exactly as opaquely
/// (design sec 2.2: "C NEVER parses the blob").
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlSavePresetPayload {
    pub preset_id: u16,
    pub blob_len: u8,
    pub blob: [u8; PL_DSP_PRESET_BLOB_LEN],
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::DeletePreset` (bead
/// `pico-link-ryw.5`, design sec 2.4/3.2).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDeletePresetPayload {
    pub preset_id: u16,
}

/// [`PlCommand`]'s payload when `tag == PlCommandTag::AssignPreset` (bead
/// `pico-link-ryw.5`, design sec 3.2). `preset_id ==
/// `[`pico_link_core::dsp::store::NO_PRESET_ID`]`` clears the assignment
/// (Off).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlAssignPresetPayload {
    pub addr: [u8; 6],
    pub preset_id: u16,
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
    /// See [`PlCancelConnectPayload`]'s doc comment. Bead `pico-link-chc3`.
    pub cancel_connect: PlCancelConnectPayload,
    /// See [`PlSetDeviceLdacQualityPayload`]'s doc comment. Bead
    /// pico-link-7jol.5.
    pub set_device_ldac_quality: PlSetDeviceLdacQualityPayload,
    /// See [`PlDisplaySettingsPayload`]'s doc comment. Bead pico-link-qivj.2.
    pub display_settings: PlDisplaySettingsPayload,
    /// See [`PlCushionPolicyPayload`]'s doc comment. Bead pico-link-8pp1.4.
    pub cushion_policy: PlCushionPolicyPayload,
    /// See [`PlAbrFloorPayload`]'s doc comment. Bead pico-link-d42g.3.
    pub abr_floor: PlAbrFloorPayload,
    /// See [`PlSavePresetPayload`]'s doc comment. Bead pico-link-ryw.5.
    pub save_preset: PlSavePresetPayload,
    /// See [`PlDeletePresetPayload`]'s doc comment. Bead pico-link-ryw.5.
    pub delete_preset: PlDeletePresetPayload,
    /// See [`PlAssignPresetPayload`]'s doc comment. Bead pico-link-ryw.5.
    pub assign_preset: PlAssignPresetPayload,
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
//
// Bead pico-link-ryw.5, design `.planning/design/2026-09-25-dsp-effects-
// stage.md` sec 3.2: bumped 2 -> 3, "because the union grows" (the design's
// own stated rationale) -- `PlCommandPayload` gains `save_preset`, whose
// `blob: [u8; PL_DSP_PRESET_BLOB_LEN]` (80 bytes) is far larger than any
// existing member, growing `sizeof(PlCommandPayload)` itself rather than
// merely adding a same-scale member the way `SetDeviceLdacQuality`/
// `SetDisplaySettings`/`SetCushionPolicy`/`SetAbrFloor` did (none of which
// bumped this constant). `SavePreset`/`DeletePreset`/`AssignPreset`'s tag
// values themselves are still purely additive -- this bump is about the
// union's size, not about any *existing* tag's payload shape changing.
//
// Bead pico-link-ryw.14, Ada's preset-id-allocation contract: bumped 3 ->
// 4. `PlSavePresetPayload`'s wire layout is UNCHANGED (`preset_id` is still
// a plain `u16`), but its MEANING changed: `preset_id == 0` no longer means
// "C, please allocate" -- every id `core` sends is now its own, already-
// Rust-allocated, real id, and C must upsert rather than treat `0`
// specially. A mismatched C build reading a `core` built against this
// contract (or vice versa) would silently misinterpret every `SavePreset`
// under the old allocate-on-0 rule (refusing every creation under rule (c)/
// (d) of the new contract) -- bumping this constant turns that into a loud
// version mismatch instead.
//
// ADA DESIGN v2 (bead `pico-link-chc3`): bumped 4 -> 5. `PlConnectPayload`
// gained `seq`, and `PlCommandTag::CancelConnect` moved off the shared
// `PlAddrPayload` onto its own `PlCancelConnectPayload { addr, seq }` -- two
// non-additive shape changes landing together, same class of bump as every
// one above.
pub const PL_COMMAND_ABI_VERSION: u32 = 5;

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
        payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0, seq: 0 } },
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
            payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0, seq: 0 } },
        },
        Command::Connect { addr, name, seq } => {
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
                payload: PlCommandPayload { connect: PlConnectPayload { addr, name: name_buf, name_len, seq } },
            }
        }
        Command::CancelScan => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::CancelScan,
            payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0, seq: 0 } },
        },
        // ADA DESIGN v2 (bead `pico-link-chc3`): its own
        // `PlCancelConnectPayload` -- see that struct's doc comment for why
        // this moved off the shared `PlAddrPayload` pico-link-4vb.6 put it
        // on.
        Command::CancelConnect { addr, seq } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::CancelConnect,
            payload: PlCommandPayload { cancel_connect: PlCancelConnectPayload { addr, seq } },
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
        // Bead pico-link-44w: no payload -- see `PlCommandTag::Disconnect`'s
        // doc comment. Reuses the zeroed `connect` payload the same way
        // `StartScan`/`CancelScan` do, since there is nothing to carry.
        Command::Disconnect => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::Disconnect,
            payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6], name: [0; 32], name_len: 0, seq: 0 } },
        },
        Command::SetDeviceLdacQuality { addr, ldac_quality } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::SetDeviceLdacQuality,
            payload: PlCommandPayload { set_device_ldac_quality: PlSetDeviceLdacQualityPayload { addr, ldac_quality } },
        },
        Command::SavePreset { preset_id, blob } => {
            // `core` is the sole producer of `Command` values and its own
            // `Preset::to_wire` never emits more than `dsp::preset::
            // BLOB_LEN` (70) bytes, well inside the 80-byte wire buffer --
            // the `.min()` here is defensive only, same discipline
            // `Command::Connect`'s `name` truncation above uses.
            let mut blob_buf = [0u8; PL_DSP_PRESET_BLOB_LEN];
            let len = blob.len().min(blob_buf.len());
            blob_buf[..len].copy_from_slice(&blob[..len]);
            let blob_len = u8::try_from(len).unwrap_or(u8::MAX);
            PlCommand {
                version: PL_COMMAND_ABI_VERSION,
                tag: PlCommandTag::SavePreset,
                payload: PlCommandPayload { save_preset: PlSavePresetPayload { preset_id, blob_len, blob: blob_buf } },
            }
        }
        Command::DeletePreset { preset_id } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::DeletePreset,
            payload: PlCommandPayload { delete_preset: PlDeletePresetPayload { preset_id } },
        },
        Command::AssignPreset { addr, preset_id } => PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::AssignPreset,
            payload: PlCommandPayload { assign_preset: PlAssignPresetPayload { addr, preset_id } },
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
    // Checked FIRST, ahead of the ordinary `commands` queue -- design D9:
    // the display-settings save latch is a separate mailbox on `App`, not
    // routed through `Command`/`poll_command` at all (see
    // `PlCommandTag::SetDisplaySettings`'s doc comment).
    if let Some(settings) = ui.app.take_display_settings_to_save() {
        let (mode, timeout_s) = settings.to_wire();
        return PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::SetDisplaySettings,
            payload: PlCommandPayload { display_settings: PlDisplaySettingsPayload { mode, timeout_s } },
        };
    }
    // Bead pico-link-8pp1.4 (S3): the cushion-policy save latch, same
    // checked-first-ahead-of-the-ordinary-queue shape as
    // `take_display_settings_to_save` above (design D9).
    if let Some(policy) = ui.app.take_cushion_policy_to_save() {
        return PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::SetCushionPolicy,
            payload: PlCommandPayload { cushion_policy: PlCushionPolicyPayload { policy: policy.to_wire() } },
        };
    }
    // Bead pico-link-d42g.3 (F3): the Adaptive-floor save latch, same
    // checked-first-ahead-of-the-ordinary-queue shape as
    // `take_cushion_policy_to_save` above (design D9).
    if let Some(floor) = ui.app.take_abr_floor_to_save() {
        return PlCommand {
            version: PL_COMMAND_ABI_VERSION,
            tag: PlCommandTag::SetAbrFloor,
            payload: PlCommandPayload { abr_floor: PlAbrFloorPayload { floor: floor.to_wire() } },
        };
    }
    match ui.app.poll_command() {
        Some(command) => pl_command_from(command),
        None => pl_command_none(),
    }
}

// --- The DSP effects program pull API (bead pico-link-ryw.5, design sec
// 3.2) ---
//
// `PlBiquad`/`PlDspProgram` are declared in `firmware/src/dsp.h`, NOT
// generated by cbindgen from this crate -- see `ui-ffi/cbindgen.toml`'s
// `export.exclude` and its `after_includes` injection of `#include
// "dsp.h"`. Both sides are pinned INDEPENDENTLY, matching this module's
// existing `PlStoreStatus`/`persist.h` convention (see that type's doc
// comment): `dsp.c`'s realtime kernel already owns this exact struct shape
// (bead `pico-link-ryw.1`, built before this bead), and a second,
// cbindgen-generated definition of the same name would collide with it in
// any translation unit that includes both `dsp.h` and the generated
// `pico_link_ui.h` (which `main.c` does). The two struct definitions below
// exist purely so THIS crate has a concrete Rust type to build a
// [`PlDspProgram`] value with at the FFI boundary -- they must stay
// field-for-field identical to `dsp.h`'s C definitions (order, types, and
// therefore layout) or the two sides silently disagree about what a given
// byte range means. `#[repr(C)]` on both makes this crate's own layout
// follow the same C ABI rules `dsp.h`'s structs do, so as long as the
// field lists match, the layouts match too.

/// Mirrors `firmware/src/dsp.h`'s `PlBiquad` exactly (one a0-normalised
/// biquad in Transposed Direct Form II) -- see this module section's doc
/// comment for why this is a second, independently-pinned definition
/// rather than a cbindgen-generated one.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlBiquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

/// `PL_DSP_MAX_BIQUADS` -- mirrors `firmware/src/dsp.h`'s constant of the
/// same name exactly (independently pinned, same convention as
/// [`PlBiquad`]/[`PlDspProgram`] themselves). [`pl_dsp_program_to_wire`]
/// clamps to this even though `core`'s own [`pico_link_core::dsp::preset::MAX_BANDS`]
/// already caps a [`pico_link_core::dsp::preset::Preset`] at the same
/// count -- the FFI boundary's own defensive backstop, same discipline
/// this bead's CONTRACT comment asks `dsp.c`'s `pl_dsp_submit` to apply on
/// the C side of the same value.
const PL_DSP_MAX_BIQUADS: usize = 10;

/// The realtime DSP path is fixed at 48kHz -- mirrors `dsp.h`'s
/// `PL_DSP_EXPECTED_FS_HZ` exactly (independently pinned; design sec 1.2:
/// "fs is fixed at 48k... The program carries `fs_hz`, and the kernel
/// bypasses on mismatch"). `core` never has occasion to compile a
/// [`pico_link_core::dsp::Program`] at any other rate, so this is the one
/// value [`pl_ui_take_dsp_program`] ever calls
/// [`pico_link_core::app::App::dsp_program`] with.
const PL_DSP_FS_HZ: u32 = 48_000;

/// Mirrors `firmware/src/dsp.h`'s `PlDspProgram` exactly -- field order,
/// types and therefore layout, per this module section's doc comment.
/// `xfeed_on`/`n_biquads` are plain `u8` (not `bool`), matching `dsp.h`'s
/// own field types (its kernel reads them with C truthiness, not as a
/// validated boolean -- see `pl_dsp_program_is_active` in `dsp.c`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlDspProgram {
    pub fs_hz: u32,
    pub preamp: f32,
    pub n_biquads: u8,
    pub xfeed_on: u8,
    pub xfeed_lp_b0: f32,
    pub xfeed_lp_a1: f32,
    pub xfeed_gain: f32,
    pub xfeed_hs_b0: f32,
    pub xfeed_hs_b1: f32,
    pub xfeed_hs_a1: f32,
    pub xfeed_norm: f32,
    pub biquad: [PlBiquad; PL_DSP_MAX_BIQUADS],
}

/// Maps a [`pico_link_core::dsp::Program`] to its wire [`PlDspProgram`]
/// shape -- the exact field mapping this bead's CONTRACT comment (bead
/// `pico-link-ryw.5`'s dispatch, from the ryw.1/ryw.2 code review) pins:
/// `CrossfeedCoeffs::lo_b0 -> xfeed_lp_b0`, `lo_a1 -> xfeed_lp_a1`,
/// `hi_b0/hi_b1/hi_a1 -> xfeed_hs_b0/b1/a1`, `norm_gain -> xfeed_norm`, and
/// `xfeed_gain` is ALWAYS `1.0` -- `g_lo` is already folded into `lo_b0`
/// (see [`pico_link_core::dsp::CrossfeedCoeffs`]'s own doc comment), so
/// deriving `xfeed_gain` from any `CrossfeedCoeffs` field would double-
/// apply it. `preamp`/`fs_hz`/the biquad field order and sign convention
/// already match `core`'s own [`pico_link_core::dsp::Biquad`] one-to-one,
/// no conversion needed beyond a plain copy.
///
/// A pure function (no `PlUi`/FFI involved), same "the actual risk here is
/// unit-testable directly" reasoning [`pl_command_from`]'s doc comment
/// gives for its own pure-mapping shape.
// `n as u8`: `n <= PL_DSP_MAX_BIQUADS == 10`, so this never truncates --
// same reasoning as `pl_command_from`'s own `name_len` cast. The
// `xfeed_lp_*`/`xfeed_hs_*` local names are deliberately kept close to
// `PlDspProgram`'s own field names (this bead's CONTRACT mapping is
// checkable line-by-line against them), same rationale
// `pico_link_core::dsp::coeffs::crossfeed_coeffs`'s own
// `#[allow(clippy::similar_names)]` gives for its `gb_lo`/`g_lo`/`gb_hi`/
// `g_hi` locals.
#[allow(clippy::cast_possible_truncation, clippy::similar_names)]
fn pl_dsp_program_to_wire(program: &pico_link_core::dsp::Program) -> PlDspProgram {
    let mut biquad = [PlBiquad { b0: 0.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }; PL_DSP_MAX_BIQUADS];
    let n = program.biquads.len().min(PL_DSP_MAX_BIQUADS);
    for (slot, bq) in biquad.iter_mut().zip(program.biquads.iter()).take(n) {
        *slot = PlBiquad { b0: bq.b0, b1: bq.b1, b2: bq.b2, a1: bq.a1, a2: bq.a2 };
    }
    let n_biquads = n as u8;

    let (xfeed_on, xfeed_lp_b0, xfeed_lp_a1, xfeed_gain, xfeed_hs_b0, xfeed_hs_b1, xfeed_hs_a1, xfeed_norm) =
        match program.crossfeed {
            Some(xf) => (1u8, xf.lo_b0, xf.lo_a1, 1.0f32, xf.hi_b0, xf.hi_b1, xf.hi_a1, xf.norm_gain),
            None => (0u8, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        };

    PlDspProgram {
        fs_hz: program.fs_hz,
        preamp: program.preamp_linear,
        n_biquads,
        xfeed_on,
        xfeed_lp_b0,
        xfeed_lp_a1,
        xfeed_gain,
        xfeed_hs_b0,
        xfeed_hs_b1,
        xfeed_hs_a1,
        xfeed_norm,
        biquad,
    }
}

/// The DSP program pull (design sec 3.2: "the PROGRAM IS STATE, NOT AN
/// EVENT, so it gets its own pull API, not a `PlCommand`" -- a
/// level-seqlock, newest wins, that keeps ~250B out of every `PlCommand`
/// copy). Returns `true` and writes `*out` when the currently active
/// program -- [`pico_link_core::app::App::dsp_program`], resolved from the
/// connected device's assigned preset, see that method's doc comment --
/// differs from the last value this function returned; returns `false`
/// and leaves `*out` untouched otherwise.
///
/// C is expected to call this once per superloop iteration, next to
/// `pl_a2dp_poll_levels` (design sec 3.2): `if
/// (pl_ui_take_dsp_program(ui, &program)) { pl_dsp_submit(&program); }`,
/// then unconditionally `pl_dsp_service()` regardless of this call's
/// result -- that second call retries a still-unacked previous publish
/// (see `dsp.h`'s `pl_dsp_service` doc comment), which is why it must run
/// every iteration and not only on a `true` return here.
///
/// Returns `false` (and never writes `*out`) if `ui` or `out` is null --
/// same "never write into a pointer the caller didn't validate" contract
/// [`pl_ui_render_ex`] documents for its own out-param.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `out`, if non-null, must point to valid, writable
/// [`PlDspProgram`] storage.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_take_dsp_program(ui: *mut PlUi, out: *mut PlDspProgram) -> bool {
    if ui.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: caller contract above.
    let ui = &mut *ui;
    let program = ui.app.dsp_program(PL_DSP_FS_HZ);
    if ui.last_dsp_program.as_ref() == Some(&program) {
        return false;
    }
    let wire = pl_dsp_program_to_wire(&program);
    // SAFETY: `out` is non-null and, per the caller contract, points to
    // valid writable `PlDspProgram` storage.
    unsafe { *out = wire };
    ui.last_dsp_program = Some(program);
    true
}

/// Encodes the page-0 Home telemetry snapshot into `buf` for the web
/// companion's `GET_TELEMETRY` control request (bead `pico-link-jyhk.3`,
/// "ADA DESIGN" comment on `pico-link-jyhk.1`, sections 3-4; wire layout
/// owned by `pico_link_core::app::telemetry`, not duplicated here). `page`
/// selects the payload -- only page `0` (Home) exists today; a future F3
/// diagnostics page is a separate payload under the same header, per the
/// design's own versioning note.
///
/// C is expected to call this from the superloop, gated by its own
/// poll-recency/regeneration-interval rules (design section 3, flow step
/// (b)) -- that gating, the 0x03/0x04 `usb_config_itf.c` request handlers,
/// and the under-`save_and_disable_interrupts` publish into the SETUP
/// reply buffer are `pico-link-jyhk.4`, not this crate.
///
/// Returns the number of bytes written into `buf[0..cap]` -- always
/// [`pico_link_core::app::App::telemetry_snapshot`]'s `HOME_SNAPSHOT_LEN`
/// (163 as of proto 1) on a successful page-0 encode -- or `0` if `ui` or
/// `buf` is null, `cap` is too small to hold the whole snapshot, or `page`
/// is unsupported. `0` here means "don't publish this poll," not "not
/// ready": the wire's `snap_seq == 0` not-ready convention is realised
/// entirely by C's own zero-initialised static reply buffer, which this
/// function never touches before the first successful call (see
/// `App::telemetry_snapshot`'s doc comment). Never performs a partial
/// write.
///
/// `ui` is `*const`, not `*mut`, matching the design's Rust contract for
/// this call ("the borrow is read-only... never touches dirty, damage or
/// idle state") -- see `App::telemetry_snapshot`'s doc comment for the one
/// piece of state (`snap_seq`) that still advances on every call, via an
/// interior-mutable counter rather than a caller-visible mutation.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `buf`, if non-null, must point to at least `cap` bytes of
/// valid, writable storage.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_telemetry(ui: *const PlUi, page: u8, buf: *mut u8, cap: usize) -> usize {
    if ui.is_null() || buf.is_null() {
        return 0;
    }
    // SAFETY: caller contract above.
    let ui = unsafe { &*ui };
    // SAFETY: `buf` is non-null and, per the caller contract, points to at
    // least `cap` bytes of valid, writable storage.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    ui.app.telemetry_snapshot(page, out)
}

// --- Bead pico-link-jyhk.20: the web companion's EQ management protocol
// (`GET_LIBRARY` 0x05 / `HOST_OP` 0x06 / `GET_OP_STATUS` 0x07), Task 3 of
// `.planning/design/2026-09-27-iface6-eq-management-protocol.md` ("ADA
// DESIGN" on `pico-link-jyhk.17`) ---
//
// Three new `pl_ui_*` entry points wrapping `pico-link-jyhk.18`/`.19`'s
// core API (`App::library_snapshot`/`host_op`/`host_op_status`/
// `host_preview_end`). No `PlEvent`/`PlCommand` ABI change: `GET_LIBRARY`
// and `GET_OP_STATUS` are C-published SETUP replies (same shape as
// `pl_ui_telemetry` above), and `HOST_OP` is a mailbox the superloop hands
// straight to `App::host_op` -- none of the three needs a new tagged
// union variant. `firmware/src/usb_config_itf.c`'s 0x05/0x06/0x07 request
// handlers, the shared mailbox/reply buffer, and the 2s host-preview
// lease timer that calls `pl_ui_host_preview_end` are `pico-link-jyhk.21`,
// not this bead.

/// Encodes the `GET_LIBRARY` (0x05) snapshot into `buf` (design section 3;
/// wire layout owned by `pico_link_core::app::library`, not duplicated
/// here) -- `ui-ffi`'s wrapper over
/// [`pico_link_core::app::App::library_snapshot`]. C is expected to call
/// this from the superloop, gated by the same poll-recency rule
/// `pl_ui_telemetry` already documents, and to run it BEFORE the page-0
/// telemetry step so page 0's `library_rev` append is always fresh
/// (design section 3: "Generation runs ... BEFORE the telemetry step").
///
/// Returns the number of bytes written -- `0` if `ui`/`buf` is null, `cap`
/// is too small to hold the whole snapshot (never a partial write, same
/// contract as [`pl_ui_telemetry`]), OR the freshly encoded snapshot's
/// `library_rev` is unchanged from the last snapshot THIS `PlUi` handed
/// back through this function (design section 11 Task 3: "returns 0 when
/// unchanged"). That third case is this function's own bookkeeping
/// ([`PlUi::last_library_rev`]) layered on top of `library_snapshot`,
/// which always fully re-encodes on every call and has no such signal of
/// its own -- see that method's doc comment. The `library_rev` field lives
/// at wire offset 4 (design section 3's header: `u8 lib_proto, u8
/// reserved, u16 len, u16 library_rev, ...`), read directly out of the
/// freshly written `buf` rather than re-deriving it, so this can never
/// drift from what C actually receives.
///
/// A caller that always wants the bytes regardless of change (e.g. to
/// seed its own cache on first use) should compare its own last-seen
/// `library_rev` from a previous successful call instead of relying on
/// `0` meaning "nothing to see" -- `0` here also covers the ordinary
/// too-small-buffer failure, exactly as [`pl_ui_telemetry`]'s `0` does.
///
/// `ui` is `*const`, not `*mut`, matching `library_snapshot`'s own `&self`
/// contract (interior-mutable `library_rev`/`library_last_bytes`, same
/// shape `telemetry_snap_seq` uses) -- see that method's doc comment.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `buf`, if non-null, must point to at least `cap` bytes of
/// valid, writable storage.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_library(ui: *const PlUi, buf: *mut u8, cap: usize) -> usize {
    if ui.is_null() || buf.is_null() {
        return 0;
    }
    // SAFETY: caller contract above.
    let ui = unsafe { &*ui };
    // SAFETY: `buf` is non-null and, per the caller contract, points to at
    // least `cap` bytes of valid, writable storage.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    let written = ui.app.library_snapshot(out);
    if written < 6 {
        // Too small to hold even the header's `library_rev` field (offset
        // 4..6) -- `library_snapshot` already returned `0` for "buffer too
        // small", but guard defensively rather than reading past what was
        // actually written.
        return written;
    }
    let rev = u16::from_le_bytes([out[4], out[5]]);
    if rev == ui.last_library_rev.get() {
        return 0;
    }
    ui.last_library_rev.set(rev);
    written
}

/// Runs one `HOST_OP` (0x06) request through `ui`'s [`App`], then
/// immediately encodes the resulting `GET_OP_STATUS` (0x07) reply into
/// `out` -- `ui-ffi`'s wrapper over
/// [`pico_link_core::app::App::host_op`]/[`host_op_status`](pico_link_core::app::App::host_op_status).
/// One call does both halves because the op runs synchronously (design
/// section 4): C's `HOST_OP` SETUP handler hands the OUT data-stage bytes
/// to the superloop, which calls this function once and publishes `out`'s
/// bytes as the `GET_OP_STATUS` reply buffer under
/// `save_and_disable_interrupts` (same publish pattern as `GET_LIBRARY`/
/// `GET_TELEMETRY`) -- a real `GET_OP_STATUS` (0x07) SETUP arriving later
/// just memcpys straight out of that already-published buffer, with no
/// further FFI call needed (design section 4's "the host polls
/// `GET_OP_STATUS` until `seq` matches ... and `state != 0`" only requires
/// the LATEST result to already be sitting there).
///
/// `request` (`in_len` bytes) is the raw `HOST_OP` request body,
/// unvalidated -- [`App::host_op`](pico_link_core::app::App::host_op)
/// never panics on a malformed/truncated request (see its own doc
/// comment), so this wrapper does no length checking of its own beyond
/// null.
///
/// Returns the number of bytes written into `out[0..out_cap]` -- always
/// nonzero on success (`GET_OP_STATUS`'s header alone is `21` bytes, see
/// `host_op`'s module doc comment for the full field list) -- or `0` if
/// `ui`/`out` is null, `request` is null with `in_len > 0`, or `out_cap` is
/// smaller than [`pico_link_core::app::MAX_OP_STATUS_LEN`] (the worst-case
/// reply length, since the actual reply isn't known until `host_op` has
/// already run -- so this checks `out_cap` against the ceiling and rejects
/// BEFORE calling `host_op`, never after: a too-small `out` must not execute
/// the op and lose its status, same contract as [`pl_ui_telemetry`] but
/// checked earlier because this call, unlike telemetry, has side effects).
/// A null `request` with `in_len == 0`
/// is accepted as an empty (necessarily malformed -- shorter than the
/// 4-byte request header) request, same as any other too-short one:
/// [`App::host_op`](pico_link_core::app::App::host_op) never panics on a
/// malformed/truncated request (see its own doc comment) and instead
/// stores a real `InvalidRequest` error status for this call to encode
/// and return, rather than this wrapper silently dropping the call. A `0`
/// return means C must not publish
/// anything new for `GET_OP_STATUS` -- the previous reply (if any) stays
/// live, same as a too-small [`pl_ui_telemetry`]/[`pl_ui_library`] call
/// never clobbers an already-published buffer.
///
/// `ui` is `*mut`, not `*const`: unlike `GET_LIBRARY`/`GET_TELEMETRY`,
/// `HOST_OP` can mutate `App` state (the preset store, `BtModel`-bound
/// commands queued for `pl_ui_poll_command`, the host-preview mailbox) --
/// see `host_op`'s module doc comment for exactly what each op does.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `request`, if non-null, must point to at least `in_len`
/// valid, readable bytes for the duration of this call (borrowed only,
/// not retained past it); if null, `in_len` must be `0`. `out`, if
/// non-null, must point to at least `out_cap` bytes of valid, writable
/// storage.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_host_op(ui: *mut PlUi, request: *const u8, in_len: usize, out: *mut u8, out_cap: usize) -> usize {
    // `out_cap` is checked against the worst-case reply length (not the
    // actual reply, which isn't known until `host_op` has already run)
    // BEFORE `host_op` is called: `host_op` mutates the store / queues
    // commands, so calling it first on a too-small `out` would execute the
    // op and then lose its status forever (bead `pico-link-jyhk.20` review
    // fix -- `host_op_status`'s own too-small-buf check runs after the
    // mutation already happened).
    if ui.is_null() || out.is_null() || (request.is_null() && in_len > 0) || out_cap < pico_link_core::app::MAX_OP_STATUS_LEN {
        return 0;
    }
    // SAFETY: caller contract above.
    let ui = unsafe { &mut *ui };
    // SAFETY: `request` is either null with `in_len == 0` (an empty slice
    // needs no valid pointer to dereference) or, per the caller contract,
    // non-null and pointing to at least `in_len` valid, readable bytes for
    // this call's duration.
    let request: &[u8] = if request.is_null() { &[] } else { unsafe { core::slice::from_raw_parts(request, in_len) } };
    ui.app.host_op(request);
    // SAFETY: `out` is non-null and, per the caller contract, points to at
    // least `out_cap` bytes of valid, writable storage.
    let out = unsafe { core::slice::from_raw_parts_mut(out, out_cap) };
    ui.app.host_op_status(out)
}

/// Clears any active host preview (`ui-ffi`'s wrapper over
/// [`pico_link_core::app::App::host_preview_end`]) -- called by C after
/// ~2s with no `iface-6` SETUP traffic (design section 7's lease), NOT by
/// the `PREVIEW_END` op itself (`op 5`, handled inside [`pl_ui_host_op`]
/// via `App::host_op`'s own dispatch). A no-op if there was no active
/// preview.
///
/// Does nothing if `ui` is null.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_host_preview_end(ui: *mut PlUi) {
    if ui.is_null() {
        return;
    }
    // SAFETY: caller contract above.
    let ui = unsafe { &mut *ui };
    ui.app.host_preview_end();
}

// --- Bead pico-link-ryw.11: debug-only EQ import over the PL_DEBUG_REMOTE
// CDC console ---
//
// `firmware/src/debug_remote.c`'s `EQ BEGIN`/`EQ <line>`/`EQ END`/`EQ OFF`
// commands strip their own `"EQ "` prefix and hand the rest of the line to
// `pl_ui_debug_eq_command` below -- `"BEGIN"`/`"END"`/`"OFF"` dispatch to
// `App`'s session state directly; anything else is treated as one
// Equalizer APO text line and fed to the in-progress session. This whole
// FFI surface is compiled unconditionally (same as every other `pl_ui_*`
// function) -- it is `debug_remote.c` that is `PL_DEBUG_REMOTE`-gated at
// the build level (see that file's own module doc / `firmware/CMakeLists.txt`),
// so a release build simply never calls these.

/// `pl_ui_debug_eq_command`'s result: `code` is `0` on success, negative on
/// failure (see the match arms in [`debug_eq_command_error_result`] for
/// the mapping); `line` is the 1-based line number within the CURRENT
/// session a parse failure occurred at (`0` if not applicable -- a
/// [`pico_link_core::app::DebugEqCommandError::NotInSession`] or a
/// `finish`/`EQ END` failure, which is session-wide, not one line's
/// fault).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlEqCommandResult {
    pub code: i32,
    pub line: u32,
}

impl PlEqCommandResult {
    const OK: Self = Self { code: 0, line: 0 };
    const NULL_OR_INVALID_UTF8: Self = Self { code: -1, line: 0 };
}

fn eq_apo_error_code(e: pico_link_core::dsp::EqApoError) -> i32 {
    use pico_link_core::dsp::EqApoError::{
        DuplicatePreamp, InvalidNumber, MalformedLine, MissingPreamp, NoBands, TooManyBands, UnknownKind,
    };
    match e {
        MalformedLine => -2,
        InvalidNumber => -3,
        UnknownKind => -4,
        TooManyBands => -5,
        MissingPreamp => -6,
        NoBands => -7,
        DuplicatePreamp => -8,
    }
}

fn debug_eq_command_error_result(e: pico_link_core::app::DebugEqCommandError) -> PlEqCommandResult {
    use pico_link_core::app::DebugEqCommandError::{Finish, Line, NotInSession};
    match e {
        NotInSession => PlEqCommandResult { code: -9, line: 0 },
        Line(line_err) => {
            // `EqApoLineError::line` is a `usize` line count within one
            // session -- always tiny (bounded by how many lines a CDC
            // console command stream could plausibly send), never
            // anywhere near `u32::MAX`.
            #[allow(clippy::cast_possible_truncation)]
            let line = line_err.line as u32;
            PlEqCommandResult { code: eq_apo_error_code(line_err.error), line }
        }
        Finish(error) => PlEqCommandResult { code: eq_apo_error_code(error), line: 0 },
    }
}

/// Dispatches one debug EQ console command against `ui`'s [`App`]:
/// `text` (`text_len` bytes, need not be NUL-terminated) is `"BEGIN"`,
/// `"END"`, `"OFF"`, or one Equalizer APO text line (a `Preamp:` or
/// `Filter N:` line) -- see [`pico_link_core::app::App::debug_eq_begin`]/
/// [`debug_eq_line`](pico_link_core::app::App::debug_eq_line)/
/// [`debug_eq_end`](pico_link_core::app::App::debug_eq_end)/
/// [`debug_eq_off`](pico_link_core::app::App::debug_eq_off) for the state
/// machine this drives. Leading/trailing whitespace in `text` is
/// tolerated (mirrors [`pico_link_core::dsp::EqApoSession::feed_line`]'s
/// own trim).
///
/// Returns [`PlEqCommandResult::NULL_OR_INVALID_UTF8`] (`code == -1`) if
/// `ui`/`text` is null or `text` isn't valid UTF-8, without touching
/// `ui`'s state -- same "never write/mutate past an unvalidated pointer"
/// contract [`pl_ui_take_dsp_program`]'s null check documents for its own
/// out-param.
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `text`, if non-null, must point to at least `text_len`
/// valid, readable bytes for the duration of this call (borrowed only,
/// not retained past it).
#[no_mangle]
pub unsafe extern "C" fn pl_ui_debug_eq_command(ui: *mut PlUi, text: *const u8, text_len: usize) -> PlEqCommandResult {
    if ui.is_null() || text.is_null() {
        return PlEqCommandResult::NULL_OR_INVALID_UTF8;
    }
    // SAFETY: caller contract above -- `ui` is a live `pl_ui_create` pointer,
    // `text` points to `text_len` valid bytes for this call's duration.
    let ui = unsafe { &mut *ui };
    let bytes = unsafe { core::slice::from_raw_parts(text, text_len) };
    let Ok(text) = core::str::from_utf8(bytes) else {
        return PlEqCommandResult::NULL_OR_INVALID_UTF8;
    };
    let text = text.trim();

    let result = match text {
        "BEGIN" => {
            ui.app.debug_eq_begin();
            Ok(())
        }
        "END" => ui.app.debug_eq_end(),
        "OFF" => {
            ui.app.debug_eq_off();
            Ok(())
        }
        line => ui.app.debug_eq_line(line),
    };

    match result {
        Ok(()) => PlEqCommandResult::OK,
        Err(e) => debug_eq_command_error_result(e),
    }
}

/// Reads the currently active debug EQ override's band count and explicit
/// preamp, for a status log line (`debug_remote.c` logs this right after
/// a successful `EQ END`, and on an `EQ STATUS` query). Returns `false`
/// (and leaves `*out_band_count`/`*out_preamp_db` untouched) if `ui` is
/// null or no override is currently active -- see
/// [`pico_link_core::app::App::debug_eq_override_info`].
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `out_band_count`/`out_preamp_db`, if non-null, must each
/// point to valid, writable storage of their respective type.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_debug_eq_status(ui: *mut PlUi, out_band_count: *mut u8, out_preamp_db: *mut f32) -> bool {
    if ui.is_null() {
        return false;
    }
    // SAFETY: caller contract above.
    let ui = unsafe { &mut *ui };
    let Some((band_count, preamp_db)) = ui.app.debug_eq_override_info() else {
        return false;
    };
    if !out_band_count.is_null() {
        // SAFETY: caller contract above.
        unsafe { *out_band_count = band_count };
    }
    if !out_preamp_db.is_null() {
        // SAFETY: caller contract above.
        unsafe { *out_preamp_db = preamp_db };
    }
    true
}

// --- Bead pico-link-ryw.12.4: whole-document preset import ---
//
// `firmware/src/*` (bead `pico-link-ryw.12.5`, built in parallel) owns ITF 6
// ("Pico Link Config")'s `IMPORT_PRESET` control transfer: it copies the
// host's `{name, APO text}` payload into a static buffer and, once the
// superloop sees the pending flag, calls `pl_ui_import_preset` below --
// currently routed through `pl_ui_debug_eq_command` as a placeholder single
// seam, per `pico-link-ryw.12`'s design sec 5's ordering ("Deps: 4 (FFI
// signature)"); ryw.12.5 switches that one call site to this function once
// it lands.

/// [`pl_ui_import_preset`]'s outcome -- mirrors
/// [`pico_link_core::dsp::ImportOutcome`] plus a negative-`code` error path,
/// the same "`code == 0` success, `code < 0` failure" shape
/// [`PlEqCommandResult`] already uses for the debug EQ console.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlImportResult {
    /// `0` on success (`Created`/`Replaced`/`Renamed`, distinguished by
    /// [`Self::outcome`]), negative on failure -- see the `match` arms in
    /// [`import_error_result`] for the exact mapping.
    pub code: i32,
    /// Valid only when `code == 0`: `0` = `Created`, `1` = `Replaced`,
    /// `2` = `Renamed` (see [`pico_link_core::dsp::ImportOutcome`]).
    pub outcome: u8,
    /// Valid only when `code == 0` -- the imported/updated preset's id
    /// (the same id a `Command::SavePreset`/`Event::PresetLoaded` round
    /// trip resolves to).
    pub preset_id: u16,
    /// The 1-based source line a parse failure occurred at
    /// ([`pico_link_core::dsp::ImportError::Line`]), else `0` -- same
    /// "`0` if not applicable" convention [`PlEqCommandResult::line`]
    /// already uses.
    pub line: u32,
    /// The 1-based `Filter N:` band index a range-check failure names
    /// ([`GainOutOfRange`](pico_link_core::dsp::ImportError::GainOutOfRange)/
    /// [`FreqOutOfRange`](pico_link_core::dsp::ImportError::FreqOutOfRange)/
    /// [`QOutOfRange`](pico_link_core::dsp::ImportError::QOutOfRange)),
    /// else `0` (a preamp range failure has no band index, same as a
    /// parse failure).
    pub band_index: u32,
    /// The offending value a range-check failure carries (a gain in dB,
    /// a frequency in Hz, a Q, or a preamp in dB, depending on which
    /// `ImportError` variant `code` maps to), else `0.0`.
    pub value: f32,
}

impl PlImportResult {
    const NULL_OR_INVALID_UTF8: Self = Self { code: -1, outcome: 0, preset_id: 0, line: 0, band_index: 0, value: 0.0 };

    fn success(preset_id: u16, outcome: pico_link_core::dsp::ImportOutcome) -> Self {
        use pico_link_core::dsp::ImportOutcome::{Created, Renamed, Replaced};
        let outcome = match outcome {
            Created => 0,
            Replaced => 1,
            Renamed => 2,
        };
        Self { code: 0, outcome, preset_id, line: 0, band_index: 0, value: 0.0 }
    }
}

/// Maps an [`pico_link_core::dsp::ImportError`] to a [`PlImportResult`]'s
/// error shape. Negative codes `-2`..`-9` reuse [`eq_apo_error_code`]/
/// [`PlEqCommandResult`]'s own scheme where an [`ImportError`] wraps the
/// same underlying [`pico_link_core::dsp::EqApoError`] (`Line`/`Session`) --
/// one parse-error vocabulary, not two independently-numbered ones. `-10`
/// onward are this bead's own range-check/store-full codes, which
/// `pl_ui_debug_eq_command`'s console path can never produce (the debug
/// console has no `to_preset` limits check -- `pico-link-ryw.11`'s module
/// doc).
fn import_error_result(e: pico_link_core::dsp::ImportError) -> PlImportResult {
    use pico_link_core::dsp::ImportError::{
        FreqOutOfRange, GainOutOfRange, Line, NotReady, PreampOutOfRange, QOutOfRange, Session, StoreFull,
    };
    match e {
        Line(line_err) => {
            // Same "always tiny" reasoning `debug_eq_command_error_result`
            // already documents for this exact cast.
            #[allow(clippy::cast_possible_truncation)]
            let line = line_err.line as u32;
            PlImportResult { code: eq_apo_error_code(line_err.error), outcome: 0, preset_id: 0, line, band_index: 0, value: 0.0 }
        }
        Session(error) => PlImportResult { code: eq_apo_error_code(error), outcome: 0, preset_id: 0, line: 0, band_index: 0, value: 0.0 },
        GainOutOfRange { band_index, gain_db } => {
            PlImportResult { code: -10, outcome: 0, preset_id: 0, line: 0, band_index: band_index as u32, value: gain_db }
        }
        FreqOutOfRange { band_index, freq_hz } => {
            PlImportResult { code: -11, outcome: 0, preset_id: 0, line: 0, band_index: band_index as u32, value: freq_hz }
        }
        QOutOfRange { band_index, q } => {
            PlImportResult { code: -12, outcome: 0, preset_id: 0, line: 0, band_index: band_index as u32, value: q }
        }
        PreampOutOfRange { preamp_db } => {
            PlImportResult { code: -13, outcome: 0, preset_id: 0, line: 0, band_index: 0, value: preamp_db }
        }
        StoreFull => PlImportResult { code: -14, outcome: 0, preset_id: 0, line: 0, band_index: 0, value: 0.0 },
        // Bead `pico-link-ryw.14`: C's boot-time preset-id high-water mark
        // (`PlEventTag::PresetStoreLoaded`'s `next_id`) hasn't arrived yet
        // -- `core` cannot safely allocate an id. See `PlPresetStoreLoadedPayload`'s
        // doc comment.
        NotReady => PlImportResult { code: -15, outcome: 0, preset_id: 0, line: 0, band_index: 0, value: 0.0 },
    }
}

/// Imports a whole Equalizer APO / `AutoEQ` document (`text`, `text_len`
/// bytes, need not be NUL-terminated) against `ui`'s [`App`], naming it
/// `name` (`name_len` bytes) if the document itself has no `Name:` line --
/// [`pico_link_core::app::App::import_preset`] does the actual parse +
/// convert + place-in-store work (see its own doc comment for the full
/// replace/suffix/reject duplicate-name policy). On success, queues
/// exactly one `Command::SavePreset` IMMEDIATELY (Andreas's ruling: no
/// on-device confirm) -- the caller's next [`pl_ui_poll_command`] call
/// drains it, same as every other queued command.
///
/// Returns [`PlImportResult::NULL_OR_INVALID_UTF8`] (`code == -1`) if
/// `ui`/`text` is null, `text` isn't valid UTF-8, or `name` is non-null
/// but isn't valid UTF-8, without touching `ui`'s state. A null `name`
/// with `name_len == 0` is accepted as an empty fallback name (only ever
/// used if the document also has no `Name:` line, in which case
/// `crate::dsp::import`'s name resolution falls through to an empty
/// string -- the same "never partially mutates on an error path"
/// discipline applies, but an empty resolved name is not itself an error
/// this function rejects; that is `core`'s call, not this FFI seam's).
///
/// # Safety
///
/// `ui` must be null or a live pointer from [`pl_ui_create`] not yet
/// destroyed. `text`, if non-null, must point to at least `text_len`
/// valid, readable bytes for the duration of this call; `name`, if
/// non-null, must point to at least `name_len` valid, readable bytes for
/// the duration of this call. Neither is retained past the call.
#[no_mangle]
pub unsafe extern "C" fn pl_ui_import_preset(
    ui: *mut PlUi,
    text: *const u8,
    text_len: usize,
    name: *const u8,
    name_len: usize,
) -> PlImportResult {
    if ui.is_null() || text.is_null() {
        return PlImportResult::NULL_OR_INVALID_UTF8;
    }
    // SAFETY: caller contract above -- `ui` is a live `pl_ui_create`
    // pointer, `text` points to `text_len` valid bytes for this call's
    // duration.
    let ui = unsafe { &mut *ui };
    let text_bytes = unsafe { core::slice::from_raw_parts(text, text_len) };
    let Ok(text) = core::str::from_utf8(text_bytes) else {
        return PlImportResult::NULL_OR_INVALID_UTF8;
    };

    let host_name = if name.is_null() {
        ""
    } else {
        // SAFETY: caller contract above -- `name` points to `name_len`
        // valid bytes for this call's duration.
        let name_bytes = unsafe { core::slice::from_raw_parts(name, name_len) };
        let Ok(host_name) = core::str::from_utf8(name_bytes) else {
            return PlImportResult::NULL_OR_INVALID_UTF8;
        };
        host_name
    };

    match ui.app.import_preset(text, host_name) {
        Ok((preset_id, outcome)) => PlImportResult::success(preset_id, outcome),
        Err(e) => import_error_result(e),
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

    /// Bead `pico-link-ryw.14`, Ada's preset-id-allocation contract:
    /// `App::presets_ready` defaults `false` (same on every build, no test-
    /// only split -- see that field's doc comment), so any test that goes
    /// on to create/import a preset needs a real `PresetStoreLoaded` push
    /// first, same as C's own real boot sequence would send. `next_id: 1`
    /// matches `PresetStore::new`'s own starting value -- these tests want
    /// an ordinary "nothing loaded yet" boot.
    ///
    /// # Safety
    ///
    /// `ui` must be a live pointer from [`pl_ui_create`], not yet destroyed.
    unsafe fn ready_ui(ui: *mut PlUi) {
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PresetStoreLoaded as u32,
            payload: PlEventPayload {
                preset_store_loaded: PlPresetStoreLoadedPayload { count: 0, status: PlStoreStatus::FirstBoot as u8, next_id: 1 },
            },
        };
        unsafe { pl_ui_push_event(ui, event) };
    }

    /// Renders once through the real FFI entry point purely to reach a
    /// known-clean (`!App::dirty()`) baseline before exercising input/tick
    /// behaviour -- the render-out payload itself is not asserted on here.
    /// Used in place of the old unversioned `pl_ui_render` (retired in
    /// `pico-link-7h5.10`); `pl_ui_render_ex` is the only render entry
    /// point now.
    fn render_ex_once(ui: *mut PlUi) {
        let mut out = PlRenderOut {
            version: 0,
            px: core::ptr::null(),
            px_len: 0,
            stride: 0,
            rect_count: 0,
            rects: core::ptr::null(),
        };
        unsafe { pl_ui_render_ex(ui, &mut out) };
    }

    #[test]
    fn pl_ui_render_ex_null_ui_degrades_to_zeroed_output_with_version_set() {
        let mut out = PlRenderOut {
            version: 0,
            px: core::ptr::null(),
            px_len: 0xDEAD,
            stride: 0xDEAD,
            rect_count: 0xDEAD,
            rects: core::ptr::null(),
        };
        unsafe {
            pl_ui_render_ex(core::ptr::null_mut(), &mut out);
        }
        assert_eq!(out.version, PL_RENDER_ABI_VERSION, "version must be set even for a null ui");
        assert!(out.px.is_null());
        assert_eq!(out.px_len, 0);
        assert_eq!(out.rect_count, 0, "a null ui must report nothing to paint");
        assert!(out.rects.is_null());
    }

    #[test]
    fn pl_ui_render_ex_null_out_is_a_no_op() {
        let ui = new_ui();
        // Must not panic/segfault -- the only assertion here is survival.
        unsafe {
            pl_ui_render_ex(ui, core::ptr::null_mut());
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_render_ex_first_frame_reports_the_whole_framebuffer_as_one_damage_rect() {
        let ui = new_ui();
        let mut out = PlRenderOut {
            version: 0,
            px: core::ptr::null(),
            px_len: 0,
            stride: 0,
            rect_count: 0,
            rects: core::ptr::null(),
        };
        unsafe {
            pl_ui_render_ex(ui, &mut out);
            assert_eq!(out.version, PL_RENDER_ABI_VERSION);
            assert!(!out.px.is_null());
            assert_eq!(out.px_len, out.stride as usize * 16, "16x16 framebuffer from new_ui()");
            assert_eq!(out.rect_count, 1, "first-ever render has no prior cache, so damage is the whole frame");
            assert!(!out.rects.is_null());
            let rect = &*out.rects;
            assert_eq!((rect.x, rect.y), (0, 0));
            assert_eq!((rect.w, rect.h), (out.stride, 16));
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_render_ex_second_call_with_no_intervening_change_reports_no_damage() {
        let ui = new_ui();
        let mut out = PlRenderOut {
            version: 0,
            px: core::ptr::null(),
            px_len: 0,
            stride: 0,
            rect_count: 0,
            rects: core::ptr::null(),
        };
        unsafe {
            pl_ui_render_ex(ui, &mut out);
            assert_eq!(out.rect_count, 1, "sanity: first render is dirty");
            // No `pl_ui_input`/`pl_ui_tick`/`pl_ui_push_event` in between --
            // nothing about the screen state changed, so the damage pass
            // should find nothing to repaint the second time.
            pl_ui_render_ex(ui, &mut out);
            assert_eq!(out.rect_count, 0, "an unchanged screen must report zero damage on the next render");
            assert!(out.rects.is_null());
            pl_ui_destroy(ui);
        }
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
        render_ex_once(ui);
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
        render_ex_once(ui);
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
            // One past PresetStoreLoaded = 23, the highest legal PlEventTag
            // as of bead pico-link-ryw.5 -- moved from 21 (one past the
            // previous highest, AbrFloorLoaded = 20) when this bead added
            // tags 21-23.
            tag: 24,
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
                payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: PlConnectStep::Pairing as u32, seq: 1 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::ConnectRetrying as u32,
                payload: PlEventPayload { connect_retrying: PlConnectRetryingPayload { attempt: 3, seq: 1 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::ConnectSucceeded as u32,
                payload: PlEventPayload { connect_succeeded: PlConnectSucceededPayload { addr: [0; 6], degraded: 1, seq: 1 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::WizardAutoDismiss as u32,
                payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: 0, seq: 1 } },
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
    fn pl_ui_push_event_levels_changed_populates_out_level_and_clears_on_disconnect() {
        // Bead pico-link-du0: a `LevelsChanged` event's four `u8` fields
        // round-trip into `BtModel::out_level`, and a later non-`Connected`
        // `LinkStateChanged` clears it back to `None` -- same lifecycle
        // `pl_ui_push_event_codec_changed_populates_connected_codec` above
        // already proves for `connected_codec` (see that test and
        // `pico_link_core::App::set_link_state`'s doc comment for why).
        let ui = new_ui();
        let link_connected = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Connected as u32 } },
        };
        let levels_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LevelsChanged as u32,
            payload: PlEventPayload { levels_changed: PlLevelsChangedPayload { peak_l: 200, peak_r: 180, rms_l: 120, rms_r: 100 } },
        };
        let link_idle = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Idle as u32 } },
        };
        unsafe {
            pl_ui_push_event(ui, link_connected);
            pl_ui_push_event(ui, levels_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            let level = (*ui).app.model().out_level.expect("out_level should be populated");
            assert_eq!(level.peak_l, 200);
            assert_eq!(level.peak_r, 180);
            assert_eq!(level.rms_l, 120);
            assert_eq!(level.rms_r, 100);

            pl_ui_push_event(ui, link_idle);
            assert!(
                (*ui).app.model().out_level.is_none(),
                "disconnecting must clear the OUT level, never leave it stale (design section 15)"
            );
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_volume_changed_populates_bt_model_volume() {
        // Bead pico-link-4v2.5 (VT5), design section 7: a `VolumeChanged`
        // event's `level`/`muted`/`source` round-trip into
        // `BtModel::volume`. `PlVolumeSource::Sink` (1) is used here
        // specifically to prove `source` survives the fold -- not just
        // whichever value happens to be the enum's zero discriminant.
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::VolumeChanged as u32,
            payload: PlEventPayload { volume_changed: PlVolumeChangedPayload { level: 42, muted: 1, source: PlVolumeSource::Sink as u8 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            let volume = (*ui).app.model().volume.expect("volume should be populated");
            assert_eq!(volume.level, 42);
            assert!(volume.muted);
            assert_eq!(volume.source, pico_link_core::VolumeSource::Sink);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_volume_changed_survives_disconnect() {
        // Design section 7 names no clearing rule for `BtModel::volume` on
        // disconnect (unlike `out_level`/`connected_codec`, which
        // explicitly do) -- the host feature-unit volume this most
        // commonly reflects is a USB-side concept, not an A2DP-link-
        // lifetime one. Prove it's NOT cleared, the opposite of
        // `pl_ui_push_event_levels_changed_populates_out_level_and_clears_
        // on_disconnect` above.
        let ui = new_ui();
        let link_connected = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Connected as u32 } },
        };
        let volume_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::VolumeChanged as u32,
            payload: PlEventPayload { volume_changed: PlVolumeChangedPayload { level: 64, muted: 0, source: PlVolumeSource::Host as u8 } },
        };
        let link_idle = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: PlLinkState::Idle as u32 } },
        };
        unsafe {
            pl_ui_push_event(ui, link_connected);
            pl_ui_push_event(ui, volume_event);
            pl_ui_push_event(ui, link_idle);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            assert_eq!(
                (*ui).app.model().volume.map(|v| v.level),
                Some(64),
                "volume must survive a disconnect -- it is not link-lifetime state (design section 7)"
            );
            pl_ui_destroy(ui);
        }
    }

    /// Builds a `VolumeChanged` [`PlEvent`] with the given fields -- shared
    /// by the wake/dim-floor tests below.
    fn volume_event(level: u8, muted: bool, source: PlVolumeSource) -> PlEvent {
        PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::VolumeChanged as u32,
            payload: PlEventPayload { volume_changed: PlVolumeChangedPayload { level, muted: u8::from(muted), source: source as u8 } },
        }
    }

    /// Builds an `AudioFault` [`PlEvent`] with the given fields -- shared
    /// by the wake-on-fault tests below (bead pico-link-9eq2.3.1).
    fn audio_fault_event(key: PlFaultKey, severity: PlFaultSeverity, count: u16) -> PlEvent {
        PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::AudioFault as u32,
            payload: PlEventPayload {
                audio_fault: PlAudioFaultPayload {
                    key: key as u8,
                    severity: severity as u8,
                    glyph: PlFaultGlyph::Neutral as u8,
                    value_kind: PlFaultValueKind::None as u8,
                    value: 0,
                    count,
                },
            },
        }
    }

    // --- Design `.planning/design/2026-09-07-volume-on-display.md`
    // section 5's wake policy, proven through the REAL FFI entry points
    // (`pl_ui_push_event`/`pl_ui_tick`), not just `IdlePolicy` directly --
    // section 5.5's trap is that a rule proven only against
    // `pico_link_core::run` never runs on the firmware, which never calls
    // `Runner::step` at all. These are the tests that close that gap. ---

    #[test]
    fn a_host_volume_change_does_not_wake_and_does_not_extend_the_idle_timer() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_tick(ui, idle_timeout_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "sanity: asleep");

            // A host-originated volume change while blanked: per section
            // 5.1/5.2, a host event is never evidence of a human at the
            // device (mixer apps, ducking, a locked machine's alarm) -- it
            // must not wake the display, unlike the sink-originated case
            // below.
            pl_ui_push_event(ui, volume_event(80, false, PlVolumeSource::Host));
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "a host volume change must not wake the display");

            pl_ui_tick(ui, idle_timeout_us + 1);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "a host volume change must not extend the idle timer either");

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn a_sink_volume_change_wakes_the_display_to_full_and_extends_the_idle_timer() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_tick(ui, idle_timeout_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "sanity: asleep");

            // Section 5.3: the headphones' own dial is the one case where a
            // human acted with no display in front of them -- must wake
            // immediately, before the next tick even runs.
            pl_ui_push_event(ui, volume_event(80, false, PlVolumeSource::Sink));
            assert!(pl_ui_display_power(ui) == PlDisplayPower::On, "a sink volume change must wake the display immediately, before the next tick");

            // And it must extend the idle timer -- one more full idle
            // timeout from THIS tick, not from the original `last_input`,
            // must be required before it blanks again.
            pl_ui_tick(ui, idle_timeout_us + 1);
            assert!(
                pl_ui_display_power(ui) == PlDisplayPower::On,
                "the sink-originated wake must reset the idle clock, not just flip the level once"
            );
            pl_ui_tick(ui, 2 * idle_timeout_us + 1);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "it must still blank again once a full idle timeout elapses from the wake");

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn muted_from_any_source_never_lets_an_already_blank_display_stay_blank() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_tick(ui, idle_timeout_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "sanity: asleep");

            // Section 5.4: a HOST-originated mute must still self-heal an
            // already-blanked display -- the "never fully blank while a
            // muted banner shows" rule (section 6.1) cannot lapse just
            // because the screen already went dark first.
            pl_ui_push_event(ui, volume_event(0, true, PlVolumeSource::Host));
            assert!(
                pl_ui_display_power(ui) == PlDisplayPower::On,
                "a host mute must promote an already-blank display back on, even though a host volume CHANGE alone (section 5.1) would not have woken it"
            );

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn muted_lands_on_dim_at_the_next_idle_timeout_while_it_holds() {
        // Andreas's Q1 ruling (pico-link-qivj.2): muted/zero + idle goes to
        // Dim, not On and not Off -- same semantics as the equivalent
        // `core::run::mute_or_zero_past_the_idle_timeout_lands_on_dim_not_on`
        // and `mute_does_not_extend_the_idle_timer` tests.
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_push_event(ui, volume_event(0, true, PlVolumeSource::Sink));
            pl_ui_tick(ui, idle_timeout_us);
            assert!(
                pl_ui_display_power(ui) == PlDisplayPower::Dim,
                "the screensaver must land on Dim (never fully blank) while muted holds, once a full idle timeout elapses (section 5.4's floor)"
            );

            // And once un-muted with no further activity, the ordinary
            // screensaver resumes promptly -- muting must not have
            // extended the idle timer (section 5.4's "no" in the table).
            pl_ui_push_event(ui, volume_event(50, false, PlVolumeSource::Host));
            pl_ui_tick(ui, idle_timeout_us + 1);
            assert!(
                pl_ui_display_power(ui) == PlDisplayPower::Off,
                "un-muting must not have reset the idle clock -- it should blank again almost immediately, not need another full timeout"
            );

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_volume_source() {
        // Bead pico-link-4v2.5 (VT5): an out-of-range `source` byte (3 is
        // `PL_VOLUME_SOURCE_CONSOLE` in `firmware/src/volume.h`, which
        // design section 7 deliberately excludes from ever reaching this
        // event) must be counted as malformed, not matched-on or
        // defaulted -- same discipline as `PlStoreStatus`'s malformed-tag
        // test above.
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::VolumeChanged as u32,
            payload: PlEventPayload { volume_changed: PlVolumeChangedPayload { level: 10, muted: 0, source: 3 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range volume source should be counted, not matched-on");
            assert!((*ui).app.model().volume.is_none(), "a malformed event must not fold into the model");
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
                paired_device_upserted: PlPairedDeviceUpsertedPayload { addr: addr_old, name: [0u8; 32], name_len: 0, mru_seq: 1, ldac_quality: 0, preset_id: 0 },
            },
        };
        let upsert_new = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::PairedDeviceUpserted as u32,
            payload: PlEventPayload {
                paired_device_upserted: PlPairedDeviceUpsertedPayload { addr: addr_new, name, name_len: 3, mru_seq: 2, ldac_quality: 0, preset_id: 0 },
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
            payload: PlEventPayload { paired_device_upserted: PlPairedDeviceUpsertedPayload { addr, name, name_len: 2, mru_seq: 7, ldac_quality: 0, preset_id: 0 } },
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
            payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: 99, seq: 1 } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range nested ConnectStep should be counted, not matched-on");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_decodes_wire_step_4_as_disconnecting_and_still_rejects_5() {
        // Bead pico-link-sfw6, design sec 7 (S3): the one new wire value
        // this bead adds -- step 4 must decode cleanly to
        // ConnectStep::Disconnecting, and the next value up (5, still
        // unknown) must still be rejected as malformed, not silently
        // accepted because 4 now is.
        let ui = new_ui();
        let disconnecting_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::ConnectStepChanged as u32,
            payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: 4, seq: 1 } },
        };
        let unknown_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::ConnectStepChanged as u32,
            payload: PlEventPayload { connect_step_changed: PlConnectStepChangedPayload { step: 5, seq: 1 } },
        };
        unsafe {
            pl_ui_push_event(ui, disconnecting_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "wire step 4 (Disconnecting) must decode cleanly");

            pl_ui_push_event(ui, unknown_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "wire step 5 is still unknown and must be rejected");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_connect_step_try_from_round_trips_every_legal_discriminant() {
        let legal = [
            PlConnectStep::Connecting,
            PlConnectStep::Pairing,
            PlConnectStep::SettingUpAudio,
            PlConnectStep::NegotiatingCodec,
            PlConnectStep::Disconnecting,
        ];
        for step in legal {
            assert!(PlConnectStep::try_from(step as u32).is_ok());
        }
        // Bead pico-link-sfw6: 4 (Disconnecting) is now legal; the first
        // unknown value moves to 5.
        assert!(PlConnectStep::try_from(5u32).is_err());
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
            PlEventTag::LevelsChanged,
            PlEventTag::VolumeChanged,
            PlEventTag::LdacBitrateChanged,
            PlEventTag::AudioFault,
            PlEventTag::DiscoveryStateChanged,
            PlEventTag::DisplaySettingsLoaded,
            PlEventTag::CushionPolicyLoaded,
            PlEventTag::AbrFloorLoaded,
            PlEventTag::PresetLoaded,
            PlEventTag::PresetDeleted,
            PlEventTag::PresetStoreLoaded,
        ];
        for tag in legal {
            assert!(PlEventTag::try_from(tag as u32).is_ok());
        }
        // 24 -- one past PresetStoreLoaded = 23, the highest legal
        // PlEventTag as of bead pico-link-ryw.5 (moved from 21, one past
        // the previous highest AbrFloorLoaded = 20, when this bead added
        // tags 21-23).
        assert!(PlEventTag::try_from(24u32).is_err());
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
            // The Ref borrowed from app.model() must drop before pl_ui_destroy
            // frees the underlying Rc<RefCell<BtModel>> allocation -- see the
            // DECISION comment on bead pico-link-bgnd.7. Scoping it in this
            // inner block ensures Drop runs here, not at the end of the
            // outer unsafe block (i.e. after destroy).
            {
                let model = (*ui).app.model();
                assert_eq!(model.discovered.len(), 1, "the device must have been folded into BtModel::discovered");
                assert_eq!(
                    model.discovered[0].class_of_device, class_of_device,
                    "class_of_device must round-trip byte-for-byte, not just its top bits"
                );
            }
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
            payload: PlEventPayload { connect_failed: PlConnectFailedPayload { addr: [0xAA; 6], reason: 255, seq: 1 } },
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
            payload: PlEventPayload { connect_failed: PlConnectFailedPayload { addr: [0xAA; 6], reason: 0xDEAD_BEEF, seq: 1 } },
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
            for state in [PlLinkState::Idle, PlLinkState::Connecting, PlLinkState::Connected] {
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
                    payload: PlEventPayload { connect_failed: PlConnectFailedPayload { addr: [0; 6], reason: reason as u32, seq: 1 } },
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
        let legal = [PlLinkState::Idle, PlLinkState::Connecting, PlLinkState::Connected];
        for state in legal {
            assert!(PlLinkState::try_from(state as u32).is_ok());
        }
        assert!(PlLinkState::try_from(4u32).is_err());
        assert!(PlLinkState::try_from(u32::MAX).is_err());
    }

    /// Bead pico-link-88xs, design section 8.5 test 6: `1` was
    /// `PlLinkState::Scanning`'s discriminant and is now reserved-and-
    /// rejected, never renumbered -- see [`PlLinkState`]'s doc comment.
    /// A stale producer that still sends it must be rejected AND counted,
    /// not silently accepted or matched on.
    #[test]
    fn pl_link_state_try_from_rejects_the_reserved_former_scanning_discriminant() {
        assert!(PlLinkState::try_from(1u32).is_err());
    }

    #[test]
    fn pl_discovery_state_try_from_round_trips_every_legal_discriminant() {
        let legal = [PlDiscoveryState::Idle, PlDiscoveryState::Scanning];
        for state in legal {
            assert!(PlDiscoveryState::try_from(state as u32).is_ok());
        }
        assert!(PlDiscoveryState::try_from(2u32).is_err());
        assert!(PlDiscoveryState::try_from(u32::MAX).is_err());
    }

    /// Bead pico-link-88xs, design section 8.5 test 6: the malformed-wire
    /// rejection itself. A raw `PlDiscoveryStateChangedPayload { state: 1
    /// }` (the reserved former `PlLinkState::Scanning` discriminant,
    /// carried on the wrong tag entirely) sent as
    /// `DiscoveryStateChanged`'s own payload must round-trip fine (`1` is
    /// legal there -- it's `PlDiscoveryState::Scanning`), and an actually
    /// out-of-range value must be rejected and counted.
    #[test]
    fn pl_ui_push_event_rejects_a_garbage_discovery_state_and_counts_it() {
        let ui = new_ui();
        let bad_event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::DiscoveryStateChanged as u32,
            payload: PlEventPayload { discovery_state_changed: PlDiscoveryStateChangedPayload { state: 0xDEAD_BEEF } },
        };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range PlDiscoveryState should be counted, not matched-on");
            assert!(!(*ui).app.model().discovering, "a rejected event must not reach App::handle_event");
            pl_ui_destroy(ui);
        }
    }

    /// The wire's own reservation, exercised end to end: `PlEvent { tag:
    /// LinkStateChanged, payload.state: 1 }` -- the raw discriminant that
    /// used to mean `Scanning` -- must be rejected and counted, and must
    /// not reach `App::handle_event` (i.e. must not silently become some
    /// other `LinkState`).
    #[test]
    fn pl_ui_push_event_rejects_the_reserved_former_scanning_link_state_and_counts_it() {
        let ui = new_ui();
        let bad_event =
            PlEvent { version: PL_EVENT_ABI_VERSION, tag: PlEventTag::LinkStateChanged as u32, payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: 1 } } };
        unsafe {
            pl_ui_push_event(ui, bad_event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "the reserved former Scanning discriminant must be counted, not matched-on");
            pl_ui_destroy(ui);
        }
    }

    /// End-to-end round trip for the new tag's happy path: a legal
    /// `DiscoveryStateChanged { scanning: true }` must fold into
    /// `BtModel::discovering` and must NOT touch `link_state` or clear any
    /// connected-model field (design section 1: the whole point of the
    /// axis split).
    #[test]
    fn pl_ui_push_event_folds_a_legal_discovery_state_changed_into_the_model() {
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::DiscoveryStateChanged as u32,
            payload: PlEventPayload { discovery_state_changed: PlDiscoveryStateChangedPayload { state: PlDiscoveryState::Scanning as u32 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);
            assert!((*ui).app.model().discovering);
            assert_eq!((*ui).app.model().link_state, LinkState::Idle, "DiscoveryStateChanged must never touch link_state");
            pl_ui_destroy(ui);
        }
    }

    /// Bead `pico-link-0cq2`, test T5: the wire's `PL_LINK_STATE_CONNECTING`
    /// discriminant (`2`) must decode to [`Event::ConnectAttemptStarted`],
    /// not a `LinkStateChanged` payload, while `0`/`3` still decode to
    /// `LinkStateChanged(Idle)`/`LinkStateChanged(Connected)` -- see
    /// `decode_link_state_event`'s doc comment. Proved end to end via
    /// `BtModel`: an already-`Connected` link must stay `Connected` and
    /// `connecting` must flip true/false around the attempt, with none of
    /// this touching `link_state`.
    #[test]
    fn pl_ui_push_event_decodes_the_connecting_wire_value_as_a_connect_attempt_not_a_link_state() {
        let ui = new_ui();
        let connect = |state: PlLinkState| PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::LinkStateChanged as u32,
            payload: PlEventPayload { link_state_changed: PlLinkStateChangedPayload { state: state as u32 } },
        };
        unsafe {
            pl_ui_push_event(ui, connect(PlLinkState::Connected));
            assert_eq!((*ui).app.model().link_state, LinkState::Connected);
            assert!(!(*ui).app.model().connecting);

            pl_ui_push_event(ui, connect(PlLinkState::Connecting));
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "wire value 2 is legal, not malformed");
            assert_eq!(
                (*ui).app.model().link_state,
                LinkState::Connected,
                "an attempt at a second device must not touch an already-established link_state"
            );
            assert!((*ui).app.model().connecting, "wire value 2 must set connecting, not link_state");

            pl_ui_push_event(ui, connect(PlLinkState::Idle));
            assert_eq!((*ui).app.model().link_state, LinkState::Idle);
            pl_ui_destroy(ui);
        }
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
    fn cancel_connect_command_maps_to_the_cancel_connect_tag_and_carries_the_addr_and_seq() {
        // pico-link-znb.7's code-review fix: B during the wizard's
        // connecting/not-responding phases now queues this instead of
        // silently leaving the abandoned attempt running in C.
        //
        // ADA DESIGN v2 (bead `pico-link-chc3`): this now reads
        // `.cancel_connect.{addr,seq}`, not `.addr.addr` -- `pl_command_from`
        // moved this tag onto its own `PlCancelConnectPayload` the instant
        // it needed a second field (see that struct's doc comment).
        let addr = [0xAA; 6];
        let wire = pl_command_from(Command::CancelConnect { addr, seq: 42 });
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::CancelConnect as u32);
        // SAFETY: `wire.tag` above confirms the union currently holds `cancel_connect`.
        let payload = unsafe { wire.payload.cancel_connect };
        assert_eq!(payload.addr, addr);
        assert_eq!(payload.seq, 42);
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
        let connect = pl_command_from(Command::Connect { addr, name: String::new(), seq: 7 });
        assert_eq!(connect.tag as u32, PlCommandTag::Connect as u32);
        // SAFETY: `connect.tag` above confirms the union currently holds `connect`.
        let payload = unsafe { connect.payload.connect };
        assert_eq!(payload.addr, addr);
        // An empty `core`-side name still zero-fills the wire buffer.
        assert_eq!(payload.name_len, 0);
        assert_eq!(payload.name, [0u8; 32]);
        // ADA DESIGN v2 (bead `pico-link-chc3`): `seq` round-trips too.
        assert_eq!(payload.seq, 7);
    }

    #[test]
    fn chc3_seq_round_trips_on_every_connect_lifecycle_event_and_command() {
        // ADA DESIGN v2 (bead `pico-link-chc3`): `seq` was added to
        // `PlConnectPayload`, `PlCancelConnectPayload`, and all four
        // connect-lifecycle event payloads at once (the ABI 4->5 / 7->8
        // bumps this test's sibling `preset_store_loaded_next_id_round_
        // trips_and_the_abi_bumps_are_pinned` pins). This test proves the
        // full round trip for the event side (the command side is covered
        // by `start_scan_and_connect_still_map_to_their_own_tags` and
        // `cancel_connect_command_maps_to_the_cancel_connect_tag_and_
        // carries_the_addr_and_seq` above).
        let ui = new_ui();
        let addr = [3, 3, 3, 3, 3, 3];
        unsafe {
            pl_ui_push_event(
                ui,
                PlEvent {
                    version: PL_EVENT_ABI_VERSION,
                    tag: PlEventTag::ConnectFailed as u32,
                    payload: PlEventPayload {
                        connect_failed: PlConnectFailedPayload { addr, reason: PlFailureReason::Timeout as u32, seq: 5 },
                    },
                },
            );
        }
        unsafe {
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "a real seq must not be rejected as malformed");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn connect_command_carries_the_name_bead_pico_link_4vb_4() {
        // Bead pico-link-4vb.4 (T4): `Command::Connect` now carries a name,
        // already truncated by `core` -- this crate copies it byte-for-byte
        // into the fixed wire buffer.
        let addr = [9, 9, 9, 9, 9, 9];
        let connect = pl_command_from(Command::Connect { addr, name: String::from("Sony WH-1000XM5"), seq: 1 });
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

    #[test]
    fn disconnect_command_maps_to_the_disconnect_tag_with_the_current_abi_version() {
        // Bead pico-link-44w: FFI surface only, no UI wiring. No address
        // payload -- see `PlCommandTag::Disconnect`'s doc comment (at most
        // one active connection, same assumption C's existing
        // `PL_BT_PENDING_DISCONNECT` pending-queue entry already makes).
        // Purely additive, so the current ABI version is unchanged.
        assert_eq!(PlCommandTag::Disconnect as u32, 7);
        let wire = pl_command_from(Command::Disconnect);
        assert_eq!(wire.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(wire.tag as u32, PlCommandTag::Disconnect as u32);
    }

    // --- `PlEventTag::AudioFault` / wake-on-fault, proven through the REAL
    // FFI entry points (bead pico-link-9eq2.3.1, design `.planning/design/
    // 2026-09-07-audio-fault-model.md` §7, `.planning/design/2026-09-07-
    // home-fault-strip.md` §7) ---

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_fault_key() {
        let ui = new_ui();
        let mut event = audio_fault_event(PlFaultKey::BufStarved, PlFaultSeverity::Audible, 1);
        // Writing a union field is safe (only reading requires `unsafe`);
        // this deliberately corrupts `key` to an out-of-range ordinal.
        event.payload.audio_fault.key = 200; // one past the highest legal ordinal (5)
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range fault key ordinal must be counted, not matched-on");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_rejects_out_of_range_fault_severity() {
        let ui = new_ui();
        let mut event = audio_fault_event(PlFaultKey::BufStarved, PlFaultSeverity::Audible, 1);
        // Writing a union field is safe (only reading requires `unsafe`);
        // this deliberately corrupts `severity` to an out-of-range ordinal.
        event.payload.audio_fault.severity = 200;
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 1, "an out-of-range severity ordinal must be counted, not matched-on");
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn a_concealed_fault_never_wakes() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_tick(ui, idle_timeout_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "sanity: asleep");

            // `BufStarved` is otherwise wake-eligible (ord 0) -- only its
            // severity differs from the waking case below.
            pl_ui_push_event(ui, audio_fault_event(PlFaultKey::BufStarved, PlFaultSeverity::Concealed, 1));
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "a Concealed fault must never wake the display");

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn an_audible_wake_eligible_fault_wakes_the_display() {
        // Sanity/control for `a_concealed_fault_never_wakes` above -- same
        // key, same setup, only `severity` differs, so the Concealed
        // test's negative result is meaningful rather than "nothing wakes
        // through this path at all".
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_tick(ui, idle_timeout_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "sanity: asleep");

            pl_ui_push_event(ui, audio_fault_event(PlFaultKey::BufStarved, PlFaultSeverity::Audible, 1));
            assert!(pl_ui_display_power(ui) == PlDisplayPower::On, "an Audible, wake-eligible, not-already-Live fault must wake the display");

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn a_repeat_of_an_already_live_fault_key_never_re_arms_the_hold() {
        let ui = new_ui();
        let idle_timeout_us = pico_link_core::DEFAULT_IDLE_TIMEOUT.as_micros() as u64;
        let hold_us = pico_link_core::run::FAULT_WAKE_HOLD.as_micros() as u64;

        unsafe {
            pl_ui_tick(ui, 0);
            pl_ui_tick(ui, idle_timeout_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "sanity: asleep");

            pl_ui_push_event(ui, audio_fault_event(PlFaultKey::BufStarved, PlFaultSeverity::Audible, 1));
            assert!(pl_ui_display_power(ui) == PlDisplayPower::On, "the first raise must wake");

            // A repeat of the SAME key, still well within the Live window,
            // arriving immediately after (no tick in between, so `now` is
            // unchanged). If this incorrectly re-armed the hold, the
            // display would still be On one full hold-window after this
            // second raise, in addition to the first.
            pl_ui_push_event(ui, audio_fault_event(PlFaultKey::BufStarved, PlFaultSeverity::Audible, 2));

            // The hold expires exactly `FAULT_WAKE_HOLD` after the FIRST
            // (and only granted) wake -- proving the repeat did not extend
            // it.
            pl_ui_tick(ui, idle_timeout_us + hold_us);
            assert!(pl_ui_display_power(ui) == PlDisplayPower::Off, "a repeat of an already-Live key must not have re-armed the hold");

            pl_ui_destroy(ui);
        }
    }

    // --- Bead pico-link-8pp1.4 (S3): the cushion-policy FFI seam ---

    #[test]
    fn pl_ui_push_event_cushion_policy_loaded_folds_into_the_model() {
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::CushionPolicyLoaded as u32,
            payload: PlEventPayload { cushion_policy: PlCushionPolicyPayload { policy: 2 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "CushionPolicyLoaded is a legal tag");
            assert_eq!((*ui).app.cushion_policy(), pico_link_core::CushionPolicy::Stable);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_poll_command_surfaces_a_cushion_policy_save_ahead_of_the_ordinary_queue() {
        // Design D9, same shape `take_display_settings_to_save` uses:
        // checked FIRST by `pl_ui_poll_command`, ahead of `App`'s ordinary
        // `commands` queue.
        let ui = new_ui();
        unsafe {
            (*ui).app.request_cushion_policy(pico_link_core::CushionPolicy::Stable);
            let cmd = pl_ui_poll_command(ui);
            assert_eq!(cmd.tag as u32, PlCommandTag::SetCushionPolicy as u32);
            let payload = cmd.payload.cushion_policy;
            assert_eq!(payload.policy, 2, "Stable must encode to wire value 2");

            // Drained -- a second poll must not resurface it.
            let cmd2 = pl_ui_poll_command(ui);
            assert_eq!(cmd2.tag as u32, PlCommandTag::None as u32);
            pl_ui_destroy(ui);
        }
    }

    // --- Bead pico-link-d42g.3 (F3): the Adaptive-floor FFI seam ---

    #[test]
    fn pl_ui_push_event_abr_floor_loaded_folds_into_the_model() {
        let ui = new_ui();
        let event = PlEvent {
            version: PL_EVENT_ABI_VERSION,
            tag: PlEventTag::AbrFloorLoaded as u32,
            payload: PlEventPayload { abr_floor: PlAbrFloorPayload { floor: 3 } },
        };
        unsafe {
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0, "AbrFloorLoaded is a legal tag");
            assert_eq!((*ui).app.abr_floor(), pico_link_core::AbrFloor::Kbps198);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_poll_command_surfaces_an_abr_floor_save_ahead_of_the_ordinary_queue() {
        // Design D9, same shape `take_cushion_policy_to_save` uses: checked
        // FIRST by `pl_ui_poll_command`, ahead of `App`'s ordinary
        // `commands` queue.
        let ui = new_ui();
        unsafe {
            (*ui).app.request_abr_floor(pico_link_core::AbrFloor::Kbps246);
            let cmd = pl_ui_poll_command(ui);
            assert_eq!(cmd.tag as u32, PlCommandTag::SetAbrFloor as u32);
            let payload = cmd.payload.abr_floor;
            assert_eq!(payload.floor, 2, "Kbps246 must encode to wire value 2");

            // Drained -- a second poll must not resurface it.
            let cmd2 = pl_ui_poll_command(ui);
            assert_eq!(cmd2.tag as u32, PlCommandTag::None as u32);
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_push_event_accepts_the_ryw5_preset_tags() {
        // Bead pico-link-ryw.5: the three new event tags round-trip
        // through the checked-conversion path without being rejected as
        // malformed and without panicking -- same shape
        // `pl_ui_push_event_accepts_the_znb7_wizard_tags` uses for its own
        // new tags.
        let ui = new_ui();
        let events = [
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::PresetLoaded as u32,
                payload: PlEventPayload {
                    preset_loaded: PlPresetLoadedPayload { id: 1, blob_len: 0, blob: [0u8; PL_DSP_PRESET_BLOB_LEN] },
                },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::PresetDeleted as u32,
                payload: PlEventPayload { preset_deleted: PlPresetDeletedPayload { id: 1 } },
            },
            PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::PresetStoreLoaded as u32,
                payload: PlEventPayload {
                    preset_store_loaded: PlPresetStoreLoadedPayload { count: 1, status: PlStoreStatus::Loaded as u8, next_id: 2 },
                },
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

    /// Bead `pico-link-ryw.14`, Ada's preset-id-allocation contract: pins
    /// both ABI bumps this bead makes (a future accidental revert of
    /// either would silently desync a mismatched C/Rust pair -- see
    /// `PL_EVENT_ABI_VERSION`/`PL_COMMAND_ABI_VERSION`'s own doc comments),
    /// and proves `PlPresetStoreLoadedPayload::next_id` actually reaches
    /// `core`'s allocator through the real `pl_ui_push_event` round trip --
    /// not just that the field exists on the wire struct.
    #[test]
    fn preset_store_loaded_next_id_round_trips_and_the_abi_bumps_are_pinned() {
        assert_eq!(PL_EVENT_ABI_VERSION, 8, "bumped 6 -> 7 for PlPresetStoreLoadedPayload::next_id, then 7 -> 8 for chc3's seq fields");
        assert_eq!(PL_COMMAND_ABI_VERSION, 5, "bumped 3 -> 4 for the SavePreset preset_id==0 semantics change, then 4 -> 5 for chc3's seq fields");

        let ui = new_ui();
        unsafe {
            let event = PlEvent {
                version: PL_EVENT_ABI_VERSION,
                tag: PlEventTag::PresetStoreLoaded as u32,
                payload: PlEventPayload {
                    preset_store_loaded: PlPresetStoreLoadedPayload { count: 0, status: PlStoreStatus::FirstBoot as u8, next_id: 100 },
                },
            };
            pl_ui_push_event(ui, event);
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);

            // The allocator must now respect C's high-water mark: the next
            // import gets an id >= 100, even though nothing is actually
            // loaded anywhere near that (a deleted-highest-id-then-
            // rebooted scenario, finding 4 of Ada's design comment).
            let name = "After Boot";
            let result = pl_ui_import_preset(ui, XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
            assert_eq!(result.code, 0);
            assert!(result.preset_id >= 100, "the allocator must respect next_id, got {}", result.preset_id);

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_command_from_maps_the_three_ryw5_preset_commands() {
        // `pl_command_from` is a pure function purely so this mapping is
        // unit-testable directly -- same rationale its own doc comment
        // gives, applied to the three commands this bead adds.
        let blob = alloc::vec![1u8, 2, 3];
        let save = pl_command_from(pico_link_core::Command::SavePreset { preset_id: 0, blob: blob.clone() });
        assert_eq!(save.version, PL_COMMAND_ABI_VERSION);
        assert_eq!(save.tag as u32, PlCommandTag::SavePreset as u32);
        unsafe {
            let payload = save.payload.save_preset;
            assert_eq!(payload.preset_id, 0);
            assert_eq!(payload.blob_len, 3);
            assert_eq!(&payload.blob[..3], &blob[..]);
        }

        let delete = pl_command_from(pico_link_core::Command::DeletePreset { preset_id: 9 });
        assert_eq!(delete.tag as u32, PlCommandTag::DeletePreset as u32);
        unsafe {
            assert_eq!(delete.payload.delete_preset.preset_id, 9);
        }

        let addr = [9u8, 8, 7, 6, 5, 4];
        let assign = pl_command_from(pico_link_core::Command::AssignPreset { addr, preset_id: 4 });
        assert_eq!(assign.tag as u32, PlCommandTag::AssignPreset as u32);
        unsafe {
            let payload = assign.payload.assign_preset;
            assert_eq!(payload.addr, addr);
            assert_eq!(payload.preset_id, 4);
        }
    }

    // --- Tests: pico-link-ryw.5, the DSP effects program pull API ---

    fn zeroed_dsp_program() -> PlDspProgram {
        PlDspProgram {
            fs_hz: 0,
            preamp: 0.0,
            n_biquads: 0,
            xfeed_on: 0,
            xfeed_lp_b0: 0.0,
            xfeed_lp_a1: 0.0,
            xfeed_gain: 0.0,
            xfeed_hs_b0: 0.0,
            xfeed_hs_b1: 0.0,
            xfeed_hs_a1: 0.0,
            xfeed_norm: 0.0,
            biquad: [PlBiquad { b0: 0.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }; PL_DSP_MAX_BIQUADS],
        }
    }

    #[test]
    fn pl_ui_take_dsp_program_null_args_return_false() {
        let ui = new_ui();
        let mut out = zeroed_dsp_program();
        unsafe {
            assert!(!pl_ui_take_dsp_program(core::ptr::null_mut(), &mut out));
            assert!(!pl_ui_take_dsp_program(ui, core::ptr::null_mut()));
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_take_dsp_program_no_connected_device_resolves_off_once_then_reports_no_change() {
        // No `ConnectSucceeded`/`PairedDeviceUpserted` at all -- `App::
        // dsp_program` resolves `Program::off` (design sec 3.1's
        // "connected device's preset_id, else Off"). The FIRST call still
        // returns `true` (the initial Off program must reach C's engine
        // once, per `PlUi::last_dsp_program`'s doc comment); the SECOND,
        // with nothing changed, returns `false`.
        let ui = new_ui();
        let mut out = zeroed_dsp_program();
        unsafe {
            assert!(pl_ui_take_dsp_program(ui, &mut out));
            assert_eq!(out.fs_hz, PL_DSP_FS_HZ);
            assert_eq!(out.preamp, 1.0);
            assert_eq!(out.n_biquads, 0);
            assert_eq!(out.xfeed_on, 0);

            let mut out2 = zeroed_dsp_program();
            assert!(
                !pl_ui_take_dsp_program(ui, &mut out2),
                "an unchanged Off program must not be reported as a change on the second call"
            );
            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_take_dsp_program_maps_crossfeed_coeffs_per_the_ryw5_contract() {
        // Bead pico-link-ryw.5's CONTRACT comment: lo_b0->xfeed_lp_b0,
        // lo_a1->xfeed_lp_a1, hi_b0/hi_b1/hi_a1->xfeed_hs_b0/b1/a1,
        // norm_gain->xfeed_norm, and xfeed_gain is ALWAYS 1.0 (never
        // derived from CrossfeedCoeffs -- g_lo is already folded into
        // lo_b0). Connects a device, assigns it a crossfeed-only preset
        // (no bands, so preamp stays unity and this isolates exactly the
        // crossfeed field mapping), and checks the wire program's fields
        // against a fresh, independent `crossfeed_coeffs` call -- not
        // against hardcoded literals, so this doesn't silently pass if
        // `core`'s own coefficient formula ever changes.
        use pico_link_core::dsp::preset::{CrossfeedLevel, Preset};

        let addr = [1u8, 2, 3, 4, 5, 6];
        let mut preset = Preset::new("Xfeed");
        preset.crossfeed = CrossfeedLevel::Weak;
        let wire_blob = preset.to_wire();
        let mut blob = [0u8; PL_DSP_PRESET_BLOB_LEN];
        blob[..wire_blob.len()].copy_from_slice(&wire_blob);

        let ui = new_ui();
        unsafe {
            pl_ui_push_event(
                ui,
                PlEvent {
                    version: PL_EVENT_ABI_VERSION,
                    tag: PlEventTag::PairedDeviceUpserted as u32,
                    payload: PlEventPayload {
                        paired_device_upserted: PlPairedDeviceUpsertedPayload {
                            addr,
                            name: [0u8; 32],
                            name_len: 0,
                            mru_seq: 1,
                            ldac_quality: 0,
                            preset_id: 7,
                        },
                    },
                },
            );
            pl_ui_push_event(
                ui,
                PlEvent {
                    version: PL_EVENT_ABI_VERSION,
                    tag: PlEventTag::PresetLoaded as u32,
                    payload: PlEventPayload {
                        preset_loaded: PlPresetLoadedPayload { id: 7, blob_len: wire_blob.len() as u8, blob },
                    },
                },
            );
            pl_ui_push_event(
                ui,
                PlEvent {
                    version: PL_EVENT_ABI_VERSION,
                    tag: PlEventTag::ConnectSucceeded as u32,
                    payload: PlEventPayload { connect_succeeded: PlConnectSucceededPayload { addr, degraded: 0, seq: 0 } },
                },
            );
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);

            let mut out = zeroed_dsp_program();
            assert!(pl_ui_take_dsp_program(ui, &mut out));
            assert_eq!(out.fs_hz, PL_DSP_FS_HZ);
            assert_eq!(out.preamp, 1.0, "a band-less preset needs no auto-preamp headroom");
            assert_eq!(out.n_biquads, 0);
            assert_eq!(out.xfeed_on, 1);
            assert_eq!(out.xfeed_gain, 1.0, "xfeed_gain must be the CONTRACT's hardcoded constant, never derived");

            let expected = pico_link_core::dsp::crossfeed_coeffs(CrossfeedLevel::Weak, PL_DSP_FS_HZ)
                .expect("Weak is not Off, must produce coefficients");
            assert_eq!(out.xfeed_lp_b0, expected.lo_b0);
            assert_eq!(out.xfeed_lp_a1, expected.lo_a1);
            assert_eq!(out.xfeed_hs_b0, expected.hi_b0);
            assert_eq!(out.xfeed_hs_b1, expected.hi_b1);
            assert_eq!(out.xfeed_hs_a1, expected.hi_a1);
            assert_eq!(out.xfeed_norm, expected.norm_gain);

            pl_ui_destroy(ui);
        }
    }

    /// Bead `pico-link-ryw.7` review fix, design sec 5.1: proves the live
    /// preview at the actual FFI boundary C calls
    /// (`pl_ui_take_dsp_program`), not just `App::dsp_program` directly --
    /// with no connected device and no editor open the program is Off;
    /// navigating Home -> Effects -> New effect (empty store, so the sole
    /// row) pushes the editor AND must itself flip the live program to
    /// the fresh draft's 5 default bands, with no `SavePreset`/
    /// `PresetLoaded` round trip in between. A second, edit-free take
    /// must report no change (coalesced, not re-sent every frame).
    #[test]
    fn pl_ui_take_dsp_program_previews_the_open_editors_draft() {
        let ui = new_ui();
        unsafe {
            let mut before = zeroed_dsp_program();
            assert!(pl_ui_take_dsp_program(ui, &mut before));
            assert_eq!(before.n_biquads, 0, "no connected device and no editor open must resolve Off");

            ready_ui(ui);
            let open_new_effect_editor = [
                PlIntent { tag: PlIntentTag::Select as u32, jump_by: 0 }, // Home status -> menu face
                PlIntent { tag: PlIntentTag::Down as u32, jump_by: 0 },   // -> Effects row
                PlIntent { tag: PlIntentTag::Select as u32, jump_by: 0 }, // -> Effects list
                PlIntent { tag: PlIntentTag::Select as u32, jump_by: 0 }, // -> "New effect" -> editor
            ];
            pl_ui_input(ui, open_new_effect_editor.as_ptr(), open_new_effect_editor.len());
            assert_eq!(pl_ui_malformed_tag_count(ui), 0);

            let mut opened = zeroed_dsp_program();
            assert!(pl_ui_take_dsp_program(ui, &mut opened), "opening the editor must itself change the live program");
            assert_eq!(opened.n_biquads, 5, "the new effect's default 5-band draft must preview immediately, before any SavePreset echo");

            let mut unchanged = zeroed_dsp_program();
            assert!(
                !pl_ui_take_dsp_program(ui, &mut unchanged),
                "no new edit since the last take -- must report no change, not a fresh program every call"
            );

            pl_ui_destroy(ui);
        }
    }

    /// Bead pico-link-ryw.11: the full `BEGIN`/line/line/`END` sequence
    /// through the real FFI entry point, checking `pl_ui_debug_eq_status`
    /// reports the resulting band count/preamp and `pl_ui_take_dsp_program`
    /// picks up a non-Off program.
    #[test]
    fn pl_ui_debug_eq_command_full_session_activates_an_override() {
        let ui = new_ui();
        unsafe {
            let send = |s: &str| pl_ui_debug_eq_command(ui, s.as_ptr(), s.len());

            assert_eq!(send("BEGIN").code, 0);
            assert_eq!(send("Preamp: -4.41 dB").code, 0);
            assert_eq!(send("Filter 1:  ON  LS  Fc 40 Hz  Gain -1.76 dB  BW Oct 1.917").code, 0);
            assert_eq!(send("Filter 2:  ON  PK  Fc 80 Hz  Gain -2 dB  BW Oct 1.485").code, 0);
            let end = send("END");
            assert_eq!(end.code, 0);

            let mut band_count = 0u8;
            let mut preamp_db = 0f32;
            assert!(pl_ui_debug_eq_status(ui, &mut band_count, &mut preamp_db));
            assert_eq!(band_count, 2);
            assert!((preamp_db - (-4.41)).abs() < 1e-4);

            let mut program = zeroed_dsp_program();
            assert!(pl_ui_take_dsp_program(ui, &mut program));
            assert_eq!(program.n_biquads, 2);

            assert_eq!(send("OFF").code, 0);
            let mut after_off_band_count = 0u8;
            let mut after_off_preamp_db = 0f32;
            assert!(!pl_ui_debug_eq_status(ui, &mut after_off_band_count, &mut after_off_preamp_db));

            let mut program_after_off = zeroed_dsp_program();
            assert!(pl_ui_take_dsp_program(ui, &mut program_after_off), "EQ OFF must itself change the live program back");
            assert_eq!(program_after_off.n_biquads, 0, "EQ OFF must revert to Off with no connected device");

            pl_ui_destroy(ui);
        }
    }

    /// A malformed line reports the right error code AND the 1-based line
    /// number within the session, not just a generic failure.
    #[test]
    fn pl_ui_debug_eq_command_malformed_line_reports_code_and_line_number() {
        let ui = new_ui();
        unsafe {
            let send = |s: &str| pl_ui_debug_eq_command(ui, s.as_ptr(), s.len());

            assert_eq!(send("BEGIN").code, 0);
            assert_eq!(send("Preamp: -1 dB").code, 0); // line 1 of this session
            let result = send("Filter 1: ON XX Fc 100 Hz Gain 1 dB Q 1"); // line 2 -- bad kind token
            assert_eq!(result.code, -4, "UnknownKind must map to code -4");
            assert_eq!(result.line, 2);

            pl_ui_destroy(ui);
        }
    }

    /// A line command with no preceding `BEGIN` reports `NotInSession`
    /// (`code == -9`), and null/invalid-UTF8 input reports `-1` without
    /// touching session state.
    #[test]
    fn pl_ui_debug_eq_command_not_in_session_and_null_args() {
        let ui = new_ui();
        unsafe {
            let result = pl_ui_debug_eq_command(ui, "Preamp: -1 dB".as_ptr(), "Preamp: -1 dB".len());
            assert_eq!(result.code, -9);

            let result = pl_ui_debug_eq_command(core::ptr::null_mut(), core::ptr::null(), 0);
            assert_eq!(result.code, -1);

            let result = pl_ui_debug_eq_command(ui, core::ptr::null(), 0);
            assert_eq!(result.code, -1);

            assert!(!pl_ui_debug_eq_status(core::ptr::null_mut(), core::ptr::null_mut(), core::ptr::null_mut()));

            pl_ui_destroy(ui);
        }
    }

    // --- Tests: pico-link-ryw.12.4, pl_ui_import_preset ---

    /// Andreas's real WH-1000XM3 EQ APO curve -- `tests/fixtures/xm3-
    /// preset.txt`, the same 10-band/1-preamp text
    /// `dsp::import::tests::XM3_PRESET` already round-trips at the `core`
    /// layer; this bead's tests exercise it through the real FFI entry
    /// point instead.
    const XM3_TEXT: &str = include_str!("../tests/fixtures/xm3-preset.txt");

    #[test]
    fn pl_ui_import_preset_the_xm3_file_returns_created_and_queues_exactly_one_save_preset() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let name = "XM3 Harman";
            let result = pl_ui_import_preset(ui, XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
            assert_eq!(result.code, 0, "the XM3 file must import cleanly");
            assert_eq!(result.outcome, 0, "a brand-new name must report Created");
            assert_ne!(result.preset_id, 0, "a real preset id must be allocated");

            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::SavePreset as u32, "import must queue exactly one SavePreset");
            assert_eq!(command.payload.save_preset.preset_id, result.preset_id);

            let next = pl_ui_poll_command(ui);
            assert_eq!(next.tag as u32, PlCommandTag::None as u32, "import must queue EXACTLY one command, not more");

            pl_ui_destroy(ui);
        }
    }

    /// Re-importing the same (unsuffixed-collision) name reports
    /// `Replaced` with the SAME id -- `dsp::import`'s replace-in-place
    /// policy, proven here through the real FFI round trip rather than
    /// just `core`'s own unit test.
    #[test]
    fn pl_ui_import_preset_re_import_returns_replaced_with_the_same_id() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let name = "XM3 Harman";
            let first = pl_ui_import_preset(ui, XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
            assert_eq!(first.code, 0);
            assert_eq!(first.outcome, 0, "Created");
            let _ = pl_ui_poll_command(ui); // drain the first SavePreset

            let second = pl_ui_import_preset(ui, XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
            assert_eq!(second.code, 0);
            assert_eq!(second.outcome, 1, "a same-name re-import of an imported effect must report Replaced");
            assert_eq!(second.preset_id, first.preset_id, "a replace must keep the same id");

            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::SavePreset as u32, "a replace must ALSO queue a SavePreset");

            pl_ui_destroy(ui);
        }
    }

    /// A full store (8 differently-named hand-made effects already
    /// occupying every slot) rejects a genuinely new imported name with
    /// `ImportError::StoreFull` (`code == -14`), and queues nothing.
    #[test]
    fn pl_ui_import_preset_a_full_store_returns_the_error() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            // Fill the 8-slot store via 8 real imports under 8 distinct
            // names -- the FFI surface has no other way to seed the
            // store, and each import is itself proven to succeed above.
            for i in 0..8 {
                let name = alloc::format!("Hand {i}");
                let result = pl_ui_import_preset(ui, XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
                assert_eq!(result.code, 0, "seeding import {i} must succeed");
                let _ = pl_ui_poll_command(ui); // drain each seeding SavePreset
            }

            let name = "One Too Many";
            let result = pl_ui_import_preset(ui, XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
            assert_eq!(result.code, -14, "a full store must reject a new name with StoreFull's code");

            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::None as u32, "a rejected import must queue nothing");

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_import_preset_null_or_invalid_utf8_reports_code_negative_one_without_touching_state() {
        let ui = new_ui();
        unsafe {
            let name = "X";
            let result = pl_ui_import_preset(core::ptr::null_mut(), XM3_TEXT.as_ptr(), XM3_TEXT.len(), name.as_ptr(), name.len());
            assert_eq!(result.code, -1);

            let result = pl_ui_import_preset(ui, core::ptr::null(), 0, name.as_ptr(), name.len());
            assert_eq!(result.code, -1);

            let invalid_utf8: &[u8] = &[0xFF, 0xFE];
            let result = pl_ui_import_preset(ui, invalid_utf8.as_ptr(), invalid_utf8.len(), name.as_ptr(), name.len());
            assert_eq!(result.code, -1);

            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::None as u32, "a rejected/null import must queue nothing");

            pl_ui_destroy(ui);
        }
    }

    // --- Bead pico-link-jyhk.20: pl_ui_library / pl_ui_host_op /
    // pl_ui_host_preview_end ---
    //
    // The `request-*`/`status-*` fixtures under `fixtures/host_op/` are
    // proven-real -- `core`'s own `host_op_fixtures.rs` builds each one by
    // actually running it through `App::host_op`/`host_op_status`, never
    // hand-assembling bytes (see that file's module doc). Reusing them
    // here round-trips this crate's `pl_ui_host_op` wrapper against the
    // exact same canonical bytes `pico-link-jyhk.22`'s web decoder is
    // checked against, rather than a second, independently-typed request.

    const REQUEST_SAVE_CREATE: &[u8] = include_bytes!("../../fixtures/host_op/request-save-create.bin");
    const STATUS_SAVE_SUCCESS: &[u8] = include_bytes!("../../fixtures/host_op/status-save-success.bin");
    const STATUS_NOT_READY: &[u8] = include_bytes!("../../fixtures/host_op/status-not-ready.bin");

    #[test]
    fn pl_ui_host_op_null_or_empty_returns_zero_without_touching_state() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let mut out = [0u8; 200];

            let n = pl_ui_host_op(core::ptr::null_mut(), REQUEST_SAVE_CREATE.as_ptr(), REQUEST_SAVE_CREATE.len(), out.as_mut_ptr(), out.len());
            assert_eq!(n, 0, "null ui must return 0");

            let n = pl_ui_host_op(ui, REQUEST_SAVE_CREATE.as_ptr(), REQUEST_SAVE_CREATE.len(), core::ptr::null_mut(), out.len());
            assert_eq!(n, 0, "null out must return 0");

            let n = pl_ui_host_op(ui, core::ptr::null(), 4, out.as_mut_ptr(), out.len());
            assert_eq!(n, 0, "null request with nonzero in_len must return 0");

            // A rejected/skipped call above must never have queued
            // anything (there is no valid SAVE in any of these calls).
            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::None as u32);

            pl_ui_destroy(ui);
        }
    }

    /// Review fix (bead `pico-link-jyhk.20`): a too-small `out_cap` must
    /// reject BEFORE `App::host_op` runs, not after -- otherwise a REAL
    /// SAVE-create request executes (mutating the preset store and queuing
    /// a `SavePreset` command) and only then discovers `out` can't hold the
    /// status, losing it forever. Sends the real `request-save-create.bin`
    /// fixture (not a garbage/malformed request -- that would prove nothing
    /// about ordering, since a rejected request never mutates anything
    /// either way) with `out_cap` one byte under
    /// [`pico_link_core::app::MAX_OP_STATUS_LEN`], and asserts the call had
    /// NO effect at all: no bytes written, no command queued, no store
    /// mutation (`pl_ui_library`'s `library_rev` unchanged).
    #[test]
    fn pl_ui_host_op_too_small_out_cap_rejects_before_mutating_anything() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);

            // Baseline: establish `PlUi::last_library_rev` so a later
            // `pl_ui_library` call returning `0` (unchanged) is meaningful
            // rather than just "first call always sets the cache".
            let mut lib_buf = [0u8; 2048];
            let baseline_n = pl_ui_library(ui, lib_buf.as_mut_ptr(), lib_buf.len());
            assert!(baseline_n > 0, "a ready store must publish a real library snapshot");

            let too_small = pico_link_core::app::MAX_OP_STATUS_LEN - 1;
            let mut out = [0xAAu8; 200];
            let n = pl_ui_host_op(ui, REQUEST_SAVE_CREATE.as_ptr(), REQUEST_SAVE_CREATE.len(), out.as_mut_ptr(), too_small);
            assert_eq!(n, 0, "out_cap one byte under MAX_OP_STATUS_LEN must reject, not truncate");
            assert_eq!(out, [0xAAu8; 200], "a rejected too-small call must not write anything into out");

            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::None as u32, "a rejected too-small call must not queue SavePreset");

            let after_n = pl_ui_library(ui, lib_buf.as_mut_ptr(), lib_buf.len());
            assert_eq!(after_n, 0, "library_rev must be unchanged -- pl_ui_library returns 0 when unchanged from the last read");

            pl_ui_destroy(ui);
        }
    }

    /// A null `request` with `in_len == 0` is accepted as an empty (too
    /// short for the 4-byte header) request -- `App::host_op` rejects it
    /// gracefully as `InvalidRequest` (`error == 1`) rather than this
    /// wrapper silently dropping the call, matching the doc comment's
    /// "never panics on malformed/truncated" contract.
    #[test]
    fn pl_ui_host_op_null_request_with_zero_len_decodes_as_invalid_request() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let mut out = [0u8; 200];
            let n = pl_ui_host_op(ui, core::ptr::null(), 0, out.as_mut_ptr(), out.len());
            assert!(n > 0, "an empty request must still produce a real GET_OP_STATUS reply");
            assert_eq!(out[3], 2, "state must be REJECTED");
            assert_eq!(out[4], 1, "error must be InvalidRequest");
            pl_ui_destroy(ui);
        }
    }

    /// `pico-link-jyhk.19`'s `NotReady` gate (`App::presets_ready` false on
    /// a fresh app): feeding the real `request-save-create` fixture before
    /// `ready_ui` reproduces `fixtures/host_op/status-not-ready.bin`
    /// byte-for-byte -- proves this wrapper doesn't reorder/skip the gate
    /// `core` already enforces.
    #[test]
    fn pl_ui_host_op_before_presets_ready_matches_the_not_ready_fixture() {
        let ui = new_ui();
        unsafe {
            let mut out = [0u8; 200];
            let n = pl_ui_host_op(ui, REQUEST_SAVE_CREATE.as_ptr(), REQUEST_SAVE_CREATE.len(), out.as_mut_ptr(), out.len());
            assert_eq!(&out[..n], STATUS_NOT_READY, "must match fixtures/host_op/status-not-ready.bin exactly");
            pl_ui_destroy(ui);
        }
    }

    /// The success path, same fixture: after `ready_ui`, the real
    /// `SAVE_EFFECT` create request reproduces
    /// `fixtures/host_op/status-save-success.bin` byte-for-byte, and
    /// queues exactly one `SavePreset` command (Andreas's "no on-device
    /// confirm" ruling, same as `pl_ui_import_preset`).
    #[test]
    fn pl_ui_host_op_save_create_matches_the_success_fixture_and_queues_save_preset() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let mut out = [0u8; 200];
            let n = pl_ui_host_op(ui, REQUEST_SAVE_CREATE.as_ptr(), REQUEST_SAVE_CREATE.len(), out.as_mut_ptr(), out.len());
            assert_eq!(&out[..n], STATUS_SAVE_SUCCESS, "must match fixtures/host_op/status-save-success.bin exactly");

            let command = pl_ui_poll_command(ui);
            assert_eq!(command.tag as u32, PlCommandTag::SavePreset as u32, "a successful SAVE create must queue exactly one SavePreset");

            pl_ui_destroy(ui);
        }
    }

    #[test]
    fn pl_ui_host_preview_end_null_is_a_no_op() {
        unsafe {
            pl_ui_host_preview_end(core::ptr::null_mut());
        }
    }

    #[test]
    fn pl_ui_library_null_or_too_small_returns_zero() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let mut buf = [0u8; 2048];

            let n = pl_ui_library(core::ptr::null(), buf.as_mut_ptr(), buf.len());
            assert_eq!(n, 0, "null ui must return 0");

            let n = pl_ui_library(ui, core::ptr::null_mut(), buf.len());
            assert_eq!(n, 0, "null buf must return 0");

            let n = pl_ui_library(ui, buf.as_mut_ptr(), 2);
            assert_eq!(n, 0, "a buffer too small for even the header must return 0");

            pl_ui_destroy(ui);
        }
    }

    /// Design section 11 Task 3 / section 3: a second call with nothing
    /// changed must return `0` (this crate's own [`PlUi::last_library_rev`]
    /// bookkeeping, layered on top of `App::library_snapshot`'s
    /// always-re-encode behaviour -- see [`pl_ui_library`]'s doc comment),
    /// and a real mutation (a `SAVE` create via [`pl_ui_host_op`], which --
    /// unlike `DELETE`/`ASSIGN` -- writes the preset store immediately, not
    /// only on the `PresetLoaded` echo) must make the next call publish
    /// again with a strictly greater `library_rev`.
    #[test]
    fn pl_ui_library_returns_zero_when_unchanged_and_republishes_after_a_real_change() {
        let ui = new_ui();
        unsafe {
            ready_ui(ui);
            let mut buf = [0u8; 2048];

            let first = pl_ui_library(ui, buf.as_mut_ptr(), buf.len());
            assert!(first >= 6, "first call must publish a real snapshot");
            let first_rev = u16::from_le_bytes([buf[4], buf[5]]);
            assert_ne!(first_rev, 0, "App::refresh_library_rev never yields 0");

            let second = pl_ui_library(ui, buf.as_mut_ptr(), buf.len());
            assert_eq!(second, 0, "an unchanged library must return 0 on the second call");

            let mut op_out = [0u8; 200];
            let n = pl_ui_host_op(ui, REQUEST_SAVE_CREATE.as_ptr(), REQUEST_SAVE_CREATE.len(), op_out.as_mut_ptr(), op_out.len());
            assert_eq!(op_out[3], 1, "the SAVE create must be accepted (state DONE)");
            let _ = n;

            let third = pl_ui_library(ui, buf.as_mut_ptr(), buf.len());
            assert!(third >= 6, "a real content change must publish again");
            let third_rev = u16::from_le_bytes([buf[4], buf[5]]);
            assert_ne!(third_rev, first_rev, "library_rev must change after a real SAVE create");

            let fourth = pl_ui_library(ui, buf.as_mut_ptr(), buf.len());
            assert_eq!(fourth, 0, "unchanged again after the settled state must return 0");

            pl_ui_destroy(ui);
        }
    }
}
