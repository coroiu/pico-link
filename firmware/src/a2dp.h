// Pico Link firmware -- M4 S1: A2DP source (bead pico-link-cz0.5.2, design
// .planning/design/2026-08-29-a2dp-source-pipeline.md). This header is the
// seam bt.c/main.c use; everything AVDTP/A2DP/AVRCP-specific stays inside
// a2dp.c.
//
// Why AVRCP is plumbed even though S1 doesn't act on transport controls:
// design sec 9 sizes MAX_NR_L2CAP_CHANNELS/MAX_NR_L2CAP_SERVICES for
// "AVDTP signalling + AVDTP media + AVRCP + SDP" and calls out "four
// service records" -- matching a2dp_source_demo.c's own SDP setup (A2DP
// Source, AVRCP Target, AVRCP Controller, Device ID). Many real sinks
// (headphones) open an AVRCP channel unprompted right after A2DP connects;
// registering the service (even with handlers that only log, not act on
// play/pause/volume) avoids that channel establishment failing against an
// unregistered PSM on first real-hardware pairing. Acting on AVRCP
// transport controls is out of scope for S1 -- deferred, not implemented.
#ifndef PL_A2DP_H
#define PL_A2DP_H

#include "btstack.h"

#include "pico_link_ui.h"

// Registers A2DP Source + AVRCP Target/Controller + Device ID SDP records,
// creates one AVDTP stream endpoint per codec_table.c row (S1: SBC only),
// and sets the class of device to 0x200408 (Audio/Video, Rendering --
// matches a2dp_source_demo.c). `ui` is retained (not copied) for the
// lifetime of the firmware -- every A2DP/AVRCP callback after this call
// pushes state into it via bt.c's event ring (pl_bt_push_connect_step/
// pl_bt_push_connect_succeeded, see bt.h).
//
// Must be called once, after cyw43_arch_init() has succeeded and BEFORE
// hci_power_control(HCI_POWER_ON) (i.e. from inside pl_bt_init(), before
// its own hci_power_control call) -- SDP/AVDTP/AVRCP registration has to
// be in place before the radio powers on and BTstack starts accepting
// signalling from a peer.
void pl_a2dp_init(struct PlUi *ui);

#ifdef PL_ENCODER_ON_CORE1
// Bead pico-link-nli.4 (G3, epic pico-link-nli): launches core1 into the
// LDAC encoder loop (a2dp.c's CORE1 section). Call once, from main.c, after
// cyw43_arch_init()/pl_bt_init() have succeeded (design sec 8) -- core1
// then runs forever; there is no corresponding "stop" call (design sec
// 4.2). Behind PL_ENCODER_ON_CORE1 so a build with this flag off never
// links pico_multicore's launch path or touches core1 at all -- the
// single-core behaviour this epic started from is one CMake flag away.
void pl_a2dp_launch_core1(void);

// Diagnostics: how many times core0's bounded quiesce wait
// (a stream-teardown handshake with core1) actually timed out instead of
// observing core1 park in time. Zero in a healthy run -- see
// a2dp.c's s_enc_quiesce_timeouts doc comment for the full mechanism.
uint32_t pl_a2dp_encoder_quiesce_timeouts(void);
#endif

// Initiates an A2DP source connection to `addr` -- wraps
// a2dp_source_establish_stream() and pushes
// Event::ConnectStepChanged(SettingUpAudio). Called from bt.c's
// PL_COMMAND_TAG_CONNECT handler, thread context (the superloop, via
// pl_bt_poll_commands) -- a2dp_source_establish_stream() itself is safe to
// call from thread context, matching a2dp_source_demo.c's own call sites
// (both its GAP_EVENT_INQUIRY_RESULT handler in IRQ context and its
// stdin_process command handler in thread context call it directly).
void pl_a2dp_connect(const uint8_t *addr);

// Tears down the current A2DP source connection, if any -- wraps
// a2dp_source_disconnect(s_ctx.a2dp_cid). No-ops (logs only) when
// a2dp_cid is 0, i.e. there is no active connection to tear down. Debug-only
// entry point (bead pico-link-nb6): called from bt.c's
// PL_BT_PENDING_DISCONNECT case in pl_bt_pending_service, IRQ context --
// a2dp_source_disconnect() is safe to call there, matching every other
// a2dp_source_* call already made from that same deferred-queue consumer's
// context class (see pl_a2dp_connect's doc comment on thread-context calls;
// this one runs on the IRQ side of the same queue instead). Not reachable
// from the product-facing FFI yet -- that is pico-link-44w, deliberately out
// of scope here.
void pl_a2dp_disconnect(void);

// Bead pico-link-cz0.6 (M5 persistence): true whenever the media pipeline is
// PRIMING or STREAMING (i.e. not IDLE) -- part of persist.c's "NO flash
// write while streaming" gate alongside pl_usb_audio_streaming(). PRIMING is
// included deliberately, not just STREAMING: it is the run-up to a stream
// actually starting, and a flash blackout during it risks the same missed
// ISO-OUT re-arm STREAMING itself must avoid. Thread-context safe to call
// (reads one enum field, no BTstack call).
bool pl_a2dp_streaming(void);

// Once-per-second instrumentation snapshot -- design sec 7's
// "a2dp: codec=... bitrate=... fill=... ovr_frames=... und=... enc_max_us=...
// pkt_sent=... pkt_fail=... misaligned=..." report line. Call from the
// superloop, thread context (this function itself does no BTstack calls,
// only pl_log and plain counter reads).
//
// Bead pico-link-okx (D11): report_dt_us is the real elapsed microseconds
// since the previous call, computed ONCE in main.c's superloop and shared
// with pl_usb_pump_report -- this function no longer rate-limits itself;
// the caller decides when a second has elapsed. Pass 0 on the very first
// call. See pl_usb_pump_report's doc comment (usb_pump.h) for why this
// isn't cosmetic.
//
// Bead pico-link-9eq2.3.2, design §5.6.5: fault_fill_min_bytes is fault.c's
// CACHED value of usb_audio.c's destructive-on-read pl_usb_audio_fill_min()
// -- fault.c is now that accessor's sole caller (a second reader would
// steal windows from the first, producing garbage for both), so this
// function's own former direct call is replaced by this parameter. Call
// site: main.c, which reads pl_fault_last_fill_min() (fault.c calls
// pl_usb_audio_fill_min() itself, once, earlier in the same 1Hz block) and
// passes the result straight through -- see fault.h's module doc for why
// fault.h itself is not included here.
void pl_a2dp_report(uint32_t report_dt_us, uint32_t fault_fill_min_bytes);

// Bead pico-link-auh, section 1: publishes slot 0 ("ctr") of the
// non-starvable priority channel (pl_prio.h) from s_ctx's cumulative
// counters -- tick_count, enc_frames_total, pkt_sent, stop_dwell,
// stop_credit, ovr_frames -- plus a monotonically-incrementing seq and
// uptime_ms. This is the LDAC PL_A2DP_MAX_ENCODE_DWELL_US (a2dp.c:149)
// falsifier's data source: unlike the verbose "a2dp:" lines pl_a2dp_report
// above emits (which pl_log_ring_drain() may shed under load), this slot
// is overwrite-in-place and cannot be dropped -- see pl_prio.h's module
// doc. Call once a second, at the same shared-report point as
// pl_a2dp_report() (main.c). Does NOT replace pl_a2dp_report()'s verbose
// lines -- both stay.
void pl_a2dp_publish_counters(void);

// Bead pico-link-nli.5 (G4, design sec 5): reads the seqlock snapshot
// pl_a2dp_publish_levels() (a2dp.c) writes, and if a new sample has landed
// since the last call, pushes Event::LevelsChanged through `ui` directly
// (thread context, no ring). Call once per superloop iteration, AFTER
// pl_ui_tick(ui, frame_start_us) -- see pl_a2dp_poll_levels's own doc
// comment in a2dp.c for why the ordering matters (it's what closes
// pico-link-8b7's staleness-at-birth bug at the root). A no-op, cheap and
// safe to call even before any stream has ever started (the seqlock's `0`
// sentinel is read as "nothing yet").
void pl_a2dp_poll_levels(struct PlUi *ui);

// Bead pico-link-7jol.5, design `.planning/design/2026-09-07-ldac-quality-
// selector.md` §6: pushes Event::LdacBitrateChanged through `ui` whenever
// the connected codec is LDAC and codec_ldac.c's live-kbps cache has
// changed since the last push (same push-only-on-change discipline as
// pl_a2dp_poll_levels, no seqlock needed here since the cache is one
// volatile word). Call once per superloop iteration, anywhere after
// pl_ui_tick -- order relative to pl_a2dp_poll_levels does not matter,
// this event carries no clock-dependent field. A no-op while the
// connected codec isn't LDAC (and resets its own dedupe state then, so a
// later LDAC session's first reading is never suppressed as "unchanged").
void pl_a2dp_poll_ldac_bitrate(struct PlUi *ui);

// Bead pico-link-7jol.5: is `addr` the currently connected device AND is
// LDAC its live codec? Gates bt.c's SET_DEVICE_LDAC_QUALITY command
// handler's live-apply path (pl_codec_ldac_pin_now) -- a pick for a
// different device, or for the connected device while it's actually
// fallen back to SBC, only stages the flash write; it must not reach for
// the LDAC encoder at all. Thread-context safe to call (reads two plain
// fields, no BTstack call, same class as pl_a2dp_streaming above).
bool pl_a2dp_is_connected_ldac(const uint8_t addr[6]);

// T3 (pico-link-4v2.3), design sec 9 (host -> headphones over AVRCP):
// consumes volume.c's outbound AVRCP latch and, if a connection exists and
// no SET_ABSOLUTE_VOLUME is already awaiting a response, sends one via
// avrcp_controller_set_absolute_volume. Call once per
// pl_bt_wdt_heartbeat_handler tick (bt.c) -- MUST run on the cyw43
// background IRQ (0xFF), the only context permitted to call BTstack
// (pico-link-ouw); do not call this from the superloop. `now_us` is the
// caller's time_us_64(), used for the in-flight timeout.
void pl_a2dp_avrcp_volume_service(uint64_t now_us);

// Raw wire values of ui-ffi's PlConnectStep enum (Connecting=0, Pairing=1,
// SettingUpAudio=2, NegotiatingCodec=3 -- see ui-ffi/src/lib.rs). Not
// emitted by cbindgen into pico_link_ui.h because no FFI struct field is
// typed as PlConnectStep itself, only as a plain u32 (see
// PlConnectStepChangedPayload's doc comment there) -- so this project
// defines its own constants matching those discriminants exactly, same
// convention as every other PlXxx enum crossing this FFI (pico-link-ptu).
#define PL_CONNECT_STEP_CONNECTING 0u
#define PL_CONNECT_STEP_PAIRING 1u
#define PL_CONNECT_STEP_SETTING_UP_AUDIO 2u
#define PL_CONNECT_STEP_NEGOTIATING_CODEC 3u

// --- Bead pico-link-9eq2.3.2, design `.planning/design/2026-09-07-audio-
// fault-model.md` §7.2: the seam fault.c evaluates against. Plain getters
// over s_ctx fields, same convention as pl_a2dp_streaming/pl_a2dp_is_
// connected_ldac above -- a single aligned-word read, thread-context safe,
// no locking. fault.h is the ONLY consumer; nothing else in the tree
// should call these. ---

// The exact PL_A2DP_MEDIA_STREAMING condition -- NOT pl_a2dp_streaming()
// above, which is also true during PRIMING. Faults are meaningless before
// real drain begins (design §5.6.1).
bool pl_a2dp_media_streaming(void);

// The media-timer handler's own host_silent latch, stashed every tick --
// design §5.6.2 ("a paused host must not be reported as under-supplying").
bool pl_a2dp_host_silent(void);

uint32_t pl_a2dp_underrun_events(void);
// Bead pico-link-rzqd: cumulative microseconds in COMPLETED starvation
// episodes -- see the underrun_events/starved_us doc comments in a2dp.c.
uint32_t pl_a2dp_starved_us(void);
uint32_t pl_a2dp_resync_events(void);
uint32_t pl_a2dp_resync_drops(void);
uint32_t pl_a2dp_stop_queue_full(void);
uint32_t pl_a2dp_dwell_max_us(void);
uint32_t pl_a2dp_link_lost_events(void);
uint32_t pl_a2dp_stop_dwell(void);
uint32_t pl_a2dp_credit_clamp_events(void);

#ifdef PL_DEBUG_REMOTE
// Bead pico-link-fhf, test A (injection). One-shot: the NEXT `ticks` calls
// to the media timer handler skip pl_a2dp_fill()'s drain entirely, so the
// ring gains fill at the full 192 B/ms rate for that duration with no
// restoring force -- proving the hysteresis-banded resync trim actually
// fires (a soak alone can pass by doing nothing; see the bead's design
// comment sec 5). Consumed at the top of the drain step in the media-timer
// IRQ handler, same consumer context pl_pcm_trim_to() itself requires --
// no SPSC violation, no push from the wrong side. Thread-context caller
// only (debug_remote.c's poll, superloop). Compiled only when
// PL_DEBUG_REMOTE is set; entirely absent from a shipping build.
void pl_a2dp_debug_skip_media_ticks(uint32_t ticks);

// Bead pico-link-8pp1.1, design sec 5: switches the resync trim's hold/
// hard-band policy live, no reflash -- see a2dp.c's s_trim_hold_ms/
// s_trim_hard_band_bytes doc comment for the single-writer discipline and
// default-preserving clamp. hold_ms/hard_band_ms are both in milliseconds
// (the setter converts hard_band_ms to bytes internally). Thread-context
// caller only (debug_remote.c's poll, superloop). Compiled only when
// PL_DEBUG_REMOTE is set.
void pl_a2dp_debug_set_trim_policy(uint32_t hold_ms, uint32_t hard_band_ms);

// Reads back the policy pl_a2dp_debug_set_trim_policy last set (or the
// compiled-in default, bit-identical to pre-8pp1.1 behaviour, if it was
// never called) -- both units milliseconds, same as the setter. Thread-
// context caller only, same contract as the setter above.
void pl_a2dp_debug_trim_policy(uint32_t *hold_ms, uint32_t *hard_band_ms);
#endif

#endif // PL_A2DP_H
