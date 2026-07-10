// Tests for url_state.js — the landing-page URL state serializer/parser.
//
// Migrated from test_url_state.js (T11); frozen core loaded via createRequire
// (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface UrlStateShape {
  bucket?: string;
  region?: string;
  prefix?: string;
  q?: string;
  tab?: string;
  tz?: string;
  last?: number;
  from?: number;
  to?: number;
}

const UrlState = require("../../url_state.js") as {
  parse: (qs: string) => UrlStateShape;
  serialize: (state?: UrlStateShape) => string;
};

describe("UrlState.parse", () => {
  it("empty query yields empty state", () => {
    expect(UrlState.parse("")).toStrictEqual({});
    expect(UrlState.parse("?")).toStrictEqual({});
  });

  it("leading '?' is optional", () => {
    expect(UrlState.parse("?bucket=b")).toStrictEqual({ bucket: "b" });
    expect(UrlState.parse("bucket=b")).toStrictEqual({ bucket: "b" });
  });

  it("reads bucket/prefix/q strings", () => {
    const s = UrlState.parse("?bucket=my-bucket&prefix=traces&q=2026-04-09");
    expect(s.bucket).toBe("my-bucket");
    expect(s.prefix).toBe("traces");
    expect(s.q).toBe("2026-04-09");
  });

  it("decodes percent-encoded values", () => {
    const s = UrlState.parse("?prefix=" + encodeURIComponent("a/b c"));
    expect(s.prefix).toBe("a/b c");
  });

  it("reads aws_region into region", () => {
    const s = UrlState.parse("?bucket=b&aws_region=us-west-2");
    expect(s.region).toBe("us-west-2");
    // An absent region stays unset (falls back to detection / default).
    expect(UrlState.parse("?bucket=b").region).toBeUndefined();
  });

  it("tab only accepts known values", () => {
    expect(UrlState.parse("?tab=raw").tab).toBe("raw");
    expect(UrlState.parse("?tab=browse").tab).toBe("browse");
    expect(UrlState.parse("?tab=bogus").tab).toBeUndefined();
  });

  it("tz only accepts known values", () => {
    expect(UrlState.parse("?tz=local").tz).toBe("local");
    expect(UrlState.parse("?tz=utc").tz).toBe("utc");
    expect(UrlState.parse("?tz=pst").tz).toBeUndefined();
  });

  it("relative 'last' is read as a number", () => {
    const s = UrlState.parse("?last=24");
    expect(s.last).toBe(24);
    expect(s.from).toBeUndefined();
    expect(s.to).toBeUndefined();
  });

  it("'last' takes precedence over from/to", () => {
    // A relative window and a precise window should never both be honored; the
    // relative one wins so the link keeps meaning "the last N hours".
    const s = UrlState.parse("?last=3&from=1000&to=2000");
    expect(s.last).toBe(3);
    expect(s.from).toBeUndefined();
    expect(s.to).toBeUndefined();
  });

  it("non-positive or invalid 'last' is ignored, from/to honored", () => {
    const s = UrlState.parse("?last=0&from=1000&to=2000");
    expect(s.last).toBeUndefined();
    expect(s.from).toBe(1000);
    expect(s.to).toBe(2000);

    const s2 = UrlState.parse("?last=abc&from=1000&to=2000");
    expect(s2.last).toBeUndefined();
    expect(s2.from).toBe(1000);
  });

  it("from/to parsed as integer epoch seconds", () => {
    const s = UrlState.parse("?from=1700000000&to=1700003600");
    expect(s.from).toBe(1700000000);
    expect(s.to).toBe(1700003600);
  });

  it("invalid from/to are dropped", () => {
    const s = UrlState.parse("?from=nope&to=");
    expect(s.from).toBeUndefined();
    expect(s.to).toBeUndefined();
  });
});

describe("UrlState.serialize", () => {
  it("empty state yields empty string", () => {
    expect(UrlState.serialize({})).toBe("");
    expect(UrlState.serialize(undefined)).toBe("");
  });

  it("omits default tab and tz", () => {
    expect(UrlState.serialize({ tab: "browse", tz: "utc" })).toBe("");
    expect(UrlState.serialize({ tab: "raw" })).toBe("tab=raw");
    expect(UrlState.serialize({ tz: "local" })).toBe("tz=local");
  });

  it("omits empty strings", () => {
    expect(UrlState.serialize({ bucket: "", prefix: "", q: "" })).toBe("");
  });

  it("relative 'last' wins over precise from/to", () => {
    const qs = UrlState.serialize({ last: 24, from: 1000, to: 2000 });
    expect(qs).toBe("last=24");
  });

  it("precise from/to when no quick range", () => {
    const qs = UrlState.serialize({ from: 1700000000, to: 1700003600 });
    expect(qs).toBe("from=1700000000&to=1700003600");
  });

  it("ignores non-positive 'last'", () => {
    const qs = UrlState.serialize({ last: 0, from: 1000, to: 2000 });
    expect(qs).toBe("from=1000&to=2000");
  });

  it("writes region as aws_region", () => {
    expect(UrlState.serialize({ bucket: "b", region: "eu-central-1" })).toBe(
      "bucket=b&aws_region=eu-central-1",
    );
    // Empty region is omitted (the ambient/default path needs no region).
    expect(UrlState.serialize({ bucket: "b", region: "" })).toBe("bucket=b");
  });

  it("stable key order", () => {
    const qs = UrlState.serialize({
      q: "x",
      to: 2000,
      from: 1000,
      tz: "local",
      tab: "raw",
      prefix: "p",
      region: "us-west-2",
      bucket: "b",
    });
    expect(qs).toBe(
      "bucket=b&aws_region=us-west-2&prefix=p&tab=raw&tz=local&from=1000&to=2000&q=x",
    );
  });

  it("percent-encodes values", () => {
    const qs = UrlState.serialize({ prefix: "a/b c" });
    expect(qs).toBe("prefix=a%2Fb+c");
  });
});

describe("UrlState round-trips", () => {
  it("relative quick range", () => {
    const state = {
      bucket: "b",
      prefix: "traces",
      tab: "browse",
      tz: "utc",
      last: 3,
    };
    const back = UrlState.parse("?" + UrlState.serialize(state));
    // Defaults (browse/utc) are omitted on serialize, so they won't reappear.
    expect(back).toStrictEqual({ bucket: "b", prefix: "traces", last: 3 });
  });

  it("precise window in raw tab, local tz", () => {
    const state = {
      bucket: "b",
      prefix: "traces",
      tab: "raw",
      tz: "local",
      from: 1700000000,
      to: 1700003600,
      q: "2026-04-09/1910",
    };
    const back = UrlState.parse("?" + UrlState.serialize(state));
    expect(back).toStrictEqual(state);
  });

  it("cross-region bucket carries aws_region", () => {
    const state = {
      bucket: "b",
      region: "ap-southeast-2",
      prefix: "traces",
      last: 1,
    };
    const back = UrlState.parse("?" + UrlState.serialize(state));
    expect(back).toStrictEqual(state);
  });
});
