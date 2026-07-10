#!/usr/bin/env node
"use strict";

// Unit tests for the S3 browser density-timeline helpers in heatmap.js.

const {
    MAX_OPEN_BYTES,
    groupByHost,
    bootTransitions,
    tileSegments,
    segmentGaps,
    accumulateDensity,
    segmentsOverlapping,
    totalBytes,
    densityColor,
    shouldClearSelectionOnClick,
    niceTimeTicks,
} = require("./heatmap.js");

let failed = 0;
let passed = 0;

function ok(cond, label) {
    if (cond) {
        passed++;
        console.log(`✓ ${label}`);
    } else {
        failed++;
        console.error(`✗ ${label}`);
    }
}

function approx(a, b, eps, label) {
    ok(Math.abs(a - b) <= (eps ?? 1e-6), `${label} (got ${a}, want ~${b})`);
}

function seg(o) {
    return Object.assign(
        { key: "k", size: 0, start: 0, end: 0, service: "svc", host: "h", bootId: "" },
        o,
    );
}

// ── groupByHost ──
{
    const rows = groupByHost([
        seg({ service: "api", host: "h2", size: 10, start: 5 }),
        seg({ service: "api", host: "h1", size: 20, start: 1 }),
        seg({ service: "api", host: "h1", size: 30, start: 3, bootId: "bbbb" }),
    ]);
    ok(rows.length === 2, "groupByHost: two host rows");
    ok(rows[0].host === "h1" && rows[1].host === "h2", "groupByHost: sorted by label");
    ok(rows[0].segments.length === 2, "groupByHost: h1 has both segments (boot not split)");
    ok(rows[0].totalBytes === 50, "groupByHost: totalBytes summed");
    ok(
        rows[0].segments[0].start <= rows[0].segments[1].start,
        "groupByHost: segments sorted by start",
    );
}

// ── bootTransitions ──
{
    const t = bootTransitions([
        seg({ start: 1, bootId: "aaaa" }),
        seg({ start: 2, bootId: "aaaa" }),
        seg({ start: 3, bootId: "bbbb" }),
        seg({ start: 4, bootId: "bbbb" }),
        seg({ start: 5, bootId: "cccc" }),
    ]);
    ok(t.length === 2, "bootTransitions: two transitions");
    ok(t[0].time === 3 && t[0].fromBoot === "aaaa" && t[0].toBoot === "bbbb", "bootTransitions: first");
    ok(t[1].time === 5 && t[1].toBoot === "cccc", "bootTransitions: second");

    ok(bootTransitions([seg({ start: 1 }), seg({ start: 2 })]).length === 0,
        "bootTransitions: none without boot ids (legacy)");
}

// ── accumulateDensity ──
{
    // One segment spanning the whole range, 400 bytes, width 4 → 100 per col.
    const cols = accumulateDensity([seg({ size: 400, start: 0, end: 4 })], 0, 4, 4);
    ok(cols.length === 4, "accumulateDensity: width respected");
    approx(cols[0], 100, 1e-6, "accumulateDensity: uniform col 0");
    approx(cols[3], 100, 1e-6, "accumulateDensity: uniform col 3");
    const sum = cols.reduce((a, b) => a + b, 0);
    approx(sum, 400, 1e-6, "accumulateDensity: total bytes conserved");
}
{
    // Baseline 1MB/min across 10 cols + a 50MB spike in one minute. The spike
    // column must dominate and be ~50x the baseline columns.
    const segs = [];
    for (let i = 0; i < 10; i++) {
        segs.push(seg({ size: 1e6, start: i * 60, end: (i + 1) * 60, key: "b" + i }));
    }
    // spike: 50MB in minute index 4
    segs[4] = seg({ size: 50e6, start: 4 * 60, end: 5 * 60, key: "spike" });
    const cols = accumulateDensity(segs, 0, 600, 10);
    let maxIdx = 0;
    for (let i = 1; i < cols.length; i++) if (cols[i] > cols[maxIdx]) maxIdx = i;
    ok(maxIdx === 4, "accumulateDensity: spike lands in the right column");
    approx(cols[4] / cols[0], 50, 0.001, "accumulateDensity: spike is ~50x baseline");
}
{
    // Out-of-range / degenerate inputs are safe.
    ok(accumulateDensity([], 0, 10, 5).every((v) => v === 0), "accumulateDensity: empty → zeros");
    ok(accumulateDensity([seg({ size: 100, start: 0, end: 1 })], 5, 5, 4).length === 4,
        "accumulateDensity: zero-width range → zeros length kept");
}
{
    // A segment with end <= start is given MIN_SEGMENT_SECONDS so its bytes
    // still land as a localized spike rather than vanishing.
    const cols = accumulateDensity([seg({ size: 500, start: 10, end: 10 })], 0, 20, 20);
    const sum = cols.reduce((a, b) => a + b, 0);
    approx(sum, 500, 1e-6, "accumulateDensity: zero-duration segment still contributes its bytes");
    ok(cols[10] > 0 && cols[15] === 0, "accumulateDensity: zero-duration spike is localized at its start");
}
{
    // Only the in-range portion of a segment's bytes is attributed when it
    // partially overlaps [t0,t1]: half of a uniformly-spread segment.
    const cols = accumulateDensity([seg({ size: 800, start: 0, end: 80 })], 40, 80, 4);
    const sum = cols.reduce((a, b) => a + b, 0);
    approx(sum, 400, 1e-6, "accumulateDensity: partial overlap attributes only the in-range bytes");
}

// ── tileSegments & segmentGaps ──
{
    // Upload-lag overlap: each segment's end (last_modified) runs past the next
    // segment's start. tileSegments clamps the end to the next start; the raw
    // overlap would double-count density at the seam.
    const segs = [
        seg({ key: "a", size: 100, start: 0, end: 17 }),  // ends 2s into b
        seg({ key: "b", size: 100, start: 15, end: 32 }), // ends 2s into c
        seg({ key: "c", size: 100, start: 30, end: 47 }),
    ];
    const tiled = tileSegments(segs);
    ok(tiled[0].end === 15 && tiled[1].end === 30,
        "tileSegments: ends clamped to the next start");
    ok(tiled[2].end === 47, "tileSegments: last segment keeps its end");
    ok(tiled[0].realEnd === 17 && tiled[1].realEnd === 32,
        "tileSegments: real end preserved for selection/gaps");
    ok(segmentGaps(segs).length === 0, "segmentGaps: overlapping rotation has no gap");

    // The seam column is no longer brighter than a body column once tiled.
    const cols = accumulateDensity(tiled, 0, 47, 47);
    const seam = cols[15]; // the a/b boundary second
    const body = cols[5];
    ok(seam <= body * 1.2,
        `tileSegments: seam no longer spikes (seam ${seam.toFixed(1)} vs body ${body.toFixed(1)})`);

    // A real coverage hole (next starts after this one's real end) is a gap.
    const withHole = [
        seg({ key: "a", size: 100, start: 0, end: 17 }),
        seg({ key: "b", size: 100, start: 60, end: 77 }), // 43s hole after a
    ];
    const gaps = segmentGaps(withHole);
    ok(gaps.length === 1 && gaps[0].start === 17 && gaps[0].end === 60,
        "segmentGaps: real coverage hole detected with [end, nextStart]");
    // Tiling must not invent coverage across the hole.
    ok(tileSegments(withHole)[0].end === 17,
        "tileSegments: no clamp across a real gap (next start is past end)");

    // Inputs need not be pre-sorted.
    const unsorted = [seg({ start: 30, end: 47 }), seg({ start: 0, end: 17 }), seg({ start: 15, end: 32 })];
    const ts = tileSegments(unsorted);
    ok(ts[0].start === 0 && ts[1].start === 15 && ts[2].start === 30,
        "tileSegments: sorts before clamping");
}

// ── segmentsOverlapping & totalBytes ──
{
    const segs = [
        seg({ key: "a", size: 10, start: 0, end: 60 }),
        seg({ key: "b", size: 20, start: 60, end: 120 }),
        seg({ key: "c", size: 40, start: 120, end: 180 }),
    ];
    const sel = segmentsOverlapping(segs, 30, 90);
    ok(sel.length === 2 && sel[0].key === "a" && sel[1].key === "b",
        "segmentsOverlapping: picks overlapping segments");
    ok(totalBytes(sel) === 30, "totalBytes: sums selected sizes");
    ok(segmentsOverlapping(segs, 200, 300).length === 0, "segmentsOverlapping: none outside range");

    // A point query (single click) exactly on a segment's start must select it
    // rather than fall through the crack between adjacent segments.
    const atStart = segmentsOverlapping(segs, 60, 60);
    ok(atStart.length === 1 && atStart[0].key === "b",
        "segmentsOverlapping: click on a segment's start selects that segment");
    const atZero = segmentsOverlapping(segs, 0, 0);
    ok(atZero.length === 1 && atZero[0].key === "a",
        "segmentsOverlapping: click on the very first start still selects");
}

// ── densityColor ──
{
    ok(densityColor(0) === "rgb(26,26,46)", "densityColor: 0 → background");
    ok(densityColor(-1) === "rgb(26,26,46)", "densityColor: negative → background");
    const lum = (c) => {
        const [r, g, b] = c.match(/\d+/g).map(Number);
        return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    const low = densityColor(0.05);
    const mid = densityColor(0.4);
    const hi = densityColor(1);
    ok(low !== "rgb(26,26,46)", "densityColor: small positive is visible (not background)");
    ok(lum(low) < lum(mid) && lum(mid) < lum(hi), "densityColor: brighter with higher density");
    ok(densityColor(1) === "rgb(255,217,61)", "densityColor: 1 → hot yellow");
}

// ── shouldClearSelectionOnClick ──
{
    // The regression: a selection drag that ends outside the pane fires a
    // synthetic click on an ancestor above #heatmap-view. That click must NOT
    // clear the just-created selection.
    ok(shouldClearSelectionOnClick({
        isBrowseTab: true, hasSelection: true, wasDrag: true,
        targetInHeatmap: false, targetInActions: false,
    }) === false, "shouldClearSelectionOnClick: drag ending outside pane keeps selection");

    // A genuine outside click (no drag) clears the selection.
    ok(shouldClearSelectionOnClick({
        isBrowseTab: true, hasSelection: true, wasDrag: false,
        targetInHeatmap: false, targetInActions: false,
    }) === true, "shouldClearSelectionOnClick: genuine outside click clears");

    // Clicks inside the timeline or on the actions bar preserve the selection.
    ok(shouldClearSelectionOnClick({
        isBrowseTab: true, hasSelection: true, wasDrag: false,
        targetInHeatmap: true, targetInActions: false,
    }) === false, "shouldClearSelectionOnClick: click inside heatmap keeps selection");
    ok(shouldClearSelectionOnClick({
        isBrowseTab: true, hasSelection: true, wasDrag: false,
        targetInHeatmap: false, targetInActions: true,
    }) === false, "shouldClearSelectionOnClick: click on actions bar keeps selection");

    // Regression (#645/#644 interaction): the TZ toggle and credentials button
    // live in the page <header>, which is neither the timeline nor the actions
    // bar. Clicking header chrome must NOT clear the selection — toggling TZ
    // only relabels the axis.
    ok(shouldClearSelectionOnClick({
        isBrowseTab: true, hasSelection: true, wasDrag: false,
        targetInHeatmap: false, targetInActions: false, targetInHeader: true,
    }) === false, "shouldClearSelectionOnClick: click in header (TZ toggle) keeps selection");

    // No-ops when not on browse tab or nothing is selected.
    ok(shouldClearSelectionOnClick({
        isBrowseTab: false, hasSelection: true, wasDrag: false,
        targetInHeatmap: false, targetInActions: false,
    }) === false, "shouldClearSelectionOnClick: not browse tab → no clear");
    ok(shouldClearSelectionOnClick({
        isBrowseTab: true, hasSelection: false, wasDrag: false,
        targetInHeatmap: false, targetInActions: false,
    }) === false, "shouldClearSelectionOnClick: no selection → no clear");
}

// ── niceTimeTicks ──
{
    // Ticks are ascending and stay within [tMin, tMax].
    const ticks = niceTimeTicks(1000, 1600, 6);
    ok(ticks.length > 0, "niceTimeTicks: returns at least one tick");
    ok(ticks.every((t, i) => i === 0 || t > ticks[i - 1]), "niceTimeTicks: strictly ascending");
    ok(ticks.every((t) => t >= 1000 && t <= 1600), "niceTimeTicks: all ticks within range");

    // A ~600s span with target 6 must snap to a round step (a multiple of a
    // human interval like 120s), not an arbitrary 100s division.
    const step = ticks[1] - ticks[0];
    ok([60, 120].includes(step), `niceTimeTicks: snaps to a round step (got ${step})`);
    ok(step % 60 === 0 || step % 30 === 0, "niceTimeTicks: step is a round interval");

    // Count stays within the target.
    ok(ticks.length <= 6, `niceTimeTicks: count <= target (got ${ticks.length})`);

    // Ticks are aligned to multiples of the step so they land on round times.
    ok(ticks.every((t) => t % step === 0), "niceTimeTicks: ticks aligned to the step");

    // A wider span still respects the target.
    const wide = niceTimeTicks(0, 86400, 8);
    ok(wide.length <= 8, `niceTimeTicks: wide span respects target (got ${wide.length})`);
    ok(wide.every((t, i) => i === 0 || t > wide[i - 1]), "niceTimeTicks: wide span ascending");
}
{
    // Degenerate range (tMax <= tMin) returns a single tick without throwing.
    ok(niceTimeTicks(500, 500, 6).length === 1, "niceTimeTicks: equal bounds → single tick");
    ok(niceTimeTicks(500, 500, 6)[0] === 500, "niceTimeTicks: single tick is tMin");
    ok(niceTimeTicks(900, 500, 6).length === 1, "niceTimeTicks: inverted bounds → single tick");
}

// ── constants ──
ok(MAX_OPEN_BYTES === 200 * 1024 * 1024, "MAX_OPEN_BYTES is 200 MiB");

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
