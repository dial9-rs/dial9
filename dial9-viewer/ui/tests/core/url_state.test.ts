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
  credentialMode?: "ambient" | "literal" | "role";
  roleArn?: string;
  prefix?: string;
  service?: string;
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

  it("reads bucket/prefix/service/q strings", () => {
    const s = UrlState.parse(
      "?bucket=my-bucket&prefix=traces&service=checkout-api&q=2026-04-09",
    );
    expect(s.bucket).toBe("my-bucket");
    expect(s.prefix).toBe("traces");
    expect(s.service).toBe("checkout-api");
    expect(s.q).toBe("2026-04-09");
  });

  it("decodes percent-encoded values", () => {
    const s = UrlState.parse(
      "?prefix=" +
        encodeURIComponent("a/b c") +
        "&service=" +
        encodeURIComponent("checkout api"),
    );
    expect(s.prefix).toBe("a/b c");
    expect(s.service).toBe("checkout api");
  });

  it("reads aws_region into region", () => {
    const s = UrlState.parse("?bucket=b&aws_region=us-west-2");
    expect(s.region).toBe("us-west-2");
    // An absent region stays unset (falls back to detection / default).
    expect(UrlState.parse("?bucket=b").region).toBeUndefined();
  });

  it("reads aws_role_arn into roleArn", () => {
    const arn = "arn:aws:iam::123456789012:role/dial9-reader";
    const s = UrlState.parse("?bucket=b&aws_role_arn=" + encodeURIComponent(arn));
    expect(s.roleArn).toBe(arn);
    // An absent role ARN stays unset (ambient / static-BYOC path).
    expect(UrlState.parse("?bucket=b").roleArn).toBeUndefined();
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
    expect(
      UrlState.serialize({ bucket: "", prefix: "", service: "", q: "" }),
    ).toBe("");
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

  it("writes roleArn as aws_role_arn", () => {
    const arn = "arn:aws:iam::123456789012:role/dial9-reader";
    const qs = UrlState.serialize({
      bucket: "b",
      region: "us-west-2",
      credentialMode: "role",
      roleArn: arn,
    });
    expect(qs).toBe(
      "bucket=b&aws_region=us-west-2&credential_mode=role&aws_role_arn=" +
        encodeURIComponent(arn),
    );
    // Empty roleArn is omitted (static-BYOC / ambient path carries none).
    expect(UrlState.serialize({ bucket: "b", roleArn: "" })).toBe("bucket=b");
  });

  it("stable key order", () => {
    const qs = UrlState.serialize({
      q: "x",
      to: 2000,
      from: 1000,
      tz: "local",
      tab: "raw",
      prefix: "p",
      service: "checkout",
      region: "us-west-2",
      bucket: "b",
    });
    expect(qs).toBe(
      "bucket=b&aws_region=us-west-2&prefix=p&service=checkout&tab=raw&tz=local&from=1000&to=2000&q=x",
    );
  });

  it("percent-encodes values", () => {
    const qs = UrlState.serialize({ prefix: "a/b c", service: "checkout api" });
    expect(qs).toBe("prefix=a%2Fb+c&service=checkout+api");
  });
});

describe("UrlState round-trips", () => {
  it("relative quick range", () => {
    const state: UrlStateShape = {
      bucket: "b",
      prefix: "traces",
      service: "checkout-api",
      tab: "browse",
      tz: "utc",
      last: 3,
    };
    const back = UrlState.parse("?" + UrlState.serialize(state));
    // Defaults (browse/utc) are omitted on serialize, so they won't reappear.
    expect(back).toStrictEqual({
      bucket: "b",
      prefix: "traces",
      service: "checkout-api",
      last: 3,
    });
  });

  it("precise window in raw tab, local tz", () => {
    const state: UrlStateShape = {
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
    const state: UrlStateShape = {
      bucket: "b",
      region: "ap-southeast-2",
      prefix: "traces",
      last: 1,
    };
    const back = UrlState.parse("?" + UrlState.serialize(state));
    expect(back).toStrictEqual(state);
  });

  it("assume-role link carries aws_role_arn", () => {
    const state: UrlStateShape = {
      bucket: "b",
      region: "us-east-1",
      credentialMode: "role",
      roleArn: "arn:aws:iam::123456789012:role/dial9-reader",
      prefix: "dial9-traces",
      last: 1,
    };
    const back = UrlState.parse("?" + UrlState.serialize(state));
    expect(back).toStrictEqual(state);
  });
});
