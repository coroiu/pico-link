# LDAC Adaptive floor (bead pico-link-d42g)

Status: Live design, 2026-09-25, Ada. Andreas approved the shape the same day:
a global setting that caps how far ABR may step down, with values 330
(default) / 246 / 198 kbps. It is not a set of extra pinned picks. Uma owns
the name and where it sits in the UI.

## 0. Facts (read, not assumed)

- libldac EQMID table order (`firmware/vendor/libldac/src/ldacBT_internal.c:31-43`),
  48 kHz: HQ 990, SQ 660, Q0 492, Q1 396, MQ 330, Q2 282, Q3 246, Q4 216,
  Q5 198. Our rung is the table index (`codec_ldac.c:317` maps MQ to rung 4).
  Floors: **330 = rung 4, 246 = rung 6, 198 = rung 8**.
- `LDACBT_LIMIT_ALTER_EQMID_PRIORITY` (`ldacBT_internal.h:34`) has exactly one
  user, `ldacBT_get_altered_eqmid` (`ldacBT_internal.c:474-478`).
  `ldacBT_assert_eqmid` (`:196-203`) separately limits `ldacBT_set_eqmid` to
  HQ/SQ/MQ. We never `set_eqmid` below MQ: init starts Adaptive at HQ, and
  every step and pin goes through `alter_eqmid_priority`.
- Readback works at every rung. `ldacBT_frmlen_to_bitrate` is generic, so
  `live_kbps`/`LdacBitrateChanged` report 246/198 honestly. `quality_applied`
  is the rung 0..8. Core renders any kbps. The pin picker's
  `[990, 660, 330]` (`core/src/app/screens/ldac_quality.rs:36`) stays.
- **Latent bug that becomes live below MQ.** The frames-per-packet hint
  (`codec_ldac.c:532-539`, modelled in `firmware/tests/test_ldac_frames_per_packet.c`)
  computes `679 / (kbps/3 + 3)`. That double-counts the 3-byte frame header
  and ignores libldac's `tx_size = 679 - 18 = 661` (`ldacBT_api.c:166-170`).
  libldac's real count is `661 / (kbps/3)`, which equals the NFRM column of
  `tbl_ldacbt_config`. The two agree from HQ to MQ only by coincidence: Q3 is
  7 vs 8, Q5 is 9 vs 10.
- Priming target = `max(one_packet + 192 + jitter, PL_PCM_TARGET_FILL_BYTES 5760)`
  (`a2dp.c:3658-3664`). At Q5 one_packet = 10 x 128 x 4 = 5120 B, which only
  exceeds 5760 when the worst tick interval is above about 2.3 ms (measured
  about 1 ms). Sizing for Q5 therefore costs about 0 ms of latency in practice.

## 1. Global vs per-device: GLOBAL

- Same reasoning as Andreas's PL:S:1 cushion ruling: the air varies, not the
  headset.
- The effects-style memory rule says forgetting a device must never lose a
  setting.
- Per-device is already covered: the Adaptive-vs-pinned choice is per-device
  (PL:D `ldac_quality`), and the floor only acts in Adaptive.
- Revisit only if F2 shows one pair breaks below 330. Even then, drop the
  failing value from the offer rather than build a per-device floor, which
  would need a PL:D schema change.
- For Uma: this is a global setting, so place it in Settings (for example
  beside BUFFER). If it appears on a device's Quality picker, the copy must
  say it applies to all devices.

## 2. Persistence: new record PL:S:2

- `PL_PERSIST_KIND_SETTINGS`, index 2 (`PL_PERSIST_INDEX_ABR_FLOOR`), with
  its own `PL_PERSIST_ABR_FLOOR_VERSION 1`. Layout
  `{u8 version; u8 floor; u16 crc16}`, packed, the same shape as PL:S:1.
- Wire byte (core-owned, like `CushionPolicy`): 0 = unset, meaning 330;
  1 = 330; 2 = 246; 3 = 198; anything else is 330. persist.c never
  interprets it. We store a wire enum, not a rung, so the stored format is
  independent of the ladder.
- **No migration.** An absent, wrong-version or bad-CRC record means the
  default 330, logged, and the record is never deleted (as PL:S:1,
  `persist.c:356-388`). No other record's version changes. Older firmware
  never reads the tag. Load it in the same unconditional pre-marker block as
  PL:S:1.
- Write is user-initiated and not streaming-gated (D11). It gets its own
  staging slot plus `PL_BT_PENDING_SET_ABR_FLOOR` plus an execute function
  (same shape as `persist.c:1152-1194`, `bt.c:976-1138`).
- **Debt, named:** this is the third copy of the 1-byte settings-record
  boilerplate.
  - Now: F3 adds static `pl_persist_load_u8_setting(index, version, *out)` /
    `pl_persist_store_u8_setting(index, version, value)` and uses them for
    PL:S:2. Moving PL:S:1 onto them is optional because the bytes are
    identical.
  - A fourth global audio setting should become one combined "audio
    settings" record with a generic latch, not a fourth copy.

## 3. libldac change

- **One line:** `ldacBT_internal.h:34` becomes
  `#define LDACBT_LIMIT_ALTER_EQMID_PRIORITY LDACBT_EQMID_Q5`. It expands at
  the use site, after `ldacBT_ex.h` defines Q5.
  - Comment the line and record it in `vendor/libldac/PROVENANCE.md` as a
    local patch (cz0.5.8 precedent).
- The rail is Q5, not END, so the library itself refuses anything below the
  lowest floor we offer. That is a second, independent limit behind our own
  clamp.
- `PL_LDAC_ADAPTIVE_LADDER_RUNGS` (`codec_ldac.h:57`) goes from 5 to **9**
  and now means the physical ladder. The floor is a separate runtime value.
  Rungs 0..4 are unchanged, so pins, `pl_ldac_quality_to_rung` and the init
  seed do not change. Fix the garbled comment at `codec_ldac.h:52-55` while
  there.
- Frames-per-packet hint (`codec_ldac.c:532-539`):
  - Size Adaptive for Q5, not `330u`, so the connect-time cushion (ABR design
    sec 4.3: never recomputed mid-stream) covers every floor. A live floor
    change then never needs a re-prime.
  - Correct the formula to
    `(PL_LDAC_INIT_MTU - LDACBT_TX_HEADER_SIZE) / (kbps * 1000 / 3000)`,
    clamped to 2..15.
  - The test asserts NFRM 2,3,4,5,6,7,8,9,10 for HQ..Q5. Pinned values are
    unchanged.

## 4. How the floor reaches ABR

codec_ldac.c owns the ladder and a2dp.c owns the decide phase (ABR design
sec 6.3, unchanged).

- **codec_ldac:**
  - Add `static volatile int32_t s_ldac_floor_rung = 4`. With that default,
    behaviour is identical to today.
  - `void pl_codec_ldac_set_floor(uint8_t floor_wire)`:
    - Wire-to-rung mapping lives in ONE function, beside
      `pl_ldac_quality_to_rung`.
    - Store the floor. If Adaptive and target > floor, write target = floor.
    - Callable from any context; it never touches the handle (same class as
      `pl_codec_ldac_pin_now`).
  - `int32_t pl_codec_ldac_floor_rung(void)`.
  - **Single enforcement point:** in `pl_codec_ldac_apply_pending_tuning`
    (`codec_ldac.c:233`), when adaptive and target > floor, clamp target to
    floor.
    - This is race-safe against the decide IRQ: in any write order, the
      encoder never applies past the floor and walks back up if it is
      already beyond it.
    - `at_rail` stays against the physical ladder.
- **a2dp.c decide** (`a2dp.c:2841`):
  - The down-step cap `applied_rung < RUNGS - 1` becomes
    `applied_rung < pl_codec_ldac_floor_rung()`.
  - New floor-hit branch: when `q_ema >= Q_HI && past_settle && applied >= floor`,
    increment `abr_floor_hits` and set `abr_last_step_us = now`. That
    rate-limits it to once per SETTLE, with no q_ema reseed.
  - This counter directly measures this bead's trigger: sitting at the floor
    and still congested.
- Q_HI 4.0, Q_LO 2.0, UP_DWELL 10 s and SETTLE 1 s are untouched, and the
  up-step path (`:2853`) is unchanged.
  - Observation only, for F2: tx-queue depth counts packets, and packets are
    longer below MQ. So Q_HI = 4 packets means more buffered audio at low
    rungs (less eager down-steps there), and Q_LO is easier to meet.
- Report (`a2dp.c:4446`): add `abr_floor_rung= abr_floor_hits=`.

**FFI** (additive, no ABI version bump):

- ui-ffi:
  - `PlAbrFloorPayload { floor: u8 }`.
  - `PL_EVENT_TAG_ABR_FLOOR_LOADED = 20`, `PL_COMMAND_TAG_SET_ABR_FLOOR = 11`.
  - Union members mirror `cushion_policy`.
- core:
  - `AbrFloor` in `core/src/audio.rs` (from_wire/to_wire/label).
  - `Event::AbrFloorLoaded { floor }`.
  - `AbrFloorState { current, save_pending }` beside `CushionPolicyState`
    (`core/src/app/ui_state.rs:156`).
  - `App::abr_floor/request_abr_floor/take_abr_floor_to_save`.
  - `pl_ui_poll_command` drains the latch the same way as the cushion
    (`ui-ffi/src/lib.rs:2651-2660`).
- bt.c: `case PL_COMMAND_TAG_SET_ABR_FLOOR` calls `pl_codec_ldac_set_floor(v)`
  then `pl_persist_request_abr_floor(v)`. It applies live and persists, like
  `bt.c:1402-1413`.
- main.c: boot load, then `pl_codec_ldac_set_floor`, then push
  `ABR_FLOOR_LOADED`, like `main.c:465-478`.

**Mid-stream behaviour:**

| Situation | Effect |
|---|---|
| Floor lowered while Adaptive | Grants permission only. ABR can step past the old floor at the next Q_HI trigger. No re-prime is needed because the cushion is sized for Q5. |
| Floor raised, applied rung past it | The rung walks up to the floor one step per fill (about 10 ms per step), ignoring Q_LO and UP_DWELL. It is a user command, like a pin. |
| Floor raised, applied rung already above it | No change. |
| Device pinned 990/660/330 | Stored, no effect (pins are at most rung 4, floors at least 4). |
| SBC stream or no stream | Stored, applies at the next Adaptive LDAC stream. |
| Reconnect | init resets rungs, never the floor (global, set at boot and on each pick). |

**Debug** (PL_DEBUG_REMOTE, `debug_remote.c` beside `TRIM POLICY`):

- `ABR FLOOR <wire>`.
- `LDAC RUNG <n>`: a debug pin to any rung 0..8 (adaptive = false,
  target = n). It lets the by-ear gate hear 246/198 without having to
  create congestion.

## 5. Implementation beads (one dispatch each)

- **F1 (Ruby, firmware C only):** libldac one-liner plus PROVENANCE.
  - RUNGS 9.
  - `set_floor`/`floor_rung` plus the apply clamp (`codec_ldac.c:233-280`).
  - Decide cap plus `abr_floor_hits` (`a2dp.c:2836-2868`) and report fields
    (`:4446`).
  - Hint fix (`codec_ldac.c:532-539`).
  - Debug commands.
  - Update the models in `tests/test_ldac_abr_controller.c` (:34, :110),
    `test_ldac_frames_per_packet.c` and `test_ldac_pin_rung_seed.c`.
  - Gate: host C tests plus firmware cross-compile; with the default floor,
    decisions are identical to main.
- **F2 (Tess, hardware, after F1, about 10 min, both pairs):**
  - Per pair: `LDAC RUNG 4`, then 6, then 8, about 1 min each on music.
  - Check live_kbps 330/246/198, apply_fail 0, no disconnect, and
    Andreas's ear.
  - Check the Adaptive log `frames_per_packet=10 priming_target_bytes=5760`.
  - Output: go/no-go per value.
- **F3 (Ruby, firmware + ui-ffi + core model, after F1, in parallel with F2):**
  - PL:S:2 plus u8 helpers, staging and pending kind.
  - bt.c handler and main.c boot.
  - FFI tags and payload.
  - `AbrFloor`, `AbrFloorState`, event and latch.
  - No screen.
  - Gate: `cargo test --workspace`, ui-ffi FFI tests mirroring
    `lib.rs:4058-4090`, firmware cross-compile.
- **F4 (Ruby, core UI, after F2, F3 and Uma):** Settings row plus
  single-select picker, offering only the values F2 passed, plus zoomed
  headless screenshots.
- **F5 (Tess, hardware, after F4, short):**
  - Pick in the UI and check for `persist: wrote abr floor`.
  - Reboot, check that it loads, and check `abr_floor_rung` in the report.

## 6. Risks

- Sinks untested below 330 (Android never goes there). F2 gates this, and
  dropping a value is cheap.
- The cross-context target write is the same class as `pin_now`. The review
  must confirm the apply-phase clamp exists, not just the decide cap.
- The packet-denominated Q thresholds shift meaning below MQ. F2 observes
  this; no retune in this bead.
