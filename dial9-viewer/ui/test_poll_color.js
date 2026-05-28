#!/usr/bin/env node
"use strict";

// Unit tests for pollHeatmapColor — a continuous, log-scale heatmap mapping
// poll duration (in nanoseconds) to a hex color string.
//
// Run with: node test_poll_color.js
// Used by viewer.html for issue #450 (item 1: poll color heatmap).

const { pollHeatmapColor } = require("./trace_analysis.js");

let failures = 0;
function fail(msg) {
  console.log(`✗ ${msg}`);
  failures++;
}
function pass(msg) {
  console.log(`✓ ${msg}`);
}

function isHex(s) {
  return typeof s === "string" && /^#[0-9a-f]{6}$/i.test(s);
}

function hexToRgb(hex) {
  return [
    parseInt(hex.slice(1, 3), 16),
    parseInt(hex.slice(3, 5), 16),
    parseInt(hex.slice(5, 7), 16),
  ];
}

// 1. Returns valid hex strings for any input duration
function testHexFormat() {
  const inputs = [0, 1, 100, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e12];
  for (const d of inputs) {
    const c = pollHeatmapColor(d);
    if (!isHex(c)) {
      fail(`pollHeatmapColor(${d}) returned ${JSON.stringify(c)} — not valid hex`);
      return;
    }
  }
  pass(`pollHeatmapColor returns valid #rrggbb for diverse inputs`);
}

// 2. Monotonic redness — longer polls should be at least as "red"
//    (R component never decreases) as shorter polls across the interesting
//    range. The blue channel doesn't need to be strictly monotonic — the ramp
//    intentionally passes through cyan (peak blue) on its way from dim navy
//    to red — but the start and end of the range must clearly differ in the
//    expected direction.
function testMonotonicRednessAcrossRange() {
  const samples = [];
  // Sample log-spaced from 1µs to 1s
  for (let lg = 3; lg <= 9; lg += 0.5) {
    samples.push(Math.pow(10, lg));
  }
  const colors = samples.map((d) => hexToRgb(pollHeatmapColor(d)));
  const reds = colors.map((c) => c[0]);
  const blues = colors.map((c) => c[2]);
  // Per-step: red never decreases (allow plateau at 255)
  for (let i = 1; i < reds.length; i++) {
    if (reds[i] < reds[i - 1]) {
      fail(`red decreased between samples ${i - 1}→${i}: ${reds[i - 1]} → ${reds[i]}`);
      return;
    }
  }
  // Overall trend: end clearly redder and less blue than start
  if (reds[reds.length - 1] <= reds[0]) {
    fail(`expected red to grow across range, got ${reds[0]} → ${reds[reds.length - 1]}`);
    return;
  }
  if (blues[blues.length - 1] >= blues[0]) {
    fail(`expected blue to shrink across range, got ${blues[0]} → ${blues[blues.length - 1]}`);
    return;
  }
  pass(`redness grows monotonically (with plateau at 255) across log-spaced 1µs–1s range`);
}

// 3. Clamps below the floor: very short polls (≤100ns) all map to the same dim color
function testClampBelowFloor() {
  const c0 = pollHeatmapColor(0);
  const c1 = pollHeatmapColor(50);
  const c2 = pollHeatmapColor(100);
  if (c0 !== c1 || c1 !== c2) {
    fail(`expected all sub-100ns durations to map to the same color, got ${c0}, ${c1}, ${c2}`);
    return;
  }
  pass(`durations ≤100ns clamp to a single floor color (${c0})`);
}

// 4. Clamps above the ceiling: very long polls all map to the same hot color
function testClampAboveCeiling() {
  const c1 = pollHeatmapColor(1e9);    // 1s
  const c2 = pollHeatmapColor(1e10);   // 10s
  const c3 = pollHeatmapColor(1e15);   // ridiculous
  if (c1 !== c2 || c2 !== c3) {
    fail(`expected very long durations to clamp to a ceiling color, got ${c1}, ${c2}, ${c3}`);
    return;
  }
  pass(`durations ≥1s clamp to a single ceiling color (${c1})`);
}

// 5. Colors at the canonical anchor points should match the legend swatches
//    so the legend stays an honest reference point.
function testAnchorPointsMatchLegend() {
  // These are the colors the heatmap legend explicitly shows. The function
  // must produce them at exactly these durations.
  const ANCHORS = [
    { ns: 100,    color: "#2a5a7a", label: "≤100ns (floor: dim navy)" },
    { ns: 10e3,   color: "#4fc3f7", label: "10µs (cyan)" },
    { ns: 100e3,  color: "#ff8a65", label: "100µs (orange)" },
    { ns: 1e6,    color: "#ff4444", label: "1ms (bright red)" },
  ];
  for (const a of ANCHORS) {
    const got = pollHeatmapColor(a.ns).toLowerCase();
    if (got !== a.color.toLowerCase()) {
      fail(`pollHeatmapColor(${a.ns}) = ${got}, expected ${a.color} for ${a.label}`);
      return;
    }
  }
  pass(`anchor points (100ns, 10µs, 100µs, 1ms) match legend swatches exactly`);
}

testHexFormat();
testMonotonicRednessAcrossRange();
testClampBelowFloor();
testClampAboveCeiling();
testAnchorPointsMatchLegend();

if (failures > 0) {
  console.log(`\n${failures} test(s) failed`);
  process.exit(1);
}
console.log(`\nAll poll color heatmap tests passed`);
