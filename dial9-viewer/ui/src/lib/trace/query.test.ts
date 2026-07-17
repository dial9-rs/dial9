// query.ts tests: the poll-at-timestamp binary search, task lookup,
// containing-span scan, and cycle-guarded ancestor walk.

import { describe, expect, it } from "vitest";
import type { PollSpan, TracingSpan } from "../../../trace_analysis.js";
import {
  SPAN_ANCESTRY_CYCLE_LIMIT,
  findContainingSpan,
  findSpanAt,
  spanAncestryAt,
  spansById,
  taskAt,
} from "./query.js";
import { ColumnarSpansBuilder } from "./columnar-spans.js";

function poll(start: number, end: number, taskId: number): PollSpan {
  return { start, end, taskId, spawnLocId: null, spawnLoc: null };
}

/** Pack fat spans into a ColumnarSpans store (start-sorted by finish()). */
function columnarize(spans: TracingSpan[]) {
  const b = new ColumnarSpansBuilder();
  const parents = new Map<string, string | null>();
  for (const s of spans) {
    parents.set(s.spanId, s.parentSpanId);
    b.push(s.start, s.end, s.spanId, s.spanName, s.parentSpanId, s.taskId, s.activeNs, s.segments, s.fields);
  }
  return b.finish(parents).store;
}

function span(
  spanId: string,
  start: number,
  end: number,
  parentSpanId: string | null,
  segments: { start: number; end: number; workerId: number }[]
): TracingSpan {
  return {
    spanId,
    start,
    end,
    parentSpanId,
    segments,
    spanName: spanId,
    fields: {},
    activeNs: end - start,
    depth: 0,
    taskId: null,
  };
}

describe("findSpanAt", () => {
  const spans = [poll(0, 10, 1), poll(20, 30, 2), poll(40, 50, 3)];

  it("finds the containing span, inclusive bounds", () => {
    expect(findSpanAt(spans, 25)?.taskId).toBe(2);
    expect(findSpanAt(spans, 20)?.taskId).toBe(2);
    expect(findSpanAt(spans, 30)?.taskId).toBe(2);
  });

  it("returns null between spans and outside the range", () => {
    expect(findSpanAt(spans, 15)).toBeNull();
    expect(findSpanAt(spans, 55)).toBeNull();
    expect(findSpanAt([], 5)).toBeNull();
  });
});

describe("taskAt", () => {
  it("returns the poll and its task id", () => {
    const hit = taskAt([poll(0, 10, 42)], 5);
    expect(hit?.taskId).toBe(42);
    expect(hit?.poll.start).toBe(0);
  });

  it("taskId 0 (no task tracking) surfaces as null, poll still returned", () => {
    const hit = taskAt([poll(0, 10, 0)], 5);
    expect(hit).not.toBeNull();
    expect(hit?.taskId).toBeNull();
  });

  it("no poll at timestamp -> null", () => {
    expect(taskAt([poll(0, 10, 1)], 15)).toBeNull();
  });
});

describe("findContainingSpan", () => {
  const a = span("a", 0, 100, null, [{ start: 0, end: 100, workerId: 0 }]);
  const b = span("b", 10, 60, "a", [
    { start: 10, end: 30, workerId: 1 },
    { start: 40, end: 60, workerId: 0 },
  ]);
  const all = [a, b];

  it("matches only spans with a segment executing on the worker at ns", () => {
    expect(findContainingSpan(all, 1, 20)?.spanId).toBe("b");
    // At ns=20, b's worker-0 segment doesn't cover it, but a's does.
    expect(findContainingSpan(all, 0, 20)?.spanId).toBe("a");
    // At ns=50, b executes on worker 0.
    expect(findContainingSpan([b], 0, 50)?.spanId).toBe("b");
  });

  it("returns null when no segment matches", () => {
    expect(findContainingSpan(all, 2, 20)).toBeNull();
    expect(findContainingSpan(all, 0, 200)).toBeNull();
  });

  it("columnar path (windowRows-bounded) matches the fat path", () => {
    // Include a far-right span so the pre-bound version would have scanned from
    // row 0; the bounded version must still find it and skip the rest.
    const far = span("far", 10_000, 10_100, null, [{ start: 10_000, end: 10_100, workerId: 3 }]);
    const cs = columnarize([a, b, far]);
    for (const [wid, ns] of [[1, 20], [0, 20], [0, 50], [2, 20], [3, 10_050], [3, 10_050]] as const) {
      const col = findContainingSpan([], wid, ns, cs)?.spanId ?? null;
      const fat = findContainingSpan([a, b, far], wid, ns)?.spanId ?? null;
      expect(col, `worker ${wid} @ ${ns}`).toBe(fat);
    }
  });
});

describe("spanAncestryAt", () => {
  it("walks to the outermost ancestor containing ns, collecting ids", () => {
    const root = span("root", 0, 100, null, []);
    const mid = span("mid", 10, 90, "root", []);
    const leaf = span("leaf", 20, 30, "mid", []);
    const byId = spansById([root, mid, leaf]);
    const { outermost, ids } = spanAncestryAt(leaf, byId, 25);
    expect(outermost.spanId).toBe("root");
    expect([...ids].sort()).toEqual(["leaf", "mid", "root"]);
  });

  it("stops at an ancestor that does not contain ns", () => {
    const root = span("root", 0, 22, null, []);
    const leaf = span("leaf", 20, 30, "root", []);
    const byId = spansById([root, leaf]);
    // ns=25 is inside leaf but outside root: focus stays on leaf.
    const { outermost, ids } = spanAncestryAt(leaf, byId, 25);
    expect(outermost.spanId).toBe("leaf");
    expect(ids).toEqual(new Set(["leaf"]));
  });

  it("stops at a missing parent", () => {
    const leaf = span("leaf", 20, 30, "gone", []);
    const { outermost, ids } = spanAncestryAt(leaf, spansById([leaf]), 25);
    expect(outermost.spanId).toBe("leaf");
    expect(ids).toEqual(new Set(["leaf"]));
  });

  it("caps a distinct-ancestor chain at the 1024-step guard (legacy parity)", () => {
    // A straight chain longer than the limit: both the legacy size guard
    // and the ported step guard stop after 1024 ancestor hops, so the id
    // set holds the start span + 1024 ancestors.
    const n = SPAN_ANCESTRY_CYCLE_LIMIT + 10;
    const chain: TracingSpan[] = [];
    for (let i = 0; i < n; i++) {
      chain.push(span(`s${i}`, 0, 100, i + 1 < n ? `s${i + 1}` : null, []));
    }
    const byId = spansById(chain);
    const { ids } = spanAncestryAt(chain[0]!, byId, 50);
    expect(ids.size).toBe(SPAN_ANCESTRY_CYCLE_LIMIT + 1);
  });

  it("terminates on a two-node parent cycle (legacy hung here)", () => {
    // The legacy guard watched the id SET's size, which stops growing the
    // moment a cycle revisits a span - a 2-cycle looped forever. The port
    // counts steps, so this returns instead of hanging.
    const x = span("x", 0, 100, "y", []);
    const y = span("y", 0, 100, "x", []);
    const byId = spansById([x, y]);
    const { outermost, ids } = spanAncestryAt(x, byId, 50);
    expect(["x", "y"]).toContain(outermost.spanId);
    expect(ids).toEqual(new Set(["x", "y"]));
  });
});
