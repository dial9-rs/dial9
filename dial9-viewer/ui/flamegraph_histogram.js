"use strict";

// Pure helpers for the poll-duration histogram minimap in flamegraph.html.
//
// The `/api/flamegraph` response carries a sample-weighted, log₂-bucketed
// poll-duration histogram in `metadata.poll_duration_histogram`: an array of
// `{ lo_ns, hi_ns, samples }` bars (ascending by range). It answers "what value
// do I set the band to?" — bar height is the samples you'd select by brushing
// that range, so the flamegraph below a brushed span is exactly those samples.
//
// These functions are DOM-free and CommonJS-exported so they can be unit-tested
// under Node; in the browser they attach as globals via the top-level `function`
// declarations. The rendering + brush wiring lives in flamegraph.html.

// Format a nanosecond duration as a short human string for axis ticks/tooltips:
//   500000 -> "500µs", 1500000 -> "1.5ms", 50000000 -> "50ms", 2e9 -> "2s".
// Sub-microsecond values fall back to "ns". Trailing ".0" is trimmed.
function fmtDurationNs(ns) {
  const n = Number(ns);
  if (!Number.isFinite(n) || n < 0) return "";
  const trim = (x) => String(Number(x.toFixed(1)));
  if (n < 1e3) return n + "ns";
  if (n < 1e6) return trim(n / 1e3) + "µs";
  if (n < 1e9) return trim(n / 1e6) + "ms";
  return trim(n / 1e9) + "s";
}

// Normalize + validate the backend histogram into ascending bars with numeric
// fields. Drops malformed entries (non-finite / negative). Empty in → empty out.
function normalizeHistogram(hist) {
  if (!Array.isArray(hist)) return [];
  const bars = [];
  for (const b of hist) {
    if (b == null) continue;
    const lo = Number(b.lo_ns);
    const hi = Number(b.hi_ns);
    const s = Number(b.samples);
    if (!Number.isFinite(lo) || !Number.isFinite(hi) || !Number.isFinite(s)) continue;
    if (hi <= lo || s < 0) continue;
    bars.push({ lo_ns: lo, hi_ns: hi, samples: s });
  }
  bars.sort((a, b) => a.lo_ns - b.lo_ns);
  return bars;
}

// The sample-weighted median poll duration (ns), used to seed the "Diff fast vs
// slow" split: the boundary where half the on-CPU samples are in slower polls.
// Returns the lo_ns of the bar that crosses the halfway sample count, or null
// when the histogram is empty. Using a bucket boundary (not an interpolated
// value) keeps the split aligned to the bars the user sees.
function sampleWeightedMedianNs(bars) {
  const norm = normalizeHistogram(bars);
  const total = norm.reduce((a, b) => a + b.samples, 0);
  if (total === 0) return null;
  const half = total / 2;
  let cum = 0;
  for (const b of norm) {
    cum += b.samples;
    if (cum >= half) return b.lo_ns;
  }
  return norm[norm.length - 1].lo_ns;
}

// Given normalized bars and a pixel width, compute the drawing geometry: each
// bar gets an x/width (equal-width columns, since buckets are log₂ and we want a
// readable strip, not a linear-time axis) and a height fraction (samples / max).
// `gap` px separates columns. Returns { cols: [{lo_ns,hi_ns,samples,x,w,hFrac}],
// maxSamples }. Empty bars → empty cols.
function histogramLayout(bars, width, gap) {
  const norm = normalizeHistogram(bars);
  const g = gap == null ? 1 : gap;
  const n = norm.length;
  if (n === 0 || width <= 0) return { cols: [], maxSamples: 0 };
  const maxSamples = norm.reduce((m, b) => Math.max(m, b.samples), 0);
  const colW = width / n;
  const cols = norm.map((b, i) => ({
    lo_ns: b.lo_ns,
    hi_ns: b.hi_ns,
    samples: b.samples,
    x: i * colW,
    w: Math.max(0, colW - g),
    hFrac: maxSamples > 0 ? b.samples / maxSamples : 0,
  }));
  return { cols, maxSamples };
}

// Map a pixel x over the strip to a CONTINUOUS poll duration (ns), so a brush
// can sub-select *within* a histogram bar rather than snapping to bucket edges.
// Columns are equal pixel width; within the column under `px` we interpolate
// geometrically (log-linear) between that bar's [lo_ns, hi_ns] — matching the
// log duration axis. Clamps to the strip ends. Returns null when there are no
// bars. `cols` may be passed in (from a prior histogramLayout) to avoid
// recomputing; otherwise it's derived from `bars`.
function pxToNs(bars, width, px, cols) {
  const c = cols || histogramLayout(bars, width, 0).cols;
  const n = c.length;
  if (n === 0 || width <= 0) return null;
  const colW = width / n;
  let idx = Math.floor(px / colW);
  if (idx < 0) idx = 0;
  if (idx > n - 1) idx = n - 1;
  let frac = (px - idx * colW) / colW; // 0..1 within the column
  if (frac < 0) frac = 0;
  if (frac > 1) frac = 1;
  const { lo_ns, hi_ns } = c[idx];
  // Geometric interpolation: lo * (hi/lo)^frac. Equivalent to linear in log
  // space, so the mapping is uniform along the log axis the bars are drawn on.
  return Math.round(lo_ns * Math.pow(hi_ns / lo_ns, frac));
}

// Map a brushed pixel range [x0, x1] over the strip to a CONTINUOUS poll-duration
// band { min_ns, max_ns } via log-interpolation (see pxToNs) — so you can drag
// from the middle of one bar to the middle of another and get exact ns bounds,
// not bucket-snapped ones. Returns null when there are no bars, or when the brush
// has ~zero width (a click, which the UI treats as "set split", not "select band").
function brushToBand(bars, width, x0, x1) {
  const { cols } = histogramLayout(bars, width, 0);
  if (cols.length === 0) return null;
  const lo = Math.min(x0, x1);
  const hi = Math.max(x0, x1);
  if (hi - lo < 2) return null; // treat a near-zero drag as a click, not a band
  return {
    min_ns: pxToNs(bars, width, lo, cols),
    max_ns: pxToNs(bars, width, hi, cols),
  };
}

var FlamegraphHistogram = {
  fmtDurationNs,
  normalizeHistogram,
  sampleWeightedMedianNs,
  histogramLayout,
  pxToNs,
  brushToBand,
};

if (typeof module !== "undefined" && module.exports) {
  module.exports = FlamegraphHistogram;
} else if (typeof window !== "undefined") {
  // Browser: expose as a namespace so flamegraph.html's renderMinimap can find
  // it. Without this the top-level `function` declarations are globals too, but
  // renderMinimap calls `FlamegraphHistogram.*`, so the namespace must exist —
  // otherwise renderEvent throws before it clears the loading spinner.
  window.FlamegraphHistogram = FlamegraphHistogram;
}
