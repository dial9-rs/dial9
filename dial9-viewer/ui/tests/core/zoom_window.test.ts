// Tests for the dial9-zoom-window skill's zoom.js.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

const zoomJsPath = fileURLToPath(
  new URL("../../../skills/dial9-zoom-window/scripts/zoom.js", import.meta.url),
);

interface Poll { start: number; end: number; taskId: number }
interface CustomEvent {
  name: string;
  timestamp: number;
  fields: Record<string, unknown>;
  singleEventSpan?: Record<string, unknown>;
}

const { innerSpanCounts } = require(zoomJsPath) as {
  innerSpanCounts: (
    trace: { customEvents: CustomEvent[] },
    workerSpans: Record<number, { polls: Poll[] }>,
    poll: { w: number; start: number; end: number },
  ) => Record<string, number> | null;
};

const TASK = 7;

// Span events as current producers write them: the owning task id, no worker.
function span(spanId: number, name: string, start: number, end: number): CustomEvent[] {
  const site = `app::${name}:src/main.rs:1`;
  return [
    {
      name: `SpanEnter:${site}`,
      timestamp: start,
      fields: { span_id: spanId, span_name: name, "dial9.tokio.task_id": TASK },
    },
    { name: `SpanExit:${site}`, timestamp: end, fields: { span_id: spanId } },
  ];
}

const polls: Poll[] = [
  { start: 1000, end: 2000, taskId: TASK },
  { start: 3000, end: 4000, taskId: TASK },
];
const dominant = { w: 0, start: 1000, end: 2000 };

describe("zoom.js inner spans", () => {
  it("counts the tracing spans that ran inside the poll", () => {
    const customEvents = [
      ...span(1, "parse", 1100, 1200),
      ...span(2, "parse", 1300, 1400),
      ...span(3, "encode", 1500, 1600),
      ...span(4, "later", 3100, 3200),
    ];
    expect(innerSpanCounts({ customEvents }, { 0: { polls } }, dominant)).toEqual({ parse: 2, encode: 1 });
  });

  it("counts a span held open across the poll", () => {
    const customEvents = span(1, "request", 500, 3500);
    expect(innerSpanCounts({ customEvents }, { 0: { polls } }, dominant)).toEqual({ request: 1 });
  });

  it("looks only at the poll's worker", () => {
    const customEvents = span(1, "parse", 1100, 1200);
    const onWorker1 = { 0: { polls: [] }, 1: { polls } };
    expect(innerSpanCounts({ customEvents }, onWorker1, dominant)).toEqual({});
    expect(innerSpanCounts({ customEvents }, onWorker1, { ...dominant, w: 1 })).toEqual({ parse: 1 });
  });

  it("leaves out single-event spans", () => {
    const record: CustomEvent = {
      name: "RequestMetrics",
      timestamp: 1100,
      fields: {},
      singleEventSpan: {
        name: "RequestMetrics", spanType: "metrique", fields: {}, units: {},
        start: 1100, end: 1200, taskId: TASK,
      },
    };
    const customEvents = [record, ...span(1, "parse", 1300, 1400)];
    expect(innerSpanCounts({ customEvents }, { 0: { polls } }, dominant)).toEqual({ parse: 1 });
    expect(innerSpanCounts({ customEvents: [record] }, { 0: { polls } }, dominant)).toBeNull();
  });
});
