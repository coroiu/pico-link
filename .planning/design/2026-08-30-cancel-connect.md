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
