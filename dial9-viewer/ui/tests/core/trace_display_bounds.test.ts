import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";
import { ColumnarEvents } from "../../src/lib/trace/columnar-events.js";
import { ColumnarSpanEvents } from "../../src/lib/trace/columnar-span-events.js";
import { ColumnarTaskDumps } from "../../src/lib/trace/columnar-task-dumps.js";
import { traceDisplayBounds } from "../../src/pages/viewer/trace-bounds.js";

const require = createRequire(import.meta.url);
const { parseTrace, parseTraceStream } = require("../../trace_parser.js") as
  typeof import("../../trace_parser.js");
const { FieldType } = require("../../decode.js") as typeof import("../../decode.js");

const u16 = (value: number): number[] => [value & 255, value >>> 8];
const text = (value: string): number[] => [...Buffer.from(value)];
function u64(value: number): number[] {
  const bytes = Buffer.alloc(8);
  bytes.writeBigUInt64LE(BigInt(value));
  return [...bytes];
}

function schema(
  id: number,
  name: string,
  fields: [name: string, type: number][],
): number[] {
  return [
    1, ...u16(id), ...u16(text(name).length), ...text(name), 1,
    ...u16(fields.length),
    ...fields.flatMap(([field, type]) => [
      ...u16(text(field).length), ...text(field), type,
    ]),
  ];
}

function event(id: number, timestamp: number, payload: number[] = []): number[] {
  // Reset before each event to exercise out-of-order captures and long gaps.
  return [5, ...u64(timestamp), 2, ...u16(id), 0, 0, 0, ...payload];
}

function annotation(id: number, key: string, value: string): number[] {
  const length = Buffer.alloc(4);
  length.writeUInt32LE(text(value).length);
  return [
    6, id, ...u16(1), ...u16(0),
    ...u16(text(key).length), ...text(key), ...length, ...text(value),
  ];
}

function traceBytes(frames: number[]): Uint8Array {
  return Uint8Array.from([
    0x54, 0x52, 0x43, 0, 1,
    ...schema(1, "QueueSampleEvent", [["global_queue", FieldType.Varint]]),
    ...schema(2, "TaskDumpEvent", [
      ["task_id", FieldType.Varint], ["callchain", FieldType.StackFrames],
    ]),
    ...schema(3, "CustomEvent", []),
    ...frames,
  ]);
}

function dump(timestamp: number): number[] {
  return event(2, timestamp, [7, 1, 0, 0, 0, ...u64(0x1234)]);
}

async function* chunks(bytes: Uint8Array): AsyncIterable<Uint8Array> {
  for (let offset = 0; offset < bytes.length; offset += 7) {
    yield bytes.subarray(offset, offset + 7);
  }
}

describe("task dumps and trace display bounds", () => {
  it.each(["buffered", "streamed", "columnar"] as const)(
    "keeps a 10s frame despite captures before and after it (%s)",
    async (mode) => {
      const bytes = traceBytes([
        ...event(1, 1e9, [0]),
        ...event(1, 11e9, [0]),
        ...dump(19.7e9),
        ...dump(0.5e9),
      ]);
      const trace = mode === "streamed"
        ? await parseTraceStream(chunks(bytes))
        : await parseTrace(bytes, mode === "columnar" ? {
          eventSink: new ColumnarEvents(),
          taskDumpSink: new ColumnarTaskDumps(),
        } : {});

      expect(traceDisplayBounds(trace)).toEqual({ minTs: 1e9, maxTs: 11e9 });
      expect(trace.recordMinTs).toBe(0.5e9);
      expect(trace.recordMaxTs).toBe(19.7e9);
      expect(trace.taskDumps.get(7)).toEqual([
        { timestamp: 0.5e9, callchain: ["0x1234"] },
        { timestamp: 19.7e9, callchain: ["0x1234"] },
      ]);
    },
  );

  it("still includes custom events beyond runtime bounds", async () => {
    const trace = await parseTrace(traceBytes([
      ...event(1, 100, [0]),
      ...event(1, 200, [0]),
      ...event(3, 50),
      ...event(3, 300),
      ...dump(400),
    ]));
    expect(traceDisplayBounds(trace)).toEqual({ minTs: 50, maxTs: 300 });
  });

  it.each([false, true])(
    "preserves projected span bounds and clips them to a time selection (columnar=%s)",
    async (columnar) => {
      const bytes = traceBytes([
        ...schema(4, "CompletedSpan", [["duration", FieldType.I64]]),
        ...annotation(4, "dial9.role", "span.duration"),
        ...event(1, 100, [0]),
        ...event(1, 200, [0]),
        ...event(4, 300, u64(250)),
        ...dump(400),
      ]);
      const options = columnar ? { spanEventSink: new ColumnarSpanEvents() } : {};
      const trace = await parseTrace(bytes, options);
      expect(traceDisplayBounds(trace)).toEqual({ minTs: 50, maxTs: 300 });
      const filtered = await parseTrace(bytes, {
        ...options,
        ...(columnar ? { spanEventSink: new ColumnarSpanEvents() } : {}),
        startTime: 150,
        endTime: 250,
      });
      expect(traceDisplayBounds(filtered)).toEqual({ minTs: 150, maxTs: 250 });
    },
  );

  it("keeps out-of-range dumps when reparsing a time selection", async () => {
    const trace = await parseTrace(traceBytes([
      ...event(1, 100, [0]),
      ...event(1, 200, [0]),
      ...dump(300),
    ]), { startTime: 100, endTime: 200 });
    expect(traceDisplayBounds(trace)).toEqual({ minTs: 100, maxTs: 200 });
    expect(trace.taskDumps.get(7)?.[0]?.timestamp).toBe(300);
  });

  it("does not create a timeline from task dumps alone", async () => {
    const trace = await parseTrace(traceBytes([...dump(100), ...dump(200)]));
    expect(traceDisplayBounds(trace)).toBeNull();
    expect(trace.taskDumps.get(7)).toHaveLength(2);
  });
});
