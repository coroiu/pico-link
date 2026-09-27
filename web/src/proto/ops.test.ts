// Unit tests for `ops.ts` behaviour that isn't a wire-layout fact covered
// by `ops.fixture.test.ts` (fixture-driven, cross-checked against
// `fixtures/host_op/*` from core's write side, bead pico-link-jyhk.19):
// input-validation errors, and round-tripping the `*ForTest` encoders
// `FakeTransport`/tests use to synthesize device replies.
import { describe, expect, it } from "vitest";
import { decodeOpStatus, decodeParseApoResult, encodeOpStatusForTest, encodeParseApoResultForTest, encodePreviewRequest, encodeSaveEffectRequest, HOST_OP_PARSE_APO, OP_PROTO, OpError } from "./ops";
import { encodePresetBlob } from "./library";

const blob = encodePresetBlob({ name: "Warm", crossfeed: "off", bands: [], preamp: { kind: "auto" }, eqLocked: false });

describe("ops.ts request encoders: input validation", () => {
  it("encodeSaveEffectRequest rejects a wrong-length blob", () => {
    expect(() => encodeSaveEffectRequest(1, 1, 0, new Uint8Array(79))).toThrow(RangeError);
  });

  it("encodePreviewRequest rejects a wrong-length blob", () => {
    expect(() => encodePreviewRequest(1, 0, new Uint8Array(79), false)).toThrow(RangeError);
  });
});

describe("decodeOpStatus: state 'none' (no op has run since boot / a stale seq)", () => {
  it("decodes state byte 0 as 'none'", () => {
    const bytes = encodeOpStatusForTest({ seq: 0, op: 0, state: "none", error: OpError.None, effectId: 0, libraryRev: 0, persistedSeq: 0, line: 0, band: 0, value: 0, payload: new Uint8Array(0) });
    const status = decodeOpStatus(bytes)!;
    expect(status.state).toBe("none");
    expect(status.opProto).toBe(OP_PROTO);
  });
});

describe("encodeOpStatusForTest / encodeParseApoResultForTest: the FakeTransport-facing inverses of the decoders", () => {
  it("encodeOpStatusForTest round-trips through decodeOpStatus", () => {
    const bytes = encodeOpStatusForTest({ seq: 3, op: HOST_OP_PARSE_APO, state: "rejected", error: OpError.ParseError, effectId: 0, libraryRev: 10, persistedSeq: 0, line: 4, band: 0, value: 0, payload: new Uint8Array(0) });
    const status = decodeOpStatus(bytes)!;
    expect(status).toEqual({ opProto: OP_PROTO, seq: 3, op: HOST_OP_PARSE_APO, state: "rejected", error: OpError.ParseError, effectId: 0, libraryRev: 10, persistedSeq: 0, line: 4, band: 0, value: 0, payload: new Uint8Array(0) });
  });

  it("encodeParseApoResultForTest round-trips through decodeParseApoResult", () => {
    const result = encodeParseApoResultForTest({ blob, collidesWith: 3, copyName: "Warm 2" });
    const decoded = decodeParseApoResult(result)!;
    expect(decoded.collidesWith).toBe(3);
    expect(decoded.copyName).toBe("Warm 2");
    expect(decoded.blob).toEqual(blob);
  });
});
