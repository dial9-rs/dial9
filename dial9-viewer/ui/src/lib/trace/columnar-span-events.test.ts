// Guards the columnar span-event sink's hot-path push: the base-field split and
// the span-id fast path were tuned for parse speed (spanKindOf + push run once
// per span event, the majority of events on a span-heavy trace), so this pins
// their semantics against regression.

import { describe, expect, it } from "vitest";
import { ColumnarSpanEvents, SPAN_KIND, spanKindOf } from "./columnar-span-events.js";
import type { DecodedFieldValue } from "../../../decode.js";
import type { SingleEventSpan } from "./index.js";

/** Read event i's non-base fields back into a plain record. */
function extrasOf(s: ColumnarSpanEvents, i: number): Record<string, DecodedFieldValue> {
  const out: Record<string, DecodedFieldValue> = {};
  for (let j = s.extraOff[i]!; j < s.extraOff[i + 1]!; j++) {
    const key = s.extraKeys[s.extraKeyId[j]!]!;
    const valId = s.extraValId[j]!;
    out[key] = valId === 0 ? null : s.extraVals[valId - 1]!;
  }
  return out;
}

const spanId = (s: ColumnarSpanEvents, i: number): string | undefined =>
  s.spanIdIdx[i]! < 0 ? undefined : s.strings[s.spanIdIdx[i]!];
const parentId = (s: ColumnarSpanEvents, i: number): string | undefined =>
  s.parentIdx[i]! < 0 ? undefined : s.strings[s.parentIdx[i]!];
const singleSpan = (overrides: Partial<SingleEventSpan> = {}): SingleEventSpan => ({
  start: 100,
  end: 600,
  name: "GetItem",
  spanType: "test-producer",
  threadId: 7,
  taskId: 42,
  workerId: null,
  fields: { Operation: "GetItem" },
  units: { Operation: "count" },
  ...overrides,
});

describe("spanKindOf", () => {
  it("classifies each span-event naming convention", () => {
    expect(spanKindOf("SpanEnter:foo")).toBe(SPAN_KIND.Enter);
    expect(spanKindOf("SpanEnter__foo")).toBe(SPAN_KIND.Enter);
    expect(spanKindOf("SpanEnterEvent")).toBe(SPAN_KIND.Enter);
    expect(spanKindOf("SpanExit:foo")).toBe(SPAN_KIND.Exit);
    expect(spanKindOf("SpanExitEvent")).toBe(SPAN_KIND.Exit);
    expect(spanKindOf("SpanClose__foo")).toBe(SPAN_KIND.Close);
    expect(spanKindOf("SpanCloseEvent")).toBe(SPAN_KIND.Close);
  });

  it("rejects non-span custom events", () => {
    expect(spanKindOf("ConnectionAcceptedWire")).toBeNull();
    expect(spanKindOf("ProcessResourceUsageEvent")).toBeNull();
    expect(spanKindOf("metrique:RequestMetrics")).toBeNull();
    expect(spanKindOf("")).toBeNull();
  });
});

describe("push base-field split", () => {
  it("keeps only non-base fields for an Enter event", () => {
    const s = new ColumnarSpanEvents();
    s.push(SPAN_KIND.Enter, 100, {
      worker_id: 3,
      task_id: 42,
      span_id: "s1",
      parent_span_id: "s0",
      span_name: "handle",
      request_id: "r1",
      route: "/api",
    });
    expect(spanId(s, 0)).toBe("s1");
    expect(parentId(s, 0)).toBe("s0");
    expect(s.taskId[0]).toBe(42);
    expect(extrasOf(s, 0)).toEqual({ request_id: "r1", route: "/api" });
  });

  it("treats parent_span_id as a NON-base field on an Exit event", () => {
    // Exit's base set omits parent_span_id, so a parent_span_id on an Exit
    // event is data, not metadata - the old Set encoded exactly this.
    const s = new ColumnarSpanEvents();
    s.push(SPAN_KIND.Exit, 200, {
      worker_id: 1,
      span_id: "s1",
      span_name: "handle",
      parent_span_id: "s0",
    });
    expect(spanId(s, 0)).toBe("s1");
    expect(extrasOf(s, 0)).toEqual({ parent_span_id: "s0" });
  });

  it("emits no extras when only base fields are present", () => {
    const s = new ColumnarSpanEvents();
    s.push(SPAN_KIND.Enter, 1, { worker_id: 0, span_id: "s", span_name: "n" });
    expect(extrasOf(s, 0)).toEqual({});
  });

  it("stores a normalized single-event span without physical-name coupling", () => {
    const s = new ColumnarSpanEvents();
    s.push(
      SPAN_KIND.Complete,
      600,
      { arbitrary_start: 100, arbitrary_task: 42 },
      singleSpan(),
    );
    expect(s.spanNameAt(0)).toBe("GetItem");
    expect(s.startAt(0)).toBe(100);
    expect(s.threadIdAt(0)).toBe(7);
    expect(s.taskIdAt(0)).toBe(42);
    expect(s.spanTypeAt(0)).toBe("test-producer");
    expect(extrasOf(s, 0)).toEqual({ Operation: "GetItem" });
    expect(s.extraUnitsAt(0)).toEqual({ Operation: "count" });
  });

  it("stores complete-only values sparsely", () => {
    const s = new ColumnarSpanEvents(2);
    s.push(SPAN_KIND.Enter, 1, { span_id: "a", span_name: "trace" });
    s.push(
      SPAN_KIND.Complete,
      2,
      {},
      singleSpan({ start: 1, end: 2 }),
    );
    s.push(SPAN_KIND.Exit, 3, { span_id: "a", span_name: "trace" });

    expect([...s.completeIdx.slice(0, 3)]).toEqual([-1, 0, -1]);
    expect(Number.isNaN(s.startAt(0))).toBe(true);
    expect(s.startAt(1)).toBe(1);
    expect(Number.isNaN(s.startAt(2))).toBe(true);
  });

  it("grows sparse complete-event columns past their initial capacity", () => {
    const s = new ColumnarSpanEvents(2);
    for (let i = 0; i < 1025; i++) {
      s.push(
        SPAN_KIND.Complete,
        i,
        {},
        singleSpan({
          start: i - 1,
          end: i,
          threadId: i + 2,
          taskId: i + 3,
        }),
      );
    }
    expect(s.completeIdx[1024]).toBe(1024);
    expect(s.startAt(1024)).toBe(1023);
    expect(s.threadIdAt(1024)).toBe(1026);
    expect(s.taskIdAt(1024)).toBe(1027);
  });

  it("uses the normalized name rather than inspecting attributes", () => {
    const s = new ColumnarSpanEvents();
    s.push(
      SPAN_KIND.Complete,
      100,
      {},
      singleSpan({
        name: "AnnotatedName",
        fields: {
          operation: "IgnoredLowercase",
          Operation: "IgnoredUppercase",
        },
      }),
    );
    expect(s.spanNameAt(0)).toBe("AnnotatedName");
  });
});

describe("span-id fast path", () => {
  it("interns a string id directly and dedupes a repeated id", () => {
    const s = new ColumnarSpanEvents();
    s.push(SPAN_KIND.Enter, 1, { span_id: "abc", span_name: "n" });
    s.push(SPAN_KIND.Exit, 2, { span_id: "abc", span_name: "n" });
    expect(spanId(s, 0)).toBe("abc");
    expect(spanId(s, 1)).toBe("abc");
    // Both events share one pooled string - the 6.4:1 dedup this relies on.
    expect(s.spanIdIdx[0]).toBe(s.spanIdIdx[1]);
  });

  it("still stringifies a numeric span id (the rare wire shape)", () => {
    const s = new ColumnarSpanEvents();
    s.push(SPAN_KIND.Enter, 1, { span_id: 42, parent_span_id: 7, span_name: "n" });
    expect(spanId(s, 0)).toBe("42");
    expect(parentId(s, 0)).toBe("7");
  });

  it("encodes an absent id as -1, not the string 'undefined'", () => {
    const s = new ColumnarSpanEvents();
    s.push(SPAN_KIND.Enter, 1, { span_name: "n" });
    expect(s.spanIdIdx[0]).toBe(-1);
    expect(s.parentIdx[0]).toBe(-1);
    expect(spanId(s, 0)).toBeUndefined();
  });
});
