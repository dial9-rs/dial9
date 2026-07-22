// The goto-time grammar: absolute and signed-relative seconds, optional 's'
// suffix, strict rejection of everything else.

import { describe, it, expect } from "vitest";
import { parseGotoTime } from "./goto-time.js";

describe("parseGotoTime", () => {
  it("absolute seconds", () => {
    expect(parseGotoTime("1.72")).toEqual({ relative: false, seconds: 1.72 });
    expect(parseGotoTime("0")).toEqual({ relative: false, seconds: 0 });
    expect(parseGotoTime("42s")).toEqual({ relative: false, seconds: 42 });
  });

  it("relative offsets keep their sign", () => {
    expect(parseGotoTime("+1.72")).toEqual({ relative: true, seconds: 1.72 });
    expect(parseGotoTime("-2")).toEqual({ relative: true, seconds: -2 });
    expect(parseGotoTime("+0.5s")).toEqual({ relative: true, seconds: 0.5 });
  });

  it("whitespace is trimmed", () => {
    expect(parseGotoTime("  +1.5  ")).toEqual({ relative: true, seconds: 1.5 });
  });

  it("rejects invalid input with null (caller re-prompts, never guesses)", () => {
    expect(parseGotoTime("")).toBeNull();
    expect(parseGotoTime("+")).toBeNull();
    expect(parseGotoTime("-")).toBeNull();
    expect(parseGotoTime("abc")).toBeNull();
    expect(parseGotoTime("1.2.3")).toBeNull();
    expect(parseGotoTime("1..2")).toBeNull();
    expect(parseGotoTime(".5")).toBeNull(); // bare fraction: not the grammar
    expect(parseGotoTime("1e3")).toBeNull(); // no exponent form
    expect(parseGotoTime("1 s")).toBeNull();
    expect(parseGotoTime("++1")).toBeNull();
  });
});
