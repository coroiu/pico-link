#include "input.h"

#include "hardware/gpio.h"
#include "pico/stdlib.h"
#include "pico/time.h"

#define PL_INPUT_PIN_COUNT 9
#define PL_INPUT_SAMPLE_INTERVAL_US 1000 // ~1ms, per the M1b design

typedef struct {
    uint8_t gpio;
    PlIntentTag tag;
} pl_input_pin_t;

// Re-derived from scratch for MADCTL=0x60 (bead pico-link-zzq, 2026-08-28),
// which corner-pattern measurement showed to be an IDENTITY mapping on this
// panel (see st7789.c's MADCTL comment) -- no rotation, no mirroring. The
// joystick is physically fixed to the PCB, so the pin that ends up meaning
// NavIntent up/down/left/right depends entirely on how the panel's MADCTL
// rotates the displayed image relative to that fixed hardware; since 0x60
// applies NO rotation, each pin means exactly what its name says, same as
// the table that predates pico-link-g7o's (wrong) 90-degree remap. Do NOT
// treat that prior remapped table as a starting point to patch -- it was
// derived by rotating 90 CW from a 0x00 baseline while the commit that
// introduced it claimed a 180-degree change from 0x60, two different and
// inconsistent baselines (see bead pico-link-zzq comments) -- this table
// was re-derived independently from the MEASURED 0x60=identity result.
// UNVERIFIED ON HARDWARE -- this needs a physical press to confirm; see
// bead pico-link-d7k.
static const pl_input_pin_t PINS[PL_INPUT_PIN_COUNT] = {
    {PL_INPUT_PIN_UP, PL_INTENT_TAG_UP},
    {PL_INPUT_PIN_DOWN, PL_INTENT_TAG_DOWN},
    {PL_INPUT_PIN_LEFT, PL_INTENT_TAG_LEFT},
    {PL_INPUT_PIN_RIGHT, PL_INTENT_TAG_RIGHT},
    // Center joystick press and button A both mean Select -- see
    // pico_link_core::NavIntent::Select's doc comment.
    {PL_INPUT_PIN_PRESS, PL_INTENT_TAG_SELECT},
    {PL_INPUT_PIN_A, PL_INTENT_TAG_SELECT},
    {PL_INPUT_PIN_B, PL_INTENT_TAG_BACK},
    {PL_INPUT_PIN_X, PL_INTENT_TAG_SHORTCUT_X},
    {PL_INPUT_PIN_Y, PL_INTENT_TAG_SHORTCUT_Y},
};

// Per-pin 8-sample shift register: bit 0 (LSB) is the most recent raw
// sample, 1 = released (line high, since these are active-low with a pull-
// up), 0 = pressed (line low). A debounced state change is only recognised
// once all 8 tracked samples agree -- 0xFF (rock solid released) or 0x00
// (rock solid pressed) -- which at the ~1ms sample rate means the input
// must have been stable for ~8ms before it's trusted, filtering out
// mechanical switch bounce without a hand-tuned single threshold.
//
// Touched ONLY from sample_and_debounce_callback, which pico-sdk's default
// alarm pool runs in hardware IRQ context on core 0 -- never read or
// written from the superloop, so it needs no locking of its own.
static uint8_t s_history[PL_INPUT_PIN_COUNT];
// Whether each pin's last *debounced* (not raw) state was "pressed" --
// this is what edge detection compares against, so a press event fires
// exactly once per press, not once per still-bouncing sample. Same
// IRQ-only ownership as s_history above.
static bool s_debounced_pressed[PL_INPUT_PIN_COUNT];

static struct repeating_timer s_sample_timer;

// Single-producer/single-consumer ring buffer decoupling the IRQ-context
// sampler (producer, writes s_ring_head) from the superloop drain (consumer,
// writes s_ring_tail) -- see pl_link_input_poll below. Each side owns
// exactly one index, so plain volatile uint8_t reads/writes are sufficient:
// there is no read-modify-write race, and RP2350 loads/stores of a single
// byte are inherently atomic. No mutex, no IRQ masking, no calls into Rust
// from IRQ context -- the FFI direction rule (C calls Rust, never the
// reverse) stays intact because pl_ui_input is only ever called from
// main.c's superloop, which drains this buffer via pl_link_input_poll.
//
// Capacity: 32 is generous headroom over anything a human can produce --
// even mashing every one of the 9 pins simultaneously only yields at most 9
// press-edge events per debounce-settled transition, and a full second of
// superloop stall (today's ~1.03s worst case at 1MHz SPI, pico-link-14l)
// would need over three such simultaneous all-button mashes to overflow it.
#define PL_INPUT_RING_CAPACITY 32

static PlIntent s_ring[PL_INPUT_RING_CAPACITY];
static volatile uint8_t s_ring_head; // producer-owned (IRQ)
static volatile uint8_t s_ring_tail; // consumer-owned (superloop)
// Diagnostics only: counts intents dropped because the ring was full when
// the sampler tried to push. Never drained/reset; a real bump here across a
// bring-up session is a bug to chase (either the superloop stopped calling
// pl_link_input_poll, or PL_INPUT_RING_CAPACITY genuinely needs to grow),
// not something silently swallowed.
static volatile uint32_t s_ring_drop_count;

// Called from a hardware alarm IRQ every PL_INPUT_SAMPLE_INTERVAL_US
// (pico-sdk's default alarm pool), decoupling debounce timing from the
// superloop's frame rate -- see pico-link-5am. MUST NOT call into Rust:
// pl_ui_t is not Sync and every call into it must come from the superloop
// on core 0, so this callback only ever touches the C-side debounce state
// and ring buffer above.
static bool sample_and_debounce_callback(struct repeating_timer *t) {
    (void)t;
    for (size_t i = 0; i < PL_INPUT_PIN_COUNT; i++) {
        bool raw_high = gpio_get(PINS[i].gpio) != 0;
        s_history[i] = (uint8_t)((s_history[i] << 1) | (raw_high ? 1 : 0));

        bool now_pressed;
        if (s_history[i] == 0x00) {
            now_pressed = true;
        } else if (s_history[i] == 0xFF) {
            now_pressed = false;
        } else {
            // Still bouncing / mid-transition -- keep the last debounced
            // state, emit nothing.
            continue;
        }

        if (now_pressed && !s_debounced_pressed[i]) {
            uint8_t head = s_ring_head;
            uint8_t next_head = (uint8_t)((head + 1) % PL_INPUT_RING_CAPACITY);
            if (next_head == s_ring_tail) {
                // Ring full -- the superloop hasn't drained in a while.
                // Drop this intent rather than overwrite an undrained one
                // (overwriting would corrupt the consumer's view instead of
                // just losing one event) and count it so the drop is
                // visible instead of silent.
                s_ring_drop_count++;
            } else {
                s_ring[head].tag = PINS[i].tag;
                s_ring[head].jump_by = 0; // unused for every tag this module emits
                s_ring_head = next_head;
            }
        }
        s_debounced_pressed[i] = now_pressed;
    }
    return true; // keep repeating
}

void pl_link_input_init(void) {
    for (size_t i = 0; i < PL_INPUT_PIN_COUNT; i++) {
        uint8_t pin = PINS[i].gpio;
        gpio_init(pin);
        gpio_set_dir(pin, GPIO_IN);
        // Pull-UP, not pull-down: RP2350 erratum E9 (see CLAUDE.md).
        gpio_pull_up(pin);
        s_history[i] = 0xFF; // start "confidently released"
        s_debounced_pressed[i] = false;
    }
    s_ring_head = 0;
    s_ring_tail = 0;
    s_ring_drop_count = 0;

    // Negative interval: fire every PL_INPUT_SAMPLE_INTERVAL_US measured
    // from the previous scheduled time, not from when the callback
    // finished -- keeps the sample rate exact regardless of how long the
    // callback itself takes (pico-sdk's add_repeating_timer_us convention).
    add_repeating_timer_us(-PL_INPUT_SAMPLE_INTERVAL_US, sample_and_debounce_callback, NULL, &s_sample_timer);
}

// Drains debounced press-edge events already queued by the timer IRQ --
// does no sampling itself, so it's cheap and safe to call every superloop
// iteration regardless of the loop's own rate. Writes at most `max` events
// into `out` and returns how many were written.
size_t pl_link_input_poll(PlIntent *out, size_t max) {
    size_t emitted = 0;
    while (emitted < max) {
        uint8_t tail = s_ring_tail;
        if (tail == s_ring_head) {
            break; // caught up
        }
        out[emitted] = s_ring[tail];
        s_ring_tail = (uint8_t)((tail + 1) % PL_INPUT_RING_CAPACITY);
        emitted++;
    }
    return emitted;
}
