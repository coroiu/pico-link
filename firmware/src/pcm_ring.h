// Pico Link firmware -- the USB <-> Bluetooth PCM seam (bead pico-link-cz0.5.1,
// M4 design .planning/design/2026-08-29-a2dp-source-pipeline.md sec 3).
//
// Producer: usb_audio.c's pl_usb_audio_task(), which runs inside usb_pump.c's
// 0xC0 worker IRQ, always on core0. Consumer: the A2DP fill loop -- today
// still core0's media timer in the cyw43/BTstack background IRQ
// (PICO_LOWEST_IRQ_PRIORITY, 0xFF); as of the `pico-link-nli` epic (G3) it
// moves to core1's thread-context encoder loop. See §3.1 of
// .planning/decisions/2026-09-03-ldac-encoder-on-core1.md.
//
// SPSC and lock-free: each side writes only its own index (s_head / s_tail),
// both indices are single aligned 32-bit words. Historically (single-core)
// that alone was sufficient, because the only ordering hazard was a 0xC0
// preemption of the 0xFF consumer mid-read on the SAME core, which an
// IRQ-nesting argument covers. **That argument does not survive core1: the
// two sides can now run on genuinely different cores, executing
// simultaneously rather than nested, so interrupt priority protects
// nothing.** RP2350's SRAM is coherent across both M33s through the bus
// fabric (no data cache; the XIP cache is flash-only), but store *ordering*
// is not free -- a `__dmb()` on each side of the seam (see pcm_ring.c) makes
// the data write visible before the index publish that hands it off, and
// the index read visible before the data read that trusts it. Same
// discipline input.c uses -- NOT bt.c's, which needed a critical section
// only because it has two producers.
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

// Default/floor target fill. Derived (pico-link-fhf, Ada 2026-08-30) from
// the two gaps the ring must absorb between drain (credit-paced, exactly
// 192 B/ms, codec-independent) and supply: worst media-tick interval
// (~11.3ms real cadence, budget 20ms = 3840B) plus worst 0xC0 producer gap
// (~1.03ms measured, pico-link-tfj, budget 5ms = 960B). 3840 + 960 = 4800B
// floor, +20% safety = 5760B = 1440 frames = 30ms. Codec-independent because
// it is keyed to the tick budget, not packet size -- even a 256-sample LDAC
// frame (1024B) is far below the 3840B tick term. This macro is the
// compile-time initial value only; the runtime setpoint
// (pl_pcm_set_target_fill_bytes/pl_pcm_target_fill_bytes below) is what
// a2dp.c, usb_audio.c and the resync trim actually read once
// STREAM_ESTABLISHED has set it from the priming-derived cushion. See
// .planning/design/2026-08-30-pcm-pacing.md and the bead's design comment
// (pico-link-fhf).
#define PL_PCM_TARGET_FILL_BYTES 5760u

// Hysteresis-banded discrete resync (pico-link-fhf). Half-width 2880B
// (720 frames, 15ms): trip when the EMA (never raw fill) exceeds
// target + this band. Sized to ~11x the worst-case EMA ripple from the tick
// sawtooth (~110B), so the controller's ~96 B/s of authority (500ppm at
// 192 B/ms) is given ~30s to clear an offset silently before a trim cuts it
// audibly. See the bead's design comment sec 2.
#define PL_PCM_TRIM_BAND_BYTES 2880u

// Minimum interval between trims: 2s, 6x the EMA's ~320ms settling time
// after a reseed, and negligible against the controller's ~30s clearing
// horizon. Safe only because a total drain stall is separately caught by
// PL_WDT_MEDIA -- this lockout is not overflow protection. See the bead's
// design comment sec 3.
#define PL_PCM_TRIM_MIN_INTERVAL_US 2000000u

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
//
// Bead pico-link-pbv (C2-6): returns the number of whole frames dropped, so
// callers can accumulate a counted flush_frames total -- without this the
// drain-vs-supply conservation check (bead's acceptance A1) cannot be
// balanced, since every silent discard is otherwise an uncounted exit from
// the ring.
uint32_t pl_pcm_reset(void);

// Cumulative whole frames dropped on overflow since boot. Design sec 7:
// once the feedback loop works this should be permanently 0 -- any nonzero
// reading is a bug signal, not a tuning signal.
uint32_t pl_pcm_overrun_frames(void);

// Cumulative pl_pcm_push() calls rejected wholesale for a non-frame-multiple
// `len`. Should be permanently 0; nonzero means an ISO packet arrived with a
// length TinyUSB's audio class driver should never produce.
uint32_t pl_pcm_misaligned(void);

// Bead pico-link-pbv (C5): trims the ring down to AT MOST target_bytes
// fill by advancing tail forward, dropping the OLDEST excess whole frames
// -- unlike pl_pcm_push()'s overrun policy (drop newest, producer side),
// this is a deliberate consumer-side resync of already-buffered audio down
// to a known-good latency, e.g. at the PRIMING to STREAMING transition
// where a few extra USB packets can land between the fill>=target check
// and the transition itself. target_bytes is rounded down to a frame
// boundary. Returns the number of whole frames dropped (0 if fill was
// already <= target_bytes -- the common case). Consumer side only, same
// ownership rule as pl_pcm_reset().
uint32_t pl_pcm_trim_to(uint32_t target_bytes);

// Bead pico-link-fhf (orchestrator decision on Ada's design sec 7): the
// runtime target-fill setpoint, initialised to PL_PCM_TARGET_FILL_BYTES.
// Lives HERE, in pcm_ring, deliberately -- not in a2dp.h -- even though
// a2dp.c already includes usb_audio.h and putting it there would compile.
// pcm_ring is included by both a2dp.c and usb_audio.c and owned by
// neither, which is the point: a2dp.c is the only writer (it sets this
// from the priming-derived cushion at STREAM_ESTABLISHED, replacing the
// bare macro that used to disagree with the priming target), and
// usb_audio.c's feedback loop plus a2dp.c's own resync trim block are both
// readers. Putting the setpoint in a2dp.h would point the module
// dependency the wrong way and hand the a2dp layer ownership of a
// quantity that belongs to neither side of the seam.
void pl_pcm_set_target_fill_bytes(uint32_t target_bytes);
uint32_t pl_pcm_target_fill_bytes(void);

#endif // PICO_LINK_PCM_RING_H
