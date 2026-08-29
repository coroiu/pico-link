#include "debug_remote.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "pico/stdio.h"
#include "pico/stdlib.h"

#include "usb_pump.h"

// Longest valid line is "NAV SHORTCUT" territory -- "NAV SELECT\n" (11
// chars) or "NAV JUMP -32768" (15 chars) -- 32 leaves comfortable headroom
// without inviting a large static buffer.
#define PL_DEBUG_REMOTE_LINE_MAX 32
// Bounds how many raw bytes a single pl_debug_remote_poll() call drains
// from the CDC RX side, so a runaway or garbled host stream can never stall
// the main loop for one iteration -- generous relative to the longest valid
// line and to how much a host script would plausibly queue between polls
// at this loop's frame rate.
#define PL_DEBUG_REMOTE_MAX_BYTES_PER_POLL 256

static char s_line[PL_DEBUG_REMOTE_LINE_MAX];
static size_t s_line_len;

// Parses one already-NUL-terminated, newline-stripped command line into
// *out. Returns true if it recognized a command, false otherwise -- a
// false return is never fatal, just logged by the caller.
static bool parse_line(const char *line, PlIntent *out) {
    if (strcmp(line, "NAV UP") == 0) {
        out->tag = PL_INTENT_TAG_UP;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV DOWN") == 0) {
        out->tag = PL_INTENT_TAG_DOWN;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV LEFT") == 0) {
        out->tag = PL_INTENT_TAG_LEFT;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV RIGHT") == 0) {
        out->tag = PL_INTENT_TAG_RIGHT;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV SELECT") == 0) {
        out->tag = PL_INTENT_TAG_SELECT;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV BACK") == 0) {
        out->tag = PL_INTENT_TAG_BACK;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV X") == 0) {
        out->tag = PL_INTENT_TAG_SHORTCUT_X;
        out->jump_by = 0;
        return true;
    }
    if (strcmp(line, "NAV Y") == 0) {
        out->tag = PL_INTENT_TAG_SHORTCUT_Y;
        out->jump_by = 0;
        return true;
    }
    if (strncmp(line, "NAV JUMP ", 9) == 0) {
        long v = strtol(line + 9, NULL, 10);
        out->tag = PL_INTENT_TAG_JUMP_BY;
        out->jump_by = (int16_t)v;
        return true;
    }
    return false;
}

size_t pl_debug_remote_poll(PlIntent *out, size_t max) {
    size_t emitted = 0;
    for (int budget = 0; budget < PL_DEBUG_REMOTE_MAX_BYTES_PER_POLL; budget++) {
        int c = getchar_timeout_us(0);
        if (c == PICO_ERROR_TIMEOUT) {
            break; // caught up -- nothing more waiting right now
        }
        if (c == '\r') {
            continue; // tolerate CRLF line endings from the host
        }
        if (c == '\n') {
            if (s_line_len > 0) {
                s_line[s_line_len] = '\0';
                if (emitted < max) {
                    PlIntent intent;
                    if (parse_line(s_line, &intent)) {
                        out[emitted++] = intent;
                        pl_log("debug-remote: %s -> injected\r\n", s_line);
                    } else {
                        pl_log("debug-remote: unrecognized line: \"%s\"\r\n", s_line);
                    }
                } else {
                    // `max` intents already queued this call -- drop the
                    // rest of the line rather than overflow `out`. The
                    // next poll() call will pick up whatever the host
                    // sends after this point; nothing here is lost from
                    // the CDC buffer itself, only from this call's batch.
                    pl_log("debug-remote: dropped \"%s\" -- out buffer full this poll\r\n", s_line);
                }
                s_line_len = 0;
            }
            continue;
        }
        if (s_line_len + 1 < PL_DEBUG_REMOTE_LINE_MAX) {
            s_line[s_line_len++] = (char)c;
        } else {
            // Overlong line -- drop what's buffered and resync on the next
            // newline rather than overflowing s_line.
            pl_log("debug-remote: line too long, discarding and resyncing\r\n");
            s_line_len = 0;
        }
    }
    return emitted;
}
