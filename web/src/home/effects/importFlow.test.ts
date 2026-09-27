import { describe, expect, it } from "vitest";
import { OpError } from "../../proto/ops";
import { canImportCopy, deriveImportBaseName, describeOpError, planImport } from "./importFlow";

describe("deriveImportBaseName", () => {
  it("strips a .txt extension", () => {
    expect(deriveImportBaseName("Warm.txt")).toBe("Warm");
  });

  it("strips a trailing ParametricEQ suffix", () => {
    expect(deriveImportBaseName("Sony WH-1000XM5 ParametricEQ.txt")).toBe("Sony WH-1000XM5");
  });

  it("falls back to Imported for an empty name", () => {
    expect(deriveImportBaseName(".txt")).toBe("Imported");
  });
});

describe("planImport", () => {
  it("creates directly when there's no name collision and room in the library", () => {
    expect(planImport(0, 3, 8, "Warm 2")).toEqual({ kind: "createDirect" });
  });

  it("refuses when the library is full and there's no collision to replace", () => {
    expect(planImport(0, 8, 8, "Warm 2")).toEqual({ kind: "full" });
  });

  it("asks to confirm Replace-vs-Copy on a name collision, even when full", () => {
    expect(planImport(5, 8, 8, "Warm 2")).toEqual({ kind: "confirmReplace", collidesWithId: 5, copyName: "Warm 2" });
  });
});

describe("canImportCopy", () => {
  it("is false only when the library is already full", () => {
    expect(canImportCopy(7, 8)).toBe(true);
    expect(canImportCopy(8, 8)).toBe(false);
  });
});

describe("describeOpError", () => {
  it("maps every known OpError to a distinct message", () => {
    const messages = new Set<string>();
    for (const value of Object.values(OpError)) {
      if (value === OpError.None) continue;
      messages.add(describeOpError(value));
    }
    expect(messages.size).toBeGreaterThan(1);
  });

  it("has a fallback for an unrecognised value", () => {
    expect(describeOpError(255 as OpError)).toMatch(/rejected/);
  });
});
