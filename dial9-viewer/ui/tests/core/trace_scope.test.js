import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { test } from "vitest";

const require = createRequire(import.meta.url);

// Tests for trace_scope.js — the compact, stateless scope that replaces
// one-`trace=`-per-file in the viewer/flamegraph navigation URL. The point is a
// URL bounded by host count (not file count) so it never approaches
// CloudFront's 8192-byte request-URI cap, and re-resolves in any browser so
// deep links stay portable.

const scope = require("../../trace_scope.js");

const CLOUDFRONT_URI_LIMIT = 8192;

// trace_scope.js is the single source of truth for parseKey/extractPrefix; the
// page scripts delegate to it, so these are the canonical fixtures.

// A realistic key in the boot_id layout.
function key(host, epoch, i) {
  return `traces/2026-06-29/1915/shale/${host}/abcd-boot/${epoch}-${i}.bin.gz`;
}

function keyForService(service, host, epoch, i) {
  return `traces/2026-06-29/1915/${service}/${host}/abcd-boot/${epoch}-${i}.bin.gz`;
}

// --- parseKey / extractPrefix (moved here; keep parity) --------------------

test("parseKey reads the boot_id layout", () => {
  const p = scope.parseKey(key("ip-10-2-3-4", 1782760500, 7));
  assert.strictEqual(p.service, "shale");
  assert.strictEqual(p.host, "ip-10-2-3-4");
  assert.strictEqual(p.bootId, "abcd-boot");
  assert.strictEqual(p.epoch, 1782760500);
});

test("parseKey reads the legacy (no boot_id) layout", () => {
  const p = scope.parseKey("traces/2026-06-29/1915/shale/ip-10-2-3-4/1782760500-7.bin.gz");
  assert.strictEqual(p.service, "shale");
  assert.strictEqual(p.host, "ip-10-2-3-4");
  assert.strictEqual(p.bootId, "");
  assert.strictEqual(p.epoch, 1782760500);
});

test("extractPrefix returns everything before the layout root", () => {
  assert.strictEqual(scope.extractPrefix(key("h", 1782760500, 1)), "traces");
  assert.strictEqual(scope.extractPrefix("2026-06-29/1915/svc/h/b/1-1.bin.gz"), "");
});

test("parseKey reads escaped versioned partitions from an opaque prefix", () => {
  const versionedKey =
    "company/date=archive/%25/version=1/date=2026-08-14/" +
    "service=payments%2Fapi/time=1937/instance=host%2Fone%3D0%25abc/" +
    "boot=boot%2Fid/1786736220-3.bin.gz";
  const p = scope.parseKey(versionedKey);
  assert.strictEqual(p.service, "payments/api");
  assert.strictEqual(p.host, "host/one=0%abc");
  assert.strictEqual(p.bootId, "boot/id");
  assert.strictEqual(p.epoch, 1786736220);
  assert.strictEqual(scope.extractPrefix(versionedKey), "company/date=archive/%25");
});

test("parseKey decodes one partition escaping layer", () => {
  const p = scope.parseKey(
    "service=prefix/version=1/date=2026-08-14/service=payments%252Fapi/" +
      "time=1937/instance=host/boot=boot/1786736220-3.bin.gz",
  );
  assert.strictEqual(p.service, "payments%2Fapi");
});

test("parseKey rejects reordered or incomplete versioned layouts", () => {
  const reordered =
    "traces/version=1/date=2026-08-14/time=1937/service=svc/" +
    "instance=host%2Fone/boot=boot/1786736220-3.bin.gz";
  const p = scope.parseKey(reordered);
  assert.strictEqual(p.service, "");
  assert.strictEqual(p.host, reordered);

  const missingBoot =
    "traces/version=1/date=2026-08-14/service=svc/time=1937/" +
    "instance=host%2Fone/1786736220-3.bin.gz";
  assert.strictEqual(scope.parseKey(missingBoot).host, missingBoot);
});

test("parseKey does not build a scope from malformed versioned values", () => {
  const rawKey =
    "service=prefix/version=1/date=2026-08-14/service=bad%2/time=1937/instance=host/" +
    "boot=boot/1786736220-3.bin.gz";
  const p = scope.parseKey(rawKey);
  assert.strictEqual(p.service, "");
  assert.strictEqual(p.host, rawKey);
});

test("parseKey: historical boot-id layout with prefix", () => {
  const p = scope.parseKey("service=prefix/traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz");
  assert.strictEqual(p.service, "checkout-api");
  assert.strictEqual(p.host, "us-east-1");
  assert.strictEqual(p.bootId, "abcd-123213");
  assert.strictEqual(p.epoch, 1744224000);
  assert.strictEqual(p.segIndex, "3");
});

test("parseKey: legacy layout (no boot_id) keeps bootId empty", () => {
  const p = scope.parseKey("traces/2026-04-09/1910/checkout-api/host1/1744224000-2.bin.gz");
  assert.strictEqual(p.service, "checkout-api");
  assert.strictEqual(p.host, "host1");
  assert.strictEqual(p.bootId, "");
  assert.strictEqual(p.segIndex, "2");
});

test("parseKey: compound instance path is best-effort, never throws", () => {
  const p = scope.parseKey("traces/2026-04-09/1910/checkout-api/us-east-1/i-0abc123/1744224000-0.bin.gz");
  assert.ok(p && typeof p === "object");
});

// --- encode / read round-trip ----------------------------------------------

test("encodeScope/readScope round-trips a scope", () => {
  const s = {
    bucket: "cell1-prod-pdx-dial9-traces",
    region: "us-west-2",
    roleArn: "arn:aws:iam::123456789012:role/Dial9TraceReader",
    credentialMode: "",
    prefix: "traces",
    service: "shale",
    hosts: ["ip-10-2-1-1", "ip-10-2-1-2"],
    from: 1782760000,
    to: 1782760800,
  };
  const { query } = scope.encodeScope(new URLSearchParams(), s);
  // The role ARN rides in the scope so a link opened in a fresh session still
  // has an identity to read the bucket with.
  assert.strictEqual(
    new URLSearchParams(query).get("s_role_arn"),
    s.roleArn,
    "role ARN serialized",
  );
  const got = scope.readScope(new URLSearchParams(query));
  assert.deepStrictEqual(got, s);
});

test("encodeScope/readScope round-trips an empty region/role as ''", () => {
  // A region- and role-less scope (bucket in the server's default region, read
  // with the tab's own creds) must still round-trip cleanly: encodeScope omits
  // s_region/s_role_arn, readScope normalizes the absence back to "".
  const s = {
    bucket: "b",
    region: "",
    roleArn: "",
    credentialMode: "",
    prefix: "traces",
    service: "shale",
    hosts: ["h1"],
    from: 1,
    to: 2,
  };
  const { query } = scope.encodeScope(new URLSearchParams(), s);
  assert.strictEqual(new URLSearchParams(query).get("s_region"), null, "empty region not serialized");
  assert.strictEqual(new URLSearchParams(query).get("s_role_arn"), null, "empty role not serialized");
  assert.deepStrictEqual(scope.readScope(new URLSearchParams(query)), s);
});

test("readScope returns null when no time window is present", () => {
  assert.strictEqual(scope.readScope(new URLSearchParams("s_bucket=b")), null);
  assert.strictEqual(scope.hasScope(new URLSearchParams("foo=bar")), false);
});

test("encodeScope preserves unrelated base params and namespaces its own", () => {
  const base = new URLSearchParams();
  base.set("svc", "shale"); // title param — must survive untouched
  const { query } = scope.encodeScope(base, { hosts: [], from: 1, to: 2, bucket: "b" });
  const p = new URLSearchParams(query);
  assert.strictEqual(p.get("svc"), "shale");
  assert.strictEqual(p.get("s_bucket"), "b");
  // No collision with the viewer's own host/from/to params.
  assert.strictEqual(p.get("host"), null);
  assert.strictEqual(p.get("from"), null);
});

test("encodeAggregationParams emits the un-namespaced names + ns window", () => {
  const s = {
    bucket: "bkt",
    region: "us-west-2",
    prefix: "traces",
    service: "shale",
    hosts: ["h1", "h2"],
    from: 1782760000, // epoch SECONDS
    to: 1782760800,
  };
  const base = new URLSearchParams();
  base.set("api", "1"); // caller-supplied (flamegraph demand-driven mode)
  const { query, hostsDropped } = scope.encodeAggregationParams(base, s);
  const p = new URLSearchParams(query);
  assert.strictEqual(hostsDropped, false, "small host set fits inline");
  assert.strictEqual(p.get("api"), "1", "base params preserved");
  assert.strictEqual(p.get("bucket"), "bkt");
  assert.strictEqual(p.get("prefix"), "traces");
  assert.strictEqual(p.get("service"), "shale");
  // Region rides as `aws_region` — the same param the server reads — so a link
  // straight to /api/flamegraph signs the right regional endpoint on its own.
  assert.strictEqual(p.get("aws_region"), "us-west-2");
  assert.deepStrictEqual(p.getAll("host"), ["h1", "h2"], "repeatable host set");
  // Window converts seconds -> nanoseconds for the aggregation endpoints.
  assert.strictEqual(p.get("start_ns"), "1782760000000000000");
  assert.strictEqual(p.get("end_ns"), "1782760800000000000");
  // Aggregation params are NOT namespaced (they go straight to the server).
  assert.strictEqual(p.get("s_bucket"), null);
});

test("encodeAggregationParams omits an empty service (all services in box)", () => {
  // A multi-service box leaves service unset (scopeFromKeys sets service='').
  const s = { bucket: "b", prefix: "", service: "", hosts: ["h1"], from: 1, to: 2 };
  const { query } = scope.encodeAggregationParams(new URLSearchParams(), s);
  const p = new URLSearchParams(query);
  assert.strictEqual(p.get("service"), null, "no service filter when scope spans many");
  assert.deepStrictEqual(p.getAll("host"), ["h1"]);
});

test("encodeAggregationParams degrades a pathological host set but stays URI-safe", () => {
  // Same failure mode as encodeScope: the server caps sampling work, not URL
  // length, so a huge fleet must not be listed inline or the aggregation
  // flamegraph / tokio-stats link would 414. Drop the host set (empty = all
  // hosts in window server-side) and signal it.
  const hosts = [];
  for (let h = 0; h < 5000; h++) hosts.push(`ip-10-2-${h}.us-west-2.compute.internal`);
  const s = { bucket: "b", prefix: "traces", service: "shale", hosts, from: 1782760000, to: 1782760800 };
  const { query, hostsDropped } = scope.encodeAggregationParams(new URLSearchParams(), s);
  assert.ok(query.length <= CLOUDFRONT_URI_LIMIT, `degraded query is ${query.length} bytes, must be <= ${CLOUDFRONT_URI_LIMIT}`);
  assert.strictEqual(hostsDropped, true, "signals the host set was dropped");
  const p = new URLSearchParams(query);
  assert.deepStrictEqual(p.getAll("host"), [], "degrades to all-hosts-in-window");
  assert.strictEqual(p.get("start_ns"), "1782760000000000000", "window preserved");
});

// --- the actual bug: a fleet-wide selection stays short --------------------

test("scope URL stays under the URI cap for thousands of files (host-bounded)", () => {
  // 5000 files across 60 hosts — inline as trace= this is ~700 KB of query;
  // as a scope it is bounded by the 60-host set, which fits comfortably.
  const keys = [];
  for (let h = 0; h < 60; h++) {
    const host = `ip-10-2-${100 + h}-50.us-west-2.compute.internal`; // one host, many files
    for (let f = 0; f < 84; f++) keys.push(key(host, 1782760000 + f, f));
  }
  const s = scope.scopeFromKeys("cell1-prod-pdx-dial9-traces", keys, 1782760000, 1782760800);
  const { query, hostsDropped } = scope.encodeScope(new URLSearchParams(), s);
  assert.ok(
    query.length <= CLOUDFRONT_URI_LIMIT,
    `scope query is ${query.length} bytes for ${keys.length} files / 60 hosts, must be <= ${CLOUDFRONT_URI_LIMIT}`,
  );
  assert.strictEqual(hostsDropped, false, "60 real hostnames fit inline");
  assert.strictEqual(scope.readScope(new URLSearchParams(query)).hosts.length, 60);
});

test("pathological host set degrades to time-range-only but stays URI-safe", () => {
  // A selection spanning thousands of hosts can't list them all in the URL;
  // encodeScope drops the host set and the scope means "all hosts in window".
  const hosts = [];
  for (let h = 0; h < 5000; h++) hosts.push(`ip-10-2-${h}.us-west-2.compute.internal`);
  const s = { bucket: "b", prefix: "traces", service: "shale", hosts, from: 1782760000, to: 1782760800 };
  const { query, hostsDropped } = scope.encodeScope(new URLSearchParams(), s);
  assert.ok(query.length <= CLOUDFRONT_URI_LIMIT, `degraded query is ${query.length} bytes, must be <= ${CLOUDFRONT_URI_LIMIT}`);
  assert.strictEqual(hostsDropped, true, "signals the host set was dropped");
  const got = scope.readScope(new URLSearchParams(query));
  assert.deepStrictEqual(got.hosts, [], "degrades to all-hosts-in-window");
  assert.strictEqual(got.from, 1782760000, "window preserved");
});

// --- resolveScope re-lists via /api/browse and filters ---------------------

test("scopeFromKeys derives service/hosts/prefix/window", () => {
  const keys = [key("h1", 1782760100, 1), key("h2", 1782760200, 1)];
  const s = scope.scopeFromKeys("bkt", keys, 1782760000, 1782760800);
  assert.strictEqual(s.service, "shale");
  assert.deepStrictEqual(s.hosts.sort(), ["h1", "h2"]);
  assert.strictEqual(s.prefix, "traces");
  assert.strictEqual(s.from, 1782760000);
  assert.strictEqual(s.to, 1782760800);
});

test("scopeFromKeys carries the region when supplied, else ''", () => {
  const keys = [key("h1", 1782760100, 1)];
  assert.strictEqual(
    scope.scopeFromKeys("bkt", keys, 1782760000, 1782760800, "us-west-2").region,
    "us-west-2",
    "region threaded through",
  );
  // Trailing/optional: existing 4-arg callers get an empty region, not undefined.
  assert.strictEqual(scope.scopeFromKeys("bkt", keys, 1782760000, 1782760800).region, "");
});

test("scopeFromKeys region survives an aggregation round-trip", () => {
  // End-to-end: a heatmap selection in a us-west-2 bucket produces an
  // /api/flamegraph link that carries aws_region, so the opened tab signs the
  // right endpoint without inheriting a detected region.
  const keys = [key("h1", 1782760100, 1), key("h1", 1782760200, 2)];
  const s = scope.scopeFromKeys("cell1-prod-pdx-dial9-traces", keys, 1782760000, 1782760800, "us-west-2");
  const base = new URLSearchParams();
  base.set("api", "1");
  const { query } = scope.encodeAggregationParams(base, s);
  assert.strictEqual(new URLSearchParams(query).get("aws_region"), "us-west-2");
});

test("scopeFromKeys carries the role ARN when supplied, else ''", () => {
  const keys = [key("h1", 1782760100, 1)];
  const arn = "arn:aws:iam::123456789012:role/Dial9TraceReader";
  assert.strictEqual(
    scope.scopeFromKeys("bkt", keys, 1782760000, 1782760800, "us-west-2", arn).roleArn,
    arn,
    "role ARN threaded through",
  );
  // Trailing/optional: existing 5-arg callers get an empty roleArn, not undefined.
  assert.strictEqual(scope.scopeFromKeys("bkt", keys, 1782760000, 1782760800, "us-west-2").roleArn, "");
});

test("scopeFromKeys role ARN survives an aggregation round-trip", () => {
  // End-to-end: an assume-role selection produces an /api/flamegraph link that
  // carries the bare aws_role_arn, so a link opened in a fresh session assumes
  // the same role rather than 401ing with no identity.
  const keys = [key("h1", 1782760100, 1)];
  const arn = "arn:aws:iam::123456789012:role/Dial9TraceReader";
  const s = scope.scopeFromKeys("cell1-prod-pdx-dial9-traces", keys, 1782760000, 1782760800, "us-west-2", arn);
  const { query } = scope.encodeAggregationParams(new URLSearchParams(), s);
  assert.strictEqual(new URLSearchParams(query).get("aws_role_arn"), arn);
});

test("scopeFromKeys derives the window from key epochs when none is supplied", () => {
  // Raw mode passes no window; it comes from the keys' epochs.
  const keys = [key("h1", 1782760100, 1), key("h1", 1782760300, 2)];
  const s = scope.scopeFromKeys("bkt", keys, null, null);
  assert.strictEqual(s.from, 1782760100, "min epoch");
  assert.strictEqual(s.to, 1782760301, "exclusive bound after max epoch");
});

test("scopeFromKeys returns null when no window and no parseable epochs", () => {
  // A key layout the filename regex misses yields epoch 0 (filtered out). With
  // no supplied window there is nothing to derive — must return null rather
  // than an Infinity/-Infinity window that would serialize to s_from=Infinity
  // and 400 the /api/browse request.
  const keys = ["custom/layout/no-epoch-here.dat"];
  assert.strictEqual(scope.scopeFromKeys("bkt", keys, null, null), null);
  // But an explicit window is still honored even when epochs don't parse.
  const s = scope.scopeFromKeys("bkt", keys, 1782760000, 1782760800);
  assert.strictEqual(s.from, 1782760000);
  assert.strictEqual(s.to, 1782760800);
});

test("resolveScope lists the window, filters to the host set, maps to /api/object", async () => {
  const s = {
    bucket: "bkt",
    prefix: "traces",
    service: "shale",
    hosts: ["h1"], // only h1 selected
    from: 1782760000,
    to: 1782760800,
  };
  // Browse returns objects for h1 (in window), h2 (not selected), and an h1
  // object outside the window — only the first should survive.
  const browse = {
    objects: [
      { key: key("h1", 1782760100, 1), size: 10, last_modified: "2026-06-29T19:15:05Z" },
      { key: key("h2", 1782760150, 1), size: 10, last_modified: "2026-06-29T19:15:06Z" },
      { key: key("h1", 1782700000, 1), size: 10, last_modified: "2026-06-29T01:00:00Z" },
    ],
  };
  let requested = null;
  const fetchJson = async (url) => {
    requested = url;
    return browse;
  };
  const urls = await scope.resolveScope(s, fetchJson);
  assert.ok(requested.startsWith("/api/browse?"), "calls /api/browse");
  assert.ok(requested.includes("from=1782760000") && requested.includes("to=1782760800"), "passes the window");
  assert.strictEqual(urls.length, 1, "only the in-window h1 object survives");
  assert.ok(urls[0].startsWith("/api/object?"), "maps to /api/object");
  assert.ok(urls[0].includes(encodeURIComponent(key("h1", 1782760100, 1))), "carries the right key");
});

test("resolveScope uses half-open boundaries for adjacent segments", async () => {
  const s = {
    bucket: "bkt",
    prefix: "traces",
    service: "shale",
    hosts: ["h1"],
    from: 1782760100,
    to: 1782760160,
  };
  const selected = key("h1", 1782760100, 1);
  const adjacent = key("h1", 1782760160, 2);
  const previous = key("h1", 1782760040, 0);
  const browse = {
    objects: [
      {
        key: previous,
        size: 10,
        last_modified: "2026-06-29T19:08:20Z",
      },
      {
        key: selected,
        size: 10,
        last_modified: "2026-06-29T19:09:20Z",
      },
      {
        key: adjacent,
        size: 10,
        last_modified: "2026-06-29T19:10:20Z",
      },
    ],
  };

  const urls = await scope.resolveScope(s, async () => browse);
  assert.strictEqual(urls.length, 1, "segments touching either scope boundary must be excluded");
  assert.strictEqual(
    new URL(urls[0], "http://dial9.test").searchParams.get("key"),
    selected,
  );
});

test("resolveScope sends an encoded service and retains client-side service filtering", async () => {
  const service = "checkout api+canary?";
  const s = {
    bucket: "bkt",
    prefix: "traces",
    service,
    hosts: [],
    from: 1782760000,
    to: 1782760800,
  };
  const matchingKey = keyForService(service, "h1", 1782760100, 1);
  const browse = {
    objects: [
      { key: matchingKey, size: 10, last_modified: "2026-06-29T19:15:05Z" },
      {
        key: keyForService("other-service", "h1", 1782760150, 1),
        size: 10,
        last_modified: "2026-06-29T19:15:06Z",
      },
    ],
  };
  let requested = "";
  const urls = await scope.resolveScope(s, async (url) => {
    requested = url;
    return browse;
  });

  assert.ok(
    requested.includes("service=checkout+api%2Bcanary%3F"),
    "service is URL-encoded on /api/browse",
  );
  assert.strictEqual(
    new URL(requested, "http://dial9.test").searchParams.get("service"),
    service,
    "browse receives the exact service value",
  );
  assert.strictEqual(urls.length, 1, "client-side filtering rejects a mismatched service from browse");
  const objectParams = new URL(urls[0], "http://dial9.test").searchParams;
  assert.strictEqual(objectParams.get("bucket"), "bkt");
  assert.strictEqual(objectParams.get("key"), matchingKey);
});

test("resolveScope with an empty host set keeps all in-window hosts", async () => {
  const s = { bucket: "bkt", prefix: "traces", service: "", hosts: [], from: 1782760000, to: 1782760800 };
  const browse = {
    objects: [
      { key: key("h1", 1782760100, 1), size: 10, last_modified: "2026-06-29T19:15:05Z" },
      { key: key("h2", 1782760150, 1), size: 10, last_modified: "2026-06-29T19:15:06Z" },
    ],
  };
  const urls = await scope.resolveScope(s, async () => browse);
  assert.strictEqual(urls.length, 2, "empty host set = all hosts in window");
});
