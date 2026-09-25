# libldac provenance

- **Source:** `https://android.googlesource.com/platform/external/libldac`
  (AOSP's mirror of Sony's own LDAC encoder release; Sony is the copyright
  holder per `NOTICE`/`LICENSE` in this directory).
- **Commit:** `eeee1a3f5f8df1282e3a6d297085885fd886737b` (2025-01-10).
- **License:** Apache License 2.0 (`LICENSE`, `NOTICE` in this directory,
  copied verbatim from upstream).
- **What was vendored:** encoder only.
  - `inc/ldacBT.h` — the public encoder API.
  - `src/*.c`, `src/*.h` — every file under upstream's `src/`.
  - **NOT vendored:** upstream's `abr/` (adaptive bitrate — not used by this
    firmware) and `fuzzer/` (OSS-Fuzz harness — not relevant here).
- **Compiled translation units:** only `src/ldaclib.c` and `src/ldacBT.c`,
  matching upstream's own `Android.bp` (`libldacBT_enc` target). Every other
  `.c` file under `src/` is pulled in via `#include` from those two files
  (see `ldaclib.c`'s own `#include` block, and `ldacBT.c`'s two `#include`
  lines) — they are not separate compilation units and must not be added to
  `CMakeLists.txt`'s source list.

## Bead pico-link-cz0.5.4 (LDAC L0) — relation to USBPods

USBPods (`github.com/wasdwasd0105/USBPods-Pico2W`, GPL-3) was read elsewhere
in this project as an architectural reference, but **no byte of it went into
this vendoring**. This is a direct, clean pull from Sony's own AOSP-hosted
upstream. Per `.planning/decisions/` project convention, a reviewer diffing
this directory against the merge base should find only files that trace back
to the AOSP commit above.

## Arithmetic mode (read AND measured on the object files — corrected after
an initial grep-only pass missed this)

`SCALAR` (`struct_ldac.h:50`) is `typedef float SCALAR` in the default
build, and no `double`-typed *variable* exists anywhere in the compiled
path (`struct_ldac.h`'s `#define PI (double)(3.14159265358979323846)` is
dead code — zero references in any `.c` file). **But there IS real
double-precision arithmetic in the compiled encoder path**: `src/sigana_ldac.c`
(`calc_mdct_pseudo_spectrum_ldac()`, 4 call sites) calls the bare C library
`sqrt()` — not `sqrtf()` — on a `SCALAR`/`float` argument, once per spectral
sample, in a loop that runs across the whole spectrum every frame. C's usual
arithmetic conversions promote the `float` argument to `double` for that
call, execute `sqrt()` at double precision, then convert the `double` result
back down to store into a `float` array.

Confirmed on the actual cross-compiled object file, not just by reading
source: `arm-none-eabi-nm -u vendor/libldac's ldaclib.c.o` shows undefined
references to `__aeabi_f2d` (float→double promotion), `__aeabi_d2f`
(double→float demotion), and plain `sqrt` (not `sqrtf`) — all absent from
`ldacBT.c.o`. Cortex-M33's FPU is single-precision only, so this `sqrt()`
call is fully software-emulated at double precision — promote, software
sqrt, demote — on every one of its ~128 calls/frame. This is very likely a
meaningful fraction of the per-frame cost and should be read as the leading
optimization target (`sqrtf()` is a trivial one-character fix upstream would
almost certainly accept, or a local patch) if L0's measured µs/frame is
tight against the ~1400 µs budget. Single-precision float arithmetic
elsewhere is hardware-accelerated on this board's FPU
(`thumb/v8-m.main+fp/softfp`, see `firmware/CMakeLists.txt:42-51`); this
`sqrt()` site is the one place that isn't.

**Fixed 2026-08-31 (bead pico-link-cz0.5.8):** all four call sites in
`src/sigana_ldac.c` (lines 44, 53, 61, 66) now call `sqrtf()` instead of
`sqrt()`. This is a local patch to vendored source — the file's Apache-2.0
header and upstream attribution are unchanged, only the four call sites
differ from upstream. With this fix, the compiled encoder path (default
float build, `_32BIT_FIXED_POINT` undefined) has **no remaining
double-precision arithmetic**: `arm-none-eabi-nm -u` on the rebuilt
`ldaclib.c.o` should no longer reference `__aeabi_f2d`/`__aeabi_d2f`/`sqrt`
for this reason (re-verify if this file changes again). See
`pico-link-cz0.5.8`'s L0 bench re-run for the measured saving.

## Adaptive-bitrate floor rail (bead pico-link-d42g, 2026-09-25)

`src/ldacBT_internal.h`'s `LDACBT_LIMIT_ALTER_EQMID_PRIORITY` is a local
patch, raised from `LDACBT_EQMID_MQ` to `LDACBT_EQMID_Q5`. Its only user is
`ldacBT_get_altered_eqmid()` (`ldacBT_internal.c`), which is the sole path
ABR uses to alter the live encoding rate — so this rail previously refused
to step ABR below MQ (330 kbps) no matter what our firmware asked for. It
now allows Q5 (198 kbps), matching the lowest rung our own adaptive ladder
offers (`firmware/src/codec_ldac.h`'s `PL_LDAC_ADAPTIVE_LADDER_RUNGS`). This
is a second, independent limit behind our own `codec_ldac.c` floor clamp —
`ldacBT_set_eqmid()`/`ldacBT_assert_eqmid()` (used for user pins) are
untouched and still refuse anything below MQ. See
`.planning/design/2026-09-25-adaptive-floor.md` section 3.

A `_32BIT_FIXED_POINT` build macro does exist (gates `mdct_fixp_ldac.c`,
`sigana_fixp_ldac.c`, `quant_fixp_ldac.c`, etc. in `src/ldaclib.c`'s
`#include` block) — upstream's own `Android.bp` documents it as "for devices
without a FPU unit such as ARM Cortex-R series". This board has a Cortex-M33
with a hardware single-precision FPU, so it is deliberately **not** defined
here; the default float path is what is vendored and benchmarked.

## Heap allocation (read, not assumed)

- `ldacBT_get_handle()` → `malloc(sizeof(STRUCT_LDACBT_HANDLE))`
  (`src/ldacBT_api.c:33`).
- `ldacBT_init_handle_encode()` → `ldaclib_get_handle()` →
  `malloc(sizeof(HANDLE_LDAC_STRUCT))` (`src/ldaclib_api.c:298`), then
  `alloc_encode_ldac()` (`src/encode_ldac.c:22`) calls `calloc_ldac()` for
  the per-channel `AC`/`ACSUB`/`AB` structures.
- The per-frame encode function, `encode_audio_block_ldac()`
  (`src/encode_ldac.c:173`), contains **no** `malloc`/`calloc`/`free` call.
  All allocation is at handle-init time (thread context, once per stream),
  matching `codec_table.h`'s encode contract (no allocation from
  `encode()`). Confirmed on-target by the L0 benchmark (heap watermark
  taken before/after `ldacBT_get_handle()`+`init_handle_encode()`, and again
  before/after the encode loop — see `main.c`'s `PL_DIAG_LDAC_BENCH` block).
