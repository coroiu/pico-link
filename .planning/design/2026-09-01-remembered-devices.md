# Remembered devices: model, store schema and FFI delta

Bead `pico-link-4vb.1`. Author: Ada (architect), 2026-09-01.
Refines design-of-record `.planning/design/2026-08-28-on-device-ui.md` sections
5, 8, 18 and 22 against what was actually built by `pico-link-a67`, `znb.7`,
`znb.8` and `cz0.6`.

## 1. What is actually there today

- `BtModel::devices: Vec<DeviceEntry>` is **scan results**, and it has two
  readers: `build_devices_screen` (app.rs:520-578) and the wizard, via the
  `wizard_devices` mirror kept in lockstep by `add_device`/`clear_devices`. The
  Devices screen's rows are `name + "AA:BB:… RSSI -52"` — inquiry data, on the
  screen the design says is the remembered list.
- Row 0 is "Scan for headphones", which since `znb.7` does not scan; it opens
  the wizard. The row's label is now a lie.
- The store holds **exactly one slot** (`PL_PERSIST_DEVICE_SLOT 0`,
  persist.c:44). The record type already has `name[32]`, `name_len`,
  `codec_id`, `ldac_quality`, `volume`, `flags`, `mru_seq`, `preset_id` — and
  `pl_persist_do_write` (persist.c:290-313) `memset`s the whole thing and fills
  **only** `addr`, `mru_seq`, `crc16`. Every other field is written as zero on
  every save.
- `PlStoreLoadedPayload` is `{status, has_device, addr[6]}`. One bare addr, no
  name — the bead's premise, confirmed.
- `NVM_NUM_LINK_KEYS` is already **8** (btstack_config.h:66). Link-key capacity
  is not the blocker; our slot count is.
- Name flow: `bt.c:410-436` gets the inquiry name and pushes it to core. **C
  never keeps it.** At the moment the record is written (`a2dp.c:1709`,
  `pl_persist_save_device_now(s_ctx.connect_addr)`), C has an address and
  nothing else. This is the structural reason the record has no name — not an
  oversight in the write path.

## 2. Constraints this design is built on

1. **Only one thing may own scan results.** The wizard. Design section 9's
   stable-first-seen-order rule and section 18's "Connected pinned, then MRU"
   are contradictory orderings of the same `Vec` if both screens read it.
2. **Every flash write happens on the cyw43/BTstack async_context**, never
   thread context (persist.h's Reentrancy section). Any new write path —
   forget, rename, per-device settings — inherits that rule.
3. **The store must never silently evict** (design S18). A user who blames the
   pairing instead of the capacity is a user who distrusts the device.
4. **Rust owns text; C owns bytes.** The 32-byte name cap must truncate on a
   UTF-8 *character* boundary (S22). C cannot be trusted with that and should
   not be asked to.
5. **This is a PoC.** Per-device codec/volume/preset are Tier 2 (E15/E16);
   their bytes are already reserved on flash. Do not build the screens or the
   model fields for them here — but do not let this bead make them harder.

## 3. Core model

Two lists, disjoint readers.

```rust
/// One remembered (paired) device. The sole source of truth is C's flash
/// store -- core never invents an entry, and never mutates this list
/// except by folding Event::PairedDeviceUpserted / PairedDeviceForgotten.
pub struct PairedDevice {
    pub addr: DeviceAddr,
    /// Possibly empty -> rendered "(unknown device)" + address tail (S13).
    pub name: String,
    /// Monotonic use-sequence (E26). Ordering key; never a wall clock --
    /// this board has no RTC.
    pub mru_seq: u32,
}

pub struct BtModel {
    /// RENAMED from `devices`. Inquiry results. Wizard-only reader.
    pub discovered: Vec<DeviceEntry>,
    /// NEW. Remembered devices, MRU-descending. Devices-screen-only reader.
    pub paired: Vec<PairedDevice>,
    // ...unchanged
}
```

The rename `devices` -> `discovered` is the point, not cosmetics: it makes "the
Devices screen reads scan results" a compile error rather than a habit. It is
one identifier and mechanically conflict-resolvable against `pico-link-4vb.2`.

`PairedDevice` deliberately carries **no** codec/volume/flags/preset fields.
Those are per-device *settings*, they have no screen, and the flash bytes are
already reserved — adding model fields nobody reads is the gold-plating failure
mode. The FFI payload (section 5) leaves room without carrying them.

**Single-writer rule.** `BtModel::paired` is mutated by exactly two events
(`PairedDeviceUpserted`, `PairedDeviceForgotten`) and by nothing else. Core does
**not** optimistically append on `ConnectSucceeded`, even though it could —
because then core's list and flash can disagree (store full, CRC failure, the
`pico-link-lmf` carve-out deferring the write), and a UI row for a device that
was never persisted is exactly the "silently forgot your pairing" failure this
whole line of work exists to kill. C echoes an upsert after every write that
actually landed; no echo means no row.

## 4. Devices screen

Rows, top to bottom:

| Row | When | Sublabel | A | X |
|---|---|---|---|---|
| Connected device | `link_state == Connected` and its addr is in `paired` | `Connected` | Device detail (stub screen, see below) | Forget confirm |
| Other paired devices, `mru_seq` descending | always | `Paired` | `Command::Connect` + push wizard at `Connecting` | Forget confirm |
| `Pair new headphones` | always | — | push wizard at `Instructions` | — |

- **No RSSI, no address, no availability dot.** S18: never claim availability we
  have not verified. `Connected` or `Paired`, nothing else. The nameless case is
  `(unknown device)` with the last three address bytes as the sublabel
  discriminator (S13's first-class-label rule).
- **`Pair new headphones` goes last**, not first. The recurring 2-press job
  (switch) belongs under the cursor; the rare job belongs at the end. On first
  run it is the only row, so discoverability is unharmed — which is the only
  thing row 0 bought.
- **Switching reuses the wizard**, per S8: `Command::Connect { addr, name }`
  plus `PushView(wizard)` with
  `WizardPhase::connecting_pending(addr, ConnectStep::Connecting)`. No new
  screen, no new phase, depth stays 2.
- `SCAN_ROW_KEY` (`[0xFF; 8]`) is replaced by `PAIR_NEW_ROW_KEY = [0xFE; 8]` —
  same collision argument (a real device key's top two bytes are always 0).
- **A on the connected row** pushes a stub `Device detail` screen, using the
  exact precedent `build_settings_screen` set for the Settings row
  (`app.rs:591`): a labelled row must have a reachable destination or the
  design's rule 2 is violated. Its content is S10's job, not this bead's.
- **Gate the wizard on `paired.len() < 8`.** When full, the `Pair new
  headphones` row opens the pick-one-to-forget flow instead of the wizard. This
  is S18's "pairing when full" flow moved *before* any radio work, which is
  strictly better than discovering fullness after BTstack has already written a
  link key (see the hazard in section 7).

## 5. FFI delta

### 5.1 Events

Three new tags, purely additive:

```c
PL_EVENT_TAG_PAIRED_DEVICE_UPSERTED = 10,
PL_EVENT_TAG_PAIRED_DEVICE_FORGOTTEN = 11,
PL_EVENT_TAG_PAIRED_STORE_FULL = 12,
```

```c
typedef struct PlPairedDeviceUpsertedPayload {
    uint8_t  addr[6];
    uint8_t  name[32];   /* UTF-8 as stored; invalid -> lossy, per module rule */
    uint8_t  name_len;   /* > 32 treated as empty, same as PlCodecChangedPayload */
    uint32_t mru_seq;
} PlPairedDeviceUpsertedPayload;

typedef struct PlPairedDeviceForgottenPayload { uint8_t addr[6]; } PlPairedDeviceForgottenPayload;
```

The name is an **inline fixed buffer copied by value**, following
`PlCodecChangedPayload` — *not* `PlDeviceDiscoveredPayload`'s borrowed pointer.
That keeps every `PlEventPayload` member `Copy` with no lifetime, and it leaves
`bt.c`'s 240-byte-per-slot ring `name_buf` path (bt.c:123-129, 202-204)
untouched and DeviceDiscovered-specific.

### 5.2 `StoreLoaded` changes shape — ABI 2 -> 3

```c
typedef struct PlStoreLoadedPayload {
    uint8_t status;
    uint8_t count;   /* how many PairedDeviceUpserted events preceded this */
} PlStoreLoadedPayload;   /* has_device / addr[6] REMOVED */
```

Boot sequence: C pushes `count` x `PairedDeviceUpserted`, **then** `StoreLoaded`
as the terminator. (Ring capacity 32; 8 records plus the terminator fits with
margin.)

`has_device`/`addr` are removed deliberately, and this is the load-bearing seam
call in this design. Those fields are **C deciding which device to
auto-reconnect to** — `pl_persist_boot_has_device()` /
`pl_persist_boot_device_addr()` (persist.h:122-123) return "the one slot", which
with one slot is not a policy. With eight slots it *is* a policy: MRU-max today,
a pinned default from Settings' "Connect to" tomorrow (S19). Design point 7
already says core owns that policy and C only loads/stages/flushes. Keeping the
fields would leave the policy on the wrong side of the seam and leave two dead
wire fields that core ignores — the shape that misleads the next reader. Core
computes the target as `paired.iter().max_by_key(|d| d.mru_seq)` and queues
`Command::Connect`, which is the same command it already queues today.

Cost: `PL_EVENT_ABI_VERSION` 2 -> 3. Cheap — both sides build from one repo in
one CMake run; the guard is defensive, not a compatibility obligation. `cz0.6`
already spent this once for the same class of reason.

### 5.3 Commands

```c
PL_COMMAND_TAG_FORGET_DEVICE = 6,

typedef struct PlConnectPayload {
    uint8_t addr[6];
    uint8_t name[32];   /* NEW */
    uint8_t name_len;   /* NEW: core truncated on a UTF-8 CHARACTER boundary */
} PlConnectPayload;

typedef struct PlAddrPayload { uint8_t addr[6]; } PlAddrPayload;  /* NEW */

typedef union PlCommandPayload {
    struct PlConnectPayload connect;  /* Connect */
    struct PlAddrPayload    addr;     /* CancelConnect, PersistDevice, ForgetDevice */
} PlCommandPayload;
```

`PL_COMMAND_ABI_VERSION` 1 -> 2. The three tags moving from `.connect.addr` to
`.addr.addr` is a *source* change only — both members start with
`uint8_t addr[6]` at offset 0, so no byte on the wire moves for them.

**Why the name rides on `Connect`, with the alternative stated.** C needs the
name at `a2dp.c:1709`, which runs long after any scan. Two ways to get it there:

- **A (chosen): name on `Connect`.** One command; the name arrives with the
  thing it describes; `bt.c`'s Connect handler caches `{addr, name, name_len}`
  as the in-flight target and `pl_persist_save_device_now` reads it from there.
  Costs one payload shape change and an ABI bump.
- **B: a separate `SetDeviceName{addr,name}` command core sends immediately
  before `Connect`.** No existing payload changes, no ABI bump. Costs an
  implicit "send this first" ordering rule that a future call site will
  eventually forget, and whose failure mode is a **silently nameless record** —
  precisely the defect class this bead exists to fix.

A. The saving in B is one version constant; the cost is an unenforceable
invariant with a silent failure. That trade is the definition of a quick fix
that is load-bearing.

`pl_bt_debug_connect` (the `PL_DEBUG_REMOTE` bypass) has no name, so it caches
`name_len = 0`, which the read-modify-write rule below turns into "keep whatever
name the record already has."

## 6. Store schema

**The on-flash record shape does not change.** `pl_persist_device_record_t` is
already 51 bytes with every field this design needs (persist.c:63-74). What
changes is how many slots are used and which fields get populated.

- **Slots**: `PL:D:0` .. `PL:D:7`. `PL_PERSIST_DEVICE_SLOTS 8`, replacing
  `PL_PERSIST_DEVICE_SLOT 0`. `PL_PERSIST_SCHEMA_VERSION` **stays 1**: an
  existing v1 store's slot-0 record loads cleanly under the new reader and
  simply reports `name_len == 0` -> `(unknown device)`. A migration would be a
  self-inflicted wound.
- **Load**: loop 0..7, `get_tag` each, CRC-check each, drop only the bad ones
  (per-record isolation is already the design's answer to failure isolation,
  S22). `s_next_mru_seq = max(loaded mru_seq) + 1`. Push one
  `PairedDeviceUpserted` per surviving record, then `StoreLoaded{status,
  count}`.
- **Write must become read-modify-write.** Today `pl_persist_do_write`
  `memset`s a fresh record. With a name — and later a codec, a volume, a preset
  id — that silently erases every field the current caller does not set. The
  rule: read the slot, overwrite only the fields this call actually carries
  (`name` only when `name_len > 0`), always bump `mru_seq`, leave
  `codec_id`/`ldac_quality`/`volume`/`flags`/`preset_id` untouched, recompute
  CRC. **This is the single most important change in this section** and it is
  cheap now and expensive later: the first per-device-setting bead that lands on
  top of a construct-from-scratch writer will produce a bug that looks like
  flash corruption.
- **Slot selection**: match by `addr` -> that slot; else first free slot; else
  **no eviction** — push `PairedStoreFull` and write nothing.
- **Forget**: `delete_tag(PL:D:i)` **plus** `gap_delete_link_key(addr)` — S18
  says forgetting removes the link key, and a record without its key is a row
  that cannot connect. Runs on the async_context via a new
  `PL_BT_PENDING_FORGET_DEVICE` entry on bt.c's existing pending queue, same as
  every other write. Echo `PairedDeviceForgotten`.
- **Budget**: 8 x 51 + 1 marker = ~409 B of the ~4KB TLV space, plus BTstack's 8
  link keys. Comfortable.

## 7. Hazards to surface, not to solve here

1. **Two stores, two capacities, two eviction policies.** BTstack's
   `btstack_link_key_db_tlv` writes link keys unconditionally from inside
   `put_link_key`, with its own full-behaviour, independent of our PL:D slots.
   Ours is 8 and `NVM_NUM_LINK_KEYS` is 8, which is the right start, but the
   policies still differ: we refuse to evict, BTstack does whatever it does. The
   consequence is an **orphaned PL:D record whose link key is gone** — a row
   that says `Paired` and cannot connect without re-pairing. Mitigation,
   recommended for this slice because it is cheap: at load, query the link-key
   DB per record and drop (or mark) records with no key. Verify BTstack's actual
   full-store behaviour before relying on any specific eviction claim — do not
   assume LRU.
2. **A successful pairing currently writes flash twice.** `a2dp.c:1709` writes
   synchronously at `STREAM_ESTABLISHED`; core then queues
   `Command::PersistDevice` on `ConnectSucceeded`, which re-stages and writes
   again ~2s later via `pl_persist_service`. Two erase cycles per pairing,
   unnamed anywhere. Fix: `pl_persist_request_save_device` drops a request whose
   `addr` matches a record written during this connection. Contained, a handful
   of lines.
3. **`PersistDevice`'s meaning has drifted.** Since Andreas's
   write-at-pairing-time ruling, it no longer means "remember this device" — the
   sync path does that. Its remaining job is the MRU bump (and, later,
   per-device settings flushes). Say so in its doc comment or the next reader
   will re-derive it wrong.
4. **The Devices screen built from Home's menu row captures a cloned `BtModel`
   snapshot** (`home.rs:178-187`). `App::rebuild_root`'s `replace_at(1)` refresh
   (app.rs:748-762) covers it today. With a paired list this matters more: after
   the wizard pops, the new device must appear. The `PairedDeviceUpserted` ->
   `rebuild_root` path delivers it — but the refresh is keyed off
   `title_at(1) == DEVICES_TITLE`, a string. That title-as-identity idiom is now
   load-bearing in two places; it is cheap-and-contained today, worth a real
   screen-id enum the third time it appears.
5. **`pico-link-lmf` widens.** With eight slots, "USB audio already streaming at
   pairing time -> stage instead of write" can now defer a save that the
   store-full check would have rejected. The staged path must re-run slot
   selection at write time, not at stage time.

## 8. Task breakdown

| # | Task | Owner | Depends on |
|---|---|---|---|
| T1 | `persist.c`: 8 slots, load loop, read-modify-write, slot selection, no-evict, forget + `gap_delete_link_key`, `s_next_mru_seq = max+1` | Ruby (firmware) | — |
| T2 | FFI: new event tags 10-12, `PlStoreLoadedPayload` reshape (ABI 2->3), `PlConnectPayload` + `PlAddrPayload` (ABI 1->2), new `ForgetDevice` command | Ruby (`ui-ffi`) | — |
| T3 | `bt.c`: in-flight connect-target name cache; push upsert/forgotten/full; `PL_BT_PENDING_FORGET_DEVICE`; drop `pl_persist_boot_*` from the store-loaded push | Ruby (firmware) | T1, T2 |
| T4 | `core`: rename `devices` -> `discovered`; add `paired` + `PairedDevice`; fold the three new events; MRU-max auto-reconnect policy on `StoreLoaded`; `Connect` carries the char-boundary-truncated name | Ruby (core) | T2, `4vb.2` merged |
| T5 | `core`: rebuild `build_devices_screen` per section 4 (pinned connected, MRU, `Pair new` last, forget-confirm on X, stub device-detail screen, wizard gate at 8) | Ruby (core) | T4 |
| T6 | Fixtures: first run (only `Pair new`); one nameless paired row; 8-full pick-one-to-forget; connected pinned above two others; store-corrupt. Zoomed, per the project's render-verification rule | Tess | T5 |
| T7 | Hardware: pair two devices, power-cycle, confirm both rows with names and MRU order; forget one; confirm the link key went too | Tess | T3, T5 |

**Dependency to respect:** T4/T5 touch `core/src/app.rs`, which
`pico-link-4vb.2` is editing now (`WizardPhase`, wizard auto-dismiss). Land
`4vb.2` first; nothing in this design contradicts it (the wizard's phase machine
is reused unchanged, including entering at `Connecting` for a switch).

**Sequencing note:** T1+T2+T3 are shippable without T4/T5 — the store would hold
8 named records that nothing renders yet. That is a safe, verifiable checkpoint,
and it front-loads the hardware risk.

## 9. Quick path vs sustainable path, for the record

The quick path is: leave `StoreLoaded` alone, add a `name[32]` to it, keep one
slot, keep C picking the reconnect target. Two days saved. What it costs later:
the auto-reconnect policy stays on the C side of the seam and has to be moved
when Settings' "Connect to" lands; the write path stays construct-from-scratch
and the first per-device-setting bead ships a field-erasing bug; and the Devices
screen keeps rendering scan results, which means the multi-device story stays a
fiction (design's own words for E14: "until it lands, pairing a second set of
headphones forgets the first").

The sustainable path above costs two ABI version bumps, one identifier rename,
and a read-modify-write rewrite of one 25-line function. That is the right
trade, and the ABI bumps are near-free in a single-repo, single-build project.
