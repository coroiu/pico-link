// GET_RADIO (0x08) snapshot decoder + the ConnectFailureReason wire table,
// per ADA DESIGN on pico-link-jyhk.24 (`.planning/design/
// 2026-09-27-iface6-eq-management-protocol.md` section 13.4) and core's
// write side, `core/src/app/radio.rs`. Same "core emits, JS asserts"
// discipline as `library.ts`/`ops.ts`: `radio.fixture.test.ts` checks this
// decoder against every `fixtures/radio/radio-*.bin+json` pair and the
// reason table against `fixtures/radio/reasons.json`.
//
// Layout, radio_proto 1 (core's doc comment on `encode_radio_snapshot`):
// header 36 B, little-endian, then up to MAX_SCAN_LIST_ITEMS (12) scan
// records of 42 B each.

export const RADIO_PROTO = 1;

const HEADER_LEN = 36;
export const SCAN_RECORD_LEN = 42;
const DEVICE_NAME_CAP = 32;
export const MAX_SCAN_LIST_ITEMS = 12;
export const RADIO_SNAPSHOT_MAX_LEN = HEADER_LEN + MAX_SCAN_LIST_ITEMS * SCAN_RECORD_LEN;

const OFF_RADIO_PROTO = 0;
const OFF_LEN = 2;
const OFF_RADIO_REV = 4;
const OFF_FLAGS = 6;
const OFF_SCAN_OWNER = 7;
const OFF_SCAN_SEQ = 8;
const OFF_ATTEMPT_SEQ = 10;
const OFF_INITIATOR = 12;
const OFF_STEP = 13;
const OFF_ATTEMPT_ADDR = 14;
const OFF_RETRIES = 20;
const OFF_OUTCOME_SEQ = 22;
const OFF_OUTCOME = 24;
const OFF_REASON = 25;
const OFF_OUTCOME_ADDR = 26;
const OFF_SCAN_COUNT = 32;
const OFF_SCAN_REC_LEN = 33;
const OFF_SCAN_TOTAL_AUDIO = 34;

const FLAG_DISCOVERING = 1 << 0;
const FLAG_CONNECTING = 1 << 1;
const FLAG_DEVICE_WIZARD_OPEN = 1 << 2;
const FLAG_PAIRED_FULL = 1 << 3;
const FLAG_STORE_READY = 1 << 4;

const SCAN_FLAG_ALREADY_PAIRED = 1 << 0;

export type ScanOwner = "none" | "device" | "host";

function scanOwnerFromWire(byte: number): ScanOwner {
  switch (byte) {
    case 1:
      return "device";
    case 2:
      return "host";
    default:
      return "none";
  }
}

function scanOwnerToWire(owner: ScanOwner): number {
  switch (owner) {
    case "device":
      return 1;
    case "host":
      return 2;
    default:
      return 0;
  }
}

export type ConnectInitiator = "device" | "host" | "autoReconnect";

function initiatorFromWire(byte: number): ConnectInitiator {
  switch (byte) {
    case 2:
      return "host";
    case 3:
      return "autoReconnect";
    default:
      return "device";
  }
}

function initiatorToWire(initiator: ConnectInitiator): number {
  switch (initiator) {
    case "host":
      return 2;
    case "autoReconnect":
      return 3;
    default:
      return 1;
  }
}

/**
 * `ConnectStep` wire byte -- design sec 13.4: "0 none, else ConnectStep
 * wire". Every named step is shifted up by one from `PlConnectStep`'s
 * ui-ffi ordinal space (core's `radio.rs` doc comment on
 * `connect_step_wire`) because this wire format needs its own "none" (`0`).
 */
export type ConnectStep = "connecting" | "pairing" | "settingUpAudio" | "negotiatingCodec" | "disconnecting";

function connectStepFromWire(byte: number): ConnectStep {
  switch (byte) {
    case 2:
      return "pairing";
    case 3:
      return "settingUpAudio";
    case 4:
      return "negotiatingCodec";
    case 5:
      return "disconnecting";
    default:
      return "connecting";
  }
}

function connectStepToWire(step: ConnectStep): number {
  switch (step) {
    case "pairing":
      return 2;
    case "settingUpAudio":
      return 3;
    case "negotiatingCodec":
      return 4;
    case "disconnecting":
      return 5;
    default:
      return 1;
  }
}

export type ConnectOutcomeResult = "ok" | "okDegraded" | "failed" | "cancelled";

function outcomeFromWire(byte: number): ConnectOutcomeResult {
  switch (byte) {
    case 1:
      return "ok";
    case 2:
      return "okDegraded";
    case 4:
      return "cancelled";
    default:
      return "failed";
  }
}

function outcomeToWire(result: ConnectOutcomeResult): number {
  switch (result) {
    case "ok":
      return 1;
    case "okDegraded":
      return 2;
    case "cancelled":
      return 4;
    default:
      return 3;
  }
}

/**
 * `ConnectFailureReason`'s core-owned wire code/retryable/text table --
 * design sec 13.3: "so JS never retypes events.rs:116-137." Checked
 * verbatim against `fixtures/radio/reasons.json`
 * (`core/src/app/radio_fixtures.rs`) by `radio.fixture.test.ts`.
 */
export const ConnectFailureReason = {
  Timeout: 1,
  Rejected: 2,
  NoA2dpSink: 3,
  NeedsPin: 4,
  RadioError: 5,
} as const;

export type ConnectFailureReason = (typeof ConnectFailureReason)[keyof typeof ConnectFailureReason];

export interface ConnectFailureReasonInfo {
  wire: ConnectFailureReason;
  retryable: boolean;
  text: string;
}

/** Wire code -> `{ retryable, text }` -- an unrecognised code falls back to `RadioError`'s entry, mirroring core's `reason_from_wire`'s "unknown decodes to the least specific, most generic reason" rule. */
export function connectFailureReasonInfo(reason: ConnectFailureReason): ConnectFailureReasonInfo {
  return CONNECT_FAILURE_REASON_TABLE[reason] ?? CONNECT_FAILURE_REASON_TABLE[ConnectFailureReason.RadioError];
}

function reasonFromWire(byte: number): ConnectFailureReason {
  switch (byte) {
    case ConnectFailureReason.Timeout:
    case ConnectFailureReason.Rejected:
    case ConnectFailureReason.NoA2dpSink:
    case ConnectFailureReason.NeedsPin:
      return byte;
    default:
      return ConnectFailureReason.RadioError;
  }
}

// Populated below `radio.fixture.test.ts` checks against
// `fixtures/radio/reasons.json` -- kept as a plain literal here (not
// generated) since there is no build step to regenerate it from, same as
// `ops.ts`'s `OpError` table.
const CONNECT_FAILURE_REASON_TABLE: Record<ConnectFailureReason, ConnectFailureReasonInfo> = {
  [ConnectFailureReason.Timeout]: { wire: ConnectFailureReason.Timeout, retryable: true, text: "No response" },
  [ConnectFailureReason.Rejected]: { wire: ConnectFailureReason.Rejected, retryable: true, text: "Pairing refused" },
  [ConnectFailureReason.NoA2dpSink]: { wire: ConnectFailureReason.NoA2dpSink, retryable: false, text: "Can't play audio" },
  [ConnectFailureReason.NeedsPin]: { wire: ConnectFailureReason.NeedsPin, retryable: false, text: "Needs a PIN" },
  [ConnectFailureReason.RadioError]: { wire: ConnectFailureReason.RadioError, retryable: true, text: "Bluetooth error" },
};

export interface RadioAttempt {
  seq: number;
  addr: string;
  initiator: ConnectInitiator;
  /** `undefined` when the wire `step` byte was `0`. */
  step: ConnectStep | undefined;
  retries: number;
}

export interface RadioOutcome {
  seq: number;
  addr: string;
  result: ConnectOutcomeResult;
  /** Only set when `result === "failed"` and the wire `reason` byte was nonzero. */
  reason: ConnectFailureReason | undefined;
}

export interface RadioScanEntry {
  addr: string;
  bars: number;
  alreadyPaired: boolean;
  name: string;
}

export interface RadioSnapshot {
  radioRev: number;
  discovering: boolean;
  connecting: boolean;
  deviceWizardOpen: boolean;
  pairedFull: boolean;
  storeReady: boolean;
  scanOwner: ScanOwner;
  scanSeq: number;
  attempt: RadioAttempt | undefined;
  lastOutcome: RadioOutcome | undefined;
  scanTotalAudio: number;
  scan: RadioScanEntry[];
}

const utf8Decoder = new TextDecoder("utf-8");
const utf8Encoder = new TextEncoder();

function formatAddr(bytes: Uint8Array): string {
  return Array.from(bytes)
    .map((b) => b.toString(16).toUpperCase().padStart(2, "0"))
    .join(":");
}

function parseAddr(addr: string): Uint8Array {
  return new Uint8Array(addr.split(":").map((h) => parseInt(h, 16)));
}

function readFixedName(u8: Uint8Array, lenOff: number, bytesOff: number, cap: number): string {
  const len = Math.min(u8[lenOff], cap);
  return utf8Decoder.decode(u8.subarray(bytesOff, bytesOff + len));
}

function writeFixedName(u8: Uint8Array, lenOff: number, bytesOff: number, cap: number, name: string): void {
  const bytes = utf8Encoder.encode(name).slice(0, cap);
  u8[lenOff] = bytes.length;
  u8.set(bytes, bytesOff);
}

/**
 * Decodes a `GET_RADIO` (0x08) payload (mirrors
 * `core::app::radio::decode_radio_snapshot`). Returns `null` on a payload
 * too short for its own declared header, an unrecognised `radio_proto`, a
 * `scan_rec_len` that doesn't match this decoder's `SCAN_RECORD_LEN` (a
 * newer proto's wider records are only skippable, not parseable, by this
 * version), or a declared `scan_count` that doesn't fit inside `bytes` --
 * same "on an unknown/malformed payload, the host bails" discipline every
 * other decoder in this crate follows.
 */
export function decodeRadioSnapshot(bytes: ArrayBuffer | Uint8Array): RadioSnapshot | null {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  if (u8.byteLength < HEADER_LEN) return null;
  const view = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);

  if (view.getUint8(OFF_RADIO_PROTO) !== RADIO_PROTO) return null;

  const radioRev = view.getUint16(OFF_RADIO_REV, true);
  const flags = view.getUint8(OFF_FLAGS);
  const scanOwner = scanOwnerFromWire(view.getUint8(OFF_SCAN_OWNER));
  const scanSeq = view.getUint16(OFF_SCAN_SEQ, true);

  const attemptSeq = view.getUint16(OFF_ATTEMPT_SEQ, true);
  let attempt: RadioAttempt | undefined;
  if (attemptSeq !== 0) {
    const stepByte = view.getUint8(OFF_STEP);
    attempt = {
      seq: attemptSeq,
      addr: formatAddr(u8.subarray(OFF_ATTEMPT_ADDR, OFF_ATTEMPT_ADDR + 6)),
      initiator: initiatorFromWire(view.getUint8(OFF_INITIATOR)),
      step: stepByte === 0 ? undefined : connectStepFromWire(stepByte),
      retries: view.getUint8(OFF_RETRIES),
    };
  }

  const outcomeSeq = view.getUint16(OFF_OUTCOME_SEQ, true);
  const outcomeByte = view.getUint8(OFF_OUTCOME);
  let lastOutcome: RadioOutcome | undefined;
  if (outcomeSeq !== 0 && outcomeByte !== 0) {
    const result = outcomeFromWire(outcomeByte);
    const reasonByte = view.getUint8(OFF_REASON);
    lastOutcome = {
      seq: outcomeSeq,
      addr: formatAddr(u8.subarray(OFF_OUTCOME_ADDR, OFF_OUTCOME_ADDR + 6)),
      result,
      reason: result === "failed" && reasonByte !== 0 ? reasonFromWire(reasonByte) : undefined,
    };
  }

  const scanCount = view.getUint8(OFF_SCAN_COUNT);
  const scanRecLen = view.getUint8(OFF_SCAN_REC_LEN);
  const scanTotalAudio = view.getUint8(OFF_SCAN_TOTAL_AUDIO);

  if (scanRecLen !== SCAN_RECORD_LEN) return null;

  const scanEnd = HEADER_LEN + scanCount * scanRecLen;
  if (u8.byteLength < scanEnd) return null;

  const scan: RadioScanEntry[] = [];
  let off = HEADER_LEN;
  for (let i = 0; i < scanCount; i++) {
    const addr = formatAddr(u8.subarray(off, off + 6));
    const bars = view.getUint8(off + 6);
    const recordFlags = view.getUint8(off + 7);
    const name = readFixedName(u8, off + 8, off + 9, DEVICE_NAME_CAP);
    scan.push({ addr, bars, alreadyPaired: (recordFlags & SCAN_FLAG_ALREADY_PAIRED) !== 0, name });
    off += scanRecLen;
  }

  return {
    radioRev,
    discovering: (flags & FLAG_DISCOVERING) !== 0,
    connecting: (flags & FLAG_CONNECTING) !== 0,
    deviceWizardOpen: (flags & FLAG_DEVICE_WIZARD_OPEN) !== 0,
    pairedFull: (flags & FLAG_PAIRED_FULL) !== 0,
    storeReady: (flags & FLAG_STORE_READY) !== 0,
    scanOwner,
    scanSeq,
    attempt,
    lastOutcome,
    scanTotalAudio,
    scan,
  };
}

/** Test/`FakeTransport` helper: the inverse of `decodeRadioSnapshot`. */
export function encodeRadioSnapshotForTest(input: RadioSnapshot): Uint8Array {
  const len = HEADER_LEN + input.scan.length * SCAN_RECORD_LEN;
  const u8 = new Uint8Array(len);
  const view = new DataView(u8.buffer);

  view.setUint8(OFF_RADIO_PROTO, RADIO_PROTO);
  view.setUint16(OFF_LEN, len, true);
  view.setUint16(OFF_RADIO_REV, input.radioRev, true);

  let flags = 0;
  if (input.discovering) flags |= FLAG_DISCOVERING;
  if (input.connecting) flags |= FLAG_CONNECTING;
  if (input.deviceWizardOpen) flags |= FLAG_DEVICE_WIZARD_OPEN;
  if (input.pairedFull) flags |= FLAG_PAIRED_FULL;
  if (input.storeReady) flags |= FLAG_STORE_READY;
  view.setUint8(OFF_FLAGS, flags);

  view.setUint8(OFF_SCAN_OWNER, scanOwnerToWire(input.scanOwner));
  view.setUint16(OFF_SCAN_SEQ, input.scanSeq, true);

  if (input.attempt) {
    view.setUint16(OFF_ATTEMPT_SEQ, input.attempt.seq, true);
    view.setUint8(OFF_INITIATOR, initiatorToWire(input.attempt.initiator));
    view.setUint8(OFF_STEP, input.attempt.step === undefined ? 0 : connectStepToWire(input.attempt.step));
    u8.set(parseAddr(input.attempt.addr), OFF_ATTEMPT_ADDR);
    view.setUint8(OFF_RETRIES, input.attempt.retries);
  }

  if (input.lastOutcome) {
    view.setUint16(OFF_OUTCOME_SEQ, input.lastOutcome.seq, true);
    view.setUint8(OFF_OUTCOME, outcomeToWire(input.lastOutcome.result));
    view.setUint8(OFF_REASON, input.lastOutcome.reason ?? 0);
    u8.set(parseAddr(input.lastOutcome.addr), OFF_OUTCOME_ADDR);
  }

  view.setUint8(OFF_SCAN_COUNT, input.scan.length);
  view.setUint8(OFF_SCAN_REC_LEN, SCAN_RECORD_LEN);
  view.setUint8(OFF_SCAN_TOTAL_AUDIO, input.scanTotalAudio);

  let off = HEADER_LEN;
  for (const entry of input.scan) {
    u8.set(parseAddr(entry.addr), off);
    view.setUint8(off + 6, entry.bars);
    view.setUint8(off + 7, entry.alreadyPaired ? SCAN_FLAG_ALREADY_PAIRED : 0);
    writeFixedName(u8, off + 8, off + 9, DEVICE_NAME_CAP, entry.name);
    off += SCAN_RECORD_LEN;
  }

  return u8;
}

/** An idle radio snapshot at the given `radioRev` -- `FakeTransport`'s and tests' starting point. */
export function emptyRadioSnapshot(radioRev = 0): RadioSnapshot {
  return {
    radioRev,
    discovering: false,
    connecting: false,
    deviceWizardOpen: false,
    pairedFull: false,
    storeReady: false,
    scanOwner: "none",
    scanSeq: 0,
    attempt: undefined,
    lastOutcome: undefined,
    scanTotalAudio: 0,
    scan: [],
  };
}
