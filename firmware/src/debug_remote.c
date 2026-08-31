#include "debug_remote.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "pico/bootrom.h"
#include "pico/stdio.h"
#include "pico/stdlib.h"
#include "tusb.h"

#include "bt.h"
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

// Returns the hex value of one ASCII hex digit, or -1 if `c` isn't one.
static int hex_nibble(char c) {
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    if (c >= 'A' && c <= 'F') {
        return c - 'A' + 10;
    }
    return -1;
}

// Bead pico-link-g48: parses "CONNECT <addr>" where <addr> is exactly 6
// bytes of hex, optionally ':'-or-'-'-separated (e.g. "AABBCCDDEEFF" or
// "AA:BB:CC:DD:EE:FF") -- anything else (wrong byte count, non-hex
// characters, extra separators) is rejected outright rather than guessed
// at. The address itself never touches static/global storage beyond this
// one out-parameter -- it is host-supplied per call, never a constant in
// this codebase (see bt.h's doc comment on pl_bt_debug_connect).
static bool parse_connect_addr(const char *line, uint8_t addr[6]) {
    if (strncmp(line, "CONNECT ", 8) != 0) {
        return false;
    }
    const char *p = line + 8;
    size_t byte_idx = 0;
    while (*p != '\0' && byte_idx < 6) {
        if (*p == ':' || *p == '-') {
            p++;
            continue;
        }
        int hi = hex_nibble(*p);
        if (hi < 0) {
            return false;
        }
        p++;
        int lo = hex_nibble(*p);
        if (lo < 0) {
            return false;
        }
        p++;
        addr[byte_idx++] = (uint8_t)((hi << 4) | lo);
    }
    return byte_idx == 6 && *p == '\0';
}

size_t pl_debug_remote_poll(PlIntent *out, size_t max) {
    size_t emitted = 0;

    // Bead pico-link-okx (F2b): getchar_timeout_us(0) went through
    // pico_stdio_usb's stdio_usb_in_chars(), which calls tud_task() from
    // THREAD context under stdio_usb_mutex -- a second re-entrancy hole
    // alongside the one F2 closed for the log drain (tud_task() is not
    // reentrant, and the 0xC0 worker in usb_pump.c also calls it, under a
    // DIFFERENT mutex -- see this bead's design comment, Ada, 2026-08-30).
    // Read the CDC RX FIFO directly instead, under the same
    // pl_usb_lock_try() seam pl_log_ring_drain() uses (usb_pump.h):
    // non-blocking, no tud_task() call from here, and if the 0xC0 worker
    // holds the lock this whole poll is skipped -- the next call, a frame
    // or so later, tries again. Held for the WHOLE poll (not per byte):
    // tud_cdc_read() never blocks internally, so one lock/unlock pair per
    // superloop iteration is strictly less contention than re-acquiring it
    // up to PL_DEBUG_REMOTE_MAX_BYTES_PER_POLL times.
    if (!pl_usb_lock_try()) {
        return 0;
    }
    if (!tud_ready()) {
        pl_usb_unlock();
        return 0;
    }

    for (int budget = 0; budget < PL_DEBUG_REMOTE_MAX_BYTES_PER_POLL; budget++) {
        uint8_t byte;
        int c = (tud_cdc_read(&byte, 1) == 1) ? (int)byte : PICO_ERROR_TIMEOUT;
        if (c == PICO_ERROR_TIMEOUT) {
            break; // caught up -- nothing more waiting right now
        }
        if (c == '\r') {
            continue; // tolerate CRLF line endings from the host
        }
        if (c == '\n') {
            if (s_line_len > 0) {
                s_line[s_line_len] = '\0';
                uint8_t connect_addr[6];
                if (strcmp(s_line, "BOOTSEL") == 0) {
                    // Bead pico-link-vu4: routes around pico-link-d74 (the
                    // vendor CONTROL transfer on interface 4 that STALLs on
                    // a healthy board). This is CDC BULK data instead --
                    // a different endpoint and code path, same structural
                    // reason tools/usb-console/cdc_reader.py's direct-USB
                    // read works where the tty and control paths don't.
                    // Log BEFORE resetting -- reset_usb_boot() is noreturn,
                    // so this is the last thing a capture will show, and
                    // it's what distinguishes "rebooted to BOOTSEL on
                    // purpose" from "the board just vanished/crashed".
                    pl_log("debug-remote: BOOTSEL -> reset_usb_boot(0, 0)\r\n");
                    reset_usb_boot(0, 0);
                    // unreachable -- reset_usb_boot() does not return.
                } else if (parse_connect_addr(s_line, connect_addr)) {
                    // Not a NavIntent -- dispatched directly to bt.c
                    // rather than going through `out`/pl_ui_input, since
                    // there is no discovered DeviceEntry backing it (see
                    // bt.h's pl_bt_debug_connect doc comment). Still
                    // thread-context-only, called from this same
                    // main-loop poll.
                    pl_log("debug-remote: CONNECT %s -> dispatched\r\n", s_line + 8);
                    pl_bt_debug_connect(connect_addr);
                } else if (strcmp(s_line, "DISCONNECT") == 0) {
                    // Bead pico-link-nb6: mirrors the CONNECT branch above --
                    // no discovered DeviceEntry involved, so this bypasses
                    // out/pl_ui_input and dispatches straight to bt.c.
                    // No address needed (there is only ever one connection).
                    pl_log("debug-remote: DISCONNECT -> dispatched\r\n");
                    pl_bt_debug_disconnect();
                } else if (emitted < max) {
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
    pl_usb_unlock();
    return emitted;
}
