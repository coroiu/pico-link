# Device switch: break-before-make (bead pico-link-sfw6)

Ada, 2026-09-25, against main 93450b4. Design only.

## Rulings (Andreas, bead comments, supersede the description)

- No dual connection. Pools stay at 1 (btstack_config.h:51 HCI, :85 AVDTP).
- Switch = disconnect A, then connect B.
- B fails -> stay disconnected. Never reconnect A.

## 1. Where the decision lives: firmware

`Command::Connect{B}` keeps its current meaning in core ("make B the device")
and core stays unchanged in shape. The firmware turns a Connect that arrives
while an ACL is OPEN into a switch.

Why not core (Disconnect, wait, Connect): the gate is a BTstack resource fact,
not a policy. The HCI slot is freed by `hci_shutdown_connection` AFTER the
DISCONNECTION_COMPLETE event has been emitted to every handler (btstack
hci.c:4707-4718). Core only ever sees `LinkStateChanged(Idle)`, which a2dp.c
pushes at SIGNALING_CONNECTION_RELEASED (a2dp.c:4093-4125) -- before the slot
is free. Core would need a new "radio idle" event to do it right, which leaks
a pool-of-one fact across the seam. Core already issues a plain Connect from
Devices row (devices.rs:166-170) and device page X (device_page.rs:425); both
become working switches with zero core logic change.

## 2. Teardown: gap_disconnect the ACL, then wait for the slot

`pl_a2dp_disconnect` (a2dp.c:4387) only calls `a2dp_source_disconnect` ->
`avdtp_disconnect` (btstack avdtp.c:1115-1123), which closes the AVDTP L2CAP
signaling channel only. AVRCP's channel keeps the ACL up and the HCI slot
stays occupied. The switch must use `gap_disconnect(handle)` (hci.c:9076) on
A's ACL: one call tears down the ACL, and L2CAP closes every channel on it
(AVDTP media + signaling, AVRCP). Existing handlers then do the rest, with no
new code:

- STREAM_RELEASED (a2dp.c:4058) and SIGNALING_CONNECTION_RELEASED (a2dp.c:4093):
  core1 quiesce (`pl_a2dp_core1_quiesce_and_wait`), watchdog disable, tx flush,
  `pl_pcm_reset`, media timer removal, urgent persist flush, `a2dp_cid = 0`,
  `pl_bt_push_link_state_disconnected`. The encoder and USB ring stop cleanly
  here already. The SIGNALING handler is defensive against firing first.
- AVRCP_SUBEVENT_CONNECTION_RELEASED (a2dp.c:1241) clears `s_avrcp_cid`.
- bt.c's DISCONNECTION_COMPLETE (bt.c:935).

Do NOT page B from inside any of those handlers. The slot is still allocated
while they run. Page B from the next heartbeat (`pl_bt_wdt_heartbeat_handler`,
bt.c:1251, 100ms, same run-loop context), and only when no ACL
`hci_connection_t` of any state remains AND `a2dp_cid == 0`.

### State machine (bt.c, run-loop context only)

```
s_switch.state : NONE | WAIT_ACL_DOWN | PAGING
s_switch.target: bd_addr_t
s_switch.deadline_ms
```

- `pl_bt_pending_service` CONNECT case (bt.c:1143) calls a new
  `pl_bt_connect_or_switch(addr)` instead of `pl_a2dp_connect` directly:
  - If an OPEN ACL exists (same iterator test as `pl_bt_any_acl_up`, bt.c:~691):
    store the target, set state WAIT_ACL_DOWN, set deadline = now + 5000ms, push
    ConnectStep Disconnecting, call a new `pl_a2dp_prepare_switch()` (cancel the
    0x0b retry and the wizard-dismiss timer, same helpers as a2dp.c:3182/3230),
    call `pl_bt_update_scan_mode()` (now forces connectable=0), then
    `gap_disconnect(handle)` for every OPEN ACL. COMMAND_DISALLOWED (already
    disconnecting) is fine: keep waiting.
  - If state is already WAIT_ACL_DOWN: overwrite the target (last press wins).
  - Otherwise (no ACL, or an ACL that is not OPEN, i.e. an attempt already in
    flight): call `pl_a2dp_connect(addr)` exactly as today. The in-flight case
    keeps today's instant 0x56 failure. No regression, no new scope.
- Heartbeat, after `pl_bt_pending_service()` (bt.c:1255):
  - WAIT_ACL_DOWN, slot free (no ACL of any state, `pl_a2dp_session_idle()`):
    state = PAGING, push ConnectStep Connecting, `pl_a2dp_connect(target)`.
  - WAIT_ACL_DOWN, past the deadline: log, push `ConnectFailed(target,
    RadioError)`, state = NONE, `pl_bt_update_scan_mode()`. A late
    DISCONNECTION_COMPLETE then only produces Idle. It must never page B,
    because the state has been cleared.
- Attempt end: `pl_bt_push_connect_failed` (bt.c:371) and
  `pl_bt_push_connect_succeeded` (bt.c:361) set state = NONE and call
  `pl_bt_update_scan_mode()` when state == PAGING. These two functions are the
  only terminal outcomes a2dp.c reports, so hooking them covers every B failure
  path (a2dp.c:3168 rejected, 3287 signaling failed, 3503/3579/3644
  post-signaling), with no a2dp.c edits per path.
  - Check these push functions' calling context first. If any caller is
    thread context, set a flag and do the scan update on the heartbeat
    instead: `gap_connectable_control` is a BTstack call.

## 3. What the UI shows (reuse the connecting axis from 0cq2)

| Stage | Events | Wizard | Home glyph / hero |
|---|---|---|---|
| Press A on B's row | Command::Connect; C pushes LinkState CONNECTING -> ConnectAttemptStarted | Connecting{B, Connecting} | Live, A's codec (connecting=true, link untouched) |
| Switch starts | ConnectStepChanged(Disconnecting) NEW | "Disconnecting" | Live, A |
| A released | LinkStateChanged(Idle) (a2dp.c:4125) | unchanged | Busy (connecting still true, fold.rs:268-305) |
| Paging B | ConnectStepChanged(Connecting), then SettingUpAudio (a2dp.c:3171), NegotiatingCodec | step labels | Busy |
| Success | ConnectSucceeded(B), LinkStateChanged(Connected), CodecChanged(B) | success/auto-dismiss | Live, B |
| Failure | ConnectFailed(B, reason) | Failed{B, reason} | Idle / NO LINK |

The only new wire item is `PL_CONNECT_STEP_DISCONNECTING 4u` (a2dp.h:175
family), `PlConnectStep::Disconnecting` (ui-ffi lib.rs:1111-1126) and
`ConnectStep::Disconnecting` with the label "Disconnecting" (events.rs:456).
This is additive, and C and Rust ship in one binary. No ABI version bump is
needed because the payload shape is unchanged. Keep it, because A's teardown
can take up to 5s in the worst case, and "Connecting" would be a lie then.
Core's fold for LinkStateChanged(Idle) already keeps `connecting` true
(fold.rs:285-296, written for exactly this flow). No other core logic changes.

## 4. Interactions

- **Auto-reconnect.** Core's only reconnect policy runs on StoreLoaded at boot
  (fold.rs:167-187). A's disconnect triggers nothing in core. The firmware has
  one retry: the 0x0b retry, which `pl_a2dp_prepare_switch` cancels and which
  retries only `pending_addr` (B). The remaining risk is A paging US back after
  the ACL drops. `pl_bt_update_scan_mode` turns page scan back on at
  DISCONNECTION_COMPLETE (bt.c:943), and A is a known device, so the pigd filter
  admits it. With a pool of one, A would take the slot and B's page would fail.
  Fix: the scan gate below.
- **Scan-mode owner.** `pl_bt_update_scan_mode` (bt.c:711) stays the ONLY
  caller of `gap_connectable_control`. Change one expression to
  `connectable = !pl_bt_any_acl_up() && s_switch.state == NONE`, and mirror it
  in `pl_bt_scan_connectable` (bt.c:731). Page scan is off from the switch
  start to the switch end, so A cannot re-page into the window. After a B
  failure, scan comes back on. If A later pages in by itself, that is A's
  choice and not our reconnect. Most headsets do not re-page after a clean
  host-initiated disconnect (reason 0x16 on their side).
- **ENABLE_EXPLICIT_CONNECTABLE_MODE_CONTROL.** Untouched. No new
  `gap_connectable_control` call site, and no `l2cap_register_service` at
  runtime.
- **MRU persistence.** Unchanged. B is saved at STREAM_ESTABLISHED
  (a2dp.c:3843) and bumped by core's PersistDevice on ConnectSucceeded. On
  failure the MRU stays A, so the next boot auto-reconnects to A (the last
  device that worked). This is intentional: boot is a new session, not the
  failed switch. Flag it to Andreas in the hardware check in case he wants
  otherwise.
- **Debug path.** `pl_bt_debug_connect` (bt.c:1620) goes through the same
  PENDING_CONNECT, so it gets switch behaviour for free. That is useful for
  unattended tests later.

## 5. Failure paths

| Case | Detection | Result |
|---|---|---|
| B off / out of range | SIGNALING_CONNECTION_ESTABLISHED status 0x04 page timeout (~5.1s), a2dp.c:3287 | ConnectFailed(B, timeout reason); disconnected; scan on |
| B refuses / auth / no sink | a2dp.c:3287 (status), 3503/3579/3644 (post-signaling) | ConnectFailed(B, ...); disconnected |
| establish_stream rejected synchronously | a2dp.c:3168 | ConnectFailed(B, RadioError). Should not happen once the slot is verified free, but covered |
| A's disconnect never completes | heartbeat deadline, 5s | ConnectFailed(B, RadioError). A is still up at HCI (upper layers only tear down on completion), so the model still says Connected A, which is honest. A late completion goes to Idle and never pages B |
| Second Connect mid-switch | state WAIT_ACL_DOWN | target overwritten; while PAGING -> today's 0x56 |

Pre-existing gap, not introduced here: a post-signaling failure on B can leave
B's ACL up. This already happens on connect-from-idle. Out of scope.

## 6. Related finding: user Disconnect is AVDTP-only

`PL_COMMAND_TAG_DISCONNECT` (bt.c:1462) -> `pl_a2dp_disconnect` ->
`avdtp_disconnect` leaves AVRCP, and therefore the ACL, up. The headphones may
still see us as connected and refuse the phone. Recommended fold-in (a few
lines): factor the "gap_disconnect every OPEN ACL" helper once and use it from
both the switch and PENDING_DISCONNECT. Hardware step 5 confirms it.

## 7. Tests

Core (core/src/app/tests.rs):
- S1 `switch_success_passes_through_disconnecting`: A connected; ConnectAttemptStarted;
  ConnectStepChanged(Disconnecting) -> wizard step label "Disconnecting", glyph Live;
  LinkStateChanged(Idle) -> glyph Busy; ConnectStepChanged(Connecting); ConnectSucceeded(B)
  + Connected + CodecChanged(B) -> connected_addr B, connecting false.
- S2 `switch_failure_stays_disconnected_and_never_reconnects_a`: same through Idle, then
  ConnectFailed(B, Timeout) -> link Idle, connected_addr None, connecting false, wizard
  Failed{B}, and `poll_command()` yields no Connect{A} (drain all queued commands).
- S3 ui-ffi decode: wire step 4 -> Disconnecting; unknown 5 still rejected.
- S4 headless wizard capture with the Disconnecting step; commit the PNG; inspect zoomed.
Firmware: cross-compile the .uf2 (no host tests for bt.c). Required console lines:
`BT: switch start target=.. handle=..`, `BT: switch ACL down, paging ..`,
`BT: switch timeout waiting for ACL down`, `BT: switch end outcome=..`.

## 8. Hardware check for Andreas (about 2 min, A = 94:DB:56:54:7C:F2 + any second paired headset B)

1. A playing. Devices -> A on B's row. Expect: wizard "Disconnecting" briefly, then
   connecting steps, then success. B plays, and Home shows B's codec. Console: scan
   `connectable=0` throughout.
2. Switch back to A the same way. Audio continues.
3. With A playing, power B off, then select B. Expect: A goes silent, and about 5s
   later the wizard shows Failed. Home shows NO LINK. A does NOT come back. Wait
   30s: still disconnected, console `connectable=1`.
4. A's own button/phone: A is free (not held by the dongle).
5. Device page X-disconnect on A, then check the console for
   HCI_EVENT_DISCONNECTION_COMPLETE (confirms the section 6 fold-in).
6. Reboot: reconnects to whichever device last succeeded.

## 9. Beads

One implementer bead (Ruby): bt.c state machine and scan gate (~80 lines),
a2dp.c `pl_a2dp_prepare_switch` / `pl_a2dp_session_idle` (~15), the additive
ConnectStep wire value across a2dp.h / ui-ffi / core (~15), and core tests.
It is one concept across the seam and does not split. The section 6
Disconnect fold-in rides along (same helper).
