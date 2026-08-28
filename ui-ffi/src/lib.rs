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
use alloc::string::String;
use alloc::vec::Vec;

use embedded_alloc::LlffHeap as Heap;
use pico_link_core::{App, Command, ConnectFailureReason, DeviceEntry, Event, LinkState, NavIntent};

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

/// Mirrors [`pico_link_core::LinkState`]'s four variants 1:1.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlLinkState {
    Idle,
    Scanning,
    Connecting,
    Connected,
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
/// structurally non-retryable.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlFailureReason {
    Timeout,
    Rejected,
    NoA2dpSink,
    NeedsPin,
    RadioError,
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

/// [`PlEvent`]'s payload when `tag == PlEventTag::LinkStateChanged`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlLinkStateChangedPayload {
    pub state: PlLinkState,
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
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlConnectFailedPayload {
    pub addr: [u8; 6],
    pub reason: PlFailureReason,
}

/// Which variant of [`PlEventPayload`] is active in a given [`PlEvent`].
/// `DevicesCleared` carries no data -- the payload union is simply unread
/// for that tag (see [`PlEventPayload`]'s doc comment).
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlEventTag {
    LinkStateChanged,
    DeviceDiscovered,
    DevicesCleared,
    ConnectFailed,
}

/// The union of every [`PlEvent`] payload shape. Which field is valid to
/// read is determined entirely by the sibling `tag` field on [`PlEvent`] --
/// reading the wrong field is a logic bug, not a memory-safety one (every
/// member is a plain, `Copy`, no-`Drop` payload struct), but is still
/// meaningless data. `PlEventTag::DevicesCleared` has no payload of its
/// own; the union simply isn't read for that tag, so no placeholder member
/// is needed for it.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PlEventPayload {
    pub link_state_changed: PlLinkStateChangedPayload,
    pub device_discovered: PlDeviceDiscoveredPayload,
    pub connect_failed: PlConnectFailedPayload,
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
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlEvent {
    pub version: u32,
    pub tag: PlEventTag,
    pub payload: PlEventPayload,
}

/// Pushes one [`PlEvent`] into the app core, folding it into the live
/// Bluetooth model and refreshing the root screen
/// (`App::handle_event`'s FFI entry point). A no-op if `ui` is null or
/// `event.version` doesn't match [`PL_EVENT_ABI_VERSION`] (see the module
/// section doc's ABI version guard -- a mismatch means the `payload` union
/// must not be read under this build's variant shapes).
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

    let core_event = match event.tag {
        PlEventTag::LinkStateChanged => {
            // SAFETY: `tag` says this union currently holds `link_state_changed`.
            let payload = unsafe { event.payload.link_state_changed };
            Event::LinkStateChanged(payload.state.into())
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
            let payload = unsafe { event.payload.connect_failed };
            Event::ConnectFailed { addr: payload.addr, reason: payload.reason.into() }
        }
    };
    ui.app.handle_event(core_event);
}

/// Which variant of [`PlCommand`] this value is. `None` is not one of
/// [`pico_link_core::Command`]'s variants; it exists purely so
/// [`pl_ui_poll_command`] has a value to return when nothing is queued (or
/// `ui` is null, or `ui`'s version check fails), since this function
/// returns by value rather than an `Option`-shaped pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub enum PlCommandTag {
    None,
    StartScan,
    Connect,
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
        Some(Command::StartScan) => {
            PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::StartScan, payload: PlCommandPayload { connect: PlConnectPayload { addr: [0; 6] } } }
        }
        Some(Command::Connect { addr }) => {
            PlCommand { version: PL_COMMAND_ABI_VERSION, tag: PlCommandTag::Connect, payload: PlCommandPayload { connect: PlConnectPayload { addr } } }
        }
        None => pl_command_none(),
    }
}
