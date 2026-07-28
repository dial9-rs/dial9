// Unit tests for the tokio-stats URL contract. These pin the switch
// round-trip's page half: the FULL query string the page keeps in its URL
// (bucket, prefix, service, repeatable host, per-period bounds) is exactly what
// the dual-UI switch carries across (the switch's own verbatim preservation is
// pinned in tests/ui_switch.test.ts).

import { describe, it, expect } from "vitest";
import {
  buildSyncQuery,
  parseInitialPeriods,
  readScope,
  scopeFromParams,
  shouldAutoLoad,
} from "./url.js";

describe("readScope", () => {
  it("reads bucket/prefix/service (single) and host (repeatable)", () => {
    const p = new URLSearchParams(
      "?bucket=b&prefix=pre&service=svc&host=h1&host=h2",
    );
    expect(readScope(p)).toEqual({
      bucket: "b",
      prefix: "pre",
      service: "svc",
      host: ["h1", "h2"],
    });
  });
  it("absent params are null / empty host list", () => {
    expect(readScope(new URLSearchParams(""))).toEqual({
      bucket: null,
      prefix: null,
      service: null,
      host: [],
    });
  });
});

describe("scopeFromParams (per-side diff scope)", () => {
  it("pulls an independent scope out of a diff side's decoded params", () => {
    // Each diff side (parseDiff decodes ?diff=1&a=&b= into per-side params) has
    // its own bucket/prefix/service/host, distinct from the page scope.
    const side = new URLSearchParams("bucket=B&prefix=p2&service=svc-b&host=hb&start_ns=9");
    expect(scopeFromParams(side)).toEqual({
      bucket: "B",
      prefix: "p2",
      service: "svc-b",
      host: ["hb"],
    });
  });
});

describe("parseInitialPeriods", () => {
  it("restores multiple periods from p{i}_start_ns/p{i}_end_ns", () => {
    const p = new URLSearchParams("?p1_start_ns=100&p1_end_ns=200&p2_start_ns=300");
    expect(parseInitialPeriods(p)).toEqual([
      { startNs: "100", endNs: "200" },
      { startNs: "300", endNs: null },
    ]);
  });
  it("falls back to a single start_ns/end_ns period (button shape)", () => {
    expect(parseInitialPeriods(new URLSearchParams("?start_ns=5&end_ns=6"))).toEqual([
      { startNs: "5", endNs: "6" },
    ]);
  });
  it("both absent -> one blank period", () => {
    expect(parseInitialPeriods(new URLSearchParams(""))).toEqual([
      { startNs: null, endNs: null },
    ]);
  });
  it("caps restore at 10 periods (an 11th silently drops)", () => {
    const parts: string[] = [];
    for (let i = 1; i <= 11; i++) parts.push(`p${i}_start_ns=${i}`);
    const periods = parseInitialPeriods(new URLSearchParams("?" + parts.join("&")));
    expect(periods).toHaveLength(10);
  });
});

describe("buildSyncQuery", () => {
  it("keeps the full scope incl. repeatable host and writes per-period bounds", () => {
    const scope = { bucket: "b", prefix: "pre", service: "svc", host: ["h1", "h2"] };
    const periods = [
      { startNs: "100", endNs: "200" },
      { startNs: "300", endNs: null },
    ];
    // This is precisely the query string the switch round-trip must preserve
    // (bucket, prefix, service, host x2, and the period bounds).
    expect(buildSyncQuery(scope, periods)).toBe(
      "bucket=b&prefix=pre&service=svc&host=h1&host=h2&p1_start_ns=100&p1_end_ns=200&p2_start_ns=300",
    );
  });
  it("omits unset scope keys and unset bounds", () => {
    expect(
      buildSyncQuery(
        { bucket: null, prefix: null, service: null, host: [] },
        [{ startNs: null, endNs: null }],
      ),
    ).toBe("");
  });
  it("round-trips: parse(sync) recovers the same scope + periods", () => {
    const scope = { bucket: "b", prefix: "pre", service: "svc", host: ["h1", "h2"] };
    const periods = [
      { startNs: "100", endNs: "200" },
      { startNs: "300", endNs: null },
    ];
    const p = new URLSearchParams("?" + buildSyncQuery(scope, periods));
    expect(readScope(p)).toEqual(scope);
    expect(parseInitialPeriods(p)).toEqual(periods);
  });
});

describe("shouldAutoLoad", () => {
  it("true when the original URL carried a non-empty start_ns or bucket", () => {
    expect(shouldAutoLoad(new URLSearchParams("?bucket=b"))).toBe(true);
    expect(shouldAutoLoad(new URLSearchParams("?start_ns=5"))).toBe(true);
  });
  it("false otherwise (empty start_ns does not trigger it)", () => {
    expect(shouldAutoLoad(new URLSearchParams(""))).toBe(false);
    expect(shouldAutoLoad(new URLSearchParams("?prefix=p"))).toBe(false);
    expect(shouldAutoLoad(new URLSearchParams("?start_ns="))).toBe(false);
  });
});
