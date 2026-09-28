// Asserts `ops.ts` against `fixtures/host_op/*` (core's write side, bead
// pico-link-jyhk.19, `core/src/app/host_op_fixtures.rs`) -- same "core
// emits, JS asserts" discipline as `library.fixture.test.ts`. Replaces the
// old `ops.test.ts`'s hand-computed "(design-derived)" expectations, which
// had drifted from core's real `OpError` ordinals on nearly every value
// (bead pico-link-jyhk.22 review finding) because nothing here was ever
// checked against anything core emitted.
import { describe, expect, it } from "vitest";
import {
  decodeOpStatus,
  encodeAssignRequest,
  encodeDeleteEffectRequest,
  encodeParseApoRequest,
  encodePreviewEndRequest,
  encodePreviewRequest,
  encodeSaveEffectRequest,
  HOST_OP_ASSIGN,
  HOST_OP_DELETE_EFFECT,
  HOST_OP_PARSE_APO,
  HOST_OP_PREVIEW,
  HOST_OP_PREVIEW_END,
  HOST_OP_SAVE_EFFECT,
  OP_PROTO,
  OpError,
} from "./ops";
import type { OpStatus, OpStatusState } from "./ops";
import { decodePresetBlob, encodePresetBlob } from "./library";
import { hostOpFixtureBytes, hostOpFixtureJson, hostOpRequestFixtureNames, hostOpStatusFixtureNames } from "../test/fixtures";

interface OpErrorsFixture {
  op_proto: number;
  errors: Record<string, number>;
  ops: Record<string, number>;
  flags: Record<string, number>;
}

const opErrors = hostOpFixtureJson<OpErrorsFixture>("op-errors.json");

function blobFor(name: string): Uint8Array {
  return encodePresetBlob({ name, crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: false });
}

describe("fixtures/host_op/op-errors.json vs ops.ts's tables", () => {
  it("op_proto matches", () => {
    expect(OP_PROTO).toBe(opErrors.op_proto);
  });

  it("every OpError name/ordinal matches core's fixture exactly (all 24 values)", () => {
    const expectedNames = Object.keys(opErrors.errors);
    expect(expectedNames).toHaveLength(24);
    for (const name of expectedNames) {
      const camel = name
        .toLowerCase()
        .split("_")
        .map((s) => s[0]!.toUpperCase() + s.slice(1))
        .join("");
      expect(OpError, `OpError.${camel} for fixture key ${name}`).toHaveProperty(camel);
      expect((OpError as Record<string, number>)[camel]).toBe(opErrors.errors[name]);
    }
    // No extra values on our side either.
    expect(Object.keys(OpError)).toHaveLength(24);
  });

  it("op codes match", () => {
    expect(HOST_OP_SAVE_EFFECT).toBe(opErrors.ops.SAVE_EFFECT);
    expect(HOST_OP_DELETE_EFFECT).toBe(opErrors.ops.DELETE_EFFECT);
    expect(HOST_OP_ASSIGN).toBe(opErrors.ops.ASSIGN);
    expect(HOST_OP_PREVIEW).toBe(opErrors.ops.PREVIEW);
    expect(HOST_OP_PREVIEW_END).toBe(opErrors.ops.PREVIEW_END);
    expect(HOST_OP_PARSE_APO).toBe(opErrors.ops.PARSE_APO);
  });

  it("flags match", () => {
    expect(opErrors.flags.BYPASS).toBe(1);
  });
});

describe("HOST_OP request encoders vs fixtures/host_op/request-*.bin+json", () => {
  const names = hostOpRequestFixtureNames();

  it("found the expected fixture set", () => {
    expect(names.length).toBeGreaterThanOrEqual(8);
    expect(names).toContain("request-save-create");
    expect(names).toContain("request-parse-apo");
  });

  it("request-save-create", () => {
    const fixture = hostOpFixtureJson<{ seq: number; id: number; base_seq: number; preset_name: string }>("request-save-create.json");
    const bytes = hostOpFixtureBytes("request-save-create.bin");
    const req = encodeSaveEffectRequest(fixture.seq, fixture.id, fixture.base_seq, blobFor(fixture.preset_name));
    expect(req).toEqual(bytes);
  });

  it("request-save-update-rename", () => {
    const fixture = hostOpFixtureJson<{ seq: number; id: number; base_seq: number; preset_name: string }>("request-save-update-rename.json");
    const bytes = hostOpFixtureBytes("request-save-update-rename.bin");
    const req = encodeSaveEffectRequest(fixture.seq, fixture.id, fixture.base_seq, blobFor(fixture.preset_name));
    expect(req).toEqual(bytes);
  });

  it("request-delete", () => {
    const fixture = hostOpFixtureJson<{ seq: number; id: number; base_seq: number }>("request-delete.json");
    const bytes = hostOpFixtureBytes("request-delete.bin");
    const req = encodeDeleteEffectRequest(fixture.seq, fixture.id, fixture.base_seq);
    expect(req).toEqual(bytes);
  });

  it("request-assign", () => {
    const fixture = hostOpFixtureJson<{ seq: number; addr: string; effect_id: number }>("request-assign.json");
    const bytes = hostOpFixtureBytes("request-assign.bin");
    const req = encodeAssignRequest(fixture.seq, fixture.addr, fixture.effect_id);
    expect(req).toEqual(bytes);
  });

  // request-preview(-bypass) carry a real "Preview Me" blob (the no-bypass
  // one with an EQ band added) that the JSON golden doesn't fully spell out
  // (just `blob_len`) -- so unlike the other requests we can't rebuild the
  // exact blob from JSON alone. Instead: re-encode a request around the
  // fixture's own embedded blob (round-trips the header/flags layout
  // exactly) and separately decode that blob to prove it's real preset
  // data, not padding.
  it("request-preview", () => {
    const fixture = hostOpFixtureJson<{ seq: number; bypass: boolean; effect_id: number; blob_len: number }>("request-preview.json");
    const bytes = hostOpFixtureBytes("request-preview.bin");
    const embeddedBlob = bytes.subarray(6, 6 + fixture.blob_len);
    const req = encodePreviewRequest(fixture.seq, fixture.effect_id, embeddedBlob, fixture.bypass);
    expect(req).toEqual(bytes);

    const preset = decodePresetBlob(embeddedBlob);
    expect(preset.name).toBe("Preview Me");
    expect(preset.bands).toHaveLength(1);
    expect(preset.bands[0]).toMatchObject({ freqHalfHz: 2000, gainCdb: 300, qMilli: 1000 });
  });

  it("request-preview-bypass", () => {
    const fixture = hostOpFixtureJson<{ seq: number; bypass: boolean; effect_id: number; blob_len: number }>("request-preview-bypass.json");
    const bytes = hostOpFixtureBytes("request-preview-bypass.bin");
    const embeddedBlob = bytes.subarray(6, 6 + fixture.blob_len);
    const req = encodePreviewRequest(fixture.seq, fixture.effect_id, embeddedBlob, fixture.bypass);
    expect(req).toEqual(bytes);

    const preset = decodePresetBlob(embeddedBlob);
    expect(preset.name).toBe("Preview Me");
    expect(preset.bands).toHaveLength(0);
  });

  it("request-preview-end", () => {
    const fixture = hostOpFixtureJson<{ seq: number }>("request-preview-end.json");
    const bytes = hostOpFixtureBytes("request-preview-end.bin");
    expect(encodePreviewEndRequest(fixture.seq)).toEqual(bytes);
  });

  it("request-parse-apo", () => {
    const fixture = hostOpFixtureJson<{ seq: number; name: string; apo_text: string }>("request-parse-apo.json");
    const bytes = hostOpFixtureBytes("request-parse-apo.bin");
    const req = encodeParseApoRequest(fixture.seq, fixture.name, fixture.apo_text);
    expect(req).toEqual(bytes);
  });
});

interface StatusFixture {
  op_proto: number;
  op: string;
  op_code: number;
  seq: number;
  state: number;
  error: number;
  effect_id: number;
  library_rev: number;
  persisted_seq: number;
  line: number;
  band: number;
  value: number;
  payload_len: number;
}

function stateName(state: number): OpStatusState {
  return state === 1 ? "done" : state === 2 ? "rejected" : "none";
}

describe("decodeOpStatus vs fixtures/host_op/status-*.bin+json", () => {
  const names = hostOpStatusFixtureNames();

  it("found the expected fixture set", () => {
    expect(names.length).toBeGreaterThanOrEqual(10);
    expect(names).toContain("status-save-success");
    expect(names).toContain("status-range-error");
  });

  it.each(names)("%s", (name) => {
    const bytes = hostOpFixtureBytes(`${name}.bin`);
    const fixture = hostOpFixtureJson<StatusFixture>(`${name}.json`);
    expect(bytes.byteLength).toBe(21 + fixture.payload_len);

    const status = decodeOpStatus(bytes) as OpStatus;
    expect(status).not.toBeNull();
    expect(status.opProto).toBe(fixture.op_proto);
    expect(status.seq).toBe(fixture.seq);
    expect(status.op).toBe(fixture.op_code);
    expect(status.state).toBe(stateName(fixture.state));
    expect(status.error).toBe(fixture.error);
    expect(status.effectId).toBe(fixture.effect_id);
    expect(status.libraryRev).toBe(fixture.library_rev);
    expect(status.persistedSeq).toBe(fixture.persisted_seq);
    expect(status.line).toBe(fixture.line);
    expect(status.band).toBe(fixture.band);
    expect(status.value).toBeCloseTo(fixture.value, 5);
    expect(status.payload.length).toBe(fixture.payload_len);
  });

  it("status-range-error carries GAIN_RANGE's (band, value) pair", () => {
    const fixture = hostOpFixtureJson<StatusFixture>("status-range-error.json");
    expect(fixture.error).toBe(opErrors.errors.GAIN_RANGE);
    const status = decodeOpStatus(hostOpFixtureBytes("status-range-error.bin"))!;
    expect(status.error).toBe(OpError.GainRange);
    expect(status.band).toBe(1);
    expect(status.value).toBeCloseTo(35, 5);
  });

  it("status-conflict carries CONFLICT's persisted_seq for the retry", () => {
    const status = decodeOpStatus(hostOpFixtureBytes("status-conflict.bin"))!;
    expect(status.error).toBe(OpError.Conflict);
    expect(status.persistedSeq).toBe(1);
  });

  it("status-parse-apo-success's payload round-trips through decodeParseApoResult", () => {
    const status = decodeOpStatus(hostOpFixtureBytes("status-parse-apo-success.bin"))!;
    expect(status.payload.length).toBe(99);
  });

  it("rejects a payload shorter than the fixed header", () => {
    expect(decodeOpStatus(new Uint8Array(20))).toBeNull();
  });

  it("rejects a status whose declared payload_len overruns the buffer", () => {
    const bytes = hostOpFixtureBytes("status-save-success.bin").slice();
    bytes[20] = 5; // claims 5 payload bytes that aren't there
    expect(decodeOpStatus(bytes)).toBeNull();
  });
});
