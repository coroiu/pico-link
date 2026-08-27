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
static uint8_t s_history[PL_INPUT_PIN_COUNT];
// Whether each pin's last *debounced* (not raw) state was "pressed" --
// this is what edge detection compares against, so a press event fires
// exactly once per press, not once per still-bouncing sample.
static bool s_debounced_pressed[PL_INPUT_PIN_COUNT];

static absolute_time_t s_next_sample_time;

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
    s_next_sample_time = get_absolute_time();
}

size_t pl_link_input_poll(PlIntent *out, size_t max) {
    absolute_time_t now = get_absolute_time();
    if (absolute_time_diff_us(now, s_next_sample_time) > 0) {
        // Not time to sample yet -- self-paced, see the header's doc
        // comment.
        return 0;
    }
    s_next_sample_time = delayed_by_us(now, PL_INPUT_SAMPLE_INTERVAL_US);

    size_t emitted = 0;
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

        if (now_pressed && !s_debounced_pressed[i] && emitted < max) {
            out[emitted].tag = PINS[i].tag;
            out[emitted].jump_by = 0; // unused for every tag this module emits
            emitted++;
        }
        s_debounced_pressed[i] = now_pressed;
    }
    return emitted;
}
