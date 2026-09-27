# iface-6 EQ management protocol (web EQ editor + preset library)

Ada, 2026-09-27, bead pico-link-jyhk.17. Unblocks pico-link-jyhk.14 (F1 EQ editor).
Inputs: UMA DESIGN + FERN DESIGN on jyhk.8, DECISIONS on jyhk.14, ADA DESIGN on
jyhk.1 (telemetry, GET_INFO), ADA DESIGN on ryw.14 (id contract), code on main.

## 1. Invariants kept (must not undo)

- CLASS bmRequestType, recipient INTERFACE, wIndex 6. VENDOR never reaches a class
  driver in pico-sdk 2.1.1 TinyUSB (usb_config_itf.c:279-289).
- Rust owns preset id allocation (ryw.14). The host never chooses an id: create is
  "id 0 = please allocate"; a nonzero id must already exist.
- Presets are global and assigned to devices; delete never rewrites device records,
  a dangling id resolves Off (events.rs Command::DeletePreset doc).
- The control callback runs in the 0xC0 IRQ worker and never calls Rust
  (usb_config_itf.h:32-41). All Rust work happens in pl_config_itf_poll* in the
  superloop.
- Core owns every wire format (telemetry precedent, ADA DESIGN jyhk.1 sec 1). C
  copies opaque bytes. JS never retypes a layout: core tests emit fixtures (Fern sec 5).

## 2. Request set

| bReq | Name | Dir | Status |
|---|---|---|---|
| 0x01 | IMPORT_PRESET | OUT | unchanged (pl_eq_import.py) |
| 0x02 | GET_STATUS | IN | unchanged (import only) |
| 0x03 | GET_TELEMETRY | IN | page 0 gains appended fields (sec 7) |
| 0x04 | GET_INFO | IN | info_ver 1 -> 2, appended capability fields (sec 8) |
| 0x05 | GET_LIBRARY | IN | NEW: one consistent snapshot of effects + paired devices |
| 0x06 | HOST_OP | OUT | NEW: every mutation, preview and parse, one op per transfer |
| 0x07 | GET_OP_STATUS | IN | NEW: result of the last HOST_OP, seq-tagged |

Rename is SAVE with a new name. Duplicate is SAVE with id 0. Bypass is a PREVIEW
flag. There is deliberately one mutation request, not six bRequests: one mailbox,
one gate, one status, and op_mask in GET_INFO versions each op.

### Why reads are a published snapshot, not per-item IN requests

The SETUP handler cannot call Rust, so an IN reply must already exist when SETUP
arrives. A deferred data stage (arm EP0 later from thread context) is exposed to
the TinyUSB arm/complete race USBPods had to patch - rejected. So reads follow the
telemetry pattern: Rust encodes in the superloop, C publishes under
save_and_disable_interrupts, SETUP memcpys into the reply buffer
(usb_config_itf.c:203-209, 346-359). One snapshot with one rev also gives the page
an atomic view: effects and the devices that reference them never disagree.

## 3. GET_LIBRARY (0x05), lib_proto 1

Little-endian. Header 14 B: u8 lib_proto=1, u8 reserved, u16 len, u16 library_rev,
u8 flags (bit0 presets_ready), u8 effect_count, u8 effect_rec_len, u8 device_count,
u8 device_rec_len, u8 max_effects (8, import.rs:58), u8 max_devices (8,
model.rs:14), u8 reserved.

Effect record (84 B): u16 id, u16 persisted_seq, blob[80] = Preset::to_wire v2
verbatim (preset.rs:392). The blob IS the effect format on the wire: it is already
versioned, core-owned and fixture-testable, so there is no second effect schema.

Device record (42 B): addr[6], u16 preset_id, u8 flags (bit0 connected), u8
name_len, name[32] (MAX_DEVICE_NAME_BYTES, model.rs:20).

Max 14 + 8x84 + 8x42 = 1022 B. Buffer 1536 for append headroom. Records are
append-only within lib_proto (the rec_len bytes let an old host skip new tails).

library_rev: Rust encodes the library and compares with the last encoding; rev++
only on a byte difference. Self-maintaining: no list of "remember to bump here"
sites to forget (the damage-key lesson). Generation runs under the same
poll-recency gate as telemetry (usb_config_itf.c:211-240), BEFORE the telemetry
step so page 0 carries the fresh rev. Zero cost with no page attached.

persisted_seq: per-id u16 in App, bumped when a PresetLoaded echo for that id is
folded. Because the store only changes on echo (ryw.14 LEARNED comment), this is
"flash truth changed" and doubles as the optimistic-concurrency token (sec 6).

## 4. HOST_OP (0x06) and GET_OP_STATUS (0x07), op_proto 1

Request header 4 B: u8 op_proto=1, u8 op, u8 seq, u8 flags. Body per op:

| op | Name | Body | Result |
|---|---|---|---|
| 1 | SAVE_EFFECT | u16 id (0 = create), u16 base_seq, blob[80] | effect_id |
| 2 | DELETE_EFFECT | u16 id, u16 base_seq | - |
| 3 | ASSIGN | addr[6], u16 effect_id (0 = Off) | - |
| 4 | PREVIEW | u16 effect_id (0 = unsaved draft), blob[80]; flags bit0 = bypass | - |
| 5 | PREVIEW_END | - | - |
| 6 | PARSE_APO | u8 name_len, name, APO text | payload: blob[80], u16 collides_with, u8 copy_name_len, copy_name[16] |

GET_OP_STATUS reply (~101 B): u8 op_proto, u8 seq, u8 op, u8 state (0 none, 1
done, 2 rejected), u8 error, u8 reserved, u16 effect_id, u16 library_rev, u16
persisted_seq, u16 line, u16 band, f32 value, u8 payload_len, payload. C serves
min(wLength, len) - never the >= check GET_STATUS uses (usb_config_itf.c:311),
which breaks old hosts the day the struct grows.

Semantics (all enforced in core, one function per op):
- SAVE create: presets_ready gate (ryw.14), STORE_FULL at 8, store.create, queue
  SavePreset{real id}. SAVE update: NOT_FOUND unless the id exists (never
  create-with-host-id), CONFLICT if base_seq != current persisted_seq, EDITOR_OPEN
  if the device editor has that id open. Both: NAME_TAKEN (byte-exact, excluding
  self), NAME_INVALID (empty / not UTF-8).
- Host blobs are validated STRICTLY, not via Preset::from_wire's tolerant path:
  from_wire turns an unknown version into an empty locked preset
  (preset.rs:443-449) - fine for flash, silently destructive for input. Reject
  BLOB_VERSION unless v2, reserved band kinds, band_count > 10. Value ranges use
  the import limits (gain, Q, Fc, preamp) factored out of import.rs:172 to_preset
  into one validate(&Preset) shared by import, SAVE and PREVIEW. Reject, never clamp.
- Lock rule (decision 1 on jyhk.14): core ignores the host's eq_locked bit and sets
  locked = previous.locked || bands differ || preamp differs (previous = empty/Auto
  on create). A crossfeed-only web edit therefore does not lock a hand-made effect.
- DELETE: NOT_FOUND, CONFLICT, EDITOR_OPEN; queue DeletePreset. Devices dangle to Off.
- ASSIGN: UNKNOWN_DEVICE if addr is not paired; effect_id 0 or existing only; queue
  AssignPreset. Last writer wins, no base_seq (a pick is idempotent).
- PREVIEW: validate, set App.host_preview = Some{effect_id, preset, bypass}. No
  flash, no command. PREVIEW_END: None.
- PARSE_APO: import.rs parse + to_preset only, no store mutation. Returns the blob,
  the id of a same-name effect (0 none) and core's lowest-free " 2" copy name
  (import.rs:269), so JS implements neither the parser nor the suffix rule. The
  page then SAVEs: Replace = SAVE(existing id, blob with the existing crossfeed),
  Copy = SAVE(0, blob renamed). IMPORT_PRESET stays for the Python tool.
- Error enum lives in core and is emitted to fixtures/ as constants.

seq: the host picks it; C copies the status bytes and never parses anything. The
host polls GET_OP_STATUS until seq matches its own and state != 0. This removes
the need for C to write a BUSY status at ACK (the IMPORT_PRESET trick,
usb_config_itf.c:398-408). On session start the page reads GET_OP_STATUS once and
starts at reply.seq + 1, so a reload cannot match a stale completion.

## 5. Transfers within control-transfer limits

No application-level chunking. TinyUSB splits a control data stage into 64 B EP0
packets; every payload fits one transfer: HOST_OP <= 1024 B (the existing mailbox,
PL_CONFIG_IMPORT_BUF_LEN, usb_config_itf.c:32), GET_LIBRARY <= 1536 B, op status
~101 B. IMPORT_PRESET and HOST_OP share the one 1 KB mailbox and its pending flag:
SETUP of either stalls while one is pending (usb_config_itf.c:299-305); the page
treats that stall as busy (Fern sec 4). EP0 serialises control transfers, so all
IN requests share ONE static reply buffer sized for the largest (1536 B), replacing
telemetry's separate 256 B reply. SRAM delta about 3 KB (library publish buffer +
larger shared reply).

Limit: APO text above ~1000 B is rejected with a clear message. Real AutoEQ files
with 10 filters are ~500 B. If pasted Equalizer APO configs hit it, raising the
mailbox to 2 KB is a constant change; chunked upload is not worth building now.

## 6. Concurrency with on-device edits

- Detection: page 0 telemetry appends library_rev; the page polls at 30 Hz already,
  so it sees any change within ~33 ms and re-reads GET_LIBRARY only then.
- Stale writes: SAVE/DELETE carry base_seq; CONFLICT returns the current
  persisted_seq, the page shows "changed on the device: reload or overwrite".
- Device editor open on the same effect: SAVE/DELETE -> EDITOR_OPEN. The device
  editor saves on every value change (effects.rs:738-741), so letting the web write
  underneath it would be silently clobbered. Needs the editor_preview mailbox to
  carry the open id: Option<(u16, Preset, bool)> (mod.rs:231, effects.rs:678,728).
- Save confirmation: DONE on SAVE means queued, not persisted (same as import,
  usb_config_itf.c:119-123). The page shows Saved when that id's persisted_seq in
  the library passes the op status's persisted_seq AND the blob matches what it
  sent. A refusal arrives as the truth echo (old blob, or the id vanishes); a 5 s
  timeout reads "not confirmed" (a dropped echo, event ring drops newest).

## 7. Preview semantics

- dsp_program precedence (mod.rs:529-546): debug override > device editor preview >
  host preview > connected device's assignment. The person holding the device wins.
- Lease: C stamps s_last_host_setup_us on EVERY iface-6 SETUP (today only 0x03
  stamps, usb_config_itf.c:338). In pl_config_itf_poll, if a host preview may be
  active and nothing arrived for 2 s, call pl_ui_host_preview_end. Telemetry polls
  are the keepalive, so no new traffic. Page closed, crashed, tab hidden (polling
  stops, Fern sec 4), computer asleep: audio reverts within 2 s. The draft stays in
  the page and is re-sent when it returns. Also cleared on configd_reset.
- Rate: the page coalesces drafts latest-wins behind the single-flight queue, at
  most ~10/s. Device cost per PREVIEW is a blob decode + coefficient recompute; the
  existing newest-wins pull (pl_ui_take_dsp_program, ui-ffi lib.rs:3184) absorbs it.
- Save does not end the preview; leaving the editor sends PREVIEW_END.

## 8. Telemetry and GET_INFO versioning

Page 0 stays proto 1: append after offset 163 (the host reads len and ignores
trailing bytes, telemetry.rs:439-445): u16 library_rev, u8 flags2 (bit0
host_preview_active, bit1 device_editor_open, bit2 presets_ready), u16
device_editor_effect_id, u8 codec_fallback_reason (0 = none). New length 169.
Bump only on reorder or resize, never on append.

codec_fallback_reason: BtModel has no fallback concept yet; home.rs:269-273 passes
fallback: None on purpose. The byte ships as 0 and must be sourced from BtModel
when that exists, never from C statics (the fork-the-truth hack rejected on
jyhk.1). Reason text comes from a core-emitted fixture table, not retyped in JS.

GET_INFO: info_ver 2 appends u8 lib_proto=1, u8 op_proto=1, u16 mailbox_len=1024,
u32 op_mask (bit N = op N), u16 library_max_len. Update the static assert
(usb_config_itf.h:220) and add fixtures/telemetry/info-v2.bin; keep info-v1.bin.
Page rule: info_ver 1 -> Home only, Effects tab hidden with "update firmware";
info_ver >= 2 -> gate each Effects action on op_mask.

## 9. Flash writes vs audio

Web ops never add a new write kind. SAVE/DELETE/ASSIGN feed the existing ungated
"just write" persist path (persist.c:1347-1391, Andreas's ruling; memory: do not
defer a user-initiated write). Preview never touches flash. Explicit Save means at
most human-rate writes. flash_safe_execute NAKs EP0 for tens of ms; the page
already treats slow replies as normal (Fern sec 4).

Web traffic makes creates and deletes routine, which raises two open C bugs from
latent to likely: pico-link-ryw.15 (next-id not persisted: delete highest id,
reboot, create -> devices that referenced the deleted effect silently get the new
one) and pico-link-j5su (a dropped pending push wedges all preset saves until
reboot). Both should land before F1 ships.

## 10. Rejected

- Per-item IN requests (GET_EFFECT(i)): no atomic view, N transfers per refresh.
- Deferred EP0 data stage: TinyUSB arm/complete race.
- Parsing APO in JS: a second parser that drifts from core's limits.
- Host-chosen ids / create-by-id: breaks the ryw.14 contract and the alias guard.
- Autosave from the web: flash write per drag event. Explicit Save (decision 2).
- A new interrupt IN endpoint for change notification: library_rev in telemetry is
  free (same argument as jyhk.1 sec 2).
- Global library_rev as the conflict token: an unrelated assignment would reject a
  save. Per-effect persisted_seq instead.

## 11. Tasks (ordered)

1. core read side: library encoder + encode-compare rev, persisted_seq folded on
   PresetLoaded/PresetDeleted, page-0 appends, editor-open id in the editor_preview
   mailbox, fixtures (library, telemetry-extended) -> Ruby.
2. core write side: op request decode / status encode, the six ops, validate()
   factored from import.rs, lock rule, host_preview + precedence, error enum
   fixtures -> Ruby (dep 1).
3. ui-ffi: pl_ui_library(ui, buf, cap) (returns 0 when unchanged),
   pl_ui_host_op(ui, in, in_len, out, out_cap), pl_ui_host_preview_end(ui); header
   regen; no event/command ABI change -> Ruby (dep 2).
4. C: 0x05/0x06/0x07, shared mailbox + shared reply buffer, SETUP stamp on every
   request + 2 s lease, library generation before telemetry, GET_INFO v2 + assert +
   info-v2.bin, loop-prof phase; tools/usb-console/pl_eq.py (list/save/delete/
   assign/preview) -> Ruby, then Tess on hardware: audio A/B with previews at 10/s,
   save confirm via persisted_seq, lease revert on tab close (dep 3; ryw.15 and
   j5su before sign-off).
5. web session API: proto/library.js, proto/ops.js vs fixtures, seq handling,
   preview coalescer, save confirmation, conflict/EDITOR_OPEN/busy states,
   FakeTransport device model for ops -> Ruby (deps 1-2 for fixtures; parallel
   with 3-4).
6. Separate, not F1-blocking: model codec fallback reason in BtModel (C must report
   why), then wire it to hero + the reserved telemetry byte.

## 12. Open (Uma, non-blocking)

While a host preview plays, the panel shows nothing unusual (Home FX line still
names the assigned effect). Should the device show a PREVIEW marker? flags2 bit0
gives the web its own indicator either way.

If the on-device effects editor opens while a host preview is active, the host
preview mailbox (`App::host_preview`) is kept, not cleared -- the precedence
rule in section 7 already makes the device editor's own preview win for as
long as it's open, and the host preview resumes on its own once the editor
closes and stops shadowing it.
