// Unit tests for the HOST_OP/GET_OP_STATUS codec, coded directly from ADA
// DESIGN on pico-link-jyhk.17 section 4 -- there is no core fixture to
// assert against yet (see ops.ts's module doc comment / the jyhk.19
// follow-up). Every test title is marked "(design-derived)".
import { describe, expect, it } from "vitest";
import {
  decodeOpStatus,
  decodeParseApoResult,
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
import { BLOB_LEN, encodePresetBlob } from "./library";

const blob = encodePresetBlob({ name: "Warm", crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: false });

describe("HOST_OP request encoders (design-derived)", () => {
  it("encodeSaveEffectRequest: header + id + base_seq + 80-byte blob", () => {
    const req = encodeSaveEffectRequest(7, 0, 3, blob);
    expect(req.length).toBe(4 + 4 + BLOB_LEN);
    expect(req[0]).toBe(OP_PROTO);
    expect(req[1]).toBe(HOST_OP_SAVE_EFFECT);
    expect(req[2]).toBe(7); // seq
    expect(req[3]).toBe(0); // flags
    const view = new DataView(req.buffer);
    expect(view.getUint16(4, true)).toBe(0); // id: create
    expect(view.getUint16(6, true)).toBe(3); // base_seq
    expect(req.subarray(8)).toEqual(blob);
  });

  it("encodeSaveEffectRequest rejects a wrong-length blob", () => {
    expect(() => encodeSaveEffectRequest(1, 1, 0, new Uint8Array(79))).toThrow(RangeError);
  });

  it("encodeDeleteEffectRequest: header + id + base_seq", () => {
    const req = encodeDeleteEffectRequest(2, 5, 9);
    expect(req.length).toBe(8);
    expect(req[1]).toBe(HOST_OP_DELETE_EFFECT);
    const view = new DataView(req.buffer);
    expect(view.getUint16(4, true)).toBe(5);
    expect(view.getUint16(6, true)).toBe(9);
  });

  it("encodeAssignRequest: header + addr[6] + effect_id", () => {
    const req = encodeAssignRequest(1, "94:DB:56:54:7C:F2", 4);
    expect(req.length).toBe(12);
    expect(req[1]).toBe(HOST_OP_ASSIGN);
    expect(Array.from(req.subarray(4, 10))).toEqual([0x94, 0xdb, 0x56, 0x54, 0x7c, 0xf2]);
    const view = new DataView(req.buffer);
    expect(view.getUint16(10, true)).toBe(4);
  });

  it("encodePreviewRequest: bypass sets flags bit0", () => {
    const req = encodePreviewRequest(3, 0, blob, true);
    expect(req.length).toBe(4 + 2 + BLOB_LEN);
    expect(req[1]).toBe(HOST_OP_PREVIEW);
    expect(req[3] & 1).toBe(1);
    const view = new DataView(req.buffer);
    expect(view.getUint16(4, true)).toBe(0); // draft
    expect(req.subarray(6)).toEqual(blob);

    const noBypass = encodePreviewRequest(3, 7, blob, false);
    expect(noBypass[3] & 1).toBe(0);
    expect(new DataView(noBypass.buffer).getUint16(4, true)).toBe(7);
  });

  it("encodePreviewEndRequest: header only", () => {
    const req = encodePreviewEndRequest(9);
    expect(req.length).toBe(4);
    expect(req[1]).toBe(HOST_OP_PREVIEW_END);
    expect(req[2]).toBe(9);
  });

  it("encodeParseApoRequest: header + name_len + name + APO text", () => {
    const req = encodeParseApoRequest(1, "Warm 2", "Preamp: -6 dB\nFilter 1: ON PK Fc 100 Hz Gain 3 dB Q 1.0\n");
    expect(req[1]).toBe(HOST_OP_PARSE_APO);
    expect(req[4]).toBe("Warm 2".length);
    const decodedName = new TextDecoder().decode(req.subarray(5, 5 + req[4]));
    expect(decodedName).toBe("Warm 2");
    const decodedText = new TextDecoder().decode(req.subarray(5 + req[4]));
    expect(decodedText).toContain("Filter 1");
  });
});

describe("decodeOpStatus (design-derived)", () => {
  function buildStatus(overrides: Partial<{ seq: number; op: number; state: number; error: number; effectId: number; libraryRev: number; persistedSeq: number; line: number; band: number; value: number; payload: Uint8Array }>): Uint8Array {
    const payload = overrides.payload ?? new Uint8Array(0);
    const out = new Uint8Array(21 + payload.length);
    const view = new DataView(out.buffer);
    out[0] = OP_PROTO;
    out[1] = overrides.seq ?? 0;
    out[2] = overrides.op ?? HOST_OP_SAVE_EFFECT;
    out[3] = overrides.state ?? 1;
    out[4] = overrides.error ?? OpError.None;
    view.setUint16(6, overrides.effectId ?? 0, true);
    view.setUint16(8, overrides.libraryRev ?? 0, true);
    view.setUint16(10, overrides.persistedSeq ?? 0, true);
    view.setUint16(12, overrides.line ?? 0, true);
    view.setUint16(14, overrides.band ?? 0, true);
    view.setFloat32(16, overrides.value ?? 0, true);
    out[20] = payload.length;
    out.set(payload, 21);
    return out;
  }

  it("decodes a DONE SAVE_EFFECT status", () => {
    const bytes = buildStatus({ seq: 3, op: HOST_OP_SAVE_EFFECT, state: 1, effectId: 4, libraryRev: 10, persistedSeq: 1 });
    const status = decodeOpStatus(bytes);
    expect(status).toEqual({ opProto: OP_PROTO, seq: 3, op: HOST_OP_SAVE_EFFECT, state: "done", error: OpError.None, effectId: 4, libraryRev: 10, persistedSeq: 1, line: 0, band: 0, value: 0, payload: new Uint8Array(0) });
  });

  it("decodes a REJECTED status with an error code", () => {
    const bytes = buildStatus({ state: 2, error: OpError.Conflict, persistedSeq: 5 });
    const status = decodeOpStatus(bytes);
    expect(status!.state).toBe("rejected");
    expect(status!.error).toBe(OpError.Conflict);
    expect(status!.persistedSeq).toBe(5);
  });

  it("decodes NONE (no op has run since boot / a stale seq)", () => {
    const bytes = buildStatus({ state: 0 });
    expect(decodeOpStatus(bytes)!.state).toBe("none");
  });

  it("rejects a payload shorter than the fixed header", () => {
    expect(decodeOpStatus(new Uint8Array(20))).toBeNull();
  });

  it("rejects a status whose declared payload_len overruns the buffer", () => {
    const bytes = buildStatus({}).slice(0, 21);
    bytes[20] = 5; // claims 5 payload bytes that aren't there
    expect(decodeOpStatus(bytes)).toBeNull();
  });

  it("round-trips a PARSE_APO result payload", () => {
    const resultBlob = encodePresetBlob({ name: "Warm", crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: false });
    const payload = new Uint8Array(BLOB_LEN + 2 + 1 + 16);
    payload.set(resultBlob, 0);
    new DataView(payload.buffer).setUint16(BLOB_LEN, 3, true); // collides_with
    const copyName = new TextEncoder().encode("Warm 2");
    payload[BLOB_LEN + 2] = copyName.length;
    payload.set(copyName, BLOB_LEN + 3);

    const bytes = buildStatus({ op: HOST_OP_PARSE_APO, state: 1, payload });
    const status = decodeOpStatus(bytes)!;
    const result = decodeParseApoResult(status.payload)!;
    expect(result.collidesWith).toBe(3);
    expect(result.copyName).toBe("Warm 2");
    expect(result.blob).toEqual(resultBlob);
  });
});
