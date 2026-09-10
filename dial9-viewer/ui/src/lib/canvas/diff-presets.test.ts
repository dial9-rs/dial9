import { describe, expect, it } from "vitest";
import {
  SHIFT_KEYS,
  presetAvailability,
  presetScope,
  shiftLabel,
  type ShiftKey,
} from "./diff-presets.js";

const HOUR_NS = 3_600_000_000_000n;
const DAY_NS = 86_400_000_000_000n;
const WEEK_NS = 604_800_000_000_000n;

/** A one-hour aggregate scope, ns values well past Number.MAX_SAFE_INTEGER. */
const START = 1_768_471_200_000_000_000n;
const END = START + HOUR_NS;

function scope(hosts: string[] = ["h1"], windowed = true): URLSearchParams {
  const p = new URLSearchParams({ api: "1", bucket: "traces", service: "api" });
  if (windowed) {
    p.set("start_ns", String(START));
    p.set("end_ns", String(END));
  }
  for (const host of hosts) p.append("host", host);
  return p;
}

describe("presetAvailability", () => {
  // Comparing a host against itself is a no-op, so a single-host scope must
  // not offer its own host as the "other" side.
  it("excludes the host a single-host scope is pinned to", () => {
    const available = presetAvailability(scope(["h1"]), ["h1", "h2", "h3"]);
    expect(available.otherHosts).toStrictEqual(["h2", "h3"]);
  });

  // Narrowing a multi-host aggregate down to one host IS a real comparison,
  // so every known host stays on offer.
  it("offers every host for a multi-host scope", () => {
    const available = presetAvailability(scope(["h1", "h2"]), ["h1", "h2", "h3"]);
    expect(available.otherHosts).toStrictEqual(["h1", "h2", "h3"]);
  });

  // A page that has not yet had a facet response still knows the hosts its
  // own scope names.
  it("unions the scope's own hosts with the page's known hosts", () => {
    expect(presetAvailability(scope(["h1", "h9"]), ["h2"]).otherHosts).toStrictEqual([
      "h1",
      "h2",
      "h9",
    ]);
    expect(presetAvailability(scope(["h1", "h2"]), []).otherHosts).toStrictEqual([
      "h1",
      "h2",
    ]);
  });

  it("reports the host preset unavailable when there is no other host", () => {
    expect(presetAvailability(scope(["h1"]), ["h1"]).otherHosts).toStrictEqual([]);
    expect(presetAvailability(scope(["h1"]), []).otherHosts).toStrictEqual([]);
  });

  it("needs a window to offer a time shift", () => {
    expect(presetAvailability(scope(["h1"], true)).canTimeShift).toBe(true);
    expect(presetAvailability(scope(["h1"], false)).canTimeShift).toBe(false);
  });

  it("does not mutate the scope it inspects", () => {
    const s = scope(["h1"]);
    const before = s.toString();
    presetAvailability(s, ["h2"]);
    expect(s.toString()).toBe(before);
  });
});

describe("presetScope", () => {
  it("swaps in a single host, keeping the rest of the scope", () => {
    const s = scope(["h1", "h2"]);
    const b = presetScope(s, { kind: "host", host: "h9" });

    expect(b.getAll("host")).toStrictEqual(["h9"]);
    expect(b.get("service")).toBe("api");
    expect(b.get("bucket")).toBe("traces");
    expect(b.get("start_ns")).toBe(String(START));
    expect(b.get("end_ns")).toBe(String(END));
    // Side A is untouched.
    expect(s.getAll("host")).toStrictEqual(["h1", "h2"]);
  });

  // Timestamps are ~1.78e18 ns, far past Number.MAX_SAFE_INTEGER (~9.0e15),
  // so the shift has to be BigInt arithmetic or it silently corrupts them.
  it.each([
    ["1h", HOUR_NS],
    ["24h", DAY_NS],
    ["7d", WEEK_NS],
  ] as [ShiftKey, bigint][])("shifts the window back by %s exactly", (shift, delta) => {
    const b = presetScope(scope(), { kind: "shift", shift });

    expect(b.get("start_ns")).toBe(String(START - delta));
    expect(b.get("end_ns")).toBe(String(END - delta));
  });

  it("preserves the window length across a shift", () => {
    const b = presetScope(scope(), { kind: "shift", shift: "24h" });
    expect(BigInt(b.get("end_ns")!) - BigInt(b.get("start_ns")!)).toBe(END - START);
  });

  it("keeps the host set across a shift", () => {
    const b = presetScope(scope(["h1", "h2"]), { kind: "shift", shift: "1h" });
    expect(b.getAll("host")).toStrictEqual(["h1", "h2"]);
  });

  it("leaves a windowless scope unchanged on a shift", () => {
    const b = presetScope(scope(["h1"], false), { kind: "shift", shift: "1h" });
    expect(b.get("start_ns")).toBeNull();
    expect(b.get("end_ns")).toBeNull();
    expect(b.get("service")).toBe("api");
  });
});

describe("shiftLabel", () => {
  it("labels each shift as a backward step", () => {
    expect(SHIFT_KEYS.map(shiftLabel)).toStrictEqual(["-1h", "-24h", "-7d"]);
  });
});
