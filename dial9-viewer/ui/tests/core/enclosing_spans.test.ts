// Unit tests for enclosingSpans — the per-worker enclosing-span resolver used
// by the viewer's Related sidebar.
//
// Migrated from test_enclosing_spans.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale).
//
// The old logic matched any span whose overall [start,end] envelope covered the
// event timestamp. That envelope is the min/max across a span's per-worker
// segments, so a span polled on another worker falsely "enclosed" events it
// never executed alongside. enclosingSpans matches the actual per-worker
// segments and requires the event to carry a worker_id.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface Segment {
  start: number;
  end: number;
  workerId: number;
}

interface SpanRecord {
  spanId: string;
  depth: number;
  start: number;
  end: number;
  segments: Segment[];
}

interface SpanEvent {
  timestamp: number;
  fields: Record<string, unknown>;
}

const { enclosingSpans } = require("../../trace_analysis.js") as {
  enclosingSpans: (allSpans: SpanRecord[], ev: SpanEvent) => SpanRecord[];
};

// Build a span record matching buildSpanData's shape (only the fields
// enclosingSpans reads: start, end, spanId, depth, segments).
function span(spanId: string, depth: number, segments: Segment[]): SpanRecord {
  const start = Math.min(...segments.map((s) => s.start));
  const end = Math.max(...segments.map((s) => s.end));
  return { spanId, depth, start, end, segments };
}

const ids = (spans: SpanRecord[]): string[] => spans.map((s) => s.spanId);

describe("enclosingSpans", () => {
  // Envelope overlap is not enough — only the span actually executing on the
  // event's worker at ts is returned. spanB has a huge envelope (its segment is
  // on worker 1, far in time) but never runs on worker 0 at ts.
  it("envelope overlap alone does not enclose (per-worker match)", () => {
    const allSpans = [
      span("A", 0, [{ start: 100, end: 200, workerId: 0 }]),
      span("B", 0, [{ start: 50, end: 500, workerId: 1 }]),
    ];
    const ev = { timestamp: 150, fields: { worker_id: 0 } };
    expect(ids(enclosingSpans(allSpans, ev))).toEqual(["A"]);
  });

  // Nested parent/child segments on the same worker -> both returned, outermost
  // (lowest depth) first.
  it("nested stack returned outermost-first", () => {
    const allSpans = [
      span("child", 1, [{ start: 120, end: 180, workerId: 0 }]),
      span("parent", 0, [{ start: 100, end: 200, workerId: 0 }]),
    ];
    const ev = { timestamp: 150, fields: { worker_id: 0 } };
    expect(ids(enclosingSpans(allSpans, ev))).toEqual(["parent", "child"]);
  });

  // Event with no worker_id (the CPU/resource-usage case from the flush thread)
  // is enclosed by nothing.
  it("event without worker_id has no enclosing spans", () => {
    const allSpans = [span("A", 0, [{ start: 100, end: 200, workerId: 0 }])];
    const ev = { timestamp: 150, fields: { cpu_ns: 42 } };
    expect(enclosingSpans(allSpans, ev)).toEqual([]);
  });

  // Event with worker_id whose timestamp falls between the span's segments (span
  // suspended/awaiting then, not executing) -> not enclosed.
  it("span not executing at ts (between segments) does not enclose", () => {
    const allSpans = [
      span("A", 0, [
        { start: 100, end: 200, workerId: 0 },
        { start: 300, end: 400, workerId: 0 },
      ]),
    ];
    const ev = { timestamp: 250, fields: { worker_id: 0 } };
    expect(enclosingSpans(allSpans, ev)).toEqual([]);
  });

  // worker_id present but pointing at a worker the span never ran on -> none.
  it("span on a different worker does not enclose", () => {
    const allSpans = [span("A", 0, [{ start: 100, end: 200, workerId: 0 }])];
    const ev = { timestamp: 150, fields: { worker_id: 3 } };
    expect(enclosingSpans(allSpans, ev)).toEqual([]);
  });

  // worker_id as a string is coerced (event fields may arrive as strings).
  it("string worker_id is coerced to number", () => {
    const allSpans = [span("A", 0, [{ start: 100, end: 200, workerId: 0 }])];
    const ev = { timestamp: 150, fields: { worker_id: "0" } };
    expect(ids(enclosingSpans(allSpans, ev))).toEqual(["A"]);
  });
});
