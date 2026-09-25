DESIGN: DSP effects -- fixed-rate stage, preset store, FFI seam (Ada, 2026-09-25, desk pass, main 6e55178)

Design of record. The hook blocked writing it to .planning/design/ on main. The next person on a branch should commit it verbatim as .planning/design/2026-09-25-dsp-effects-stage.md and index it.

The ryw gate is LIFTED: p1r is closed, LDAC is streaming (checked by ear), and the core1 encoder is the default with libldac in SRAM (d29d741). The feasibility numbers and the global-preset rule are taken as given.

== 0. SUMMARY ==
- The DSP runs on core1, inside pl_a2dp_fill, between pl_pcm_read and pl_a2dp_accumulate_levels.
  - It processes one codec block at a time (128 frames for LDAC) in f32.
  - An AUTO preamp replaces a fixed 6dB of headroom.
  - It saturates to int16 and counts clipped samples. The codec vtable does not change.
- Rust owns presets and computes ALL coefficients.
  - C stores the preset bytes but never parses them.
  - C moves a finished PlDspProgram to core1 through a two-bank, generation-acked handoff.
  - Rust never runs on core1 or in an IRQ. Core1 never blocks, and no update is ever torn mid-block.
- Persistence is a new PL:P:<slot> store, opaque to C, with never-reused ids.
  - The existing device-record preset_id (persist.c:101) is the reference.
  - Forgetting a device touches no PL:P tag.
  - Deleting a preset leaves dangling references, which read as Off.

== 1. FIXED-RATE STAGE ==
1.1 Today's path:
- USB ISO OUT -> pl_pcm_push (pcm_ring.c:33, core0) -> SRAM ring -> pl_pcm_read (pcm_ring.c:88).
- pl_pcm_read is called from pl_a2dp_fill (a2dp.c:1778), which core1 runs from pl_a2dp_core1_entry (a2dp.c:2469, fill at 2579).
- The block lands in s_pcm_scratch (a2dp.c:917, int16 interleaved) -> accumulate_levels (a2dp.c:1791) -> codec->encode (a2dp.c:1794).
- Core1's credit clock paces it one codec block per iteration, so this IS the fixed-rate point. The missing stage is a call at the right spot, not new plumbing.
- NOT in pl_pcm_push: that runs on core0 at the ISO cadence, under the 2ms re-arm bar, with irregular batch sizes.

1.2 The insertion goes after the short-read check (a2dp.c:1779-1785):
- Sequence:
  - t0 = now (moved up from 1793)
  - pl_dsp_process(s_pcm_scratch, pcm_frame_count), in place, int16 in and out
  - t_dsp = now
  - accumulate_levels (now POST-DSP)
  - encode
  - dt_enc = now - t_dsp; dwell_us += now - t0
- pl_dsp_apply_pending() runs once per fill, beside apply_pending_tuning (a2dp.c:1611-1613). Same decide/apply split, same encoder-owning context.
- Filter state (biquad z1/z2 and crossfeed) is PRIVATE to core1. It is zeroed on the entering-RUNNING edge (a2dp.c:2573-2575), which covers both a fresh stream and a resume.
- Kernel stages, in order:
  - int16 -> f32
  - preamp
  - bs2b-style crossfeed: first-order lowpass on the cross path, first-order high shelf on the direct path, normalised gain
  - EQ cascade of TDF-II f32 biquads, hand-written; CMSIS-DSP arm_biquad_cascade_df2T_f32 (Apache-2.0) is the fallback only if the bench misses
  - saturate to int16, counting s_dsp_clip_samples
- Crossfeed and EQ commute when the EQ is identical L/R. Per-channel EQ is out of scope.
- HEADROOM: Rust computes the auto preamp.
  - It is minus the peak of the combined magnitude response on a log grid, clamped to [-12, 0] dB. A fixed -6dB would waste 6dB on presets that have no boost.
  - Everything is float internally, so the only clip point is the int16 output. The clip counter is the tripwire, and slice 1 has no limiter.
  - Requantising to 16 bits costs about 1 bit, which is inaudible under LDAC. Feeding LDAC F32 (LDACBT_SMPL_FMT_F32) is a later option, not needed now.
- BYPASS IS STRUCTURAL: an Off program (0 biquads, crossfeed off) returns before touching a sample, so output is bit-exact with main. This is the A/B arm for every measurement.
- TRANSITIONS: when the generation changes, do a one-block (128 samples, 2.67ms) linear crossfade from the old program's output to the new one's.
  - It costs double for that one block.
  - Off<->On uses the same path with a dry old side, so there is no preamp click.
  - Live edits get no zipper noise.
- MEMORY:
  - The float scratch (static float[512], 2KB) and both banks are file-scope SRAM statics, NEVER on core1's 4KB stack (s_core1_stack, high-water 1672B).
  - The kernel is __not_in_flash_func.
  - Extend check_ldac_not_in_flash.cmake (or add a sibling) to FAIL the build if dsp.c.o text lands at 0x10xxxxxx. This is the nli.8 XIP lesson.
- Core1 needs no libm. The FPU is already live on core1: libldac is float (vendor/libldac/src/struct_ldac.h:50).
- Set FPSCR.FZ on core1 in pl_a2dp_core1_entry, so subnormal IIR tails during silence are flushed to zero.
- fs is fixed at 48k (codec_ldac.c:518). The program carries fs_hz, and the kernel bypasses on mismatch. 96k would double the cost.

1.3 Budget at 990 kbps:
- 128 frames = 2667us, and real time needs 375 blocks/s.
- Encode is about 809us/block on core1 with libldac in SRAM (nli.9/nli.10), about 30% of core1. THE BITRATE OF THAT FIGURE IS UNRECORDED, so re-measure it at 990 as the baseline arm.
- DSP at 2-4% of a core is about 53-107us/block for 10 stereo bands. Crossfeed adds under 27us. Worst case is about 135us plus one doubled block per transition.
- About 945 of 2667us, so core1 is about 35% busy. The 2ms cap (PL_A2DP_CORE1_FILL_BUDGET_US, a2dp.c:296) still fits 2 blocks per call.
- HONEST DWELL: today dwell_us counts encode only (a2dp.c:1793-1814).
  - The DSP MUST be added to dwell_us. Otherwise a call's real duration exceeds what the quiesce _Static_assert (a2dp.c:2440-2445) claims.
  - Add PL_DSP_WORST_CASE_US=300 to that assert: 2000+2000+300 < 5000.
  - Keep enc_* encode-only, so the two costs stay separable.
  - Add windowed dsp_mean_us, dsp_win_max_us and dsp_clip to pl_a2dp_report.

1.4 MEASURE BEFORE ANY UI.
- ryw.1 ships a debug-only DSPPROG <n> command (debug_remote.c, PL_DEBUG_REMOTE):
  - 0 = Off
  - 1 = crossfeed only
  - 2 = 10 peaking bands + crossfeed (worst case)
  - 3 = +9dB low shelf (clip test)
- M0 BENCH (uncontended, ldac_bench.c style): 1000 blocks of program 2 on core1.
  - Gate: 150us/block or less.
  - Above 250us: stop, then move to CMSIS-DSP or cap the band count.
- M1 IN SITU AT 990: A/B in the same session and RF environment, arms 0/1/2, about 3 min each of continuous streaming. Validate the duty cycle first.
  - Record per window: enc_mean_us, dsp_mean_us, dsp_win_max_us, stop_core1_budget/s, ovr_frames/s, stop_ring_empty/s, core0 render iters/s.
  - Pass criteria:
    - dsp_mean 150us or less.
    - enc_mean within 5% of arm 0. A bigger shift means SRAM/bus contention.
    - Overrun and underrun rates no worse than arm 0.
    - Core0 iters/s within 5% of arm 0.
- M2 CLIP: program 3 on loud music gives dsp_clip == 0 under the auto preamp.
- M3 BY EAR, seconds: toggle 0<->2 while streaming and listen for a click. Also confirm dsp_mean_us is NONZERO in arms 1 and 2; zero means the stage never ran.

== 2. PERSISTENCE ==
2.1 Existing:
- One btstack TLV bank, shared with the link keys. Tags are P,L,kind,index.
- PL_PERSIST_KIND_PRESET 0x50 is already reserved (persist.h).
- The device record already has a uint16 preset_id (persist.c:101) that rides the RMW untouched (persist.c:817); 0 means none.
- PL:S:0..2 load before the PL:M:0 marker and version independently. NO device-schema migration is needed.

2.2 PL:P:<slot> store:
- PL_PERSIST_PRESET_SLOTS = 8.
- Record framing, owned by C: {u8 version, u16 preset_id, u8 blob_len, u8 blob[80], u16 crc16}.
- C NEVER parses the blob. Rust owns the wire format (to_wire/from_wire, version byte, per-field fallback, the AbrFloor/CushionPolicy discipline).
- Blob v1 is about 70B:
  - name[16] + name_len
  - crossfeed u8 (0 = off, 1..3 = strengths)
  - band_count u8
  - per band: {type u8 (peak/lowshelf/highshelf), freq_hz u16, gain_half_db i8, q_idx u8} x10
  - the preamp is derived, never stored
- IDS ARE MONOTONIC AND NEVER REUSED.
  - At boot, next_id = max(stored) + 1, starting at 1.
  - The id lives in the record header, separate from the slot index.
  - So a delete-then-create in the same slot gets a new id, and a stale device reference dangles rather than aliasing another EQ.
- Boot: load every valid PL:P record before the marker check. Push PresetLoaded{id, blob} for each, then PresetStoreLoaded{count, status}.

2.3 Device reference:
- preset_id on PL:D. Both 0 and an unknown id mean Off; core resolves this, not C.
- PairedDeviceUpserted gains preset_id (the 7jol.5 ldac_quality echo is the precedent).
- QUICK FIX NAMED: a fifth bespoke staging slot plus execute_pending pair. persist.h already has four near-identical pairs (ldac_quality, display, cushion, abr floor).
- SUSTAINABLE: one field-masked pl_persist_request_device_settings(addr, mask, codec_id, ldac_quality, preset_id) over the existing pl_persist_write_device_settings RMW.
  - Present cost: about one bead of refactor plus an ldac_quality re-test.
  - Future cost of skipping it: one more pair per future per-device field, each another chance to break the async_context rule.
  - RECOMMEND the sustainable path, done inside ryw.6.

2.4 Forget and delete:
- FORGET DEVICE deletes PL:D:<slot> and the link key, and touches no PL:P tag.
  - This holds by construction: forget knows nothing about presets.
  - Regression test: forget the last device referencing preset N; N survives a reload.
- DELETE PRESET deletes PL:P:<slot> and does NOT rewrite device records, which avoids up to 8 writes.
  - Dangling ids read as Off.
  - The next settings write for that device clears its dangling id.
- NEVER garbage-collect a preset because its last device was forgotten.

2.5 Write timing:
- Save and assign are user-initiated, so they are not stream-gated (D11 precedent).
- LIVE EDITS NEVER TOUCH FLASH: only the program goes to core1. The editor persists ONCE, on exit or confirm, so a d-pad hold never becomes a write storm.
- RISK: when the 4KB bank fills, btstack_tlv_flash_bank migrates: it erases a sector and copies all live entries.
  - That is far longer than the ~9ms record write.
  - Measure one forced migration while streaming in ryw.6.
- Live contents: 8 keys, 8 devices at about 60B, 8 presets at about 90B, plus settings, for about 1.5KB of 4KB.

== 3. C/RUST SPLIT AND FFI ==
3.1 Ownership:
- Rust core (a new dsp.rs), on the core0 superloop, owns:
  - the preset model, editing state, validation and names
  - active-preset resolution (connected device's preset_id, else Off)
  - params -> coefficients: RBJ cookbook biquads, bs2b crossfeed, auto preamp, and response sampling for any future curve
  - Uses the libm crate (pure Rust, no_std, platform-free).
- C persist.c: opaque blob storage, on async_context only.
- C dsp.c: the bank handoff (core0 writes, core1 reads), plus the kernel, filter state and clip counter (core1 only).
- Why the coefficients go in Rust: one implementation, host-testable against reference magnitudes, reusable for an on-screen curve.
- Why the kernel stays in C: Rust never runs on core1. That keeps the C-first ADR seam, and keeps Rust panic paths and Rust .text in XIP off the realtime core.

3.2 FFI additions:
- The PROGRAM IS STATE, NOT AN EVENT, so it gets its own pull API, not a PlCommand. This is the level-seqlock argument: newest wins, and it keeps about 250B out of every PlCommand copy.
  - PL_DSP_MAX_BIQUADS 10
  - PlBiquad {f32 b0,b1,b2,a1,a2}, a0-normalised
  - PlDspProgram fields:
    - u32 version (PL_DSP_ABI_VERSION=1)
    - u32 fs_hz
    - f32 preamp (linear)
    - u8 n_biquads, u8 xfeed_on
    - f32 crossfeed coefficients: lowpass b0/a1, cross gain, high-shelf b0/b1/a1, norm
    - PlBiquad biquad[10]
  - bool pl_ui_take_dsp_program(PlUi*, PlDspProgram* out) returns true when the program changed since the last take.
  - C calls it once per superloop iteration, next to pl_a2dp_poll_levels (main.c:710): if it returns true, pl_dsp_submit(&p); then always pl_dsp_service().
- COMMANDS (PL_COMMAND_ABI_VERSION 2->3, because the union grows):
  - SavePreset{preset_id (0 = allocate), blob_len, blob[80]}
  - DeletePreset{preset_id}
  - AssignPreset{addr, preset_id}
- EVENTS (PL_EVENT_ABI_VERSION 5->6):
  - PresetLoaded{id, blob_len, blob[80]}, which is also the save echo and carries the allocated id
  - PresetDeleted{id}
  - PresetStoreLoaded{count, status}
  - PlPairedDeviceUpsertedPayload gains preset_id
  - The single-writer echo rule holds.

3.3 Handoff: no torn updates, and core1 never blocks or enters a critical section.
- State:
  - s_bank[2] in SRAM .bss
  - volatile s_pub_gen, written by core0
  - volatile s_ack_gen, written by core1
  - s_pending + s_pending_valid, core0-private
- core0 pl_dsp_submit(p): s_pending = *p; valid = true; then call service.
- core0 pl_dsp_service(): if valid && ack == pub:
  - s_bank[(pub+1)&1] = s_pending
  - __dmb
  - pub++
  - valid = false
- core1 pl_dsp_apply_pending(), at the block boundary: g = pub; if g != ack:
  - __dmb
  - start a crossfade from the old active to s_bank[g&1]
  - active = s_bank[g&1]
  - ack = g
- Why it is safe:
  - Core0 writes a bank only when ack == pub. Core1 is then on bank[pub&1] and cannot switch until pub moves, which happens only after the write and the barrier. So a bank is never written while it is readable.
  - An unacked submit waits in s_pending, and a newer one overwrites it. A d-pad burst therefore coalesces to at most one publish per core1 block.
  - Core1 applies only at block boundaries, so a block never mixes programs except via the deliberate crossfade.
  - It is also correct for the deprecated ON_CORE1=OFF build, where fill runs in IRQ on core0 and the thread only writes the unacked bank. No extra work (79tt deletes that build).
- Host C test in firmware/tests (test_pcm_ring_cross_core.c precedent):
  - bank protocol invariants
  - bypass bit-exactness
  - biquad impulse response vs a Rust-emitted coefficient fixture
  - saturation and the clip count
  - crossfade endpoints

== 4. FIRST SHIPPABLE SLICE ==
Slice 1 = the stage + crossfeed + one editable EQ preset, assignable per device. It proves every seam end to end.
- Out: HRTF and per-channel EQ.
- The response graph is optional (Uma's call).

DECISIONS FOR ANDREAS, bundled:
(1) Does one preset hold EQ AND crossfeed together (one reference per device), or is crossfeed an independent global toggle?
  - Recommend together: simplest, and crossfeed-only is just a preset with 0 bands.
  - But crossfeed is about the listener's comfort, which argues for a global toggle, so it is his call.
(2) Default for unassigned or new devices.
  - Recommend Off. A global default-preset setting can come later with no format change.
(3) Should the meter read post-DSP?
  - Recommend yes: it shows what is actually sent, and it makes a clip cue meaningful. Goes to Uma.

CHILD BEADS, in order, one design step each:
- ryw.1 DSP ENGINE, C (Ruby). Deps: none.
  - dsp.c/h: kernel, bank handoff, fill insertion, dwell accounting, assert term, report counters, entering-RUNNING reset, FZ
  - SRAM placement plus the build check
  - DSPPROG debug programs
  - host C test
  - No Rust, no UI.
- ryw.2 HARDWARE GATE (Tess). Deps: ryw.1.
  - M0-M3 from 1.4.
  - Go/no-go before any UI.
- ryw.3 CORE DSP MODEL, Rust only, host tests (Ruby). Deps: none; parallel with ryw.1.
  - Preset type and blob wire v1 with fallback.
  - RBJ and bs2b coefficients, auto preamp, PlDspProgram builder, active-preset resolution.
  - Tests: magnitude at probe frequencies vs a reference formula, a dangling id resolving to Off, wire round-trip.
- ryw.4 UX DESIGN (Uma). Deps: Andreas's 3 rulings; parallel otherwise.
  - Preset list.
  - Parametric band editor on 240x240 with d-pad and A/B/X/Y (press-edge only, B is always Back).
  - Crossfeed level.
  - Naming without a keyboard.
  - Device-page assignment row.
  - Delete confirmation.
  - Post-DSP meter and clip cue.
- ryw.5 FFI SEAM (Ruby). Deps: ryw.1, ryw.3.
  - pl_ui_take_dsp_program, the 3 commands, the 3 events plus the upsert field, the ABI bumps, and main.c/bt.c wiring.
  - Acceptance: a Rust-built program replaces DSPPROG with the same measured dsp_mean_us.
- ryw.6 PERSISTENCE (Ruby). Deps: ryw.5.
  - PL:P store, id allocation, boot load.
  - Field-masked device-settings refactor carrying preset_id.
  - Forget-leaves-presets regression test.
  - Delete-preset dangling semantics.
  - One forced TLV migration measured while streaming.
- ryw.7 UI SLICE 1 (Ruby, then Tess screenshots at zoom, then code-reviewer). Deps: ryw.4, ryw.6.
  - Emulator first.
- ryw.8 BY-EAR ACCEPTANCE AND CROSSFEED TUNING (Andreas, with Tess on the rig). Deps: ryw.7.
  - Pick the 3 crossfeed strengths. Taste, not engineering.
- LATER, a separate epic: HRTF.
  - Overlap-save FFT, HRIRs in PSRAM.
  - Re-run the ryw.2 gate at 10-15% of a core.

Uma blocks only ryw.7. ryw.1, 2, 3, 5 and 6 are UI-independent.

== 5. RISKS ==
- TLV bank migration mid-stream is a long stall. Measured in ryw.6; mitigated by persist-on-exit.
- SRAM or bus contention could slow the encode or core0 render. The M1 enc_mean and iters/s criteria exist to catch it.
- Tuning quality, not CPU, decides whether this is worth having. Budget listening time, not engineering.
- Zipper noise on live edits. The crossfade is in ryw.1 from day one.
