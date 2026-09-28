// Asserts `radio.ts` against `fixtures/radio/*` (core's write side, bead
// pico-link-jyhk.27, `core/src/app/radio_fixtures.rs`) -- same "core emits,
// JS asserts" discipline as `library.fixture.test.ts`/`ops.fixture.test.ts`.
import { describe, expect, it } from "vitest";
import { connectFailureReasonInfo, ConnectFailureReason, decodeRadioSnapshot, encodeRadioSnapshotForTest, RADIO_PROTO, SCAN_RECORD_LEN } from "./radio";
import { radioFixtureBytes, radioFixtureJson, radioFixtureNames } from "../test/fixtures";

interface ReasonsFixture {
  reasons: Record<string, { wire: number; retryable: boolean; text: string }>;
}

interface RadioFixtureJson {
  wire_len: number;
  radio_rev: number;
  discovering: boolean;
  connecting: boolean;
  device_wizard_open: boolean;
  paired_full: boolean;
  store_ready: boolean;
  scan_owner: string;
  scan_seq: number;
  attempt: { seq: number; addr: string; initiator: string; step: string | null; retries: number } | null;
  last_outcome: { seq: number; addr: string; result: string; reason: string | null } | null;
  scan_total_audio: number;
  scan: Array<{ addr: string; bars: number; already_paired: boolean; name: string }>;
}

const initiatorMap: Record<string, string> = {
  device: "device",
  host: "host",
  auto_reconnect: "autoReconnect",
};

const outcomeMap: Record<string, string> = {
  ok: "ok",
  ok_degraded: "okDegraded",
  failed: "failed",
  cancelled: "cancelled",
};

const stepMap: Record<string, string> = {
  connecting: "connecting",
  pairing: "pairing",
  setting_up_audio: "settingUpAudio",
  negotiating_codec: "negotiatingCodec",
  disconnecting: "disconnecting",
};

// core's `reasons.json` text differs slightly by name convention
// (SCREAMING_SNAKE -> the reason's fixture text is already human text, used
// verbatim) -- map name -> ConnectFailureReason ordinal for lookups below.
const reasonNameToOrdinal: Record<string, ConnectFailureReason> = {
  TIMEOUT: ConnectFailureReason.Timeout,
  REJECTED: ConnectFailureReason.Rejected,
  NO_A2DP_SINK: ConnectFailureReason.NoA2dpSink,
  NEEDS_PIN: ConnectFailureReason.NeedsPin,
  RADIO_ERROR: ConnectFailureReason.RadioError,
};

const reasonTextToOrdinal = new Map<string, ConnectFailureReason>();

function seedReasonTextMap(): void {
  const fixture = radioFixtureJson<ReasonsFixture>("reasons.json");
  for (const [name, info] of Object.entries(fixture.reasons)) {
    const ordinal = reasonNameToOrdinal[name];
    expect(ordinal, `reasonNameToOrdinal missing ${name}`).toBeDefined();
    reasonTextToOrdinal.set(info.text, ordinal);
  }
}
seedReasonTextMap();

describe("fixtures/radio/reasons.json vs radio.ts's ConnectFailureReason table", () => {
  const fixture = radioFixtureJson<ReasonsFixture>("reasons.json");

  it("every reason's wire/retryable/text matches exactly", () => {
    const names = Object.keys(fixture.reasons);
    expect(names).toHaveLength(5);
    for (const name of names) {
      const expected = fixture.reasons[name];
      const ordinal = reasonNameToOrdinal[name];
      expect(ordinal, `reasonNameToOrdinal missing ${name}`).toBeDefined();
      const info = connectFailureReasonInfo(ordinal);
      expect(info.wire).toBe(expected.wire);
      expect(info.retryable).toBe(expected.retryable);
      expect(info.text).toBe(expected.text);
    }
  });

  it("an unrecognised wire code falls back to RADIO_ERROR's info", () => {
    const info = connectFailureReasonInfo(99 as ConnectFailureReason);
    expect(info).toEqual(connectFailureReasonInfo(ConnectFailureReason.RadioError));
  });
});

describe("decodeRadioSnapshot vs fixtures/radio/radio-*.bin+json", () => {
  const names = radioFixtureNames();

  it("found the expected fixture set", () => {
    expect(names.length).toBeGreaterThanOrEqual(9);
    expect(names).toContain("radio-idle");
    expect(names).toContain("radio-scan-list-filters-and-caps");
  });

  it.each(names)("%s", (name) => {
    const bytes = radioFixtureBytes(`${name}.bin`);
    const fixture = radioFixtureJson<RadioFixtureJson>(`${name}.json`);
    expect(bytes.byteLength).toBe(fixture.wire_len);

    const snap = decodeRadioSnapshot(bytes);
    expect(snap).not.toBeNull();
    const s = snap!;
    expect(s.radioRev).toBe(fixture.radio_rev);
    expect(s.discovering).toBe(fixture.discovering);
    expect(s.connecting).toBe(fixture.connecting);
    expect(s.deviceWizardOpen).toBe(fixture.device_wizard_open);
    expect(s.pairedFull).toBe(fixture.paired_full);
    expect(s.storeReady).toBe(fixture.store_ready);
    expect(s.scanOwner).toBe(fixture.scan_owner);
    expect(s.scanSeq).toBe(fixture.scan_seq);
    expect(s.scanTotalAudio).toBe(fixture.scan_total_audio);

    if (fixture.attempt === null) {
      expect(s.attempt).toBeUndefined();
    } else {
      expect(s.attempt).toBeDefined();
      expect(s.attempt!.seq).toBe(fixture.attempt.seq);
      expect(s.attempt!.addr).toBe(fixture.attempt.addr);
      expect(s.attempt!.initiator).toBe(initiatorMap[fixture.attempt.initiator]);
      expect(s.attempt!.step).toBe(fixture.attempt.step === null ? undefined : stepMap[fixture.attempt.step]);
      expect(s.attempt!.retries).toBe(fixture.attempt.retries);
    }

    if (fixture.last_outcome === null) {
      expect(s.lastOutcome).toBeUndefined();
    } else {
      expect(s.lastOutcome).toBeDefined();
      expect(s.lastOutcome!.seq).toBe(fixture.last_outcome.seq);
      expect(s.lastOutcome!.addr).toBe(fixture.last_outcome.addr);
      expect(s.lastOutcome!.result).toBe(outcomeMap[fixture.last_outcome.result]);
      if (fixture.last_outcome.reason === null) {
        expect(s.lastOutcome!.reason).toBeUndefined();
      } else {
        expect(s.lastOutcome!.reason).toBe(reasonTextToOrdinal.get(fixture.last_outcome.reason));
      }
    }

    expect(s.scan).toHaveLength(fixture.scan.length);
    for (let i = 0; i < fixture.scan.length; i++) {
      expect(s.scan[i].addr).toBe(fixture.scan[i].addr);
      expect(s.scan[i].bars).toBe(fixture.scan[i].bars);
      expect(s.scan[i].alreadyPaired).toBe(fixture.scan[i].already_paired);
      expect(s.scan[i].name).toBe(fixture.scan[i].name);
    }
  });

  it("header constants match this proto", () => {
    expect(RADIO_PROTO).toBe(1);
    expect(SCAN_RECORD_LEN).toBe(42);
  });

  it("rejects a payload shorter than the fixed header", () => {
    expect(decodeRadioSnapshot(new Uint8Array(35))).toBeNull();
  });

  it("rejects an unknown radio_proto", () => {
    const bytes = radioFixtureBytes("radio-idle.bin").slice();
    bytes[0] = 99;
    expect(decodeRadioSnapshot(bytes)).toBeNull();
  });

  it("round-trips through encodeRadioSnapshotForTest for the golden populated fixture", () => {
    const bytes = radioFixtureBytes("radio-max-scan-list.bin");
    const decoded = decodeRadioSnapshot(bytes)!;
    const reencoded = encodeRadioSnapshotForTest(decoded);
    expect(reencoded).toEqual(bytes);
  });
});
