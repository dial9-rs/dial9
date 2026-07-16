"use strict";

// Pure helpers for the Span Explorer page (span_explorer.html).
//
// The `/api/span-stats` endpoint streams SSE events containing span type
// statistics: occurrence counts, duration histograms, and percentiles. These
// helpers normalise and format the response for the catalog table, histogram,
// time-composition bar, and flamegraph integration.
//
// DOM-free and CommonJS-exported so they can be unit-tested under Node.

// ── Duration formatting (reuse from flamegraph_histogram.js when available) ──

function fmtNs(ns) {
  if (ns == null) return "—";
  const n = Number(ns);
  if (!Number.isFinite(n) || n < 0) return "—";
  if (n === 0) return "0";
  if (n < 1e3) return n + "ns";
  if (n < 1e6) return (n / 1e3).toFixed(1).replace(/\.0$/, "") + "µs";
  if (n < 1e9) return (n / 1e6).toFixed(2).replace(/\.?0+$/, "") + "ms";
  return (n / 1e9).toFixed(2).replace(/\.?0+$/, "") + "s";
}

// ── Span type catalog helpers ────────────────────────────────────────────────

// Sort span types by a given key. Default: descending by count.
function sortSpanTypes(types, key, ascending) {
  const sorted = types.slice();
  const dir = ascending ? 1 : -1;
  sorted.sort((a, b) => {
    const av = a[key], bv = b[key];
    if (av == null && bv == null) return 0;
    if (av == null) return dir;
    if (bv == null) return -dir;
    if (typeof av === "string") return dir * av.localeCompare(bv);
    return dir * (av - bv);
  });
  return sorted;
}

// Build a display label for a span type (name + callsite disambiguation).
function spanTypeLabel(st) {
  let label = st.name || "(unnamed)";
  if (st.callsite_file && st.callsite_line != null) {
    const file = st.callsite_file.split("/").pop();
    label += ` (${file}:${st.callsite_line})`;
  }
  return label;
}

// ── Histogram helpers ────────────────────────────────────────────────────────

// Normalize a span-stats histogram (same shape as poll histogram: [{lo_ns, hi_ns, count}])
// into ascending bars with numeric fields, suitable for rendering.
function normalizeSpanHistogram(hist) {
  if (!Array.isArray(hist)) return [];
  const bars = [];
  for (const b of hist) {
    if (b == null) continue;
    const lo = Number(b.lo_ns);
    const hi = Number(b.hi_ns);
    const count = Number(b.count);
    if (!Number.isFinite(lo) || !Number.isFinite(hi) || !Number.isFinite(count)) continue;
    if (hi <= lo || count < 0) continue;
    bars.push({ lo_ns: lo, hi_ns: hi, count });
  }
  bars.sort((a, b) => a.lo_ns - b.lo_ns);
  return bars;
}

// Compute the layout geometry for a span histogram (equal-width columns on a
// log duration axis). Returns { cols, maxCount }.
function spanHistogramLayout(bars, width, gap) {
  const norm = normalizeSpanHistogram(bars);
  const g = gap == null ? 1 : gap;
  const n = norm.length;
  if (n === 0 || width <= 0) return { cols: [], maxCount: 0 };
  const maxCount = norm.reduce((m, b) => Math.max(m, b.count), 0);
  const colW = width / n;
  const cols = norm.map((b, i) => ({
    lo_ns: b.lo_ns,
    hi_ns: b.hi_ns,
    count: b.count,
    x: i * colW,
    w: Math.max(0, colW - g),
    hFrac: maxCount > 0 ? b.count / maxCount : 0,
  }));
  return { cols, maxCount };
}

// Map a pixel x to a continuous duration (ns) via geometric interpolation
// within the histogram column under that pixel. Same approach as
// FlamegraphHistogram.pxToNs.
function spanPxToNs(bars, width, px) {
  const { cols } = spanHistogramLayout(bars, width, 0);
  const n = cols.length;
  if (n === 0 || width <= 0) return null;
  const colW = width / n;
  let idx = Math.floor(px / colW);
  if (idx < 0) idx = 0;
  if (idx > n - 1) idx = n - 1;
  let frac = (px - idx * colW) / colW;
  if (frac < 0) frac = 0;
  if (frac > 1) frac = 1;
  const { lo_ns, hi_ns } = cols[idx];
  return Math.round(lo_ns * Math.pow(hi_ns / lo_ns, frac));
}

// Map a brushed pixel range to a span-duration band { min_ns, max_ns }.
// Returns null for near-zero-width drags (treated as clicks).
function spanBrushToBand(bars, width, x0, x1) {
  const lo = Math.min(x0, x1);
  const hi = Math.max(x0, x1);
  if (hi - lo < 2) return null;
  return {
    min_ns: spanPxToNs(bars, width, lo),
    max_ns: spanPxToNs(bars, width, hi),
  };
}

// Count instances within a duration band from the histogram.
function countInBand(bars, minNs, maxNs) {
  let count = 0;
  for (const b of normalizeSpanHistogram(bars)) {
    // A bar contributes if it overlaps the band.
    if ((minNs == null || b.hi_ns > minNs) && (maxNs == null || b.lo_ns < maxNs)) {
      count += b.count;
    }
  }
  return count;
}

// ── Five-way time composition ────────────────────────────────────────────────

// The five time categories from the design. Each span row has nullable
// estimates; null means the category wasn't evaluated (not that it's zero).
// We show "Unknown" explicitly rather than hiding it.
const TIME_CATEGORIES = [
  { key: "on_cpu", label: "On CPU", color: "#ff6b35" },
  { key: "blocked", label: "Blocked", color: "#e63946" },
  { key: "async_wait", label: "Async wait (not ready)", color: "#457b9d" },
  { key: "sched_delay", label: "Scheduling delay", color: "#f4a261" },
  { key: "unknown", label: "Unknown", color: "#6c757d" },
];

// Sum the `composition_histogram` buckets that fall inside a brushed duration
// band into a single `{on_cpu_ns, ...}` object, using the same overlap rule as
// the count histogram (a bucket is in-band when hi_ns > min && lo_ns < max).
// Returns null when there is no per-bucket composition data. `min`/`max` may be
// null (open-ended); a null band on both ends sums every bucket.
function bandComposition(spanType, min_ns, max_ns) {
  const buckets = spanType.composition_histogram;
  if (!Array.isArray(buckets) || buckets.length === 0) return null;
  const acc = { on_cpu_ns: 0, blocked_ns: 0, async_wait_ns: 0, scheduler_delay_ns: 0, unknown_ns: 0 };
  for (const b of buckets) {
    const lo = Number(b.lo_ns);
    const hi = Number(b.hi_ns);
    const inBand = (min_ns == null || hi > min_ns) && (max_ns == null || lo < max_ns);
    if (!inBand) continue;
    acc.on_cpu_ns += Number(b.on_cpu_ns) || 0;
    acc.blocked_ns += Number(b.blocked_ns) || 0;
    acc.async_wait_ns += Number(b.async_wait_ns) || 0;
    acc.scheduler_delay_ns += Number(b.scheduler_delay_ns) || 0;
    acc.unknown_ns += Number(b.unknown_ns) || 0;
  }
  return acc;
}

// Compute the five-way composition from a SpanTypeStats response. When the
// backend returns `composition` (an object with `on_cpu_ns`, `blocked_ns`,
// `async_wait_ns`, `scheduler_delay_ns`, `unknown_ns`), we use those summed
// values directly. Unknown is always preserved explicitly — even when zero —
// so the UI communicates that the category was evaluated rather than hidden.
//
// When a duration `band` ({min_ns, max_ns}, either may be null) is supplied and
// the backend returned `composition_histogram`, the composition is scoped to the
// buckets inside the band — so it tracks the histogram brush. Without a band (or
// without per-bucket data) the whole-type `composition` total is used.
//
// Falls back to a histogram-estimated total with a single "Unknown" bar when
// `composition` is absent (older backends / partial data).
function computeTimeComposition(spanType, band) {
  let comp = spanType.composition;
  // Band-scoped composition takes precedence when available.
  if (band && (band.min_ns != null || band.max_ns != null)) {
    const scoped = bandComposition(spanType, band.min_ns, band.max_ns);
    if (scoped) comp = scoped;
  }
  if (comp && typeof comp === "object") {
    const on_cpu = Number(comp.on_cpu_ns) || 0;
    const blocked = Number(comp.blocked_ns) || 0;
    const async_wait = Number(comp.async_wait_ns) || 0;
    const sched_delay = Number(comp.scheduler_delay_ns) || 0;
    const unknown = Number(comp.unknown_ns) || 0;
    const total = on_cpu + blocked + async_wait + sched_delay + unknown;
    if (total <= 0) {
      // All zeros — degenerate; show a zero-width Unknown bar.
      return { total_ns: 0, categories: [{ key: "unknown", label: "Unknown", color: "#6c757d", ns: 0, frac: 1.0 }] };
    }
    const cats = [
      { key: "on_cpu", label: "On CPU", color: "#ff6b35", ns: on_cpu, frac: on_cpu / total },
      { key: "blocked", label: "Blocked", color: "#e63946", ns: blocked, frac: blocked / total },
      { key: "async_wait", label: "Async wait (not ready)", color: "#457b9d", ns: async_wait, frac: async_wait / total },
      { key: "sched_delay", label: "Scheduling delay", color: "#f4a261", ns: sched_delay, frac: sched_delay / total },
      { key: "unknown", label: "Unknown", color: "#6c757d", ns: unknown, frac: unknown / total },
    ];
    return { total_ns: total, categories: cats };
  }

  // Fallback: no backend composition data. Approximate total elapsed from the
  // histogram midpoints and show a single Unknown bar.
  const totalElapsed = (spanType.histogram || []).reduce((sum, b) => {
    const mid = (Number(b.lo_ns) + Number(b.hi_ns)) / 2;
    return sum + mid * Number(b.count);
  }, 0);

  return {
    total_ns: totalElapsed,
    categories: [
      { key: "unknown", label: "Unknown", color: "#6c757d", ns: totalElapsed, frac: 1.0 },
    ],
  };
}

// Compute a quality ratio: fraction of instances with full composition data.
// Returns null when fields are absent (older backend).
function spanTypeQuality(spanType) {
  const complete = spanType.details_complete_count;
  const partial = spanType.partial_count;
  if (complete == null && partial == null) return null;
  const c = Number(complete) || 0;
  const p = Number(partial) || 0;
  const total = c + p;
  if (total === 0) return null;
  return c / total;
}

// ── URL / deep-link helpers ──────────────────────────────────────────────────

// Build the URL params that deep-link the current span explorer state:
// selected span type, duration band, and scope.
function encodeSpanExplorerState(state) {
  const p = new URLSearchParams();
  p.set("api", "1");
  if (state.bucket) p.set("bucket", state.bucket);
  if (state.region) p.set("aws_region", state.region);
  if (state.prefix) p.set("prefix", state.prefix);
  if (state.service) p.set("service", state.service);
  if (state.hosts) for (const h of state.hosts) p.append("host", h);
  if (state.start_ns) p.set("start_ns", state.start_ns);
  if (state.end_ns) p.set("end_ns", state.end_ns);
  if (state.span_type_uid) p.set("span_type_uid", state.span_type_uid);
  if (state.min_span_ns != null) p.set("min_span_ns", String(state.min_span_ns));
  if (state.max_span_ns != null) p.set("max_span_ns", String(state.max_span_ns));
  return p;
}

// Read span explorer state from URL params.
function decodeSpanExplorerState(params) {
  return {
    bucket: params.get("bucket") || null,
    region: params.get("aws_region") || null,
    prefix: params.get("prefix") || null,
    service: params.get("service") || null,
    hosts: params.getAll("host"),
    start_ns: params.get("start_ns") || null,
    end_ns: params.get("end_ns") || null,
    span_type_uid: params.get("span_type_uid") || null,
    min_span_ns: params.get("min_span_ns") != null ? Number(params.get("min_span_ns")) : null,
    max_span_ns: params.get("max_span_ns") != null ? Number(params.get("max_span_ns")) : null,
  };
}

// Build the flamegraph URL for a selected span type + duration band.
// Uses the existing /api/flamegraph endpoint with span_type_uid filter.
function flamegraphUrl(state, phase) {
  const p = new URLSearchParams();
  p.set("api", "1");
  if (state.bucket) p.set("bucket", state.bucket);
  if (state.region) p.set("aws_region", state.region);
  if (state.prefix) p.set("prefix", state.prefix);
  if (state.service) p.set("service", state.service);
  if (state.hosts) for (const h of state.hosts) p.append("host", h);
  if (state.start_ns) p.set("start_ns", state.start_ns);
  if (state.end_ns) p.set("end_ns", state.end_ns);
  if (state.span_type_uid) p.set("span_type_uid", state.span_type_uid);
  if (state.min_span_ns != null) p.set("min_span_ns", String(state.min_span_ns));
  if (state.max_span_ns != null) p.set("max_span_ns", String(state.max_span_ns));
  // Phase selects the sample source for the flamegraph.
  if (phase === "blocking") {
    p.set("source", "sched");
  } else {
    p.set("source", "cpu");
  }
  return "flamegraph.html?" + p.toString();
}

// Build the viewer deep link that jumps to a specific slow-exemplar span
// instance. Mirrors tokio_stats_api.js's exemplarViewerUrl: one `/api/object`
// component pointing at the exemplar's own source file (bucket + source_key),
// plus svc/host, and a non-destructive `focus_start`/`focus_end` window that
// pans/zooms the loaded trace to the span WITHOUT filtering events out.
//
// `focus_*` params are DISTINCT from `start`/`end` (which the viewer treats as
// a hard parse filter that would drop every surrounding event and load an empty
// page for a narrow single-span window). We never emit start/end here.
//
// `exemplar` is a SlowExemplar: { start_ns, end_ns, source_key, host, ... }.
// `scope` carries the page scope: { bucket, region, service, spanName } (the
// exemplar's own host takes precedence — exemplars can come from different
// hosts). `spanName`, when given, is forwarded as `focus_span_name` so the
// viewer selects the exact span (a long span's window overlaps many others).
//
// Returns "" when the exemplar has no source_key to link to.
function exemplarViewerUrl(exemplar, scope) {
  const ex = exemplar || {};
  const sc = scope || {};
  if (!ex.source_key) return "";
  // The backend stores source_key as the fully-qualified `s3://{bucket}/{key}`
  // (that's the `full_key` decode_samples was handed). But `/api/object` wants a
  // bucket + a BUCKET-RELATIVE key — passing the whole `s3://…` URI as `key=`
  // 404s. Split it back into bucket + relative key; fall back to the scope
  // bucket and the raw key for a already-relative key (local mode / older data).
  let bucket = sc.bucket || "";
  let key = ex.source_key;
  const s3 = /^s3:\/\/([^/]+)\/(.+)$/.exec(ex.source_key);
  if (s3) {
    bucket = s3[1];
    key = s3[2];
  }
  // Mirror the landing page's objectTraceUrls(): one `/api/object?bucket=&key=`
  // component, built via URLSearchParams so the key is correctly encoded.
  const oq = new URLSearchParams();
  oq.set("bucket", bucket);
  oq.set("key", key);
  const traceUrl = "/api/object?" + oq.toString();

  const p = new URLSearchParams();
  p.set("trace", traceUrl);
  if (sc.service) p.set("svc", sc.service);
  // Prefer the exemplar's own host (it may differ from the scope's host set).
  if (ex.host) p.set("host", ex.host);
  if (sc.region) p.set("aws_region", sc.region);
  // Non-destructive focus on the exact span. `focus_start` alone triggers the
  // pan; `focus_end` frames the zoom; `focus_span_name` lets the viewer select
  // the matching span rather than just pan to a time window.
  if (ex.start_ns != null) {
    p.set("focus_start", String(ex.start_ns));
    if (ex.end_ns != null) p.set("focus_end", String(ex.end_ns));
    if (sc.spanName) p.set("focus_span_name", sc.spanName);
  }
  return "viewer.html?" + p.toString();
}

// ── Raw trace → SpanTypeStats catalog ────────────────────────────────────────

// Parse a SpanEnter event name "SpanEnter:{target}::{name}:{file}:{line}" into
// { target, name, file, line }. Returns null for unparseable names.
function parseSpanEventName(evName) {
  // Strip the prefix: "SpanEnter:" or "SpanExit:" or "SpanClose__"
  let body = null;
  if (evName.startsWith("SpanEnter:")) body = evName.slice("SpanEnter:".length);
  else if (evName.startsWith("SpanExit:")) body = evName.slice("SpanExit:".length);
  else return null;

  // Format: {target}::{name}:{file}:{line}
  // Target can itself contain "::" so find the last "::" that separates target from name
  // Actually the format is: target::name:file:line where target is a Rust module path
  // But file can contain /, and name doesn't contain ":"
  // Real format: everything between first "::" (after the target module) and the next ":" gives name,
  // then file:line. But the target itself uses "::" (e.g. metrics_service::buffer).
  // Looking at actual data: "metrics_service::buffer::flush_to_ddb:examples/metrics-service/src/buffer.rs:55"
  // The schema name format from the tracing layer is "{target}::{name}:{file}:{line}"
  // where target = "metrics_service::buffer", name = "flush_to_ddb",
  // file = "examples/metrics-service/src/buffer.rs", line = "55"
  //
  // Strategy: split from the END. The last segment after ":" is the line number.
  // Then work backwards to find the file (contains "/" or ".rs" etc).
  // Actually easier: line is the last colon-segment (a number), the file is the
  // second-to-last colon-separated group that contains a "/" or ".". The rest is target::name.

  const lastColon = body.lastIndexOf(":");
  if (lastColon < 0) return null;
  const line = body.slice(lastColon + 1);
  const rest = body.slice(0, lastColon);

  // Now find the file: it's the last colon-separated segment that looks like a path
  const secondLastColon = rest.lastIndexOf(":");
  if (secondLastColon < 0) return null;
  const file = rest.slice(secondLastColon + 1);
  const targetAndName = rest.slice(0, secondLastColon);

  // target::name — find last "::" to split
  const lastDblColon = targetAndName.lastIndexOf("::");
  if (lastDblColon < 0) {
    return { target: "", name: targetAndName, file, line: Number(line) || 0 };
  }
  const target = targetAndName.slice(0, lastDblColon);
  const name = targetAndName.slice(lastDblColon + 2);

  return { target, name, file, line: Number(line) || 0 };
}

// Build a unique key for grouping spans by target/name/callsite.
function spanCallsiteKey(target, name, file, line) {
  return `${target}::${name}:${file}:${line}`;
}

// Given the output of buildSpanData (from trace_analysis.js), build a
// SpanTypeStats-like catalog suitable for the catalog table in raw mode.
// Each entry has: { span_type_uid, name, target, callsite_file, callsite_line,
//   kind, count, p50_ns, p95_ns, p99_ns, max_ns, histogram, composition }.
function buildSpanCatalog(allSpans, customEvents) {
  // Determine callsite info for each span by finding its enter event name.
  // Build a map: spanName → first SpanEnter event name seen (all spans with the
  // same name come from the same callsite in practice since the schema name
  // encodes target/name/file/line).
  const nameToCallsite = new Map();
  for (const ev of customEvents) {
    if (ev.name.startsWith("SpanEnter:") || ev.name.startsWith("SpanEnter__")) {
      const spanName = ev.fields && ev.fields.span_name;
      if (spanName && !nameToCallsite.has(spanName)) {
        const parsed = parseSpanEventName(ev.name);
        if (parsed) nameToCallsite.set(spanName, parsed);
      }
    }
  }

  // Group spans by callsite key (target::name:file:line).
  const groups = new Map(); // callsiteKey → { spans: [], callsite }
  for (const s of allSpans) {
    const callsite = nameToCallsite.get(s.spanName);
    let key;
    if (callsite) {
      key = spanCallsiteKey(callsite.target, callsite.name, callsite.file, callsite.line);
    } else {
      key = s.spanName || "unknown";
    }
    if (!groups.has(key)) {
      groups.set(key, { spans: [], callsite: callsite || null, spanName: s.spanName });
    }
    groups.get(key).spans.push(s);
  }

  // Build catalog entries with exact percentiles and log histogram.
  const catalog = [];
  for (const [key, group] of groups) {
    const { spans, callsite, spanName } = group;
    const durations = spans.map(s => s.activeNs).sort((a, b) => a - b);
    const count = durations.length;
    if (count === 0) continue;

    const p = (frac) => durations[Math.min(count - 1, Math.floor(count * frac))];
    const p50_ns = p(0.5);
    const p95_ns = p(0.95);
    const p99_ns = p(0.99);
    const max_ns = durations[count - 1];
    const min_ns = durations[0];

    // Log-scale histogram: divide the range [min, max] into ~20 log-spaced bins.
    const histogram = buildLogHistogram(durations, min_ns, max_ns);

    // All-Unknown composition (raw mode has no CPU/sched breakdown).
    const totalNs = durations.reduce((sum, d) => sum + d, 0);
    const composition = null; // signals fallback in computeTimeComposition

    catalog.push({
      span_type_uid: key,
      name: callsite ? callsite.name : spanName,
      target: callsite ? callsite.target : null,
      callsite_file: callsite ? callsite.file : null,
      callsite_line: callsite ? callsite.line : null,
      kind: "tracing",
      count,
      p50_ns,
      p95_ns,
      p99_ns,
      max_ns,
      histogram,
      composition,
      // Mark as partial quality: zero detailed instances.
      details_complete_count: 0,
      partial_count: count,
    });
  }

  // Sort by count descending (default).
  catalog.sort((a, b) => b.count - a.count);
  return catalog;
}

// Build a log-scale histogram with ~20 bins from sorted durations.
function buildLogHistogram(sortedDurations, minNs, maxNs) {
  if (sortedDurations.length === 0) return [];
  // Ensure we have a valid range
  const lo = Math.max(1, minNs);
  const hi = Math.max(lo + 1, maxNs);

  const NUM_BINS = 20;
  const logLo = Math.log(lo);
  const logHi = Math.log(hi);
  const step = (logHi - logLo) / NUM_BINS;

  // Build bin edges
  const edges = [];
  for (let i = 0; i <= NUM_BINS; i++) {
    edges.push(Math.round(Math.exp(logLo + i * step)));
  }
  // Deduplicate (very tight ranges can collapse bins)
  const dedupEdges = [edges[0]];
  for (let i = 1; i < edges.length; i++) {
    if (edges[i] > dedupEdges[dedupEdges.length - 1]) dedupEdges.push(edges[i]);
  }
  if (dedupEdges.length < 2) {
    dedupEdges.push(dedupEdges[0] + 1);
  }

  // Count into bins
  const bins = [];
  let di = 0;
  for (let i = 0; i < dedupEdges.length - 1; i++) {
    const binLo = dedupEdges[i];
    const binHi = dedupEdges[i + 1];
    let count = 0;
    while (di < sortedDurations.length && sortedDurations[di] < binHi) {
      count++;
      di++;
    }
    bins.push({ lo_ns: binLo, hi_ns: binHi, count });
  }
  // Put remaining into last bin
  if (di < sortedDurations.length) {
    bins[bins.length - 1].count += sortedDurations.length - di;
  }

  return bins;
}

// ── Exports ──────────────────────────────────────────────────────────────────

var SpanExplorer = {
  fmtNs,
  sortSpanTypes,
  spanTypeLabel,
  normalizeSpanHistogram,
  spanHistogramLayout,
  spanPxToNs,
  spanBrushToBand,
  countInBand,
  TIME_CATEGORIES,
  computeTimeComposition,
  bandComposition,
  spanTypeQuality,
  encodeSpanExplorerState,
  decodeSpanExplorerState,
  flamegraphUrl,
  exemplarViewerUrl,
  parseSpanEventName,
  buildSpanCatalog,
  buildLogHistogram,
};

if (typeof module !== "undefined" && module.exports) {
  module.exports = SpanExplorer;
} else if (typeof window !== "undefined") {
  window.SpanExplorer = SpanExplorer;
}
