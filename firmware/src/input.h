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

// Configures all 9 GPIOs as inputs with internal pull-ups.
void pl_link_input_init(void);

// Self-paced: internally tracks the last sample time and is a no-op unless
// at least ~1ms has elapsed since the previous call, so it is safe (and
// intended) to call this every superloop iteration regardless of the
// loop's own rate. Writes at most `max` newly edge-triggered press events
// into `out` (one entry per pin that debounced from released to pressed
// this sample) and returns how many were written -- 0 most calls, since
// presses are comparatively rare events.
size_t pl_link_input_poll(PlIntent *out, size_t max);

#endif // PICO_LINK_INPUT_H
