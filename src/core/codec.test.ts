import { describe, expect, test } from "bun:test";
import { normalizeCodec } from "./codec.js";

describe("normalizeCodec", () => {
  test("passes H265 through", () => {
    expect(normalizeCodec("H265")).toBe("H265");
  });

  test("passes H264 through", () => {
    expect(normalizeCodec("H264")).toBe("H264");
  });

  test("defaults unknown values to H264", () => {
    expect(normalizeCodec("VP9")).toBe("H264");
    expect(normalizeCodec("h265")).toBe("H264"); // case-sensitive by design
    expect(normalizeCodec(undefined)).toBe("H264");
    expect(normalizeCodec(null)).toBe("H264");
    expect(normalizeCodec(123)).toBe("H264");
  });
});
