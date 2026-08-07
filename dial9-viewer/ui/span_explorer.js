"use strict";

// Pure helpers for the Span Explorer page (span_explorer.html).
//
// The `/api/span-stats` endpoint streams SSE events containing span type
// statistics: occurrence counts, duration histograms, and percentiles. These
// helpers normalise and format the response for the catalog table, histogram,
// time-composition bar, and flamegraph integration.
//
// DOM-free and CommonJS-exported so they can be unit-tested under Node.


// Apply the optional refinement cap consistently to API and browser URLs.
function setMaxFilesParam(searchParams, maxFiles) {
  if (maxFiles == null) searchParams.delete("max_files");
  else searchParams.set("max_files", String(maxFiles));
  return searchParams;
}

// Decide whether a streamed full snapshot may replace the visible catalog.
// Refinement reconstructs cached state from bounded seed batches, so snapshots
// below the already-visible baseline must not temporarily shrink the page.
function shouldAdoptCatalogSnapshot(mode, baselineFilesFolded, incomingFilesFolded) {
  if (mode === "replace") return true;
  if (mode === "exemplars") return false;
  if (mode === "refine") {
    return Number(incomingFilesFolded || 0) >= Number(baselineFilesFolded || 0);
  }
  return false;
}
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

// Inverse of spanPxToNs: map a duration (ns) to a pixel x within the histogram,
// using the same geometric (log) interpolation inside the containing column.
// Durations at/below the first bar clamp to x=0; at/above the last bar clamp to
// x=width. Returns null when there is no histogram. Used to place percentile
// markers on the same axis the bars are drawn on.
function spanNsToPx(bars, width, ns) {
  const { cols } = spanHistogramLayout(bars, width, 0);
  const n = cols.length;
  if (n === 0 || width <= 0 || ns == null) return null;
  const colW = width / n;
  const target = Number(ns);
  if (target <= cols[0].lo_ns) return 0;
  const last = cols[n - 1];
  if (target >= last.hi_ns) return width;
  for (let i = 0; i < n; i++) {
    const c = cols[i];
    if (target >= c.lo_ns && target <= c.hi_ns) {
      const lo = c.lo_ns > 0 ? c.lo_ns : 1;
      const hi = c.hi_ns > 0 ? c.hi_ns : 1;
      const frac = Math.log(target / lo) / Math.log(hi / lo);
      return i * colW + Math.max(0, Math.min(1, frac)) * colW;
    }
  }
  // Target sits in a gap between non-contiguous bars: snap to the column start
  // of the first bar whose floor exceeds it.
  for (let i = 0; i < n; i++) {
    if (target < cols[i].lo_ns) return i * colW;
  }
  return width;
}

// Estimate the duration (ns) at a given percentile p (0..100) from the count
// histogram: the inverse of percentileForDuration. Walks cumulative bar counts
// to the bar containing the target rank, then interpolates geometrically within
// it. Returns null when there is no histogram. Used to draw P50/P99/P99.9 hints.
function durationAtPercentile(bars, p) {
  const norm = normalizeSpanHistogram(bars);
  if (norm.length === 0) return null;
  const total = norm.reduce((s, b) => s + b.count, 0);
  if (total <= 0) return null;
  const target = (p / 100) * total;
  let cumulative = 0;
  for (const b of norm) {
    if (cumulative + b.count >= target) {
      // The target rank falls within this bar. Interpolate its position on the
      // log axis by how far into the bar's count the target lies.
      const within = b.count > 0 ? (target - cumulative) / b.count : 0;
      const lo = b.lo_ns > 0 ? b.lo_ns : 1;
      const hi = b.hi_ns > 0 ? b.hi_ns : 1;
      return Math.round(lo * Math.pow(hi / lo, Math.max(0, Math.min(1, within))));
    }
    cumulative += b.count;
  }
  return norm[norm.length - 1].hi_ns;
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

// Estimate the percentile rank (0..100) of a duration within a span type's
// duration histogram: the fraction of instances whose duration is <= `ns`.
// Uses the cumulative bar counts, interpolating geometrically within the
// containing bar (matching spanPxToNs's log-scale treatment). Returns null when
// there is no histogram to rank against.
//
// This answers "where does this exemplar sit in the broader distribution?" — a
// 200ms instance in a type whose p99 is 190ms lands around p99+.
function percentileForDuration(bars, ns) {
  const norm = normalizeSpanHistogram(bars);
  if (norm.length === 0 || ns == null || !Number.isFinite(Number(ns))) return null;
  const target = Number(ns);
  const total = norm.reduce((s, b) => s + b.count, 0);
  if (total <= 0) return null;
  let below = 0;
  for (const b of norm) {
    if (target >= b.hi_ns) {
      // Entire bar is at or below the target duration.
      below += b.count;
      continue;
    }
    if (target <= b.lo_ns) {
      // Target is at/under this bar's floor; no more counts are below it.
      break;
    }
    // Target falls inside this bar. Interpolate its position within the bar on
    // a log (geometric) axis, matching how the histogram maps pixels↔duration.
    const lo = b.lo_ns > 0 ? b.lo_ns : 1;
    const hi = b.hi_ns > 0 ? b.hi_ns : 1;
    const frac = Math.log(target / lo) / Math.log(hi / lo);
    below += b.count * Math.max(0, Math.min(1, frac));
    break;
  }
  return (below / total) * 100;
}

// Format a percentile rank (0..100) as a compact "pNN" / "pNN.N" label. Values
// very close to 100 keep more precision (p99.9) since that's where the
// interesting tail lives. Returns "—" for null.
function fmtPercentile(p) {
  if (p == null || !Number.isFinite(Number(p))) return "—";
  const v = Number(p);
  if (v >= 99.95) return "p" + Math.min(99.99, v).toFixed(2).replace(/0$/, "");
  if (v >= 99.5) return "p" + v.toFixed(1);
  if (v >= 10) return "p" + Math.round(v);
  return "p" + v.toFixed(v < 1 ? 1 : 0);
}

// Collect the union of attribute keys across a set of exemplars, in first-seen
// order. Used to render attributes as a table with one column per key (rather
// than per-row chips), since exemplars of a span type almost always share keys.
function collectExemplarAttributeKeys(exemplars) {
  const keys = [];
  const seen = new Set();
  for (const ex of exemplars || []) {
    for (const a of (ex && ex.attributes) || []) {
      if (a && a.key != null && !seen.has(a.key)) {
        seen.add(a.key);
        keys.push(a.key);
      }
    }
  }
  return keys;
}

// Look up an exemplar's value for an attribute key, or null when absent.
function exemplarAttrValue(ex, key) {
  const attrs = (ex && ex.attributes) || [];
  for (const a of attrs) {
    if (a && a.key === key) return a.value;
  }
  return null;
}

// Decide whether a column carries no distinguishing information across a set of
// rows: every row maps (via `valueOf`) to the same value, or all are empty.
// `null`/`undefined`/`""` all normalize to the empty string, so an all-missing
// column counts as degenerate. Requires at least 2 rows — a single row can't
// establish uniformity, and a 1-row table shouldn't hide its columns. Used to
// auto-hide uniform exemplar-table columns (host, callsite, attributes, …).
function columnIsDegenerate(rows, valueOf) {
  if (!Array.isArray(rows) || rows.length < 2 || typeof valueOf !== "function") return false;
  let first;
  for (let i = 0; i < rows.length; i++) {
    const v = valueOf(rows[i]);
    const norm = v == null || v === "" ? "" : String(v);
    if (i === 0) first = norm;
    else if (norm !== first) return false;
  }
  return true;
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
// band into a single composition object, using the same overlap rule as the
// count histogram (a bucket is in-band when hi_ns > min && lo_ns < max).
// Accumulates both the raw ns totals and the per-instance fraction sums +
// instance count, so callers can show actual time AND an equal-weighted
// (per-instance) breakdown. Returns null when there is no per-bucket data.
// `min`/`max` may be null (open-ended); a null band on both ends sums all.
function bandComposition(spanType, min_ns, max_ns) {
  const buckets = spanType.composition_histogram;
  if (!Array.isArray(buckets) || buckets.length === 0) return null;
  const acc = {
    on_cpu_ns: 0, blocked_ns: 0, async_wait_ns: 0, scheduler_delay_ns: 0, unknown_ns: 0,
    instance_count: 0,
    on_cpu_frac_sum: 0, blocked_frac_sum: 0, async_wait_frac_sum: 0,
    scheduler_delay_frac_sum: 0, unknown_frac_sum: 0,
  };
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
    acc.instance_count += Number(b.instance_count) || 0;
    acc.on_cpu_frac_sum += Number(b.on_cpu_frac_sum) || 0;
    acc.blocked_frac_sum += Number(b.blocked_frac_sum) || 0;
    acc.async_wait_frac_sum += Number(b.async_wait_frac_sum) || 0;
    acc.scheduler_delay_frac_sum += Number(b.scheduler_delay_frac_sum) || 0;
    acc.unknown_frac_sum += Number(b.unknown_frac_sum) || 0;
  }
  return acc;
}

// Compute the five-way composition from a SpanTypeStats response.
//
// Each returned category carries:
//   - `frac`: the share for the bar/percentage. When the backend supplies
//     per-instance fraction sums + `instance_count`, this is the EQUAL-WEIGHTED
//     mean share (each span instance counts once), so a handful of very long
//     spans no longer dominate the picture. Without that data we fall back to
//     the ns-weighted share.
//   - `ns`: the actual total time spent in the category across instances.
//   - `meanNs`: the mean time per instance (ns / instance_count) when known.
//   - `weighting`: "equal" or "time", so the UI can label what it's showing.
//
// When a duration `band` is supplied and the backend returned
// `composition_histogram`, composition is scoped to the buckets inside the band
// (tracking the histogram brush); otherwise the whole-type total is used.
// Falls back to a histogram-estimated single "Unknown" bar when `composition`
// is absent (older backends / partial data).
function computeTimeComposition(spanType, band) {
  let comp = spanType.composition;
  // Band-scoped composition takes precedence when available.
  if (band && (band.min_ns != null || band.max_ns != null)) {
    const scoped = bandComposition(spanType, band.min_ns, band.max_ns);
    if (scoped) comp = scoped;
  }
  if (comp && typeof comp === "object") {
    const ns = {
      on_cpu: Number(comp.on_cpu_ns) || 0,
      blocked: Number(comp.blocked_ns) || 0,
      async_wait: Number(comp.async_wait_ns) || 0,
      sched_delay: Number(comp.scheduler_delay_ns) || 0,
      unknown: Number(comp.unknown_ns) || 0,
    };
    const totalNs = ns.on_cpu + ns.blocked + ns.async_wait + ns.sched_delay + ns.unknown;

    // Equal-weighted fractions when the backend provided them.
    const instanceCount = Number(comp.instance_count) || 0;
    const fracSum = {
      on_cpu: Number(comp.on_cpu_frac_sum) || 0,
      blocked: Number(comp.blocked_frac_sum) || 0,
      async_wait: Number(comp.async_wait_frac_sum) || 0,
      sched_delay: Number(comp.scheduler_delay_frac_sum) || 0,
      unknown: Number(comp.unknown_frac_sum) || 0,
    };
    const equalWeighted = instanceCount > 0;

    if (totalNs <= 0 && !equalWeighted) {
      // All zeros — degenerate; show a zero-width Unknown bar.
      return {
        total_ns: 0,
        instance_count: instanceCount,
        weighting: equalWeighted ? "equal" : "time",
        categories: [{ key: "unknown", label: "Unknown", color: "#6c757d", ns: 0, frac: 1.0, meanNs: null }],
      };
    }

    // Bar share: equal-weighted mean fraction if available, else ns share.
    const share = (key) => {
      if (equalWeighted) return fracSum[key] / instanceCount;
      return totalNs > 0 ? ns[key] / totalNs : 0;
    };
    const meanNs = (key) => (instanceCount > 0 ? ns[key] / instanceCount : null);

    const cats = TIME_CATEGORIES.map((cat) => ({
      key: cat.key,
      label: cat.label,
      color: cat.color,
      ns: ns[cat.key],
      frac: share(cat.key),
      meanNs: meanNs(cat.key),
    }));
    return {
      total_ns: totalNs,
      instance_count: instanceCount,
      weighting: equalWeighted ? "equal" : "time",
      categories: cats,
    };
  }

  // Fallback: no backend composition data. Approximate total elapsed from the
  // histogram midpoints and show a single Unknown bar.
  const totalElapsed = (spanType.histogram || []).reduce((sum, b) => {
    const mid = (Number(b.lo_ns) + Number(b.hi_ns)) / 2;
    return sum + mid * Number(b.count);
  }, 0);

  return {
    total_ns: totalElapsed,
    instance_count: 0,
    weighting: "time",
    categories: [
      { key: "unknown", label: "Unknown", color: "#6c757d", ns: totalElapsed, frac: 1.0, meanNs: null },
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

// ── Attribute filters ────────────────────────────────────────────────────────

// Parse repeated `attr=key=value` URL params into [{key, value}] filters.
// Mirrors the server's parse_attr_filters: split on the FIRST '='; the value may
// contain further '='. Entries with an empty key or no '=' are dropped (the UI
// only ever produces well-formed values, so this is just defensive).
function parseAttrFilterParams(rawList) {
  const out = [];
  for (const raw of rawList || []) {
    const eq = raw.indexOf("=");
    if (eq <= 0) continue; // no '=' or empty key
    out.push({ key: raw.slice(0, eq), value: raw.slice(eq + 1) });
  }
  return out;
}

// Serialize [{key, value}] filters back to `key=value` strings for URL params.
function formatAttrFilterParams(filters) {
  return (filters || []).map((f) => `${f.key}=${f.value}`);
}

// True when a filter for this exact key/value pair is already active.
function hasAttrFilter(filters, key, value) {
  return (filters || []).some((f) => f.key === key && f.value === value);
}

// Append a key/value filter if not already present; returns a new array.
function addAttrFilter(filters, key, value) {
  if (hasAttrFilter(filters, key, value)) return filters.slice();
  return (filters || []).concat([{ key, value }]);
}

// Remove the filter matching this key/value pair; returns a new array.
function removeAttrFilter(filters, key, value) {
  return (filters || []).filter((f) => !(f.key === key && f.value === value));
}

// ── URL / deep-link helpers ──────────────────────────────────────────────────

// Build the URL params that deep-link the current span explorer state:
// selected span type, duration band, and scope.
function encodeSpanExplorerState(state) {
  const p = new URLSearchParams();
  p.set("api", "1");
  if (state.data_dir) p.set("data_dir", state.data_dir);
  setMaxFilesParam(p, state.max_files);
  if (state.bucket) p.set("bucket", state.bucket);
  if (state.region) p.set("aws_region", state.region);
  if (state.credentialMode) p.set("credential_mode", state.credentialMode);
  if (state.credentialMode === "role" && state.roleArn) {
    p.set("aws_role_arn", state.roleArn);
  }
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
    data_dir: params.get("data_dir") || null,
    max_files: params.get("max_files") != null ? Number(params.get("max_files")) : null,
    bucket: params.get("bucket") || null,
    region: params.get("aws_region") || null,
    credentialMode: params.get("credential_mode") || null,
    roleArn: params.get("aws_role_arn") || null,
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
  if (state.data_dir) p.set("data_dir", state.data_dir);
  setMaxFilesParam(p, state.max_files);
  if (state.bucket) p.set("bucket", state.bucket);
  if (state.region) p.set("aws_region", state.region);
  if (state.credentialMode) p.set("credential_mode", state.credentialMode);
  if (state.credentialMode === "role" && state.roleArn) {
    p.set("aws_role_arn", state.roleArn);
  }
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


// Keep only exemplars whose durations fall inside the inclusive bounds used by
// the span flamegraph API. The backend refetches bounded candidates so this
// also protects the UI from briefly showing stale, out-of-band rows.
function exemplarsInBand(exemplars, minNs, maxNs) {
  if (!Array.isArray(exemplars)) return [];
  return exemplars.filter((exemplar) => {
    const elapsed = Number(exemplar && exemplar.elapsed_ns);
    return Number.isFinite(elapsed) &&
      (minNs == null || elapsed >= minNs) &&
      (maxNs == null || elapsed <= maxNs);
  });
}

// Patch only query-scoped exemplar fields into an existing progressive catalog.
// Duration-bounded refreshes must not replace catalog statistics with an early
// partial SSE snapshot or rerender unrelated rows.
function mergeSelectedExemplarSnapshot(currentTypes, incomingTypes, selectedUid) {
  const current = Array.isArray(currentTypes) ? currentTypes : [];
  const incoming = Array.isArray(incomingTypes) ? incomingTypes : [];
  const selected = incoming.find((spanType) => spanType.span_type_uid === selectedUid);
  if (!selected) return { spanTypes: current, matched: false };
  return {
    matched: true,
    spanTypes: current.map((spanType) =>
      spanType.span_type_uid === selectedUid
        ? Object.assign({}, spanType, {
            exemplars: selected.exemplars || [],
            selected_duration_count: selected.selected_duration_count,
          })
        : spanType
    ),
  };
}

// A seed-only stream advertises the exact final folded set it is working toward.
// Its cumulative subsets are safe to preview when that target matches the full
// catalog, but cache validity still requires the current successful set itself
// to match exactly.
function classifyExemplarSnapshot(baselineSetId, currentSetId, targetSetId) {
  const complete = baselineSetId != null && currentSetId === baselineSetId;
  return {
    preview: complete || (baselineSetId != null && targetSetId === baselineSetId),
    complete,
  };
}

function completeExemplarRefresh(currentTypes, currentCoverage, snapshotAdopted) {
  return {
    spanTypes: currentTypes,
    coverage: currentCoverage,
    pending: !snapshotAdopted,
  };
}

// Compare query-independent catalog data while ignoring duration-scoped
// exemplar fields. Preserve-mode refreshes use this at stream completion to
// avoid rerendering an already-complete catalog, while still adopting a final
// snapshot if the previous stream had been interrupted mid-refinement.
function sameSpanCatalogStatistics(leftTypes, rightTypes) {
  function normalized(types) {
    if (!Array.isArray(types)) return [];
    return types
      .map((spanType) => {
        const copy = Object.assign({}, spanType);
        delete copy.exemplars;
        delete copy.selected_duration_count;
        return copy;
      })
      .sort((a, b) => String(a.span_type_uid).localeCompare(String(b.span_type_uid)));
  }
  return JSON.stringify(normalized(leftTypes)) === JSON.stringify(normalized(rightTypes));
}

// A preserve-mode response is valid only for the exact type and duration scope
// that initiated it. This rejects late A:X responses after an A→B→A switch
// has returned the UI to A:global.
function exemplarRequestMatches(requestUid, requestScopeKey, currentUid, currentScopeKey) {
  return requestUid === currentUid && requestScopeKey === currentScopeKey;
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
// `exemplar` is a Exemplar: { start_ns, end_ns, source_key, host, ... }.
// `scope` carries the page scope: { trace, bucket, region, service, spanName }.
// Raw mode reuses `trace`; aggregate mode builds an `/api/object` component
// from the exemplar source. The exemplar's own host takes precedence because
// exemplars can come from different hosts. `spanName`, when given, is forwarded
// as `focus_span_name` so the viewer selects the exact span (a long span's
// window overlaps many others).
//
// Returns "" when neither a raw trace nor an exemplar source is available.
function exemplarViewerUrl(exemplar, scope) {
  const ex = exemplar || {};
  const sc = scope || {};

  let traceUrl = sc.trace || "";
  if (!traceUrl) {
    if (!ex.source_key) return "";
    // The backend stores source_key as the fully-qualified
    // `s3://{bucket}/{key}` (that's the `full_key` decode_samples was handed).
    // But `/api/object` wants a bucket + a BUCKET-RELATIVE key — passing the
    // whole `s3://…` URI as `key=` 404s. Split it back into bucket + relative
    // key; fall back to the scope bucket and an already-relative key.
    let bucket = sc.bucket || "";
    let key = ex.source_key;
    const s3 = /^s3:\/\/([^/]+)\/(.+)$/.exec(ex.source_key);
    if (s3) {
      bucket = s3[1];
      key = s3[2];
    }
    // Mirror the landing page's objectTraceUrls(): one
    // `/api/object?bucket=&key=` component, built via URLSearchParams so the
    // key is correctly encoded.
    const oq = new URLSearchParams();
    oq.set("bucket", bucket);
    oq.set("key", key);
    traceUrl = "/api/object?" + oq.toString();
  }

  const p = new URLSearchParams();
  p.set("trace", traceUrl);
  if (sc.service) p.set("svc", sc.service);
  // Prefer the exemplar's own host (it may differ from the scope's host set).
  if (ex.host) p.set("host", ex.host);
  if (sc.region) p.set("aws_region", sc.region);
  if (sc.credentialMode) p.set("credential_mode", sc.credentialMode);
  if (sc.credentialMode === "role" && sc.roleArn) p.set("aws_role_arn", sc.roleArn);
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

// ── Exports ──────────────────────────────────────────────────────────────────

var SpanExplorer = {
  setMaxFilesParam,
  shouldAdoptCatalogSnapshot,
  fmtNs,
  sortSpanTypes,
  spanTypeLabel,
  normalizeSpanHistogram,
  spanHistogramLayout,
  spanPxToNs,
  spanNsToPx,
  spanBrushToBand,
  durationAtPercentile,
  countInBand,
  TIME_CATEGORIES,
  computeTimeComposition,
  bandComposition,
  spanTypeQuality,
  encodeSpanExplorerState,
  decodeSpanExplorerState,
  flamegraphUrl,
  exemplarsInBand,
  percentileForDuration,
  fmtPercentile,
  collectExemplarAttributeKeys,
  exemplarAttrValue,
  columnIsDegenerate,
  mergeSelectedExemplarSnapshot,
  classifyExemplarSnapshot,
  completeExemplarRefresh,
  exemplarRequestMatches,
  sameSpanCatalogStatistics,
  exemplarViewerUrl,
  parseAttrFilterParams,
  formatAttrFilterParams,
  hasAttrFilter,
  addAttrFilter,
  removeAttrFilter,
};

if (typeof module !== "undefined" && module.exports) {
  module.exports = SpanExplorer;
} else if (typeof window !== "undefined") {
  window.SpanExplorer = SpanExplorer;
}
