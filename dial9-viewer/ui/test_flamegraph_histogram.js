#!/usr/bin/env node
"use strict";

// Unit tests for the poll-duration histogram minimap helpers in
// flamegraph_histogram.js: duration formatting, histogram normalization, the
// sample-weighted median (which seeds the fast/slow diff split), the bar layout,
// and brush-pixels → poll-duration-band mapping. All DOM-free.

const {
  fmtDurationNs,
  normalizeHistogram,
  sampleWeightedMedianNs,
  histogramLayout,
  pxToNs,
  brushToBand,
} = require("./flamegraph_histogram.js");

let passed = 0;
let failed = 0;

function assertEq(actual, expected, desc) {
  if (actual === expected) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

function assertDeepEq(actual, expected, desc) {
  assertEq(JSON.stringify(actual), JSON.stringify(expected), desc);
}

function assert(cond, desc) {
  if (cond) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(`✗ ${desc}`);
    failed++;
  }
}

// ── fmtDurationNs ──
assertEq(fmtDurationNs(500), "500ns", "sub-µs → ns");
assertEq(fmtDurationNs(500000), "500µs", "500,000ns → 500µs");
assertEq(fmtDurationNs(1500000), "1.5ms", "1,500,000ns → 1.5ms");
assertEq(fmtDurationNs(50000000), "50ms", "50,000,000ns → 50ms");
assertEq(fmtDurationNs(2000000000), "2s", "2,000,000,000ns → 2s");
assertEq(fmtDurationNs(-1), "", "negative → empty");

// ── normalizeHistogram: sort ascending, drop malformed ──
{
  const raw = [
    { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 1 },
    { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
    { lo_ns: 5, hi_ns: 3, samples: 1 }, // hi <= lo → dropped
    { lo_ns: 10, hi_ns: 20, samples: -1 }, // negative samples → dropped
    null, // → dropped
  ];
  const norm = normalizeHistogram(raw);
  assertEq(norm.length, 2, "malformed entries dropped");
  assertEq(norm[0].lo_ns, 1 << 18, "sorted ascending by lo_ns");
  assertEq(norm[1].lo_ns, 1 << 25, "slow bucket sorts last");
}
assertDeepEq(normalizeHistogram(null), [], "non-array → empty");
assertDeepEq(normalizeHistogram([]), [], "empty → empty");

// ── sampleWeightedMedianNs: the fast/slow split ──
// 2 samples at bucket 2^18, 1 sample at 2^25. Total 3, half = 1.5. Cumulative
// hits 1.5 at the first bar (2 >= 1.5) → split at its lo_ns.
assertEq(
  sampleWeightedMedianNs([
    { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
    { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 1 },
  ]),
  1 << 18,
  "median lands in the sample-heavy fast bucket",
);
// Flip the weights: now the slow bucket holds the majority, so the median moves
// to it (half=1.5, first bar has 1 < 1.5, second reaches 3).
assertEq(
  sampleWeightedMedianNs([
    { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 1 },
    { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 2 },
  ]),
  1 << 25,
  "median follows sample weight, not bar count",
);
assertEq(sampleWeightedMedianNs([]), null, "empty histogram → null median");

// ── histogramLayout: equal-width columns, height fraction vs max ──
{
  const bars = [
    { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 },
    { lo_ns: 1 << 20, hi_ns: 1 << 21, samples: 8 },
    { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 4 },
  ];
  const { cols, maxSamples } = histogramLayout(bars, 300, 0);
  assertEq(cols.length, 3, "one column per bar");
  assertEq(maxSamples, 8, "max samples across bars");
  assertEq(cols[0].x, 0, "first column at x=0");
  assertEq(cols[1].x, 100, "columns equal width (300/3)");
  assertEq(cols[0].hFrac, 0.25, "height fraction = samples/max (2/8)");
  assertEq(cols[1].hFrac, 1, "tallest bar is full height");
}
assertDeepEq(histogramLayout([], 300, 1), { cols: [], maxSamples: 0 }, "empty → no cols");

// ── brushToBand: pixels → bucket-snapped { min_ns, max_ns } ──
{
  const bars = [
    { lo_ns: 1 << 18, hi_ns: 1 << 19, samples: 2 }, // column 0: x [0,100)
    { lo_ns: 1 << 20, hi_ns: 1 << 21, samples: 8 }, // column 1: x [100,200)
    { lo_ns: 1 << 25, hi_ns: 1 << 26, samples: 4 }, // column 2: x [200,300)
  ];
  // Brush is CONTINUOUS (log-interpolated), not bucket-snapped. A brush from the
  // start of column 1 (x=100 → lo of col1) to the end of column 2 (x=300 → hi of
  // col2) gives exactly those edges.
  assertDeepEq(
    brushToBand(bars, 300, 100, 300),
    { min_ns: 1 << 20, max_ns: 1 << 26 },
    "brush from col1 start to col2 end = their outer edges",
  );
  // Full-width brush → whole range.
  assertDeepEq(
    brushToBand(bars, 300, 0, 300),
    { min_ns: 1 << 18, max_ns: 1 << 26 },
    "full brush spans all buckets",
  );
  // Sub-bucket selection: a brush INSIDE column 0 (x 25→75, i.e. frac 0.25→0.75)
  // yields ns strictly between that bar's lo and hi — you can filter tighter than
  // one bucket. Geometric interp: lo·(hi/lo)^0.25 .. lo·(hi/lo)^0.75.
  {
    const band = brushToBand(bars, 300, 25, 75);
    assert(band.min_ns > (1 << 18) && band.min_ns < (1 << 19),
      "sub-bucket brush min is inside col0, not snapped to its edge");
    assert(band.max_ns > band.min_ns && band.max_ns < (1 << 19),
      "sub-bucket brush max is inside col0 and above min");
  }
  // A near-zero-width drag (a click) is NOT a band — the UI treats it as "set
  // split" instead, so brushToBand returns null.
  assertEq(brushToBand(bars, 300, 150, 150), null, "click (zero width) → null band");
  assertEq(brushToBand(bars, 300, 150, 151), null, "sub-2px drag → null (still a click)");
}
assertEq(brushToBand([], 300, 0, 100), null, "empty histogram → null band");

// ── pxToNs: continuous pixel → ns via log interpolation ──
{
  const bars = [
    { lo_ns: 1000, hi_ns: 2000, samples: 1 }, // col 0: x [0,100)
    { lo_ns: 2000, hi_ns: 4000, samples: 1 }, // col 1: x [100,200)
  ];
  assertEq(pxToNs(bars, 200, 0, null), 1000, "left edge → first bar lo");
  assertEq(pxToNs(bars, 200, 200, null), 4000, "right edge → last bar hi (clamped)");
  // Midpoint of column 0 (x=50, frac 0.5) → geometric mean of [1000,2000] ≈ 1414.
  assertEq(pxToNs(bars, 200, 50, null), Math.round(1000 * Math.sqrt(2)), "col midpoint = geometric mean");
  assertEq(pxToNs([], 200, 50, null), null, "empty → null");
}

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
