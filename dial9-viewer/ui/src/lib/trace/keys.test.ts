// keys.ts tests (T09 DoD): the documented key layouts from features/01 I2
// (#225 layout, legacy layout, positional fallback) plus the
// unknown-layout discriminant (ADR-0004 section 1 defect fix; features/01
// "Live validation" Finding 1). T15 amendments covered here: the unknown
// variant carries the layout-independent filename epoch/segIndex, and
// extractPrefix (features/01 I8) lives in this module.

import { describe, expect, it } from "vitest";
import { extractPrefix, formatEpoch, parseKey } from "./keys.js";

describe("parseKey: #225 layout (date + 5 components)", () => {
  it("with prefix", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz"
    );
    expect(p).toEqual({
      layout: "known",
      service: "checkout-api",
      host: "us-east-1",
      bootId: "abcd-123213",
      epoch: 1744224000,
      segIndex: "3",
    });
  });

  it("without prefix", () => {
    const p = parseKey(
      "2026-04-09/1910/checkout-api/us-east-1/xyzw-asdfasdf/1744224000-0.bin.gz"
    );
    expect(p.layout).toBe("known");
    if (p.layout !== "known") return;
    expect(p.service).toBe("checkout-api");
    expect(p.host).toBe("us-east-1");
    expect(p.bootId).toBe("xyzw-asdfasdf");
  });

  it("uncompressed .bin filename still yields epoch/segIndex", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/abcd/1744224000-7.bin"
    );
    expect(p.layout).toBe("known");
    if (p.layout !== "known") return;
    expect(p.epoch).toBe(1744224000);
    expect(p.segIndex).toBe("7");
  });
});

describe("parseKey: legacy pre-#225 layout (date + 4 components)", () => {
  it("with prefix - unchanged behavior, empty bootId", () => {
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/host1/1744224000-2.bin.gz"
    );
    expect(p).toEqual({
      layout: "known",
      service: "checkout-api",
      host: "host1",
      bootId: "",
      epoch: 1744224000,
      segIndex: "2",
    });
  });

  it("compound instance segment parses as the #225 layout (best-effort, unchanged)", () => {
    // An instance path with an embedded slash cannot be distinguished from
    // the boot_id layout on component count alone; it parses as #225.
    const p = parseKey(
      "traces/2026-04-09/1910/checkout-api/us-east-1/i-0abc123/1744224000-0.bin.gz"
    );
    expect(p.layout).toBe("known");
    if (p.layout !== "known") return;
    expect(p.service).toBe("checkout-api");
    expect(p.host).toBe("us-east-1");
    expect(p.bootId).toBe("i-0abc123");
  });
});

describe("parseKey: positional fallback (no date-shaped segment)", () => {
  it("keeps the legacy best-effort positional fields", () => {
    const p = parseKey("custom/prefix/checkout-api/host9/1744224000-4.bin.gz");
    expect(p).toEqual({
      layout: "known",
      service: "checkout-api",
      host: "host9",
      bootId: "",
      epoch: 1744224000,
      segIndex: "4",
    });
  });

  it("non-epoch filename yields epoch 0 and empty segIndex", () => {
    const p = parseKey("custom/prefix/checkout-api/host9/oddly-named.bin");
    expect(p.layout).toBe("known");
    if (p.layout !== "known") return;
    expect(p.epoch).toBe(0);
    expect(p.segIndex).toBe("");
  });
});

describe("parseKey: unknown-layout discriminant (defect fix)", () => {
  it("the dev-server 6-segment demo key yields unknown, NOT shifted fields", () => {
    // features/01 "Live validation" Finding 1: six components after the
    // date. The legacy parser positionally shifted this to Service=host-0,
    // Host=abcd. The filename epoch/segIndex are layout-independent and
    // still parsed (T15).
    const rawKey =
      "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744224000-0.bin.gz";
    expect(parseKey(rawKey)).toEqual({
      layout: "unknown",
      rawKey,
      epoch: 1744224000,
      segIndex: "0",
    });
  });

  it("date present with too FEW components yields unknown", () => {
    const rawKey = "traces/2026-04-09/1900/checkout-api/1744224000-0.bin.gz";
    expect(parseKey(rawKey)).toEqual({
      layout: "unknown",
      rawKey,
      epoch: 1744224000,
      segIndex: "0",
    });
  });

  it("short dateless keys yield unknown (legacy returned host=<raw key>)", () => {
    const rawKey = "some/file.bin";
    expect(parseKey(rawKey)).toEqual({
      layout: "unknown",
      rawKey,
      epoch: 0,
      segIndex: "",
    });
  });

  it("unknown key with a non-epoch filename carries epoch 0 / segIndex ''", () => {
    const rawKey =
      "traces/2026-04-09/1900/demo-service/local/host-0/abcd/oddly-named.bin";
    expect(parseKey(rawKey)).toEqual({
      layout: "unknown",
      rawKey,
      epoch: 0,
      segIndex: "",
    });
  });
});

describe("extractPrefix (features/01 I8)", () => {
  it("returns everything before the first date segment", () => {
    expect(
      extractPrefix("traces/2026-04-09/1900/checkout-api/host1/1744224000-0.bin.gz")
    ).toBe("traces");
    expect(
      extractPrefix("a/b/2026-04-09/1900/checkout-api/host1/1744224000-0.bin.gz")
    ).toBe("a/b");
  });

  it("returns '' when the date layer is at the root", () => {
    expect(
      extractPrefix("2026-04-09/1900/checkout-api/host1/1744224000-0.bin.gz")
    ).toBe("");
  });

  it("returns '' when no date segment exists", () => {
    expect(extractPrefix("custom/prefix/checkout-api/host9/1744224000-4.bin.gz")).toBe("");
  });
});

describe("formatEpoch", () => {
  it("formats UTC by default", () => {
    // 2025-04-09 18:40:00 UTC.
    expect(formatEpoch(1744224000)).toBe("2025-04-09 18:40:00");
  });

  it("returns '' for a missing (0) epoch", () => {
    expect(formatEpoch(0)).toBe("");
  });

  it("localTz variant formats with the same shape", () => {
    expect(formatEpoch(1744224000, { localTz: true })).toMatch(
      /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/
    );
  });
});
