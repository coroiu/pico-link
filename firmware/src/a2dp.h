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

// Initiates an A2DP source connection to `addr` -- wraps
// a2dp_source_establish_stream() and pushes
// Event::ConnectStepChanged(SettingUpAudio). Called from bt.c's
// PL_COMMAND_TAG_CONNECT handler, thread context (the superloop, via
// pl_bt_poll_commands) -- a2dp_source_establish_stream() itself is safe to
// call from thread context, matching a2dp_source_demo.c's own call sites
// (both its GAP_EVENT_INQUIRY_RESULT handler in IRQ context and its
// stdin_process command handler in thread context call it directly).
void pl_a2dp_connect(const uint8_t *addr);

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
void pl_a2dp_report(uint32_t report_dt_us);

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

#endif // PL_A2DP_H
