// GPIO input polling + debounce for the Waveshare Pico-LCD-1.3's 5-way
// joystick + 4 buttons, mapped to pl_intent_t (see pico_link_ui.h).
//
// Boundary rule (M1b design): debounce is C's job; anything the emulator
// must be able to reproduce headlessly (auto-repeat, held-direction
// acceleration into NavIntent::JumpBy) stays in Rust core instead, which is
// why this module only ever emits discrete press-edge events, never
// sustained/held state.
//
// Pull-UPS, not pull-downs: RP2350 erratum E9 affects pull-downs on GPIO
// inputs (see CLAUDE.md).
#ifndef PICO_LINK_INPUT_H
#define PICO_LINK_INPUT_H

#include <stddef.h>

#include "pico_link_ui.h"

// Joystick.
#define PL_INPUT_PIN_UP 2
#define PL_INPUT_PIN_DOWN 18
#define PL_INPUT_PIN_LEFT 16
#define PL_INPUT_PIN_RIGHT 20
#define PL_INPUT_PIN_PRESS 3
// Buttons A/B/X/Y.
#define PL_INPUT_PIN_A 15
#define PL_INPUT_PIN_B 17
#define PL_INPUT_PIN_X 19
#define PL_INPUT_PIN_Y 21

// Configures all 9 GPIOs as inputs with internal pull-ups and starts a
// pico-sdk repeating timer that samples and debounces them every ~1ms in
// hardware IRQ context, independent of the caller's own loop rate (see
// pico-link-5am -- sampling used to be driven from the superloop, which
// meant the debounce needed ~8x the *render* frame time to register a
// press). The IRQ callback never touches Rust; it only pushes debounced
// press-edge events into an internal ring buffer for pl_link_input_poll to
// drain.
void pl_link_input_init(void);

// Drains already-debounced press-edge events queued by the background
// timer -- does no sampling itself, so it is cheap and safe (and intended)
// to call every superloop iteration regardless of the loop's own rate.
// Writes at most `max` newly edge-triggered press events into `out` (one
// entry per pin that debounced from released to pressed) and returns how
// many were written -- 0 most calls, since presses are comparatively rare
// events. If the internal ring buffer overflows (the superloop hasn't
// drained fast enough for a burst of presses), the oldest-unseen events
// already queued are preserved and the newest overflowing ones are dropped
// with a diagnostic counter bumped internally -- see input.c.
size_t pl_link_input_poll(PlIntent *out, size_t max);

#endif // PICO_LINK_INPUT_H
