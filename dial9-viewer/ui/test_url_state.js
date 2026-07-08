"use strict";

// Tests for url_state.js — the landing-page URL state serializer/parser.
// Run with: node dial9-viewer/ui/test_url_state.js

const { test, assert, summarize } = require("./test_harness.js");
const UrlState = require("./url_state.js");

// ── parse ──

test("parse: empty query yields empty state", () => {
  assert.deepStrictEqual(UrlState.parse(""), {});
  assert.deepStrictEqual(UrlState.parse("?"), {});
});

test("parse: leading '?' is optional", () => {
  assert.deepStrictEqual(UrlState.parse("?bucket=b"), { bucket: "b" });
  assert.deepStrictEqual(UrlState.parse("bucket=b"), { bucket: "b" });
});

test("parse: reads bucket/prefix/q strings", () => {
  const s = UrlState.parse("?bucket=my-bucket&prefix=traces&q=2026-04-09");
  assert.strictEqual(s.bucket, "my-bucket");
  assert.strictEqual(s.prefix, "traces");
  assert.strictEqual(s.q, "2026-04-09");
});

test("parse: decodes percent-encoded values", () => {
  const s = UrlState.parse("?prefix=" + encodeURIComponent("a/b c"));
  assert.strictEqual(s.prefix, "a/b c");
});

test("parse: reads aws_region into region", () => {
  const s = UrlState.parse("?bucket=b&aws_region=us-west-2");
  assert.strictEqual(s.region, "us-west-2");
  // An absent region stays unset (falls back to detection / default).
  assert.strictEqual(UrlState.parse("?bucket=b").region, undefined);
});

test("parse: reads aws_role_arn into roleArn", () => {
  const arn = "arn:aws:iam::123456789012:role/dial9-reader";
  const s = UrlState.parse("?bucket=b&aws_role_arn=" + encodeURIComponent(arn));
  assert.strictEqual(s.roleArn, arn);
  // An absent role ARN stays unset (ambient / static-BYOC path).
  assert.strictEqual(UrlState.parse("?bucket=b").roleArn, undefined);
});

test("serialize: writes roleArn as aws_role_arn", () => {
  const arn = "arn:aws:iam::123456789012:role/dial9-reader";
  const qs = UrlState.serialize({ bucket: "b", region: "us-west-2", roleArn: arn });
  assert.strictEqual(
    qs,
    "bucket=b&aws_region=us-west-2&aws_role_arn=" + encodeURIComponent(arn)
  );
  // Empty roleArn is omitted (static-BYOC / ambient path carries none).
  assert.strictEqual(UrlState.serialize({ bucket: "b", roleArn: "" }), "bucket=b");
});

test("round-trip: assume-role link carries aws_role_arn", () => {
  const state = {
    bucket: "b",
    region: "us-east-1",
    roleArn: "arn:aws:iam::123456789012:role/dial9-reader",
    prefix: "dial9-traces",
    last: 1,
  };
  const back = UrlState.parse("?" + UrlState.serialize(state));
  assert.deepStrictEqual(back, state);
});

test("parse: tab only accepts known values", () => {
  assert.strictEqual(UrlState.parse("?tab=raw").tab, "raw");
  assert.strictEqual(UrlState.parse("?tab=browse").tab, "browse");
  assert.strictEqual(UrlState.parse("?tab=bogus").tab, undefined);
});

test("parse: tz only accepts known values", () => {
  assert.strictEqual(UrlState.parse("?tz=local").tz, "local");
  assert.strictEqual(UrlState.parse("?tz=utc").tz, "utc");
  assert.strictEqual(UrlState.parse("?tz=pst").tz, undefined);
});

test("parse: relative 'last' is read as a number", () => {
  const s = UrlState.parse("?last=24");
  assert.strictEqual(s.last, 24);
  assert.strictEqual(s.from, undefined);
  assert.strictEqual(s.to, undefined);
});

test("parse: 'last' takes precedence over from/to", () => {
  // A relative window and a precise window should never both be honored; the
  // relative one wins so the link keeps meaning "the last N hours".
  const s = UrlState.parse("?last=3&from=1000&to=2000");
  assert.strictEqual(s.last, 3);
  assert.strictEqual(s.from, undefined);
  assert.strictEqual(s.to, undefined);
});

test("parse: non-positive or invalid 'last' is ignored, from/to honored", () => {
  const s = UrlState.parse("?last=0&from=1000&to=2000");
  assert.strictEqual(s.last, undefined);
  assert.strictEqual(s.from, 1000);
  assert.strictEqual(s.to, 2000);

  const s2 = UrlState.parse("?last=abc&from=1000&to=2000");
  assert.strictEqual(s2.last, undefined);
  assert.strictEqual(s2.from, 1000);
});

test("parse: from/to parsed as integer epoch seconds", () => {
  const s = UrlState.parse("?from=1700000000&to=1700003600");
  assert.strictEqual(s.from, 1700000000);
  assert.strictEqual(s.to, 1700003600);
});

test("parse: invalid from/to are dropped", () => {
  const s = UrlState.parse("?from=nope&to=");
  assert.strictEqual(s.from, undefined);
  assert.strictEqual(s.to, undefined);
});

// ── serialize ──

test("serialize: empty state yields empty string", () => {
  assert.strictEqual(UrlState.serialize({}), "");
  assert.strictEqual(UrlState.serialize(undefined), "");
});

test("serialize: omits default tab and tz", () => {
  assert.strictEqual(UrlState.serialize({ tab: "browse", tz: "utc" }), "");
  assert.strictEqual(UrlState.serialize({ tab: "raw" }), "tab=raw");
  assert.strictEqual(UrlState.serialize({ tz: "local" }), "tz=local");
});

test("serialize: omits empty strings", () => {
  assert.strictEqual(UrlState.serialize({ bucket: "", prefix: "", q: "" }), "");
});

test("serialize: relative 'last' wins over precise from/to", () => {
  const qs = UrlState.serialize({ last: 24, from: 1000, to: 2000 });
  assert.strictEqual(qs, "last=24");
});

test("serialize: precise from/to when no quick range", () => {
  const qs = UrlState.serialize({ from: 1700000000, to: 1700003600 });
  assert.strictEqual(qs, "from=1700000000&to=1700003600");
});

test("serialize: ignores non-positive 'last'", () => {
  const qs = UrlState.serialize({ last: 0, from: 1000, to: 2000 });
  assert.strictEqual(qs, "from=1000&to=2000");
});

test("serialize: writes region as aws_region", () => {
  assert.strictEqual(
    UrlState.serialize({ bucket: "b", region: "eu-central-1" }),
    "bucket=b&aws_region=eu-central-1"
  );
  // Empty region is omitted (the ambient/default path needs no region).
  assert.strictEqual(UrlState.serialize({ bucket: "b", region: "" }), "bucket=b");
});

test("serialize: stable key order", () => {
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
  assert.strictEqual(
    qs,
    "bucket=b&aws_region=us-west-2&prefix=p&tab=raw&tz=local&from=1000&to=2000&q=x"
  );
});

test("serialize: percent-encodes values", () => {
  const qs = UrlState.serialize({ prefix: "a/b c" });
  assert.strictEqual(qs, "prefix=a%2Fb+c");
});

// ── round-trips ──

test("round-trip: relative quick range", () => {
  const state = { bucket: "b", prefix: "traces", tab: "browse", tz: "utc", last: 3 };
  const back = UrlState.parse("?" + UrlState.serialize(state));
  // Defaults (browse/utc) are omitted on serialize, so they won't reappear.
  assert.deepStrictEqual(back, { bucket: "b", prefix: "traces", last: 3 });
});

test("round-trip: precise window in raw tab, local tz", () => {
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
  assert.deepStrictEqual(back, state);
});

test("round-trip: cross-region bucket carries aws_region", () => {
  const state = { bucket: "b", region: "ap-southeast-2", prefix: "traces", last: 1 };
  const back = UrlState.parse("?" + UrlState.serialize(state));
  assert.deepStrictEqual(back, state);
});

summarize();
