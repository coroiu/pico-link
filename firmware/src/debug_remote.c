#include "debug_remote.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "pico/bootrom.h"
#include "pico/stdio.h"
#include "pico/stdlib.h"

// Bead pico-link-fhf: a2dp.h (below, for pl_a2dp_debug_skip_media_ticks)
// pulls in btstack.h, which collides with tusb.h's class/hid/hid_device.h
// in any TU that includes both (identical `hid_report_type_t` enum member
// names) -- see main.c's own doc comment on this exact conflict, first hit
// there when CFG_TUD_HID was enabled (bead pico-link-47z.1). Same fix:
// this TU calls no tud_hid_*() function directly (media keys go through
// media_keys.c, which has its own TU with the real config), so force
// CFG_TUD_HID off locally before pulling in tusb.h here.
#include "tusb_config.h"
#undef CFG_TUD_HID
#define CFG_TUD_HID 0
#include "tusb.h"

#include "a2dp.h"
#include "bt.h"
#include "media_keys.h"
#include "pl_prio.h"
#include "usb_audio.h"
#include "usb_pump.h"
#include "volume.h"

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

// Bead pico-link-fhf, test A: parses "SKIPTICKS <K>" where <K> is a
// non-negative decimal integer -- anything else (missing argument,
// non-digit characters, empty digit string) is rejected outright, same
// discipline as parse_connect_addr above. *out_ticks is only written on a
// true return.
//
// Clamped to PL_DEBUG_SKIP_TICKS_MAX (code review, 2026-09-07, non-
// blocking): the injection test only ever needs K=10 (~113ms of skipped
// drain at the real ~11.3ms tick cadence); an unbounded K typed at the CDC
// console -- or a garbled/malicious one -- could otherwise idle the drain
// for minutes, well past PL_WDT_MEDIA's stall deadline, on a debug-only
// path with no other guard. The accumulate-then-clamp order (rather than
// rejecting outright) matches this file's "never fatal, never wedges"
// contract for malformed input -- an oversized K still does SOMETHING
// bounded rather than nothing.
#define PL_DEBUG_SKIP_TICKS_MAX 1000u
static bool parse_skip_ticks(const char *line, uint32_t *out_ticks) {
    if (strncmp(line, "SKIPTICKS ", 10) != 0) {
        return false;
    }
    const char *p = line + 10;
    if (*p == '\0') {
        return false;
    }
    // Saturating parse: once `value` reaches the clamp, stop advancing it
    // (and pin it there) rather than let further digits keep multiplying --
    // that would wrap a uint32_t on a long-enough digit string and could
    // land back BELOW the clamp by chance, defeating the point of it.
    uint32_t value = 0;
    for (; *p != '\0'; p++) {
        if (*p < '0' || *p > '9') {
            return false;
        }
        if (value < PL_DEBUG_SKIP_TICKS_MAX) {
            value = value * 10u + (uint32_t)(*p - '0');
            if (value > PL_DEBUG_SKIP_TICKS_MAX) {
                value = PL_DEBUG_SKIP_TICKS_MAX;
            }
        }
    }
    *out_ticks = value;
    return true;
}

// Bead pico-link-4v2.1 (VT1): formats the feature unit's current state
// (per-channel volume/mute plus the SET/GET call counters) into ONE line
// and publishes it through pl_prio.h's slot 4 -- see that header's slot-4
// doc comment for why this does not use pl_log(). CFG_TUD_AUDIO_FUNC_1_N_
// CHANNELS_RX is 2 (tusb_config.h) -> 3 channels (master + L + R), so the
// whole snapshot fits comfortably inside one PL_PRIO_SLOT_LEN=128 slot.
// Thread context only (pl_prio_publish()'s own contract) -- this is only
// ever called from pl_debug_remote_poll(), itself thread-context-only.
static void log_vol_snapshot(void) {
    uint8_t n_ch = pl_usb_audio_fu_channel_count();
    int16_t v0 = n_ch > 0 ? pl_usb_audio_fu_volume(0) : 0;
    int8_t m0 = n_ch > 0 ? pl_usb_audio_fu_mute(0) : 0;
    int16_t v1 = n_ch > 1 ? pl_usb_audio_fu_volume(1) : 0;
    int8_t m1 = n_ch > 1 ? pl_usb_audio_fu_mute(1) : 0;
    int16_t v2 = n_ch > 2 ? pl_usb_audio_fu_volume(2) : 0;
    int8_t m2 = n_ch > 2 ? pl_usb_audio_fu_mute(2) : 0;
    pl_prio_publish(
        4,
        "vol c0 v=%d m=%d c1 v=%d m=%d c2 v=%d m=%d set=%lu get=%lu",
        (int)v0,
        (int)m0,
        (int)v1,
        (int)m1,
        (int)v2,
        (int)m2,
        (unsigned long)pl_usb_audio_fu_set_calls(),
        (unsigned long)pl_usb_audio_fu_get_calls()
    );
}

size_t pl_debug_remote_poll(PlIntent *out, size_t max) {
    size_t emitted = 0;

    // Bead pico-link-4v2.1 (VT1) "VOL WATCH": independent of whatever CDC
    // bytes are or aren't available below, republish the slot-4 snapshot
    // every time fu_set_calls() has advanced since the last check -- i.e.
    // once per FU SET macOS actually sends, at this poll's cadence (once
    // per superloop iteration, unconditional -- does not need
    // pl_usb_lock_try(), pl_prio_publish() touches only its own module's
    // memory). A burst of SETs faster than one superloop iteration apart
    // coalesces to the latest value in the burst, same latch semantics as
    // every other coalescing point in this firmware (pl_prio.h's own
    // module doc) -- acceptable for a step-grid measurement, since T1's
    // question is what values arrive, not their exact arrival timing.
    static uint32_t s_last_fu_set_calls;
    if (pl_usb_audio_watch_enabled()) {
        uint32_t calls = pl_usb_audio_fu_set_calls();
        if (calls != s_last_fu_set_calls) {
            s_last_fu_set_calls = calls;
            log_vol_snapshot();
        }
    }

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
                uint32_t skip_ticks;
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
                } else if (parse_skip_ticks(s_line, &skip_ticks)) {
                    // Bead pico-link-fhf, test A: one-shot injection, not
                    // a NavIntent -- dispatched directly to a2dp.c's
                    // debug counter, same "direct dispatch, not through
                    // `out`" pattern as CONNECT below.
                    pl_log("debug-remote: SKIPTICKS %lu -> dispatched\r\n", (unsigned long)skip_ticks);
                    pl_a2dp_debug_skip_media_ticks(skip_ticks);
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
                } else if (strcmp(s_line, "MEDIA PLAYPAUSE") == 0) {
                    // Bead pico-link-47z.2 (T2): exercises the USB HID half
                    // of media keys with NO Bluetooth/AVRCP involved -- see
                    // media_keys.h's module doc. Mirrors CONNECT/DISCONNECT
                    // above: dispatched directly, not a NavIntent, so it
                    // bypasses out/pl_ui_input entirely.
                    pl_log("debug-remote: MEDIA PLAYPAUSE -> dispatched\r\n");
                    pl_media_keys_push_tap(PL_MEDIA_KEY_USAGE_PLAY_PAUSE);
                } else if (strcmp(s_line, "MEDIA NEXT") == 0) {
                    pl_log("debug-remote: MEDIA NEXT -> dispatched\r\n");
                    pl_media_keys_push_tap(PL_MEDIA_KEY_USAGE_SCAN_NEXT);
                } else if (strcmp(s_line, "MEDIA PREV") == 0) {
                    pl_log("debug-remote: MEDIA PREV -> dispatched\r\n");
                    pl_media_keys_push_tap(PL_MEDIA_KEY_USAGE_SCAN_PREV);
                } else if (strcmp(s_line, "VOL GET") == 0) {
                    // Bead pico-link-4v2.1 (VT1): dumps the feature unit's
                    // stored state and call counters -- answers "did macOS
                    // ever write our feature unit, and what does it hold
                    // now", per design doc sec 9 (T1) question (i).
                    //
                    // Published via pl_prio.h's slot 4, NOT pl_log() --
                    // this file's own MEDIA/CONNECT/etc acknowledgements
                    // above use pl_log() and can be silently dropped under
                    // this firmware's background log-ring congestion (see
                    // pl_prio.h's slot-4 doc comment); a one-shot
                    // measurement command like this one must not be lost
                    // to that.
                    pl_log("debug-remote: VOL GET -> dispatched\r\n");
                    log_vol_snapshot();
                } else if (strcmp(s_line, "VOL WATCH") == 0) {
                    // Toggle. While on, pl_debug_remote_poll()'s own
                    // per-iteration check below (thread context, same as
                    // this handler) republishes the slot-4 snapshot every
                    // time pl_usb_audio_fu_set_calls() changes -- i.e.
                    // every FU SET macOS sends -- answering question (ii),
                    // the actual step grid macOS uses, not what the RANGE
                    // descriptor invites. Polled rather than logged
                    // straight from feature_unit_set_request()'s 0xC0 IRQ
                    // for the same reliability reason as VOL GET above,
                    // and because pl_prio_publish() is thread-context-only
                    // by its own contract (pl_prio.h).
                    bool now_on = !pl_usb_audio_watch_enabled();
                    pl_usb_audio_set_watch(now_on);
                    pl_log("debug-remote: VOL WATCH -> %s\r\n", now_on ? "ON" : "OFF");
                } else if (strncmp(s_line, "VOL SET", 7) == 0) {
                    // Bead pico-link-4v2.2 (VT2): drives volume.c's
                    // canonical value through the console, exercising the
                    // loop rule and circuit breaker (design sec 4) without
                    // any real USB/AVRCP producer wired yet. Applies and
                    // LOGS what it would emit to each peer via pl_prio.h's
                    // slot 4 -- it does not actually emit anything, since
                    // nothing reads volume.c's outbound latches until
                    // T3/T4. See volume.h's pl_volume_debug_set() doc
                    // comment.
                    const char *arg = s_line + 7;
                    long n = (*arg == ' ') ? strtol(arg + 1, NULL, 10) : -1;
                    if (n < 0 || n > 127) {
                        pl_log("debug-remote: VOL SET requires 0..127, got \"%s\"\r\n", s_line + 8);
                    } else {
                        pl_log("debug-remote: VOL SET %ld -> dispatched\r\n", n);
                        pl_volume_debug_set((uint8_t)n, time_us_64());
                    }
                } else if (strncmp(s_line, "VOL FUSET", 9) == 0) {
                    // Bead pico-link-rmp (VT4a.1): unlike "VOL SET" above
                    // (which drives volume.c's canonical AVRCP-domain
                    // value and does not touch the feature unit at all --
                    // nothing reads its outbound latches pre-T3/T4), this
                    // OVERWRITES fu_volume[0] (master channel) directly --
                    // simulates the device itself deciding a new volume,
                    // which is what VT4/AVRCP will eventually do. Takes
                    // the same 0..127 AVRCP-domain input as "VOL SET" and
                    // maps it through the bijection declared in
                    // feature_unit_get_request's GET_RANGE (bMin=-12700,
                    // bRes=100): raw = n*100 - 12700. Does NOT notify the
                    // host -- pair with "VOL INT" to do that, so the two
                    // steps can be measured independently.
                    const char *arg = s_line + 9;
                    long n = (*arg == ' ') ? strtol(arg + 1, NULL, 10) : -1;
                    if (n < 0 || n > 127) {
                        pl_log("debug-remote: VOL FUSET requires 0..127, got \"%s\"\r\n", s_line + 10);
                    } else {
                        int16_t raw = (int16_t)(n * 100 - 12700);
                        pl_usb_audio_fu_set_volume(0, raw);
                        pl_log("debug-remote: VOL FUSET %ld -> fu_volume[0]=%d\r\n", n, (int)raw);
                    }
                } else if (strcmp(s_line, "VOL INT") == 0) {
                    // Bead pico-link-2ue (VT4a): sends ONE UAC2
                    // feature-unit-volume-changed status packet on the AC
                    // interrupt endpoint (design doc sec 6, mechanism M1)
                    // and reports whether TinyUSB accepted/confirmed the
                    // send. Whether macOS's OWN slider then moves, or it
                    // follows up with a GET (pl_usb_audio_fu_get_calls()),
                    // is observed externally (AppleScript/afplay + a "VOL
                    // GET" afterward) -- this command only proves the
                    // packet left the device.
                    // Published via pl_prio.h's slot 4 (reusing log_vol_snapshot's
                    // slot -- both are one-shot VOL-family measurement commands
                    // from this same locked context, never concurrent), NOT
                    // pl_log(), for the same reliability reason as VOL GET/WATCH
                    // above: this result must survive this firmware's
                    // background log-ring congestion.
                    bool accepted = pl_usb_audio_send_fu_status_interrupt();
                    pl_prio_publish(
                        4,
                        "vol-int accepted=%d sent=%lu done=%lu",
                        (int)accepted,
                        (unsigned long)pl_usb_audio_int_sent(),
                        (unsigned long)pl_usb_audio_int_done()
                    );
                } else if (strncmp(s_line, "VOL HOSTUP", 10) == 0 || strncmp(s_line, "VOL HOSTDOWN", 12) == 0) {
                    // Bead pico-link-4v2.1 (VT1): pushes n HID Consumer
                    // Volume Increment/Decrement taps through the
                    // already-proven media_keys.c ring -- answers question
                    // (iii)/(iv), whether mechanism M2 moves macOS's own
                    // slider and whether macOS then writes our FU back in
                    // response (design doc sec 6, sec 9 T1).
                    bool up = (strncmp(s_line, "VOL HOSTUP", 10) == 0);
                    const char *arg = s_line + (up ? 10 : 12);
                    long n = (*arg == ' ') ? strtol(arg + 1, NULL, 10) : 1;
                    if (n < 1) {
                        n = 1;
                    }
                    if (n > 100) {
                        n = 100; // guard against a typo flooding the HID ring
                    }
                    pl_log("debug-remote: VOL %s %ld -> dispatched\r\n", up ? "HOSTUP" : "HOSTDOWN", n);
                    uint16_t usage = up ? PL_MEDIA_KEY_USAGE_VOLUME_INCREMENT : PL_MEDIA_KEY_USAGE_VOLUME_DECREMENT;
                    for (long i = 0; i < n; i++) {
                        pl_media_keys_push_tap(usage);
                    }
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
