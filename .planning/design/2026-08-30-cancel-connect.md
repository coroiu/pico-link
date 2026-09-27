# Cancel-connect semantics

**Bead:** `pico-link-2pq` · **Author:** Ada (architect) · **Date:** 2026-08-30
**Status:** design of record

No hardware was available. Every conclusion is marked **[src]** (derived from
source read on the day) or **[measure]** (needs on-target confirmation). The
orchestrator independently re-verified the two load-bearing claims before this
was committed.

## Context

`core/` emits `Command::CancelConnect { addr }` from `core/src/render/wizard.rs:309`,
only from `WizardPhase::Connecting` and `WizardPhase::NotResponding`. It reaches
C as `PL_COMMAND_TAG_CANCEL_CONNECT = 4` (`pico_link_ui.h:75`) and falls through
`default: break` at `bt.c:519-520`. Safe, but a no-op. **[src]**

Consequence: B leaves the wizard screen and the abandoned attempt keeps running
in C, still delivering `ConnectStepChanged` / `ConnectRetrying` / `ConnectFailed`
/ `ConnectSucceeded`. **This is not cosmetic.** `App::on_connect_succeeded`
(`core/src/app.rs:738-741`) sets `WizardPhase::Succeeded` **unconditionally** —
unlike its two siblings `on_connect_step_changed` (`:707`) and
`on_connect_retrying` (`:723`), which both guard on the current phase. So a late
success after an abort re-opens a success state for a device the user walked
away from, with a live A2DP stream behind it. **[src]**

"Cancel" is not one operation. The attempt passes through an SDP query, an ACL
page, an SSP exchange, AVDTP signalling, codec configuration and stream open,
and these abort differently — several not at all.

## Decision

**D1. Abort is `a2dp_source_disconnect(s_ctx.a2dp_cid)`, and nothing else.** It
is the only BTstack call that does anything useful at any stage, and it is valid
from the instant `pl_a2dp_connect` returns, because `avdtp_connect` assigns the
cid synchronously (`avdtp.c:356-364`) before any radio work begins. **[src]**

**D2. Everything the abort cannot stop is handled by *ignoring its result*,**
via a monotone attempt epoch held in C. The epoch never crosses the FFI. `core`
is unchanged.

**D3. The guard on a late `ConnectSucceeded` is not "discard" — it is "discard
*and tear the stream down*".** This is the single most important rule in this
document. A cancel that loses the race and silently drops the success event
leaves audio flowing to the headphone the user aborted while the screen says
idle. That is strictly worse than today's no-op, because today at least the UI
and the radio agree.

**D4. No BTstack call is made from `pl_bt_poll_commands`.** The
`CANCEL_CONNECT` case does plain C bookkeeping; the teardown is serviced from
`pl_bt_wdt_heartbeat_handler` (`bt.c:448`), the existing permanent 100ms
run-loop timer.

**D5. `CancelConnect` never means "disconnect an established device".** That is
a different command with different UX, to be added when the
manage-connected-device screen lands.

**D6. The observable effect of a cancel is `LinkStateChanged(Idle)`, pushed
immediately.** The UI never waits for the radio.

## Rationale

### Per-stage abort table

| # | Stage | Abortable? | Race behaviour |
|---|-------|-----------|----------------|
| S0 | Command queued, `pl_a2dp_connect` not yet called | Yes, trivially — drop the queued command | none |
| S1 | AVDTP registered, SDP query pending or in flight | **AVDTP layer yes; SDP query no.** `a2dp_source_disconnect(cid)` -> `avdtp_disconnect` default arm, `avdtp.c:1128-1133` **[src]** | Emits a synthetic `SIGNALING_CONNECTION_ESTABLISHED(ERROR_CODE_UNSPECIFIED_ERROR)` **synchronously** and frees the connection. The SDP query keeps running — `sdp_client.h` exposes no abort. Its late result hits `avdtp_handle_sdp_client_query_result`, finds `NULL` for the freed cid, logs and returns (`avdtp.c:732-736`). **Not a use-after-free.** **[src]** |
| S2 | ACL page in progress (`hci_connection_t` in `SENT_CREATE_CONNECTION`) | **No.** `gap_connect_cancel()` is LE-only — it switches on `hci_stack->le_connecting_request` (`hci.c:8620-8642`). `hci_create_connection_cancel` exists as an opcode (`hci_cmd.h:386`) but **no BTstack path sends it and none handles its completion**. **[src]** | Runs to page timeout (<=5.12s). Result ignored by epoch. |
| S3 | SSP / authentication exchange | **No, and should not be** | Aborting mid-SSP can leave the remote with a half-formed link key. Let it complete; the AVDTP teardown that follows drops the ACL. Whether a cancelled attempt still leaves the headphone bonded is **[measure]** and a UX question for Andreas. |
| S4 | AVDTP signalling `OPENED`, discovery / get-capabilities in flight | **Yes, cleanly** (`avdtp.c:1120-1124`) **[src]** | Sweeps stream endpoints, `l2cap_disconnect` on signalling, state -> `W4_L2CAP_DISCONNECTED`. Async. Lands as `SIGNALING_CONNECTION_RELEASED`, already fully handled at `a2dp.c:1410-1426`. |
| S5 | `SET_CONFIGURATION` sent, stream opening | **Yes, but this is the dangerous window** | `avdtp_disconenct_streamendpoints` (`avdtp.c:1096-1113`) only acts on endpoints already `OPENED`/`STREAMING`. An endpoint **mid-open is missed** and can complete into `OPENED` after the teardown is queued. This is the concrete mechanism by which a cancel loses the race. Frequency **[measure]**; D3 is the backstop. |
| S6 | Stream `ESTABLISHED` / `STARTED` | Out of scope (D5) | C still tolerates it (D3 tears it down), but core never sends it here. |

### Why an epoch counter, and why in C

- **`addr` is not sufficient.** Cancel-then-retry-the-same-device is exactly the
  case an address comparison cannot separate, and it is the *most likely* user
  sequence after a failed attempt.
- **`core` cannot filter at all.** `ConnectSucceeded { degraded }`,
  `ConnectStepChanged(step)` and `ConnectRetrying { attempt }`
  (`app.rs:182/189/196`) carry **no address**. Only `ConnectFailed` does.
  **[src]** Filtering in core would need an ABI change to the surface
  `pico-link-a67` just stabilised, plumbed through every screen, to put a
  firmware-lifecycle concept into platform-free `core`. Rejected as a boundary
  smell: only C knows what an "attempt" is.
- **Every connect-lifecycle event has exactly one emit path.** The
  `pl_bt_push_connect_*` helpers (`bt.c:267-293`) plus
  `pl_bt_push_link_state_connected` (`:263`) and `pl_bt_push_codec_changed`
  (`:300`) are called only from `a2dp.c`. A single gate in front of those call
  sites suppresses every late event with no change anywhere else.

**Where it lives:** `pl_a2dp_ctx_t` in `a2dp.c` — `uint32_t attempt_epoch;
bool attempt_live; bool cancel_pending; bd_addr_t cancel_addr;`. The counter is
what makes the design correct across retries; the boolean is what makes each
check one instruction.

### Why the teardown is deferred to the heartbeat

`avdtp_disconnect`'s S1 arm **re-enters `pl_a2dp_packet_handler` synchronously**
(`avdtp.c:1130`, before the function returns). Calling it from
`pl_bt_poll_commands` would run that handler in **thread context**,
mid-superloop — a handler that today only ever runs in the cyw43/BTstack
background IRQ. That is precisely the single-context invariant `pico-link-6o2`
exists to restore. Deferring into `pl_bt_wdt_heartbeat_handler` (`bt.c:448`)
keeps the handler single-context by construction. **[src]**

Latency: <=100ms against a connect measured in seconds. **[measure, low risk]**
`btstack_run_loop_execute_on_main_thread` would give ~0ms but calls
`async_context_acquire_lock_blocking` — a blocking acquire added to the exact
switch `pico-link-okx` localises a >2s stall to. Rejected. **[src]**

### Stall-risk verdict (`pico-link-okx`)

**This design adds zero blocking work and zero BTstack calls to the dispatch
switch.** The `CANCEL_CONNECT` case is four field writes, a 6-byte `memcpy`, and
one `pl_bt_ring_push` — the same push the `CONNECT` case at `bt.c:510` already
does.

Separately, and **not this bead's fix**: `pl_bt_start_scan` (`bt.c:496`) and
`pl_a2dp_connect` (`bt.c:511`) already call BTstack API from thread context
**without taking the async_context lock**, while the background worker can run
concurrently under `pico_cyw43_arch_threadsafe_background`. That is
unsynchronised access to run-loop state and a plausible contributor to the okx
stall. **[src]** that the lock exists and neither call site takes it;
**[measure]** whether it is the cause. Filed separately.

### `pico-link-dgx` neighbour question

**Out of scope, and core's phase gating is a real guarantee — not an accident.**
`wizard.rs:308` matches `WizardPhase::Connecting | NotResponding` explicitly, and
`on_connect_succeeded` moves the phase to `Succeeded` the instant the stream is
up, so no B-press after success can produce a `CancelConnect`. Enforced by match
arms. **[src]**

Two caveats: it is *one screen's* guarantee — any future "disconnect this
device" affordance must emit a **new** `Command::Disconnect { addr }`; **do not
widen `CancelConnect`.** An abort is silent and unconfirmed, a disconnect is
user-visible and confirmed, and overloading one command with two lifecycles is
load-bearing and hard to undo. And D3 means C tolerates a post-establishment
cancel anyway — defence in depth without widening the contract.

### What the user sees

1. B pops the wizard (unchanged).
2. C pushes `LinkStateChanged(Idle)`. The Devices screen stops reading
   "Connecting...". **This is the entire observable difference from today's
   no-op**, and it is deliberately small: today's failure is invisible, and so
   is today's success.
3. No "Aborting..." spinner — the UI must not be tied to a stage we cannot bound
   (S2's page timeout is up to 5.12s). The user's model is "I pressed back, it's
   gone."
4. **No `ConnectFailed` toast for a user-initiated cancel.** A cancel that
   produced a failure screen would read as a bug.
5. A cancelled attempt followed immediately by a retry must not show a spurious
   failure. See C5.

## Alternatives

| | Why rejected |
|---|---|
| **A. Do nothing** | Late `ConnectSucceeded` overwrites the phase unconditionally (`app.rs:738`). Not benign. |
| **B. Filter in `core` by address** | Those events carry no address **[src]**, and address cannot separate cancel-then-retry-same-device. |
| **C. Epoch in `PlEvent`, filtered in core** | ABI change to the surface `a67` just stabilised, plumbed through every screen, and puts a firmware-lifecycle concept in platform-free `core`. |
| **D. Send `hci_create_connection_cancel` for S2** | No BTstack path sends or handles it; the resulting `Connection Complete(error)` would be processed against state BTstack never modelled as cancellable. **[src]** Revisit only if page-timeout latency proves user-visible **[measure]**. |
| **E. Synchronous `a2dp_source_disconnect` in the switch** | Stall risk (`okx`) plus synchronous cross-context re-entrancy into an IRQ-only handler (`avdtp.c:1130`). |

## Consequences

**Good:** B genuinely aborts. No late event reaches `core`. The dangerous S5 race
is not merely detected but corrected. `core` needs no change for the mechanism.
The counters make the race falsifiable on hardware.

**Costs and risks:**
- Up to 100ms between press and radio-side teardown. Invisible, but real.
  **[measure]**
- S2/S3 cannot be aborted: the page and any SSP exchange run to completion after
  the user has moved on. The device is briefly busier than the screen suggests.
- **A cancel during SSP may still leave the headphone bonded** to a pairing the
  user aborted. **[measure]** — needs a UX answer from Andreas (a "forget
  device" affordance) rather than a firmware one.
- `PL_FAILURE_REASON_*` gains no "cancelled" variant, on purpose: a cancel is not
  a failure and must not render as one.

## Implementation breakdown

C1-C4 are the minimum coherent unit and **must land together** — C1 without C2
gives a silent-success bug worse than today.

- **C1 — attempt epoch + emit gate** (`a2dp.c`). Add the four fields; set
  `attempt_live = true; attempt_epoch++` in `pl_a2dp_connect` (`:1512`). Route
  every connect-lifecycle emit (`pl_bt_push_connect_failed` at
  `:981/:1075/:1140/:1179`, `pl_bt_push_connect_step` at `:1150/:1523`,
  `pl_bt_push_link_state_connected` and `pl_bt_push_connect_succeeded` at
  `:1370/:1375`, `pl_bt_push_codec_changed` at `:1159`) through local
  `pl_a2dp_emit_*` wrappers that return early when `!attempt_live`.
- **C2 — act on late success** (`a2dp.c`). In `..._SBC_CONFIGURATION`,
  `STREAM_ESTABLISHED` and `STREAM_STARTED`: if `!attempt_live`, `pl_log` it, do
  **not** emit, do **not** init the codec or arm the media timer, and call
  `a2dp_source_disconnect`. **This is D3 and must not be dropped as
  "defensive".**
- **C3 — cancel entry point** (`a2dp.h`, `a2dp.c`, `bt.c`). New
  `pl_a2dp_cancel_connect(const uint8_t *addr)` — bookkeeping only, no BTstack
  call. New `case PL_COMMAND_TAG_CANCEL_CONNECT:` at `bt.c:519` calling it plus
  `pl_bt_push_link_state(PL_LINK_STATE_IDLE)`.
- **C4 — deferred teardown** (`a2dp.c`, `bt.c`). New `pl_a2dp_service_cancel()`:
  if `cancel_pending && a2dp_cid != 0`, call `a2dp_source_disconnect` and clear
  the flag. Call it from `pl_bt_wdt_heartbeat_handler` (`bt.c:448`).
- **C5 — connect-during-teardown hold** (`bt.c`, `a2dp.c`). While a teardown is
  outstanding, `a2dp_source_establish_stream` returns
  `ERROR_CODE_COMMAND_DISALLOWED` (`a2dp_source.c:171`), which
  `pl_a2dp_connect:1518-1521` turns into `ConnectFailed(RadioError)` — a
  spurious failure one press after a cancel. **[src]** Fix: stash the addr and
  issue the connect from the `SIGNALING_CONNECTION_RELEASED` handler
  (`a2dp.c:1410`). One-deep is sufficient.
- **C6 — counters** (`a2dp.c`). `cancels_requested`, `cancels_late_success`,
  `events_suppressed`, printed from `pl_a2dp_report`. **`cancels_late_success ==
  0` on its own proves nothing** about whether the S5 race exists — read it
  against a non-zero `cancels_requested`.
- **C7 — core consistency fix** (`core/src/app.rs:738`). Guard
  `on_connect_succeeded` on `Connecting | NotResponding`, matching its siblings
  at `:707`/`:723`. Independent of this bead and cheap defence if C regresses.
- **T1 — tests.** Core: `CancelConnect` emitted only from phases 4/5; C7's guard.
- **V1 — hardware verification, blocked.** Cancel at each of S1-S5; capture the
  `a2dp:` report lines. Open **[measure]** items: S3 bonding side-effect; S5
  late-establish frequency; heartbeat latency perceptibility.

---

## v2 (2026-09-27, Ada) -- attempt identity, incoming connections, the cid=0 timeouts

Supersedes C1-C4 above where they conflict. Keeps: bookkeeping-only cancel
(D4), heartbeat-deferred disconnect, held-connect reissue, bounded 3 s
fallback, debug CANCELCONNECT, the C6 counters. Line refs are branch
`bd-pico-link-chc3` @ `827fba3` unless marked `main:`.

### Findings

**F1 -- incoming connections are real, on main, today.** The jyhk.25 reviewer's
claim is wrong. `pl_bt_connection_filter` (`bt.c:880-900`, identical on main)
accepts ANY inbound ACL whose addr is in the persist store AND has a link key;
it has no reference to the 0x0b retry or any attempt. `pl_bt_update_scan_mode`
(`bt.c:805-819`) sets connectable whenever no ACL is up and no switch is
running. BTstack then accepts inbound AVDTP signaling
(`avdtp.c:1664` registers PSM; `avdtp.c:904-951` accepts unless an outgoing
signaling to the same addr is active). So a bonded headset powering on pages
us, and `A2DP_SUBEVENT_SIGNALING_CONNECTION_ESTABLISHED` fires with no local
attempt. With v1, `attempt_live == false` then kills it at the D3 guards
(`a2dp.c:3167`, `:3871`, `:4123`) and suppresses every emit (`:3113-3150`).
Two windows: cold boot before core's auto-reconnect, and permanently after any
cancel (`attempt_live` is only set at `:4677`).

**F2 -- `attempt_live` is never concluded.** It stays true after success or
failure; only a cancel clears it (`:4702`). A stale/duplicate CancelConnect
arriving at any later time tears down whatever session is live, including one
the user is happily streaming on.

**F3 -- why 3/3 cancels hit the timeout with cid=0x0000.** Tess cancelled
50-150 ms after CONNECT, i.e. during SDP query / L2CAP connect (S1/S2), before
signaling OPENED. Trace:
1. `pl_a2dp_cancel_connect` sees `a2dp_cid != 0` (written synchronously by
   `a2dp_source_establish_stream`'s out-param, `a2dp.c:3290` ->
   `a2dp_source.c:167`), arms the timer (`:4711`).
2. `pl_a2dp_service_cancel` calls `a2dp_source_disconnect` (`:4739`).
3. `avdtp_disconnect` (`avdtp.c:1128-1133`), for a connection not yet OPENED,
   does NOT close L2CAP: it synchronously emits
   `SIGNALING_CONNECTION_ESTABLISHED(status=0x1f)` and finalizes. BTstack's
   a2dp layer forwards it and cascades a `STREAM_ESTABLISHED` failure
   (`classic/a2dp.c:507-514`). **No `SIGNALING_CONNECTION_RELEASED` is ever
   emitted for a pre-OPEN connection.**
4. Our ESTABLISHED-failure branch sets `a2dp_cid = 0` (`a2dp.c:3484`) but never
   disarms the cancel timer or reissues a held connect -- only RELEASED does
   (`:4399-4418`). Timer fires 3 s later, logs `cid=0x0000` (`:3442-3445`).
The timer can only fire if `service_cancel` took the disconnect branch (its
`cid == 0` branch disarms it, `:4727-4736`), so the missing
"servicing pending cancel" lines were console loss, not a skipped path -- the
"cancel_connect -- attempt_epoch" line (`:4701`) was also missing while
`cancels_requested` incremented. Lesson for the test plan: read counters, not
log lines. Not a cid confusion: BTstack's a2dp cid IS the avdtp cid.

**F4 -- `held_connect_*` is not attempt-scoped.** Cancel never clears it
(`:4692-4718`), so a cancelled held attempt is reissued on the next RELEASED
or timeout (`:3447-3453`, `:4412-4418`).

**F5 -- incoming sessions report the wrong address (pre-existing, main).**
ESTABLISHED success copies `pending_addr` into `connect_addr` (`a2dp.c:3521`)
-- the last *local* attempt's addr, all-zero at cold boot -- and calls
`avrcp_connect(connect_addr)` (`:3547`). The event carries the real addr
(`a2dp_subevent_signaling_connection_established_get_bd_addr`).

**F6 -- a Connect to an already-connected device tears it down (pre-existing,
main).** `pl_bt_connect_or_switch` (`bt.c:1255-1283`) treats any ACL-up as a
switch. At cold boot, a headset that paged us first is disconnected and
re-paged by core's auto-reconnect.

**F7 -- core's addr-keyed marker is the same bug one layer up.**
jyhk.25's `cancelled_attempt: (seq, addr)` drops a `ConnectSucceeded` for that
addr while `attempt` is None -- which is exactly what a headset-initiated
reconnect after a cancel looks like. And if C does not actually tear down (main
today, where CancelConnect has no C handler), it drops the only success for a
live stream.

### Decision: core-allocated attempt `seq`, echoed by C

v1 rejected core-side filtering because events carried no identity and "only C
knows what an attempt is". jyhk.25 changed the premise: core now allocates
`ConnectAttempt::seq` (u16) at `radio_actions::connect`. The sustainable model
is one identity shared by both layers:

- **Core allocates, C echoes.** `Command::Connect { addr, name, seq }`,
  `Command::CancelConnect { addr, seq }`. C tags every connect-lifecycle event
  (`ConnectStepChanged`, `ConnectRetrying`, `ConnectSucceeded`,
  `ConnectFailed`) with the `seq` of the attempt that owns it.
- **`seq == 0` means "not core's attempt"**: a remote-initiated session, or the
  PL_DEBUG_REMOTE CONNECT bypass. Core's counter skips 0 and 0xFFFF on wrap.
  `0xFFFF` is `PL_SEQ_ANY`, used only by debug CANCELCONNECT ("cancel whatever
  is in flight").
- **Suppression is scoped to the cancelled cid, never global.** C remembers
  `cancel_cid` (sticky until the next cancel); an event is suppressed / D3
  torn down iff its own `a2dp_cid == cancel_cid`. Any other cid -- including
  every incoming session -- is untouched.
- **Incoming connections are always accepted and always reported.** C emits
  `LinkStateChanged(Connected)`, `CodecChanged`, and `ConnectSucceeded{seq:0}`
  for them exactly as for a local session. Core treats `seq:0` success as a
  session appearing: set connected addr, PersistDevice (MRU bump), never touch
  `attempt`/`WizardPhase`.
- **Why a seq and not an initiator flag:** a flag says "remote" but cannot tell
  cancelled-attempt-N's late echo from attempt-N+1 to the same addr
  (cancel-then-retry, the most likely sequence -- v1's own argument). The seq
  does both; `seq == 0` IS the initiator flag.

Rejected: keeping C-only suppression with a global flag plus special-casing
incoming (quick fix -- it keeps F2 and F4 and needs a new hack for every new
attempt source, e.g. the web HOST_OP connect). Rejected: core-only filtering by
addr (F7).

### C state (a2dp.c `pl_a2dp_ctx_t`)

Replace `attempt_live`, `cancel_addr`, `held_connect_pending/addr` with:

    struct { uint16_t seq; bd_addr_t addr; uint16_t cid;
             enum { ATT_NONE, ATT_HELD, ATT_IN_FLIGHT } phase; } attempt;
    uint16_t session_seq;      // seq owning the live a2dp_cid; 0 = remote/none
    uint16_t cancel_cid;       // sticky; 0 = none
    bool     cancel_disconnect_pending;   // was cancel_pending
    // attempt_epoch stays as the instrumentation counter

Rules:
1. `pl_a2dp_connect(addr, seq)`: if a cancel teardown is outstanding ->
   `phase = HELD` (replacing any older held attempt; last press wins). Else
   `phase = IN_FLIGHT`, `attempt.cid` = establish_stream's out-param. The
   COMMAND_DISALLOWED branch (`:3291`) also goes to HELD.
2. **Adopt rule** (fixes F6), in `pl_bt_connect_or_switch` before the switch
   branch: if a session to the same addr is up (`a2dp_cid != 0 &&
   connect_addr == addr`), set `session_seq = seq`; if the stream is already
   established emit `ConnectSucceeded{seq}` now, else the normal success emit
   will carry it. No disconnect.
3. ESTABLISHED success (`:3479`): `connect_addr` = event bd_addr (F5). If
   `attempt.phase == IN_FLIGHT && (cid == attempt.cid || addr == attempt.addr)`
   -> `session_seq = attempt.seq`; else `session_seq = 0` (remote). If
   `cid == cancel_cid` -> disconnect, suppress (defensive; should not occur).
4. Every emit wrapper takes the event's cid: suppress iff `cid == cancel_cid`;
   otherwise tag with `session_seq` (post-signaling) or `attempt.seq`
   (pre-signaling failures and bt.c's switch steps). The D3 guards at
   `:3167`, `:3871`, `:4123` use the same `cid == cancel_cid` predicate.
   STREAM_STARTED also fires on every resume after a pause (`:3100`), which is
   why a global predicate there was doubly wrong.
5. **Conclusion:** emitting `ConnectSucceeded` or `ConnectFailed` for
   `attempt.seq` sets `attempt.phase = ATT_NONE` (session_seq keeps the seq).
6. `pl_a2dp_cancel_connect(seq)`; `seq == 0` -> ignore. Match in order:
   - `attempt.seq` (or ANY) with `phase == HELD` -> drop it (fixes F4), conclude.
     Also cancel a matching 0x0b retry timer (retry has `a2dp_cid == 0`).
   - `attempt.seq` with `phase == IN_FLIGHT` -> `cancel_cid = attempt.cid`,
     arm teardown, conclude.
   - `session_seq == seq && a2dp_cid != 0` -> the S5 late success core has not
     seen yet: `cancel_cid = a2dp_cid`, arm teardown (D3).
   - else no-op (stale/duplicate; counted).
   bt.c's switch reset on cancel (`bt.c:1411-1424`) stays, but only when the
   switch target's seq matches.
7. **Teardown completion** `pl_a2dp_cancel_teardown_done()`: disarm timer,
   reissue HELD attempt. Called from RELEASED when `cid == cancel_cid`, **from
   the ESTABLISHED-failure branch when `cid == cancel_cid` (the F3 fix)**, and
   from `service_cancel` when `a2dp_source_disconnect` returns non-success
   (`ERROR_CODE_UNKNOWN_CONNECTION_IDENTIFIER`: already gone). The 3 s timer
   stays as the fallback; `cancel_teardown_timeouts` must now read 0.
8. Counters to add: `cancel_done_released`, `cancel_done_prefail`
   (pre-OPEN path), `cancels_stale`, `remote_sessions`. These are what the
   hardware plan reads.

### FFI (ui-ffi/src/lib.rs)

- `PlConnectPayload` (`:2717`) + `seq: u16`. CancelConnect moves off
  `PlAddrPayload` to its own `PlCancelConnectPayload { addr, seq }`.
  `PL_COMMAND_ABI_VERSION` 4 -> 5 (`:2862`).
- `PlConnectStepChangedPayload`, `PlConnectRetryingPayload`,
  `PlConnectSucceededPayload`, `PlConnectFailedPayload` (`:1339-1384`)
  + `seq: u16`. `PL_EVENT_ABI_VERSION` 7 -> 8 (`:2117`).
- bt.c pending queue entry carries `seq` alongside `addr` for CONNECT and
  CANCEL_CONNECT; `pl_bt_debug_connect` passes 0, debug CANCELCONNECT passes
  `PL_SEQ_ANY`.

### Core fold (jyhk.25's fold.rs/model.rs)

- Delete `cancelled_attempt` and both addr-keyed guards.
- For step/retry/succeeded/failed with `seq != 0`: apply iff
  `attempt.is_some_and(|a| a.seq == seq)`; otherwise drop (stale echo of a
  cancelled or superseded attempt). This is the only guard.
- `seq == 0`: succeeded -> session appeared (connected addr, PersistDevice,
  `last_outcome` untouched, wizard untouched); step/retry -> ignore;
  failed -> ignore for attempt/wizard (log only).
- The PL_DEBUG_REMOTE bypass relies on seq-0 success still updating the
  connected state -- covered by the seq-0 rule.
- `bump_attempt_seq` skips 0 and 0xFFFF.

### Landing order

The ABI bumps force C, ui-ffi and core to land in one branch. jyhk.25 should
merge first **with the `cancelled_attempt` marker removed** (on main, C does
not act on CancelConnect, so the honest behaviour is to let a real success
land). Then chc3 v2 rebases on main and carries C + ui-ffi + the core fold
change together.

### Tasks (Ruby)

- **R1 (jyhk.25, before merge):** remove `cancelled_attempt` and its two
  guards; make `bump_attempt_seq` skip 0/0xFFFF. Update the tests that assert
  the marker.
- **R2 (chc3 v2, rebased on main after R1) -- FFI:** seq fields and ABI bumps
  above; `pl_command_from` / event decode; cbindgen header regenerated.
- **R3 -- C a2dp.c:** attempt struct, `session_seq`, `cancel_cid`,
  cid-scoped emit wrappers and D3 guards, conclusion rule, cancel match order,
  `pl_a2dp_cancel_teardown_done` called from RELEASED + ESTABLISHED-failure +
  disconnect-error, ESTABLISHED uses event bd_addr, new counters.
- **R4 -- C bt.c:** seq through the pending queue, adopt rule in
  `pl_bt_connect_or_switch`, switch reset only on matching seq, debug
  CONNECT seq 0 / CANCELCONNECT `PL_SEQ_ANY`.
- **R5 -- core fold:** seq rules above.
- **R6 -- tests (core/ui-ffi):** stale seq dropped; seq-0 success sets
  connected with no attempt and leaves wizard alone; seq-0 success while an
  attempt for another seq is in flight does not conclude it; cancel then
  retry same addr: old-seq success dropped, new-seq success applied; ABI
  round-trip of every new field. C has no unit harness -- R3/R4 are proven by
  the hardware plan.

### Hardware plan (Tess, headless via cdc_sender/cdc_reader)

Read the `a2dp:` report counters before and after each case; log lines are
lossy (F3) and are supporting evidence only. Start each case from Idle
(`--disconnect`, wait for `link_state` Idle).

- **H1 S1/S2 cancel:** `--connect <addr> --delay 0.05 --cancel-connect`.
  Pass: `cancel_done_prefail` +1, `cancel_teardown_timeouts` +0,
  `scan_connectable=1` within 5 s, next `--connect` reaches LDAC.
- **H2 S5 cancel:** same with `--delay 1.5` (after STREAM_ESTABLISHED).
  Pass: `cancels_late_success` or `cancel_done_released` +1, timeouts +0,
  link ends Idle, no audio packets after.
- **H3 cancel-then-retry:** `--connect --delay 0.05 --cancel-connect` then
  `--connect` immediately. Pass: exactly one new session reaches LDAC,
  timeouts +0, held reissue logged or counted.
- **H4 stale cancel:** connect to LDAC, wait 5 s, send `--cancel-connect`.
  Pass: `cancels_stale` +1, stream still up (proves F2 is fixed).
- **H5 headset-initiated reconnect -- NEEDS ANDREAS:** with the board Idle
  and connectable, Andreas power-cycles the headphones (94:DB:56:54:7C:F2).
  Pass: `remote_sessions` +1, report shows LDAC streaming, link Connected,
  Home shows the headset. Repeat once right after an H1 cancel (the v1
  permanent-refusal window) -- same pass criteria.
- **H6 cold boot race -- NEEDS ANDREAS:** Andreas replugs the board and
  powers the headphones on at the same moment. Whichever side wins, pass is one
  session reaching LDAC with no disconnect/re-page (adopt rule); count
  `remote_sessions` to see which won.

Note on H4: debug CANCELCONNECT uses `PL_SEQ_ANY`, which by rule 6 matches
only a HELD/IN_FLIGHT attempt, never a concluded session -- so H4 must be a
no-op. `PL_SEQ_ANY` never matches `session_seq`.
