"use strict";

// trace_scope.js — carry an S3 trace selection as a compact, stateless *scope*
// (bucket + prefix + service + host set + time window) instead of one
// `trace=/api/object?…` component per file.
//
// WHY
// ---
// Opening the viewer/flamegraph from the S3 browser is a navigation (a GET), so
// the whole selection rides in the URL. One `trace=` per file pushed a large
// heatmap selection past CloudFront's hard 8192-byte request-URI cap, so the
// new tab 414'd before it could load. A *scope* is bounded by host count, not
// file count (many files per host per minute), so it stays short — and, unlike
// a localStorage handoff, it is **stateless**, so a deep link built from it
// re-resolves in any browser (the cross-browser share path depends on this).
//
// The viewer re-lists the matching files from the scope via `/api/browse` (the
// same listing the S3 browser already uses) and feeds the resulting
// `/api/object?…` URLs to `TraceParser.fetchTraces`, exactly as the inline
// `trace=` path did. Demand-driven aggregation mode already worked this way
// server-side; this brings the exact-decode path in line.
//
// Re-listing means a scope opened later can pick up files that landed in the
// window since it was shared. For a finished trace that is nil; it is the
// deliberate trade for a portable, length-safe link.
//
// `parseKey` / `extractPrefix` live here as the single source of truth so the
// browser pages and Node tests share one implementation. Dependency-free so
// both contexts can require/script it.

(function (exports) {
  // Scope params are namespaced `s_*` so they never collide with the viewer's
  // existing `host`/`from`/`to` title params or `start`/`end` zoom params.
  const P = {
    bucket: "s_bucket",
    region: "s_region", // AWS region the bucket lives in (see scopeFromKeys)
    roleArn: "s_role_arn", // reader role to assume for this bucket (see scopeFromKeys)
    credentialMode: "s_credential_mode",
    prefix: "s_prefix",
    service: "s_svc",
    host: "s_host", // repeatable
    from: "s_from", // epoch seconds (inclusive)
    to: "s_to", // epoch seconds (exclusive)
  };

  // Parse an S3 trace key into its {service, host, bootId, epoch, segIndex}.
  // Mirrors the key layouts the browser understands:
  //   {prefix}/version=1/date={YYYY-MM-DD}/service={service}/time={HHMM}/instance={instance}/boot={boot_id}/{epoch}-{i}.bin[.gz]
  //   {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{boot_id}/{epoch}-{i}.bin[.gz]
  //   {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{epoch}-{i}.bin[.gz]   (legacy)
  // The versioned layout is fixed and historical layouts stay positional.
  function parseKey(key) {
    const parts = key.split("/");
    const dateRe = /^\d{4}-\d{2}-\d{2}$/;
    const timeRe = /^\d{4}$/;
    const file = parts[parts.length - 1];
    const match = file.match(/^(\d+)-(\d+)\.bin(?:\.gz)?$/);
    let epoch = 0;
    let segIndex = "";
    if (match) {
      epoch = parseInt(match[1], 10);
      segIndex = match[2];
    }
    const version1 = version1Layout(parts);
    if (version1) {
      if (
        typeof version1.date === "string" &&
        dateRe.test(version1.date) &&
        typeof version1.service === "string" &&
        typeof version1.time === "string" &&
        timeRe.test(version1.time) &&
        typeof version1.instance === "string" &&
        typeof version1.boot === "string"
      ) {
        return {
          service: version1.service,
          host: version1.instance,
          bootId: version1.boot,
          epoch,
          segIndex,
        };
      }
      return { service: "", host: key, bootId: "", epoch, segIndex };
    }
    const historical = historicalLayout(parts, dateRe, timeRe);
    if (historical) {
      const { start, hasBoot } = historical;
      return {
        service: parts[start + 2],
        host: parts[start + 3],
        bootId: hasBoot ? parts[start + 4] : "",
        epoch,
        segIndex,
      };
    }
    if (parts.some((part) => dateRe.test(part) || part === "version=1")) {
      return { service: "", host: key, bootId: "", epoch, segIndex };
    }
    if (parts.length >= 5) {
      return {
        service: parts[parts.length - 3],
        host: parts[parts.length - 2],
        bootId: "",
        epoch,
        segIndex,
      };
    }
    return { service: "", host: key, bootId: "", epoch: 0, segIndex: "" };
  }

  // The key prefix: everything before the recognized layout root (e.g. `traces`).
  // Empty string when the layout starts at the object-key root.
  function extractPrefix(key) {
    const parts = key.split("/");
    const dateRe = /^\d{4}-\d{2}-\d{2}$/;
    const timeRe = /^\d{4}$/;
    const version1 = version1Layout(parts);
    if (version1) return parts.slice(0, version1.start).join("/");
    const historical = historicalLayout(parts, dateRe, timeRe);
    if (historical) return parts.slice(0, historical.start).join("/");
    return "";
  }

  function historicalLayout(parts, dateRe, timeRe) {
    for (const hasBoot of [true, false]) {
      const width = hasBoot ? 6 : 5;
      if (parts.length < width) continue;
      const start = parts.length - width;
      if (dateRe.test(parts[start]) && timeRe.test(parts[start + 1])) {
        return { start, hasBoot };
      }
    }
    return null;
  }

  function version1Layout(parts) {
    if (parts.length < 7) return null;
    const start = parts.length - 7;
    if (parts[start] !== "version=1") return null;
    return {
      start,
      date: decodePartition(parts[start + 1], "date"),
      service: decodePartition(parts[start + 2], "service"),
      time: decodePartition(parts[start + 3], "time"),
      instance: decodePartition(parts[start + 4], "instance"),
      boot: decodePartition(parts[start + 5], "boot"),
    };
  }

  function decodePartition(segment, name) {
    const prefix = name + "=";
    return segment.startsWith(prefix)
      ? decodePartitionValue(segment.slice(prefix.length))
      : null;
  }

  function decodePartitionValue(value) {
    try {
      return decodeURIComponent(value);
    } catch (_) {
      return null;
    }
  }

  // One viewer `trace=` component per key, each pointing at /api/object (which
  // serves that single file's raw, still-gzipped bytes). fetchTraces downloads
  // them in parallel and concatenates client-side.
  function objectTraceUrls(bucket, keys) {
    return keys.map((key) => {
      const p = new URLSearchParams();
      if (bucket) p.set("bucket", bucket);
      p.set("key", key);
      return "/api/object?" + p.toString();
    });
  }

  // Derive a scope from a heatmap selection's keys + [t0,t1] window (epoch
  // seconds). `hosts` is the distinct host set; a single service is assumed (a
  // box almost always spans one). Returns null when there is nothing to scope.
  //
  // `region` is the AWS region the bucket lives in — a property of the bucket,
  // not a credential, so it travels in the (shareable) scope alongside the
  // bucket. Carrying it makes the opened tab's URL self-contained: a
  // cross-region bucket (e.g. one in us-west-2) is signed for the right S3
  // endpoint without relying on the tab having inherited a region-detection
  // result from the page that opened it. Optional and trailing so existing
  // callers keep working; falsy region simply isn't carried.
  //
  // `roleArn` is the same story for the *assume-role* read path: when the bucket
  // is read by assuming a role (the `aws_role_arn` credential), that ARN must
  // ride in the scope too, or a link opened in a fresh session (no stored creds)
  // has a bucket+region but no identity and every /api/browse 401s. The ARN is
  // not a secret (it grants nothing on its own — the server must be allowed to
  // assume it), so it is safe to carry in a shareable URL, exactly as the home
  // page already carries `aws_role_arn`. Optional and trailing; falsy isn't carried.
  // Current callers pass canonical SourceScope as the first argument. The
  // positional form remains accepted for compatibility; both normalize to the
  // same URL-safe scope (literal values never enter this object, only their
  // mode marker).
  function scopeFromKeys(sourceOrBucket, keys, t0, t1, region, roleArn, credentialMode) {
    let bucket = sourceOrBucket;
    if (sourceOrBucket && typeof sourceOrBucket === "object") {
      const source = sourceOrBucket;
      bucket = source.bucket || "";
      region = source.region || "";
      credentialMode = source.credentials && source.credentials.kind || "ambient";
      roleArn = credentialMode === "role" ? source.credentials.roleArn : "";
    }
    if (!keys || !keys.length) return null;
    const parsed = keys.map(parseKey);
    const services = [...new Set(parsed.map((p) => p.service).filter(Boolean))];
    const hosts = [...new Set(parsed.map((p) => p.host).filter(Boolean))];
    const epochs = parsed.map((p) => p.epoch).filter((e) => e > 0);
    // When no window is supplied (raw-mode selection), derive it from the keys'
    // epochs. If none parse, there is no valid bounded scope to produce.
    if ((t0 == null || t1 == null) && !epochs.length) return null;
    const from = t0 != null ? Math.floor(t0) : Math.min(...epochs);
    const to = t1 != null ? Math.ceil(t1) : Math.max(...epochs) + 1;
    return {
      bucket: bucket || "",
      region: region || "",
      roleArn: roleArn || "",
      credentialMode: credentialMode || "",
      prefix: extractPrefix(keys[0]),
      service: services.length === 1 ? services[0] : "",
      hosts,
      from,
      to,
    };
  }

  // Inline host set is kept while the whole query stays at/under this many
  // bytes. CloudFront rejects a request URI over 8192 bytes (path + query); the
  // margin leaves room for the path and any title/zoom params the caller added.
  const URI_SAFE_QUERY_LIMIT = 7000;

  // Write a scope's params onto a copy of `baseParams` (left unmutated) and
  // return { query, hostsDropped }. The host set is repeatable `s_host`. The
  // window (from/to) is bounded and always included; the host set is bounded by
  // host *count*, far smaller than file count — but a selection spanning a huge
  // fleet could still overflow. When including every host would exceed the URI
  // limit, the host set is dropped and the scope degrades to "all hosts in the
  // window" (an empty host set already means that in resolveScope). `query` is
  // therefore always URI-safe; `hostsDropped` lets the caller warn that the
  // result is broader than the literal selection.
  function encodeScope(baseParams, scope, opts) {
    const o = opts || {};
    const limit = o.limit != null ? o.limit : URI_SAFE_QUERY_LIMIT;
    const base = new URLSearchParams(baseParams ? baseParams.toString() : "");
    if (scope.bucket) base.set(P.bucket, scope.bucket);
    if (scope.region) base.set(P.region, scope.region);
    if (scope.roleArn) base.set(P.roleArn, scope.roleArn);
    if (scope.credentialMode) base.set(P.credentialMode, scope.credentialMode);
    if (scope.prefix) base.set(P.prefix, scope.prefix);
    if (scope.service) base.set(P.service, scope.service);
    if (scope.from != null) base.set(P.from, String(scope.from));
    if (scope.to != null) base.set(P.to, String(scope.to));

    const withHosts = new URLSearchParams(base.toString());
    for (const h of scope.hosts || []) withHosts.append(P.host, h);
    const withHostsQs = withHosts.toString();
    if (withHostsQs.length <= limit) {
      return { query: withHostsQs, hostsDropped: false };
    }
    // Pathological host set: fall back to a time-range-only scope so the URL is
    // never oversized. resolveScope then lists every host in the window.
    return { query: base.toString(), hostsDropped: (scope.hosts || []).length > 0 };
  }

  // Encode a scope into the *aggregation* param vocabulary onto a copy of
  // `baseParams` (left unmutated): the un-namespaced `bucket`/`prefix`/`service`/
  // repeatable `host` names the `/api/flamegraph` refinement loop and the
  // `/api/tokio-stats` endpoint expect, with the window as `start_ns`/`end_ns`
  // in NANOSECONDS (the scope's `from`/`to` are epoch seconds). Unlike
  // encodeScope there is no `s_*` namespacing (these URLs go straight to a
  // server endpoint), but the same host-set length guard applies: the server
  // caps *sampling* work, not URL length, so a box spanning a huge fleet would
  // still list every host in the query and overflow CloudFront's 8192-byte cap
  // (a 414 — the exact failure the scope design avoids). When including every
  // host would exceed the limit the host set is dropped and the scope degrades
  // to "all hosts in the window" (an empty `host` set already means that
  // server-side). Returns { query, hostsDropped } so the caller can warn.
  function encodeAggregationParams(baseParams, scope, opts) {
    const o = opts || {};
    const limit = o.limit != null ? o.limit : URI_SAFE_QUERY_LIMIT;
    const base = new URLSearchParams(baseParams ? baseParams.toString() : "");
    if (scope.bucket) base.set("bucket", scope.bucket);
    // The un-namespaced `aws_region` is exactly the query param the server reads
    // for the bucket's region (credentials::QUERY_REGION), so a scope link that
    // points straight at /api/flamegraph or /api/tokio-stats is self-contained.
    if (scope.region) base.set("aws_region", scope.region);
    // Likewise the bare `aws_role_arn` (credentials::QUERY_ROLE_ARN): a scope
    // read via an assumed role must carry it so the server assumes the same role,
    // else the endpoint has no identity for the bucket and 401s.
    if (scope.roleArn) base.set("aws_role_arn", scope.roleArn);
    if (scope.credentialMode) base.set("credential_mode", scope.credentialMode);
    if (scope.prefix) base.set("prefix", scope.prefix);
    if (scope.service) base.set("service", scope.service);
    if (scope.from != null) base.set("start_ns", String(Math.round(scope.from * 1e9)));
    if (scope.to != null) base.set("end_ns", String(Math.round(scope.to * 1e9)));

    const withHosts = new URLSearchParams(base.toString());
    for (const h of scope.hosts || []) withHosts.append("host", h);
    const withHostsQs = withHosts.toString();
    if (withHostsQs.length <= limit) {
      return { query: withHostsQs, hostsDropped: false };
    }
    // Pathological host set: fall back to a time-range-only scope so the URL is
    // never oversized. The server then folds every host in the window.
    return { query: base.toString(), hostsDropped: (scope.hosts || []).length > 0 };
  }

  // Read a scope back from URL params, or null if no scope is present. A scope
  // requires at least the time window (from/to); the host set may be empty.
  function readScope(params) {
    const from = params.get(P.from);
    const to = params.get(P.to);
    if (from == null || to == null) return null;
    return {
      bucket: params.get(P.bucket) || "",
      region: params.get(P.region) || "",
      roleArn: params.get(P.roleArn) || "",
      credentialMode: params.get(P.credentialMode) || "",
      prefix: params.get(P.prefix) || "",
      service: params.get(P.service) || "",
      hosts: params.getAll(P.host),
      from: Number(from),
      to: Number(to),
    };
  }

  function hasScope(params) {
    return params.get(P.from) != null && params.get(P.to) != null;
  }

  // Resolve a scope to the list of /api/object URLs its files map to. `fetchJson`
  // is an injected `(url) => Promise<parsedJson>` so each page supplies its own
  // credentialed fetch and the resolver stays unit-testable. Lists the window
  // via /api/browse, then keeps only objects whose parsed key overlaps the
  // window and (when the host set is non-empty) matches a selected host and the
  // scope's service. Returns the URLs sorted by (epoch, key) for determinism.
  async function resolveScope(scope, fetchJson) {
    const q = new URLSearchParams();
    if (scope.bucket) q.set("bucket", scope.bucket);
    if (scope.prefix) q.set("prefix", scope.prefix);
    if (scope.service) q.set("service", scope.service);
    q.set("from", String(scope.from));
    q.set("to", String(scope.to));
    const result = await fetchJson("/api/browse?" + q.toString());
    const objects = (result && result.objects) || [];

    const hostSet = new Set(scope.hosts || []);
    const matched = [];
    for (const obj of objects) {
      const p = parseKey(obj.key);
      // Window overlap: segment start (epoch) to last_modified (upload time).
      const start = p.epoch;
      if (!start) continue;
      const end = obj.last_modified
        ? new Date(obj.last_modified).getTime() / 1000
        : start;
      if (start >= scope.to || end <= scope.from) continue;
      if (scope.service && p.service && p.service !== scope.service) continue;
      if (hostSet.size && !hostSet.has(p.host)) continue;
      matched.push({ key: obj.key, epoch: start });
    }
    matched.sort((a, b) => a.epoch - b.epoch || (a.key < b.key ? -1 : 1));
    return objectTraceUrls(scope.bucket, matched.map((m) => m.key));
  }

  exports.PARAMS = P;
  exports.parseKey = parseKey;
  exports.extractPrefix = extractPrefix;
  exports.objectTraceUrls = objectTraceUrls;
  exports.scopeFromKeys = scopeFromKeys;
  exports.encodeScope = encodeScope;
  exports.encodeAggregationParams = encodeAggregationParams;
  exports.readScope = readScope;
  exports.hasScope = hasScope;
  exports.resolveScope = resolveScope;

  if (typeof window !== "undefined") {
    window.Dial9TraceScope = exports;
  }
})(typeof exports === "undefined" ? {} : exports);
