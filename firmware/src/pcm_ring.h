// Pico Link firmware -- the USB <-> Bluetooth PCM seam (bead pico-link-cz0.5.1,
// M4 design .planning/design/2026-08-29-a2dp-source-pipeline.md sec 3).
//
// Producer: usb_audio.c's pl_usb_audio_task(), which runs inside usb_pump.c's
// 0xC0 worker IRQ. Consumer: the coming A2DP module's media timer, in the
// cyw43/BTstack background IRQ (PICO_LOWEST_IRQ_PRIORITY, 0xFF).
//
// SPSC and lock-free: each side writes only its own index (s_head / s_tail),
// both indices are single aligned 32-bit words, so a 0xC0 preemption of the
// 0xFF consumer mid-read is safe. Same discipline input.c uses -- NOT bt.c's,
// which needed a critical section only because it has two producers.
//
// Owned by neither side of the seam it crosses: this module must not
// #include usb_pump.h or any A2DP header. usb_audio.c and the future a2dp.c
// both #include this file directly.
//
// SRAM, not PSRAM. The producer runs against a hard ~1ms re-arm deadline
// that has already wedged this board once (see usb_pump.h's module doc); an
// IRQ-context write through PSRAM's QMI/XIP path has cache- and
// arbitration-dependent latency that is not a trade worth making to save
// 32KB of a 520KB SRAM budget. See design sec 3.4.
#ifndef PICO_LINK_PCM_RING_H
#define PICO_LINK_PCM_RING_H

#include <stdint.h>

#define PL_PCM_FRAME_BYTES 4u // 16-bit stereo: 2 channels x 2 bytes

// Power of two; 8192 frames = 170.7ms at 48kHz/16-bit/stereo (192 B/ms).
// Sized for target fill (below) plus transient headroom -- see design sec
// 3.3. SRAM cost (32KiB) is negligible against the 520KB budget.
#define PL_PCM_RING_CAPACITY (32u * 1024u)

// STARTING VALUE, not a measured one -- M4 stage S3 measures and tunes this.
// 1152 frames = 24ms: covers one A2DP media packet (~13.3ms of SBC) plus
// radio scheduling jitter. See design sec 3.3/3.5.
#define PL_PCM_TARGET_FILL_BYTES 4608u

// Appends whole frames only, producer side. `len` MUST be a multiple of
// PL_PCM_FRAME_BYTES -- a misaligned length is rejected WHOLESALE and
// counted via pl_pcm_misaligned(), never partially accepted (a partial
// accept would itself desync channel phase, the exact bug this module
// exists to prevent).
//
// On overflow, drops the NEWEST whole frames from this call and counts them
// via pl_pcm_overrun_frames(). The producer owns only `head` and NEVER
// writes `tail` -- advancing `tail` to drop-oldest would be a cross-index
// write into the consumer's variable, which is forbidden by the SPSC
// contract above.
//
// Call only from the 0xC0 worker IRQ (or another context proven to be the
// sole producer) -- never from thread context concurrently with the worker.
void pl_pcm_push(const uint8_t *data, uint32_t len);

// Consumer-side drain. Writes at most `max` bytes into `out`, always a
// whole number of frames (rounds `max` down to a frame boundary first), and
// returns how many bytes were written. Call only from the consumer context
// (the coming A2DP media timer) -- never concurrently with itself.
uint32_t pl_pcm_read(uint8_t *out, uint32_t max);

// Current fill level in bytes. Safe to call from either side or from
// thread-context instrumentation; it is a snapshot, not synchronized
// against concurrent producer/consumer activity.
uint32_t pl_pcm_fill_bytes(void);

// Drops all buffered PCM by moving the consumer's tail up to the producer's
// current head. Consumer side only -- touches only `tail`, per the SPSC
// ownership rule above. For stream open/close (design sec 3.5's "resume"
// priming and "host silent" reset), not for routine drain.
void pl_pcm_reset(void);

// Cumulative whole frames dropped on overflow since boot. Design sec 7:
// once the feedback loop works this should be permanently 0 -- any nonzero
// reading is a bug signal, not a tuning signal.
uint32_t pl_pcm_overrun_frames(void);

// Cumulative pl_pcm_push() calls rejected wholesale for a non-frame-multiple
// `len`. Should be permanently 0; nonzero means an ISO packet arrived with a
// length TinyUSB's audio class driver should never produce.
uint32_t pl_pcm_misaligned(void);

#endif // PICO_LINK_PCM_RING_H
