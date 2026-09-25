# Seam `bt`: Bluetooth link management, persistence, supervision/observability

Reviewed tree: `2e37164` (branch `claude/gallant-goldberg-pxdjvq`). Reviewer: `bt` seam.
Files read fully: `firmware/src/{bt,persist,flash_lockout,watchdog_sup,panic_recorder,fault,usb_reset,debug_remote,pl_log_ring,pl_prio,pl_loop_prof}.{c,h}`, `btstack_config.h`, the wiring parts of `main.c`, the connect/teardown/core1 regions of `a2dp.c`, `firmware/CMakeLists.txt` (options/defines), `tools/usb-console/*`, `firmware/tests/test_fault_evaluator.c`, `test_paired_device_upserted_ldac_quality_echo.c`, and the eight design docs named in the brief plus `roadmap.md`.
To raise confidence on the flash path I also fetched and read pico-sdk 2.1.1 `pico_flash/flash.c`, `pico_btstack/btstack_flash_bank.c`, `pico_multicore/multicore.c`, and BTstack `platform/embedded/btstack_tlv_flash_bank.c` + `classic/btstack_link_key_db_tlv.c` (master; the TLV code is stable).

Host tests measured: `test_fault_evaluator` **5/5 pass**; `test_paired_device_upserted_ldac_quality_echo` **2/2 pass** (needs `pico_link_ui.h`, generated here with `cbindgen --config ui-ffi/cbindgen.toml --crate ui-ffi`; the header is not checked in and the test's own build line assumes `firmware/include/` exists — it does not in a fresh checkout).

---

## 1. Verdict

This seam is in materially better shape than its history suggests: the two hard invariants ("Rust is only called from the superloop" and "flash is only touched on the BTstack async_context") are now true by construction and I could not find a violation. The supervision layer (panic recorder, log ring, priority channel, boot-reason capture) is careful, well-argued C. What is **not** true is the link lifecycle story the design docs promise: `CancelConnect` is still a silent no-op in firmware (design of record 2026-08-30 never landed), the peer identity for a sink-initiated connection is taken from the *last outgoing target*, and the flash-lockout "START timeout is benign" claim is wrong at the TLV layer. **0 P0, 3 P1, 7 P2, 1 P3 batch.**

## 2. What is well done (do not touch)

- **The IRQ→thread event ring (`bt.c:52-216`).** MPSC with one short critical section, embedded name buffers so the borrowed `name` pointer never dangles across the deferral, drop-newest with a drop counter reported only from thread context. `pico-link-6o2` is fixed: `pl_bt_drain_events` (`bt.c:197`) is the only BT-domain caller of `pl_ui_push_event`. I checked every other `pl_ui_push_event` site in `firmware/src`: `fault.c:265` (called from `main.c:908`), `a2dp.c:1110` (`pl_a2dp_poll_levels`, from `main.c:668`), `a2dp.c:4148` (`pl_a2dp_poll_ldac_bitrate`, from `main.c:674`), `main.c:456` (boot) — all thread context. The class of bug is gone, not just the instance.
- **The thread→run-loop pending queue (`bt.c:710-902`, `pico-link-ouw`).** Every BTstack API call from a user command is deferred onto the 100 ms heartbeat timer, which runs on the same cyw43 background IRQ as every packet handler. This is the correct answer to `async_context_threadsafe_background`'s contract and it is also what makes persist's reentrancy story work.
- **Persistence reentrancy discipline (`persist.h:49-98`, `persist.c:493-517`).** Every post-init flash mutation runs on the one context BTstack's own `put_link_key` uses, so our writes and BTstack's cannot interleave in the unlocked TLV bookkeeping. One RMW core (`pl_persist_rmw`, `persist.c:600-702`) for pairing and settings writes, per-record CRC16, per-slot corruption isolation, blank/corrupt/version-mismatch distinguished, a live in-RAM slot mirror updated in the same call as the write, marker/schema separated from the independently-versioned `PL:S:0` record. The firmware/storage-region collision check (`persist.c:252-266`) being unconditional is exactly right given pico-sdk's forced-Release NDEBUG.
- **`flash_lockout.c`'s analysis** of why pico-sdk's default `flash_safety_helper_t` is unusable here (`flash_lockout.c:78-96`) is correct — I confirmed against `pico_flash/flash.c:86-110`: with `pico_multicore` linked the default helper would assert or silently skip on a single-core boot. The runtime `multicore_lockout_victim_is_initialized(1)` check makes one binary correct before and after core1 launches. The END-timeout-is-fatal reasoning (`flash_lockout.c:234-248`) matches `multicore.c:307-318` (`lockout_in_progress` only clears on a successful END). One correction to this file is F-bt-02.
- **Panic recorder ordering** (`panic_recorder.c:124-176`): watchdog armed as the literal first statement of every entry point, scratch[3] as the recursion latch, scratch[4..7] correctly left to the SDK, the BOOTSEL-ROM-clobber fix (`panic_recorder.c:420-446`) that clears scratch on every early return, and the HardFault path that records core number/BFAR/LR by hand with no library calls. The shared vector table means core1 faults land here too.
- **Boot-reason capture as the first statement of `main()`** (`watchdog_sup.c:450-466`, `main.c:97`) removes the ordering landmine the design flagged; the NOLOAD loop-trace ring with a magic guard on the index (`watchdog_sup.c:174-181`) cannot corrupt memory on a cold boot.
- **Log ring + priority channel** (`pl_log_ring.c`, `pl_prio.c`): bounded, non-blocking, whole-line-or-nothing, single drainer, the `tud_cdc_write_available()` gate documented as load-bearing (`pl_log_ring.c:305-313`), fixed-width priority slots that cannot overflow or go stale, and the `_Static_assert` that a slot fits an empty CDC FIFO. `pl_prio_publish`'s truncation clamp at `PL_PRIO_SLOT_LEN - 3` (`pl_prio.c:140-151`) is the kind of off-by-one someone actually thought about.
- **`fault.c` matches the audio-fault-model design** secs 5–7: 1 s window, raise-on-1/clear-on-3, 10 s refresh, raises-only on the wire, absolute counts, negative-delta-is-a-reset, reset-all when not `STREAMING`/host-silent (the cry-wolf gate), dynamic `AIR CONGESTED` severity, `wakes_display` kept Rust-side. Constants differ from the doc where the doc said they should (`CONGEST_MIN` 8→2 measured; the ord-2 "rate" half deleted by `pico-link-47us` with a correct argument about explicit feedback). The 5 host tests cover every named trap.
- **`usb_reset.c`** is a faithful, provenance-marked port with both branches unconditional — the exact fix the 2026-08-29 picotool design prescribes.
- **core1 hygiene**: I grepped the core1-reachable ranges of `a2dp.c` (`pl_a2dp_fill` 1550→, resync 2193–2270, `pl_a2dp_core1_entry` 2371→) and `codec_ldac.c`'s encode/tuning ranges for `pl_log`, `pl_wdt_kick`, `pl_prio_publish`, `save_and_disable_interrupts`: none. The "core1 never logs, never disables IRQs, never calls BTstack" invariant holds, and `pl_a2dp_seal_head` (`a2dp.c:1482-1497`) correctly moves `request_can_send_now` back to core0.

## 3. Architecture assessment

**Context model (question 1).** Four execution contexts, and each module knows which one it is in:

| Context | Who runs there | Touches |
|---|---|---|
| core0 thread (superloop) | `pl_bt_poll_commands`, `pl_bt_drain_events`, `pl_persist_service` (enqueue only), `pl_fault_evaluate`, `pl_a2dp_poll_*`, `pl_debug_remote_poll`, `pl_log_ring_drain`, `pl_wdt_service` | Rust (only here), CDC write/read under `pl_usb_lock_try` |
| core0 IRQ 0xFF (cyw43 `async_context_threadsafe_background`) | every BTstack packet handler and timer: `pl_bt_packet_handler`, `pl_a2dp_packet_handler`, AVRCP handlers, heartbeat (`pl_bt_pending_service`), media/retry/dismiss timers, BTstack's own `put_link_key` | BTstack API, **all post-init flash writes**, event ring push |
| core0 IRQ 0xC0 (`usb_pump.c:33,390`; preempts 0xFF) | `tud_task`, USB audio, HID | never BTstack, never flash, never Rust |
| core1 | LDAC encoder loop; SIO FIFO IRQ for lockout | `s_ctx` audio fields only |

Nothing is called from the wrong context. Flash from an IRQ is deliberate and coherent (BTstack does the same). The one thing the docs get wrong is `watchdog_sup.h:48-55`, which says `PL_WDT_ENCODER` is fed by core0's *superloop*; it is fed by the media-timer IRQ handler (`a2dp.c:2809-2819`). Single producer, so not a bug — a doc drift (F-bt-11).

**Link state machine (question 2).** There is no explicit link-state variable in firmware; the state is the product of `a2dp.c`'s `s_ctx.{a2dp_cid, state ∈ {IDLE,PRIMING,STREAMING}, connect_succeeded_pushed, reconnect_retry_armed}` and `s_enc_state ∈ {IDLE,RUNNING,DRAINING}`. What crosses to core:

| Push | Site | Trigger |
|---|---|---|
| `LinkState(Connecting)` | `bt.c:1046`, `bt.c:1198` | Connect command accepted (before any radio work) |
| `ConnectStep(*)` | `a2dp.c:2890, 2969` | negotiation milestones |
| `ConnectFailed(reason)` | `a2dp.c:2880, 2966, 3076, 3284, 3360, 3425` | core folds to `Idle` |
| `LinkState(Connected)` + `ConnectSucceeded` | `a2dp.c:3645-3646` | `STREAM_ESTABLISHED` (guarded once per signaling session) |
| `LinkState(Idle)` | `a2dp.c:3908` | `SIGNALING_CONNECTION_RELEASED` only |
| `DiscoveryState(*)` | `bt.c:1017, 553, 672` | the second axis, per the 2026-09-08 design — implemented exactly as its §7 table says |

Stuck/racy paths found: (a) `CancelConnect` is unimplemented so cancel-vs-success is unhandled (F-bt-01); (b) a dropped `PL_BT_PENDING_CONNECT` leaves `Connecting` with no outcome ever (F-bt-04); (c) a signaling session that never reaches `STREAM_ESTABLISHED` and never releases (a sink that connects and idles) stays `Connecting` indefinitely — there is no firmware-side attempt timeout, and the only escape is the cancel that does nothing (folded into F-bt-01); (d) a user `DISCONNECT` during the SDP/AVDTP phase surfaces as a synthetic `SIGNALING_CONNECTION_ESTABLISHED(error)` → `ConnectFailed` → failure toast, which the cancel design explicitly forbids for a user-initiated abort. The 0x0b single-retry (`a2dp.c:2944-3000`) and the wizard-dismiss timer are correctly cancelled on every teardown path.

**Cancel-connect design vs code.** The design of record (`2026-08-30-cancel-connect.md`, C1–C7) is **entirely unbuilt** on both sides: `PL_COMMAND_TAG_CANCEL_CONNECT` (=4) falls into `default:` at `bt.c:1171-1173`; `core/src/app/fold.rs:141-146` `on_connect_succeeded` is still unconditional (C7). The doc is right; the code is behind it.

**Remembered-devices design vs code.** T1–T3 landed and match §6 (8 slots, RMW, no-evict, forget drops the link key, `StoreLoaded{status,count}` terminator, name cached on Connect). Its §7 hazards 1 (orphaned record after BTstack link-key LRU eviction) and 2 (two flash writes per pairing) are unmitigated (F-bt-09, F-bt-08); hazard 3's doc comment landed (`bt.c:1062-1071`).

**Watchdog design vs code.** Steps 1–6 landed faithfully. Step 7 (enable tripping) never happened: `PL_WDT_OBSERVE_ONLY` defaults ON (`CMakeLists.txt:324`), so 26 days on, the supervisor is a logger for everything but `ENCODER`. Step 8 (`pl_wdt_blackout_*`) is unnecessary — see F-bt-07 for why the design's "guaranteed false trip" cannot actually happen.

**Persistence (question 3).** Layout: 8 KB two-bank BTstack TLV at `0xFFD000` (one sector below pico-sdk's default, correctly avoiding the UF2 metadata block — `CMakeLists.txt:447-469`), shared with BTstack's link keys under a disjoint tag namespace. Atomicity: TLV writes value-then-header, so a power loss mid-entry leaves an unreadable entry that the next boot treats as end-of-bank and migrates around; our records carry CRC16, link keys do not (BTstack's choice). Versioning: `PL_PERSIST_SCHEMA_VERSION 1` with a wipe-on-mismatch policy (records only; link keys untouched) and reserved bytes (`volume/flags/preset_id`) so the next fields need no bump. Corrupt record: dropped individually, status surfaced to the UI. Wear: fine in absolute terms, but see F-bt-08. Flash-lockout correctness: core1 parks in a RAM-resident handler with IRQs off (`multicore.c:213-226`), core0 disables IRQs after the handshake, XIP is safe. The defect is what happens when the handshake *fails* (F-bt-02).

**Debug remote / CDC (question 6).** Parsing is bounded (`s_line` 32 B with resync on overflow, 256 B/poll budget, hex parser cannot run past the NUL, `SKIPTICKS` saturates). Release builds really exclude `BOOTSEL`: `debug_remote.c` is compiled only under `PL_DEBUG_REMOTE` (`CMakeLists.txt:313-315`). The log ring cannot lose or corrupt *lines* under its three core0 producers (one critical section, whole-line-or-drop); core1 is kept out by convention and verified above. The protocol between `debug_remote.c` and `cdc_sender.py` is coherent for the subset the sender knows, but unversioned and documented in three drifting places (F-bt-10).

**Security (question 7).** Proportionate view for a personal dongle: the device is discoverable forever, bondable, Just-Works (F-bt-09). The MVP flow never needs us to be discoverable at all — we page the headphones, not the reverse.

**Magic numbers (question 8).** Derived and documented: `PL_WDT_TIMEOUT_MS 2000`, `PL_FLASH_LOCKOUT_TIMEOUT_US 20ms`, `PL_FAULT_*` (measured), `PL_PANIC_WATCHDOG_TIMEOUT_MS 500`, ring 32/name 240. Judgement, admitted as such: `PL_PERSIST_SETTLE_US 2s` / `MIN_INTERVAL 10s`, `PL_BT_PENDING_CAPACITY 8` (whose overflow is invisible to the UI — F-bt-04), heartbeat 100 ms. Duplicated state: "which device am I connected to" lives in three places — `bt.c`'s connect-target cache, `a2dp.c`'s `s_ctx.connect_addr`, and core's `connected_addr` — and F-bt-03 is the case where they diverge.

## 4. Findings

### F-bt-01: `CancelConnect` is a silent no-op — the 2026-08-30 cancel-connect design never landed
- Severity: P1   Confidence: High   Effort: L   Tier: Opus
- Location: `firmware/src/bt.c:1171-1173`, `firmware/src/a2dp.c:4151-4172`, `core/src/render/wizard.rs:393`, `core/src/app/fold.rs:141-146`; design `.planning/design/2026-08-30-cancel-connect.md` C1–C7
- Evidence: `bt.c:1171` `default: pl_wdt_mark(PL_WDT_CP_CMD_OTHER); break;` is the only handler for tag 4 (the enum comment at `watchdog_sup.h:179` even names it). Core still emits it: `wizard.rs:393 self.commands.borrow_mut().push_back(Command::CancelConnect { addr });`. Core's guard (design C7) is also absent: `fold.rs:144 *self.wizard_phase.borrow_mut() = WizardPhase::Succeeded { degraded };` unconditionally.
- Why it matters: B during Connecting/NotResponding pops the wizard and nothing stops the attempt. A late `STREAM_ESTABLISHED` pushes `Connected`+`ConnectSucceeded` (`a2dp.c:3645`), core persists it and re-enters `Succeeded`, and audio flows to a headset the user walked away from — the design's D3 case, "strictly worse than today's no-op". Symmetrically, the only abort the user *can* reach (`DISCONNECT`, `bt.c:1107`, when a screen eventually queues it) during S1 produces a synthetic failure → `ConnectFailed` → failure toast, which the design forbids for a user abort. There is also no firmware-side attempt timeout, so a sink that accepts signaling and idles leaves `Connecting` forever with no escape.
- Fix sketch: implement the design as written: C3 a `case PL_COMMAND_TAG_CANCEL_CONNECT:` that does bookkeeping + `pl_bt_push_link_state(IDLE)` and enqueues a pending `CANCEL_CONNECT`; C4 the heartbeat services it via `a2dp_source_disconnect(cid)`; C1 an attempt epoch/`attempt_live` gate in front of every `pl_bt_push_connect_*`/`_link_state_connected`/`_codec_changed` call in `a2dp.c`; C2 on a late `STREAM_ESTABLISHED` with `!attempt_live`, do not init the codec, do not arm the media timer, disconnect; C5 the one-deep connect-during-teardown hold. Core's C7 guard is a one-line change for the FFI reviewer.
- Verification: host model test of the epoch gate (copy the `pl_a2dp_emit_*` wrappers as `test_fault_evaluator.c` does); `cargo test -p pico-link-core` for C7; hardware: `--connect` then `NAV BACK` within 1 s, expect `a2dp: signaling connection released` and no `ConnectSucceeded` in the capture (`cancels_requested` ≥ 1, `cancels_late_success` counted).
- Related: `pico-link-2pq`, `pico-link-dgx`, `pico-link-44w`, `pico-link-ouw`

### F-bt-02: A flash-lockout START timeout is not benign — it silently corrupts the TLV bank's bookkeeping and can park core1 forever
- Severity: P1   Confidence: High (source read end to end incl. pico-sdk/BTstack; the trigger needs hardware)   Effort: M   Tier: Opus
- Location: `firmware/src/flash_lockout.c:212-222`, `flash_lockout.h:39-42`; `persist.c:670-687, 803-807, 1071`; pico-sdk `btstack_flash_bank.c:73-76,161-164`; BTstack `btstack_tlv_flash_bank.c:435-496, 258-266`; `multicore.c:213-226, 287-296`
- Evidence: `flash_lockout.c:220-221` counts and returns `PICO_ERROR_TIMEOUT`, and the header says "benign and recoverable ... the RAM-staged value survives". But `pico_flash_bank_write` discards the rc (`// currently we have no way to return an error to the caller anyway`), `btstack_tlv_flash_bank_store_tag` then runs `self->write_offset += ...` (`:495`) regardless, and `persist.c:677-687` updates `s_slots` and `persist.c:805` clears `s_pending` regardless.
- Why it matters: three consequences, all silent. (1) RAM says the device is remembered; flash has 0xFF at that offset; the boot-time scan (`btstack_tlv_flash_bank.c:565-581`) stops at the gap, so **every entry written after it — including BTstack link keys — is invisible on the next boot**; the "not empty after last tag → migrate" path migrates only what the iterator reached. (2) If the skipped op was the erase inside `btstack_tlv_flash_bank_migrate` (`:264`), the subsequent writes program into an un-erased bank: garbage for everything. (3) The `LOCKOUT_MAGIC_START` word stays in core1's FIFO; if core1 was merely late (not dead) it later enters `multicore_lockout_handler`, disables IRQs and waits for an END that never comes (`multicore.c:217-221`) — core1 is dead until `PL_WDT_ENCODER` notices (only while streaming) and every later START also times out. The header's "recoverable" is wrong in all three cases.
- Fix sketch: the TLV layer cannot roll back, so do not pretend. Either (preferred) treat a START timeout exactly like END — `pl_panic_record_flash_lockout_*` with a distinct message — using the ADR §7.1 argument that a core1 that cannot answer a FIFO IRQ within 20 ms with interrupts enabled is already a corrupted machine; or, if a soft path is wanted, snapshot `pl_flash_lockout_timeout_count()` before/after every `store_tag`/`delete_tag` in `persist.c` and on a delta keep `s_pending`, leave `s_slots` untouched, force a TLV re-init, and log. Correct `flash_lockout.h:39-42` either way.
- Verification: a `PL_DEBUG_REMOTE` command that makes core1 spin with IRQs off for 100 ms, then `--raw "NAV ..."` a settings write: expect the recorder's report on the next boot, never `persist: wrote ...` followed by a record missing after reboot. Static: `grep -n "s_timeout_count++" flash_lockout.c` must be followed by a panic or a propagated error, not `return`.
- Related: `pico-link-nli.2`, ADR `2026-09-03-ldac-encoder-on-core1.md` §6/§7.1, `pico-link-lmf`

### F-bt-03: A sink-initiated (incoming) A2DP connection is attributed to the last *outgoing* target
- Severity: P1   Confidence: Medium (code path certain; BTstack's acceptance of an incoming AVDTP session under `ENABLE_A2DP_EXPLICIT_CONFIG` not run here)   Effort: S   Tier: Sonnet
- Location: `firmware/src/a2dp.c:3060-3090` (`SIGNALING_CONNECTION_ESTABLISHED`), `a2dp.c:2953` (only writer of `connect_addr`), consumers `a2dp.c:2875, 2897, 3624, 3646, 4119`
- Evidence: the established handler reads `status` and `cid` only; `a2dp_subevent_signaling_connection_established_get_bd_addr` is never called anywhere in `firmware/src`. `s_ctx.connect_addr` is set solely in `pl_a2dp_establish_stream_now` (`:2953`).
- Why it matters: headsets routinely page the last source when powered on. `a2dp_source_init` registers the AVDTP L2CAP service, `gap_connectable` is BTstack's default, so the dongle accepts. Then `STREAM_ESTABLISHED` persists `connect_addr` (`:3624`) — the previous outgoing target, or all-zeros on a boot where core's auto-reconnect had not fired yet — pushes `ConnectSucceeded{addr}` and `CodecChanged{addr}` for the wrong device, and `pl_persist_get_device_settings(connect_addr)` (`:2875`) applies the *wrong headset's* LDAC pin. With two remembered headsets (the next milestone), the non-MRU one auto-connecting corrupts the MRU one's record. Today the boot auto-reconnect usually masks it, which is why it has not been seen.
- Fix sketch: on a successful `SIGNALING_CONNECTION_ESTABLISHED`, read the event's `bd_addr` into `s_ctx.connect_addr` and, when it differs from the in-flight target (or there was none), treat it as a new attempt: `pl_bt_set_connect_target(addr, NULL, 0)`-equivalent via a new `pl_bt_note_incoming_peer(addr)`, push `LinkState(Connecting)` so core's model agrees. Combine with F-bt-01's epoch so an incoming session is an attempt like any other.
- Verification: hardware — pair, reboot the dongle, power-cycle the headphones *before* the auto-reconnect fires (or forget-then-reconnect): the console must show `persist: wrote device record ... <headphone addr>` and never `00:00:00:00:00:00`; a `bd_addr` comparison test in a host model of the handler.
- Related: `2026-09-01-remembered-devices.md` §4 (device switching), `2026-09-08-link-state-vs-discovery-axis.md` §3.1 (who owns "the link is up")

### F-bt-04: `pl_bt_pending_push` overflow is invisible to callers — flags stick and `Connecting` never resolves
- Severity: P2   Confidence: High (path certain; needs 7 queued entries in one 100 ms heartbeat to trigger)   Effort: S   Tier: Sonnet
- Location: `firmware/src/bt.c:823-844`; `persist.c:955-956, 976-977, 984-985`; `bt.c:1046-1048`, `bt.c:1198-1199`
- Evidence: `pl_bt_pending_push` is `void`; on full it logs and returns (`bt.c:827-832`). `pl_persist_service` sets `s_settings_write_enqueued = true; pl_bt_enqueue_ldac_quality_write();` (`:976-977`) — the flag is only ever cleared by the execute function that now never runs. `PL_COMMAND_TAG_CONNECT` pushes `LinkState(CONNECTING)` at `:1046` and only then enqueues at `:1048`.
- Why it matters: after one dropped `SET_DEVICE_LDAC_QUALITY`/`SET_DISPLAY_SETTINGS` entry, that setting is never persisted again this boot (the pairing slot is immune because `pl_persist_request_save_device` re-arms its flag; the other two are not). After one dropped `CONNECT`, core shows Connecting with no radio work and no `ConnectFailed` ever — F-bt-01's missing cancel is the only way out and it is a no-op.
- Fix sketch: return `bool`; callers set `*_write_enqueued` only on `true`; the CONNECT/debug-connect paths push `ConnectFailed(addr, RADIO_ERROR)` on `false` and skip the Connecting push. Consider `PL_BT_PENDING_CAPACITY 16` with a comment that the 8 was never measured.
- Verification: host model test of `pl_persist_service` with a stub enqueue returning `false` — assert the flag stays clear and the next call retries; a hardware soak line: `BT: pending-action queue full` must never appear.
- Related: `pico-link-ouw`, `pico-link-7jol.5`, `pico-link-qivj.5`

### F-bt-05: Every boot starts an unconditional 10 s GAP inquiry that races the auto-reconnect
- Severity: P2   Confidence: Medium on the radio-level cost, High on the code   Effort: S   Tier: Haiku
- Location: `firmware/src/bt.c:641-643`; `core/src/app/fold.rs:162-166`
- Evidence: `bt.c:641 pl_bt_push_store_loaded(...); bt.c:643 pl_bt_start_scan();` in the `HCI_STATE_WORKING` case — an M2 leftover from BTstack's `gap_inquiry.c` example shape (`bt.c:13-19`). Core answers `StoreLoaded` by queuing `Command::Connect` to the MRU device (`fold.rs:166`), which lands ≤100 ms later, inside the inquiry.
- Why it matters: the page for the auto-reconnect runs concurrently with an inquiry on the same 2.4 GHz radio (slower page, and the CYW43439 may serialise them), the discovered-devices list is cleared and repopulated with nobody watching (the wizard is not open), and the device advertises `Scanning` on Home at every power-on. Since `pico-link-znb` scanning is user-driven; this call has no consumer.
- Fix sketch: delete `pl_bt_start_scan()` (and the now-unused `pl_bt_start_scan` wrapper) from the `HCI_STATE_WORKING` case; keep the `pl_bt_start_scan_radio` path for `START_SCAN`.
- Verification: boot capture shows `BT: PL_CMD_CONNECT` without a preceding `BT: starting GAP inquiry`; reconnect time from `HCI_STATE_WORKING` to `stream established` measured before/after.
- Related: `pico-link-cz0.3`, `pico-link-znb.2`, `2026-09-08-link-state-vs-discovery-axis.md` §1.1

### F-bt-06: User-initiated settings writes bypass the streaming gate, including the bank-migration erase
- Severity: P2   Confidence: Medium (the ruling is documented; whether the ISO-OUT wedge is still reachable under sdk-patch 04 is unverified)   Effort: M   Tier: Opus
- Location: `firmware/src/persist.c:959-986, 999-1025, 1047-1081`; `flash_lockout.c:224`; `btstack_tlv_flash_bank.c:441-443, 258-266`
- Evidence: `persist.c:964-975` "Andreas: just write. It's fine if audio skips" — the LDAC-quality and display-settings paths enqueue with no `pl_usb_audio_streaming()||pl_a2dp_streaming()` check, and their execute functions re-check nothing. Every `store_tag` may trigger `btstack_tlv_flash_bank_migrate` → `erase` of a 4 KB sector (the watchdog design budgets 400 ms worst case) with core0 IRQs off and core1 parked.
- Why it matters: the ruling priced "audio skips"; the mechanism is a core1 encoder stall of 3–400 ms *and* a core0 IRQ blackout of the same length while USB alt-1 is live — the exact `pico-link-lmf` hazard that the *pairing-time* write (`persist.c:864-877`) refuses to take. Two paths, one hazard, opposite policies; the settings path is the one a user exercises while listening.
- Fix sketch: keep the ruling but bound the blackout: before an ungated write, check `write_offset + required_space` against the bank size and, if a migration would be needed, do the migration in a gated window first (or mark the write pending-gated). Alternatively state in `persist.h` that a quality/display pick can cost a ≤400 ms gap and that the ISO wedge is accepted. Either way, retire the contradiction between `persist.h:78-83` and `:282-287`.
- Verification: hardware: fill the bank artificially (repeat `SET_DISPLAY_SETTINGS` ~60×) while streaming; capture must show `usb-audio` packet counts resuming after the `persist: wrote display settings` line and no permanent ISO stall.
- Related: `pico-link-xcmx`, `pico-link-lmf`, D11 in `2026-09-24-screensaver-dim-and-timeout.md`

### F-bt-07: Watchdog rollout parked at "observe-only"; `pl_wdt_blackout_*` is dead code built on a false premise
- Severity: P2   Confidence: High   Effort: S (code) / product call (step 7)   Tier: Sonnet
- Location: `firmware/CMakeLists.txt:324`; `watchdog_sup.c:301-367, 369-389`; `watchdog_sup.h:96-103, 48-55`
- Evidence: `option(PL_WDT_OBSERVE_ONLY ... ON)`; `pl_wdt_service` feeds `watchdog_update()` unconditionally on staleness for every subsystem but `ENCODER` (`:345-355`). `pl_wdt_blackout_begin/end` have no callers (grep) and the header says cz0.6.1 must wire them "or a save is a guaranteed false trip".
- Why it matters: (a) the only reset the device can take today is the unattributed 2 s hardware expiry or a dead core1; the staleness table — the half of the design that catches "still executing, no longer doing its job" — has been measurement-only for 26 days and step 7's fault-injection proof was never run. (b) The blackout premise is wrong: `pl_wdt_service` judges staleness only when a counter did *not* change between two consecutive service calls, and the superloop cannot run during an IRQs-off window at all (the write executes inside the 0xFF handler), so the first service call after any blackout sees every counter changed and re-bases. No false trip is possible; the seam is unnecessary and its comment will send the next implementer wiring it into `flash_lockout.c` for nothing. (c) `watchdog_sup.h:48-55` attributes the `ENCODER` feed to the superloop; it is the media-timer IRQ (`a2dp.c:2809-2819`).
- Fix sketch: delete `pl_wdt_blackout_*` and the header paragraph; fix the `ENCODER` comment; file step 7 as a dated decision with the per-subsystem `max_stale_ms` data from a soak.
- Verification: `grep -rn pl_wdt_blackout firmware/` empty; `-DPL_WDT_OBSERVE_ONLY=OFF` build plus a `PL_DIAG_WDT_TEST` that stops the media timer — next boot prints `subsystem: MEDIA`.
- Related: `pico-link-ufh`, `pico-link-4ju`, `2026-08-30-watchdog.md` steps 7–8

### F-bt-08: Write amplification — marker rewritten on every record write, and two writes per pairing
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `firmware/src/persist.c:621-622` (marker), `persist.c:803, 883` (two write paths), `bt.c:1053-1078`; `btstack_tlv_flash_bank.c:435-496`
- Evidence: `pl_persist_rmw` unconditionally `store_tag(PL:M:0)` before every device write; BTstack's TLV is append-only (no identical-value dedupe), so each device write appends two entries (and two delete-marks). After `pl_persist_save_device_now` at `STREAM_ESTABLISHED`, core's `PersistDevice` restages the same address (`bt.c:1077`) and the urgent flush at the first pause writes it again with a second MRU bump — the remembered-devices design's hazard 2, still open.
- Why it matters: the bank is 4 KB; each device write costs ~2×(8+4+52 aligned) B; halving the entry rate roughly halves the migration-erase rate, and a migration erase is the 400 ms blackout every other finding here worries about. The double MRU bump is harmless but the second write is pure wear.
- Fix sketch: cache `s_marker_written` at init and write the marker only when absent; in `pl_persist_request_save_device` drop a request whose `addr` matches the record written by `pl_persist_save_device_now` in this session (the design's own suggestion).
- Verification: one pairing on hardware → exactly one `persist: wrote device record` line; host model test of the request-dedupe.
- Related: `2026-09-01-remembered-devices.md` §7 hazard 2

### F-bt-09: Always discoverable + Just-Works bonding, and BTstack's link-key LRU can orphan a remembered device
- Severity: P2   Confidence: High on the code; Medium on how much it matters for a personal dongle   Effort: S   Tier: Sonnet
- Location: `firmware/src/a2dp.c:4046`; `btstack_config.h:300`; BTstack `btstack_link_key_db_tlv.c:119-160`; `persist.c:353-410`
- Evidence: `gap_discoverable_control(1)` at init, never cleared; no `gap_ssp_set_io_capability`/`gap_set_bondable_mode`/`gap_ssp_set_auto_accept` anywhere in `firmware/src` (BTstack defaults: NoInputNoOutput, auto-accept, bondable). `put_link_key` evicts the entry with the lowest `seq_nr` when all `NVM_NUM_LINK_KEYS` (8) are used. `pl_persist_init` never checks that a loaded `PL:D` record still has a key.
- Why it matters: the MVP flow never needs discoverability — we page the headphones, they do not find us — so the flag buys nothing and lets any phone/laptop bond unprompted, each bond consuming one of 8 key slots. When the 9th bond lands, the oldest remembered headset's key is evicted while its `PL:D` row still says "Paired" — a row that cannot connect and needs a physical headphone reset to fix (`pico-link-7ur`). The design named this (§7 hazard 1) and asked for the cheap mitigation.
- Fix sketch: `gap_discoverable_control(0)` at init (make it a scan-time toggle only if a future feature needs it); at load, `gap_get_link_key_for_bd_addr()` per record and drop (or flag) records without a key, pushing `PairedDeviceForgotten`; optionally `gap_set_bondable_mode(0)` outside the wizard.
- Verification: `hcitool scan`/a phone's BT list must not show "Pico Link" while idle; host model test for the load-time key check with an injected DB.
- Related: `2026-09-01-remembered-devices.md` §7 hazard 1

### F-bt-10: The debug-remote wire protocol is unversioned and documented in three drifting places
- Severity: P2   Confidence: High   Effort: S   Tier: Sonnet
- Location: `firmware/src/debug_remote.h:28-102`, `debug_remote.c:280-491`, `tools/usb-console/cdc_sender.py:17-58, 259-273`
- Evidence: the header's protocol list omits `TRIM POLICY *`, `VOL FUSET`, `VOL INT`, `MEDIA *`; `cdc_sender.py` knows only NAV/CONNECT/DISCONNECT/BOOTSEL (everything else needs `--raw`); `cdc_sender.py:54` still says "the firmware persists no link keys either", false since `pico-link-cz0.6`. There is no `HELLO`/`PROTO` reply, and an unrecognized line is acknowledged only via `pl_log`, which the ring may drop.
- Why it matters: a host script cannot tell what a given build accepts, and the only negative signal is a droppable log line — an unattended test that sends a command the build does not know reports success. Three hand-maintained lists have already drifted.
- Fix sketch: one command table in `debug_remote.c` (name, arity, handler); a `PROTO` command that publishes the accepted names through `pl_prio` slot 4 (undroppable); generate or at least cross-check the header/py lists from it; fix the stale sentence.
- Verification: `python3 cdc_sender.py --raw PROTO` then the capture contains every name in the header's list.
- Related: `pico-link-cd3`, `pico-link-g48`, `pico-link-vu4`

### F-bt-11: Nits (batched)
- Severity: P3   Confidence: High   Effort: S   Tier: Haiku
- Location / Evidence / Fix:
  - `CLAUDE.md` ("Release builds deliberately do not expose a remote-reboot command") vs `usb_reset.c:118-131`: the picotool vendor BOOTSEL/FLASH path is unconditional in every build by design (`pico-link-l60`). Behaviour is fine; fix the sentence.
  - `persist.h:120-131` says STORE_FULL is "not yet inspected ... deferred T3 wiring"; T3 landed (`persist.c:616` pushes `PairedStoreFull`). Stale.
  - `persist.c:455-457, 467` cite `bt.c:814`/`bt.c:613`; `bt.c:52-53` cites `bt.c:102`; `test_paired_device_upserted_ldac_quality_echo.c:11-19` cites `bt.c:451/595-621`, `persist.c:359/626` — all stale line refs. Prefer function names.
  - `watchdog_sup.h:48-55`: `ENCODER` is fed from `a2dp.c:2809-2819` (media timer IRQ), not the superloop.
  - `debug_remote.c:374, 396`: on a bare `VOL SET`/`VOL FUSET` the log reads `s_line + 8`/`+ 10`, one past the NUL — inside the 32 B buffer, harmless, prints a stale byte.
  - `panic_recorder.h:3-10` "not for ship, no compile-time gate exists": still true; nothing tracks it. File a bead.
  - `gap_set_local_name("Pico Link 00:00:00:00:00:00")` (`a2dp.c:4045`) relies on BTstack substituting the local address for that literal pattern; one comment would save the next reader a search.
  - `test_paired_device_upserted_ldac_quality_echo.c` documents itself as unable to fail on regression (hand-copied model). Its build line assumes `firmware/include/pico_link_ui.h` exists; in a fresh checkout it must be generated with cbindgen first — say so in the header.
- Verification: `grep` for each string; the two tests still pass.

## 5. Test coverage

Covered on host: fault evaluator model (5 tests, every design trap), PairedDeviceUpserted echo (2 tests, model-copy — pins intent, cannot catch regression). Covered only by hardware soaks: everything else in this seam.

Not covered, and cheap for a Sonnet-tier agent to add as `firmware/tests/test_*.c` in the existing convention:
1. **`persist.c` slot selection + RMW + CRC + boot load** behind an injected `btstack_tlv_t` (three function pointers). Highest value: it would have caught the construct-from-scratch bug T1 fixed and would catch F-bt-08's marker rewrite. Requires lifting the pure parts of `persist.c` into an includable unit (or the copy-verbatim convention the other tests use).
2. **`debug_remote.c` parsers** (`parse_line`, `parse_connect_addr`, `parse_skip_ticks`): pure functions, 30 lines of test.
3. **`bt.c` MPSC ring and pending queue** as a model: full-ring drop accounting, name clamp, and (after F-bt-04) the `bool` return.
4. **`pl_prio` padding/round-robin** and **`pl_log_ring` wrap + drop-whole-line**: both are self-contained C with no SDK dependencies beyond `save_and_disable_interrupts` (stub).
5. **Panic-recorder scratch protocol** (magic/retry/clear matrix) with a fake `watchdog_hw`.
6. **flash_lockout policy** (F-bt-02) with fake `multicore_lockout_*` returning false.

## 6. Open questions for Andreas

1. **Discoverability** (F-bt-09): is there any product reason for the dongle to be discoverable/bondable outside the pairing wizard? If not, turning it off is a two-line change with a real robustness win.
2. **Settings writes while streaming** (F-bt-06): the "just write" ruling was made for a page program; a bank migration erase is the same call site and up to two orders of magnitude longer. Accept, or bound?
3. **Watchdog step 7** (F-bt-07): go/no-go on `PL_WDT_OBSERVE_ONLY=OFF`, with the soak's `max_stale_ms` numbers in hand.
4. **Cancel during SSP** (design §S3): should a cancelled attempt's half-formed bond be dropped (`gap_drop_link_key_for_bd_addr`) so the headset does not keep auto-reconnecting to a dongle the user aborted? UX call, then one line in F-bt-01's C3.
5. **Fault retirement lag**: by design a fault that cleared in C stays on Home until Rust's 120 s retire. Is that the intended feel?

## 7. Unverifiable here (needs the board)

- F-bt-02's trigger (core1 missing a 20 ms FIFO handshake) and the exact TLV aftermath.
- F-bt-03: that BTstack accepts and completes a sink-initiated session on this build (`ENABLE_A2DP_EXPLICIT_CONFIG` + `MAX_NR_HCI_CONNECTIONS 1`); the code path that follows is certain.
- F-bt-05: whether the CYW43439 serialises page behind inquiry or runs them concurrently (cost, not correctness).
- F-bt-06: whether the ISO-OUT "one missed re-arm is permanent" wedge is still reachable with sdk-patch 04 (`PL_USB_ISO_XFER_ISR=ON`).
- SRAM retention of `.uninitialized_data` across a watchdog reset (claimed proven by prior sessions; consistent with the RP2350 PSM behaviour).
- The panic recorder's report on the boot after a real HardFault on core1.
- The CDC protocol end to end (`cdc_sender.py` ↔ `debug_remote.c`) on macOS.
