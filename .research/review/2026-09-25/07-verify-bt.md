# Adversarial verification of `02-bluetooth-persist-supervision.md`

Tree: working tree at `1104a58`. `git diff --stat 2e37164 HEAD -- firmware core ui-ffi` is empty, so the firmware/core under review is identical to the reviewed tree.
Upstream sources: fetched fresh from GitHub, not taken from the reviewer's scratchpad. pico-sdk **2.1.1** `pico_flash/flash.c`, `pico_btstack/btstack_flash_bank.c`, `pico_multicore/multicore.c`. BTstack **v1.6.2** `platform/embedded/btstack_tlv_flash_bank.c`, `src/hci.c`, `src/l2cap.c`, `src/classic/{avdtp,a2dp,a2dp_source}.c`.
Caveat: the GitHub API returned 403, so I could not read the exact submodule SHA that pico-sdk 2.1.1 pins. I assumed v1.6.2, the release that SDK series ships. The reviewer read BTstack `master`. Every line I rely on below reads the same way in both.

Summary: F-bt-01 **CONFIRMED**. F-bt-02 **PARTIALLY CONFIRMED** (severity lowered to P2). F-bt-03 **CONFIRMED**. Spot-checks: F-bt-05 **CONFIRMED** (plus a stronger consequence), F-bt-06 **CONFIRMED**. New findings: 1 (F-bt-V1).

---

## F-bt-01 — CancelConnect is a silent no-op: CONFIRMED

**Evidence (independent trace)**
- `bt.c:1007-1174` has a case for each of these tags: START_SCAN, CONNECT, PERSIST_DEVICE, CANCEL_SCAN, FORGET_DEVICE, DISCONNECT, SET_DEVICE_LDAC_QUALITY, SET_DISPLAY_SETTINGS, NONE. Tag 4 (`PlCommandTag::CancelConnect = 4`, `ui-ffi/src/lib.rs:2315`) falls to `default: pl_wdt_mark(PL_WDT_CP_CMD_OTHER)` at `bt.c:1171-1173`.
- `grep -n "epoch\|attempt_live\|cancels_\|pl_a2dp_service_cancel\|pl_a2dp_cancel" firmware/src/*` returns nothing. None of design C1–C6 exists.
- Core does emit the command: `wizard.rs:392-395` pushes `Command::CancelConnect { addr }` on Back in `Connecting | NotResponding`. `Navigator` then pops the screen.
- Nothing else mitigates it:
  - The only timeout is the controller page timeout (≤5.12 s, `a2dp.c:2918-2922`). It covers only the ACL phase, and its `ConnectFailed` is still delivered.
  - `pl_a2dp_wizard_dismiss_timer_arm` is armed on every success (`a2dp.c:3645-3659`), so a late success also produces a pop-to-root 2 s later.
  - Nothing in `a2dp.c` disconnects a success nobody asked for.
- `fold.rs:141-146` `on_connect_succeeded` sets `WizardPhase::Succeeded` and `connected_addr` unconditionally, so design C7 is absent too.
- Late `STREAM_ESTABLISHED` → `pl_persist_save_device_now` + `ConnectSucceeded` (`a2dp.c:3624, 3643-3646`). So pressing B during Connecting really does leave the attempt running, and it can finish into a persisted, live stream.
- Sub-claim "a sink that opens signaling and idles leaves Connecting forever": in the v1.6.2 source I found no AVDTP response timer, only `retry_timer` for the `incoming_declined` retry at `avdtp.c:808-827`. Plausible, but I **did not verify** it end to end. Treat it as Low.
- Sub-claim (d), DISCONNECT during S1 → synthetic failure toast: true in code (`a2dp.c:3062-3077` pushes `ConnectFailed` for any non-success status), but hypothetical today. `bt.c:1107-1117` states that no screen queues DISCONNECT yet; only debug-remote does.

**Severity: P1** (agree). **Tier: Opus** (agree: `a2dp.c` connect state machine, IRQ context).

**Fix-sketch corrections**
- Remove C7 from this bead. It belongs to F-app-01's fix, see the dedup ruling. F-bt-01 becomes "C1–C6, C side only".
- The design's own "C1–C4 must land together" constraint stands.

**Dedup ruling**
- **F-app-03, CancelConnect half = F-bt-01.** It is the same defect with the same missing `case` at the same line. Merge it into F-bt-01's bead and do not file it twice.
- **F-app-03, ConnectRetrying half is a separate defect.** Phase 5 has no firmware producer (`grep CONNECT_RETRYING firmware/src` is empty). Give it its own bead, P2: either implement the producer or delete the phase.
- **F-app-01 is a separate root cause.** Core folds connection events into global wizard state with no wizard-open or addr scoping, and auto-dismiss keys on the phase rather than the stack. It bites with no cancel involved at all (the boot auto-reconnect sequence), and a C-side cancel does not fix it.
  - F-app-01's fix items (1) and (4) subsume design C7, so C7 moves to F-app-01's bead.
  - The two beads are independent. Both are needed before "B during Connecting then late success" is fully correct: F-app-01 stops the UI yank, F-bt-01 stops the audio.
  - Relate the two beads. Do not add a dependency.

## F-bt-02 — flash-lockout START timeout is not benign: PARTIALLY CONFIRMED

**Confirmed: the silent-corruption mechanism**
- `flash_lockout.c:146-157` (not `:212-222` as cited; the file is 211 lines): on `!multicore_lockout_start_timeout_us(...)` it does `s_timeout_count++; return PICO_ERROR_TIMEOUT;`.
- pico-sdk `flash.c:75-84`: `flash_safe_execute` skips `func` and returns `rc`.
- `btstack_flash_bank.c:73-76` (erase) and `:161-164` (each 256 B page program) call `flash_safe_execute(...)` and drop the rc. The comment reads "currently we have no way to return an error". `hal_flash_bank_t` erase/write are `void`.
- BTstack `btstack_tlv_flash_bank_store_tag` (`:435-498`) writes the value, then the header, then the delete-marks, then does `self->write_offset += ...` and `return 0`. It cannot see the skip.
- `persist.c:622, 670` ignore the `store_tag` rc. `persist.c:677-697` updates `s_slots` and pushes `PairedDeviceUpserted` anyway, so the UI echo claims a write that never happened. `persist.c:805` clears `s_pending`.
- Aftermath:
  - A skipped header leaves `tag == 0xFFFFFFFF`. The iterator (`:191-194`) stops there.
  - This bites **immediately in-session**, not only on the next boot: `get_tag` and `delete_tag_until_offset` walk the same iterator, so BTstack link keys stored after the gap are unreadable at once.
  - On the next boot, the init scan (`:560-581`) finds "not erased after last tag" and migrates only the entries before the gap.
  - A skipped migrate erase (`:259-266`) leaves the other bank un-erased, and the following programs AND onto stale data.
- All confirmed as stated.

**Refuted: "core1 parks forever … every later START also times out"**
- Traced in `multicore.c:213-226, 260-296`, assuming a merely-late core1:
  1. When core1 finally takes the stale START, it disables IRQs, pushes a START echo into core0's RX FIFO, then spins on `pop != END`.
  2. The **next** core0 START pushes START2, which core1 pops and ignores. Core0 then pops the **stale echo**, `word == magic`, so `rc = true`.
  3. The lockout proceeds correctly, because core1 really is parked in RAM. END then releases core1 cleanly.
- So core1 is parked (IRQs off, encoder dead) only **until the next flash write**, and that write succeeds.
- A2DP `STREAM_ESTABLISHED` itself performs a flash write (`pl_persist_save_device_now`, `a2dp.c:3624`). The ENCODER watchdog is enabled only from `STREAM_STARTED` (`a2dp.c:3675`), after that write, so a parked core1 is normally un-parked before anyone would notice.
- "Every later START times out" is true only when core1 is **permanently** dead. In that case every flash write that boot is silently dropped while the UI echoes success. That is the real worst case, and the report did not isolate it.

**Trigger reachability (why I lower the severity)**
- core1 is launched at boot (`a2dp.c:2556`) and registers as victim first (`:2372`).
- Grep of core1 code and `codec_ldac.c` for `save_and_disable|spin_lock_blocking|critical_section_enter|mutex_enter|irq_set_enabled`: none.
- An infinite loop in thread mode still takes the FIFO IRQ, and a core1 HardFault reboots via the recorder's 500 ms watchdog.
- So a START timeout needs a core1 that is wedged with interrupts effectively blocked and has not faulted. That already requires another defect.
- The header's "benign and recoverable" claim (`flash_lockout.h:39-42`, `flash_lockout.c:51, 149-155`, and ADR 2026-09-03 §7.1 as quoted there) is **false**, and the fix is cheap. The report's "High" confidence on the consequences is fair. P1 overstates how likely this is to bite within the roadmap.

**Severity: P2** (was P1): a latent data-loss path that only follows a separate core1 defect. The doc claim is wrong. **Effort: S. Tier: Sonnet.** The change mirrors the existing END-timeout branch 25 lines below in the same file, with no ownership change.

**Fix-sketch correction**
- Only option (a) is sound: panic into the recorder on a START timeout, with a distinct message, exactly like END. Also correct the header and ADR text.
- Option (b), "keep `s_pending`, re-init TLV", does not work:
  - It cannot cover BTstack's own `put_link_key` writes on the same instance.
  - A re-init scan migrates around the gap and costs a 4 KB erase.
  - It leaves the stale START in core1's FIFO, which parks core1 on its next FIFO IRQ.
  - Drop option (b).

**Verification:** the report's static grep is fine. Add a host model test (`firmware/tests`) with fake `multicore_lockout_start_timeout_us` returning false that asserts the panic hook is called.

## F-bt-03 — incoming A2DP attributed to the last outgoing target: CONFIRMED

**Evidence**
- `a2dp.c:3060-3119` `SIGNALING_CONNECTION_ESTABLISHED` reads only `status` and `a2dp_cid`. `grep get_bd_addr firmware/src` finds only inquiry results (`bt.c:575`) and a comment.
- `s_ctx.connect_addr` has a single writer: `pl_a2dp_establish_stream_now` at `a2dp.c:2953`. It is zero-initialised (static).
- Its consumers:
  - `avrcp_connect(s_ctx.connect_addr, …)` at `:3112`. The report missed this: AVRCP is dialled to the wrong or all-zero address.
  - `pl_persist_get_device_settings` at `:2875`, which applies the wrong device's LDAC pin.
  - `CodecChanged` at `:2897`.
  - `pl_persist_save_device_now` at `:3624`.
  - `ConnectSucceeded` at `:3646`.
  - `pl_a2dp_is_connected_ldac` at `:4119`.
- Is an incoming connection possible? **Yes, from source:**
  - `avdtp_init` → `l2cap_register_service(PSM_AVDTP)` (`avdtp.c:1664`) → `gap_connectable_control(1)` (`l2cap.c:5025`). `ENABLE_EXPLICIT_CONNECTABLE_MODE_CONTROL` is not defined in `btstack_config.h`, so page scan is on.
  - `avdtp.c:904-950` accepts `L2CAP_EVENT_INCOMING_CONNECTION` when no outgoing signaling is active.
  - BTstack `a2dp.c:519-533` emits `A2DP_SUBEVENT_SIGNALING_CONNECTION_ESTABLISHED` and, in the SOURCE role, **itself starts SEP discovery** for the incoming case.
  - Pico Link's `CAPABILITIES_COMPLETE` handler then configures on `s_ctx.a2dp_cid`, which matches, so the session runs through to `STREAM_ESTABLISHED`.
  - `gap_discoverable_control(1)` (`a2dp.c:4046`) is not needed for this: page scan suffices.
- The persisted name is safe: `pl_bt_get_connect_target_name` (`bt.c:325-333`) address-matches, so the wrong device gets no name rather than the wrong name.

**Exposure nuance**
- With one remembered headset, a failed boot auto-reconnect has already set `connect_addr` to that headset, so the attribution is usually right by accident.
- The wrong cases are:
  - the headset pages within the ~100 ms between `HCI_STATE_WORKING` and the queued Connect (all zeros);
  - an empty store with a still-bonded headset;
  - two headsets (next milestone).

**Severity: P1** (agree, justified by the next milestone). With a single headset the MVP exposure is narrow. **Confidence:** raise to Medium-High. The acceptance path is now traced through BTstack source, and only on-air behaviour is unverified. **Tier: Sonnet** as a standalone fix. It becomes Opus if it is done inside F-bt-01's epoch work.

**Fix-sketch correction:** on success, copy `a2dp_subevent_signaling_connection_established_get_bd_addr` into `s_ctx.connect_addr` **before** the `avrcp_connect` call at `:3112`. Push `LinkState(Connecting)` only when no outgoing attempt is in flight for that addr. Everything else as sketched.

## Spot-check F-bt-05 — unconditional boot inquiry: CONFIRMED, consequence understated

- `bt.c:641-643`: `pl_bt_push_store_loaded(...)` then `pl_bt_start_scan()`. The scan pushes `DevicesCleared` + `Scanning` and calls `gap_inquiry_start`, unconditionally on `HCI_STATE_WORKING`.
- BTstack `hci.c` has no inquiry/page serialisation (no `inquiry_state` check on the create-connection path), so the page for the auto-reconnect overlaps the inquiry. Only the radio cost is unverifiable.
- Missed consequence:
  - `pl_bt_start_scan_radio` (`bt.c:541-544`) ignores `gap_inquiry_start`'s return. It returns `ERROR_CODE_COMMAND_DISALLOWED` while `inquiry_state != IDLE` (`hci.c:9388`).
  - So if the user opens "Pair headphones" during the first ~10 s after boot, the wizard's START_SCAN clears the list and starts nothing.
  - The list is then fed only by the tail of the boot inquiry. Devices already reported are gone, because controllers normally report each device once per inquiry (Medium).
  - The scan also ends early, at the boot inquiry's complete event.
- **Severity: P2** (agree). **Tier: Haiku** (agree: delete one call). Also log the rc in `pl_bt_start_scan_radio`, which is F-bt-V1.

## Spot-check F-bt-06 — ungated settings writes incl. migration erase: CONFIRMED

- `persist.c:959-986`: the LDAC-quality and display blocks enqueue with no streaming check. The comments cite Andreas's "just write" ruling.
- `persist.c:1003-1005`: the execute path re-checks nothing.
- Each write goes through `pl_persist_rmw` → marker `store_tag` + record `store_tag` (`persist.c:622, 670`). Each `store_tag` may call `migrate` (`btstack_tlv_flash_bank.c:441-443`) → `erase` of `PICO_FLASH_BANK_SIZE` = 4 KB (`btstack_flash_bank.c:42-48`) inside one `flash_safe_execute`, so core0 IRQs are off and core1 is parked for the full sector erase.
- Alignment is 1 (`btstack_flash_bank.c:31-34`), so roughly 60 settings picks cost one migration. The report's "~60×" repro is consistent with that.
- The pairing path gates on `pl_usb_audio_streaming()` (`persist.h:70-84`); the settings path does not. The contradiction is real.
- Addition: `persist.h`'s "~9 ms worst case" for the pairing-time write also ignores that the same `store_tag` can trigger the migrate erase.
- **Severity: P2** (agree). **Tier: Opus** (agree, as it is a policy question). Andreas's product call stands, see open question 2.

---

## New findings

### F-bt-V1: `pl_bt_start_scan_radio` discards `gap_inquiry_start`'s status, so a user scan during the boot inquiry is silently a no-op
- Severity: P2   Confidence: Medium (code certain; how many results are lost depends on controller duplicate filtering)   Effort: S   Tier: Haiku
- Location: `firmware/src/bt.c:541-544`, `bt.c:1008-1019`, `bt.c:641-643`; BTstack `hci.c:9388`
- Evidence: `gap_inquiry_start(PL_INQUIRY_DURATION_UNITS);` has its return value unused. BTstack returns `ERROR_CODE_COMMAND_DISALLOWED` while any inquiry is active. The START_SCAN command has already pushed `DevicesCleared` + `DiscoveryState(Scanning)` inline before the deferred radio call.
- Why it matters: while the boot inquiry (F-bt-05) or any earlier scan is still running, a user scan clears the list and starts nothing. The discovered list then shows only devices the old inquiry has not reported yet, and "Scanning" ends at the old inquiry's deadline. It fails silently: no log line, no UI signal.
- Fix sketch: check the return value. On `COMMAND_DISALLOWED`, `gap_inquiry_stop()` and re-issue on `GAP_EVENT_INQUIRY_COMPLETE` (or just log and let F-bt-05's deletion remove the common trigger). Log any other non-zero status.
- Verification: after the fix, a boot capture followed by an immediate `--raw "NAV ..."` into Pair shows `BT: starting GAP inquiry` with status 0 and a full 10.24 s scan. Grep check: the `gap_inquiry_start(` return value is assigned and logged.
- Related: F-bt-05, `pico-link-znb.2`

## Minor corrections to the report

- `flash_lockout.c` line citations are stale: START is `:146-157`, END is `:162-185`, and the file ends at 211. Every other line I checked matched.
- F-bt-01 lists "Connecting forever for an idle sink" as certain. I could not confirm that AVDTP has no response timeout; mark it Low.
