#!/usr/bin/env node
"use strict";

// Unit tests for enclosingSpans — the per-worker enclosing-span resolver used
// by the viewer's Related sidebar.
//
// Run with: node test_enclosing_spans.js
//
// The old logic matched any span whose overall [start,end] envelope covered the
// event timestamp. That envelope is the min/max across a span's per-worker
// segments, so a span polled on another worker falsely "enclosed" events it
// never executed alongside. enclosingSpans matches the actual per-worker
// segments and requires the event to carry a worker_id.

const { assert, test, summarize } = require("./test_harness.js");
const { enclosingSpans } = require("./trace_analysis.js");

// Build a span record matching buildSpanData's shape (only the fields
// enclosingSpans reads: start, end, spanId, depth, segments).
function span(spanId, depth, segments) {
  const start = Math.min(...segments.map(s => s.start));
  const end = Math.max(...segments.map(s => s.end));
  return { spanId, depth, start, end, segments };
}

const ids = spans => spans.map(s => s.spanId);

// Envelope overlap is not enough — only the span actually executing on the
// event's worker at ts is returned. spanB has a huge envelope (its segment is
// on worker 1, far in time) but never runs on worker 0 at ts.
test("envelope overlap alone does not enclose (per-worker match)", () => {
  const allSpans = [
    span("A", 0, [{ start: 100, end: 200, workerId: 0 }]),
    span("B", 0, [{ start: 50, end: 500, workerId: 1 }]),
  ];
  const ev = { timestamp: 150, fields: { worker_id: 0 } };
  assert.deepStrictEqual(ids(enclosingSpans(allSpans, ev)), ["A"]);
});

// Nested parent/child segments on the same worker -> both returned, outermost
// (lowest depth) first.
test("nested stack returned outermost-first", () => {
  const allSpans = [
    span("child", 1, [{ start: 120, end: 180, workerId: 0 }]),
    span("parent", 0, [{ start: 100, end: 200, workerId: 0 }]),
  ];
  const ev = { timestamp: 150, fields: { worker_id: 0 } };
  assert.deepStrictEqual(ids(enclosingSpans(allSpans, ev)), ["parent", "child"]);
});

// Event with no worker_id (the CPU/resource-usage case from the flush thread)
// is enclosed by nothing.
test("event without worker_id has no enclosing spans", () => {
  const allSpans = [span("A", 0, [{ start: 100, end: 200, workerId: 0 }])];
  const ev = { timestamp: 150, fields: { cpu_ns: 42 } };
  assert.deepStrictEqual(enclosingSpans(allSpans, ev), []);
});

// Event with worker_id whose timestamp falls between the span's segments (span
// suspended/awaiting then, not executing) -> not enclosed.
test("span not executing at ts (between segments) does not enclose", () => {
  const allSpans = [
    span("A", 0, [
      { start: 100, end: 200, workerId: 0 },
      { start: 300, end: 400, workerId: 0 },
    ]),
  ];
  const ev = { timestamp: 250, fields: { worker_id: 0 } };
  assert.deepStrictEqual(enclosingSpans(allSpans, ev), []);
});

// worker_id present but pointing at a worker the span never ran on -> none.
test("span on a different worker does not enclose", () => {
  const allSpans = [span("A", 0, [{ start: 100, end: 200, workerId: 0 }])];
  const ev = { timestamp: 150, fields: { worker_id: 3 } };
  assert.deepStrictEqual(enclosingSpans(allSpans, ev), []);
});

// worker_id as a string is coerced (event fields may arrive as strings).
test("string worker_id is coerced to number", () => {
  const allSpans = [span("A", 0, [{ start: 100, end: 200, workerId: 0 }])];
  const ev = { timestamp: 150, fields: { worker_id: "0" } };
  assert.deepStrictEqual(ids(enclosingSpans(allSpans, ev)), ["A"]);
});

summarize();
