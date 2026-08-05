import { describe, expect, it } from "vitest";
import { createRequire } from "node:module";
import { FieldType, TraceDecoder } from "../../../decode.js";

const require = createRequire(import.meta.url);
const { parseTrace } = require("../../../trace_parser.js") as {
  parseTrace: (bytes: Uint8Array, options?: unknown) => Promise<{
    customEvents: Array<{
      units: Record<string, string> | null;
      fieldKinds: Record<string, string> | null;
      singleEventSpan: {
        start: number;
        end: number;
        fields: Record<string, unknown>;
      } | null;
    }>;
  }>;
};

const TYPE_ID = 1;
const utf8 = (value: string): number[] => [
  ...new TextEncoder().encode(value),
];
const u16 = (value: number): number[] => [value & 0xff, value >>> 8];
const u32 = (value: number): number[] => [
  value & 0xff,
  (value >>> 8) & 0xff,
  (value >>> 16) & 0xff,
  value >>> 24,
];

function annotationFrame(key: string, value: string, fieldIndex = 0): number[] {
  const keyBytes = utf8(key);
  const valueBytes = utf8(value);
  return [
    0x06,
    TYPE_ID, // one-byte ULEB128
    ...u16(1),
    ...u16(fieldIndex),
    ...u16(keyBytes.length),
    ...keyBytes,
    ...u32(valueBytes.length),
    ...valueBytes,
  ];
}

function schemaFrame(): number[] {
  const name = utf8("Metric");
  const field = utf8("value");
  return [
    0x01,
    ...u16(TYPE_ID),
    ...u16(name.length),
    ...name,
    1,
    ...u16(1),
    ...u16(field.length),
    ...field,
    FieldType.Varint,
  ];
}

function durationSpanSchemaFrame(): number[] {
  const name = utf8("Metric");
  const field = utf8("duration");
  return [
    0x01,
    ...u16(TYPE_ID),
    ...u16(name.length),
    ...name,
    1,
    ...u16(1),
    ...u16(field.length),
    ...field,
    FieldType.Varint,
  ];
}

function timestampLessSpanSchemaFrame(): number[] {
  const name = utf8("Detached");
  const start = utf8("start");
  const duration = utf8("duration");
  return [
    0x01,
    ...u16(TYPE_ID),
    ...u16(name.length),
    ...name,
    0,
    ...u16(2),
    ...u16(start.length),
    ...start,
    FieldType.Varint,
    ...u16(duration.length),
    ...duration,
    FieldType.Varint,
  ];
}

describe("TraceDecoder schema annotations", () => {
  it("accumulates unit and kind from separate annotation frames", () => {
    const bytes = Uint8Array.from([
      0x54, 0x52, 0x43, 0x00, 1,
      ...schemaFrame(),
      ...annotationFrame("unit", "bytes"),
      ...annotationFrame("kind", "counter"),
    ]);
    const decoder = new TraceDecoder(bytes);

    expect(decoder.decodeHeader()).toBe(true);
    decoder.decodeAll();

    expect(decoder.schemas.get(TYPE_ID)).toMatchObject({
      annotations: [
        { fieldIndex: 0, key: "unit", value: "bytes" },
        { fieldIndex: 0, key: "kind", value: "counter" },
      ],
      units: { value: "bytes" },
      fieldKinds: { value: "counter" },
    });
  });

  it("keeps absent metadata nullable and attaches late annotations", async () => {
    const eventFrames = [
      0x54, 0x52, 0x43, 0x00, 1,
      ...schemaFrame(),
      0x02,
      ...u16(TYPE_ID),
      1, 0, 0,
      7,
    ];
    const unannotated = await parseTrace(Uint8Array.from(eventFrames));
    expect(unannotated.customEvents[0]).toMatchObject({
      units: null,
      fieldKinds: null,
    });

    const trace = await parseTrace(Uint8Array.from([
      ...eventFrames,
      ...annotationFrame("unit", "bytes"),
      ...annotationFrame("kind", "counter"),
    ]));

    expect(trace.customEvents).toHaveLength(1);
    expect(trace.customEvents[0]).toMatchObject({
      units: { value: "bytes" },
      fieldKinds: { value: "counter" },
    });
  });

  it("does NOT reclassify an event when its span-role annotation trails the event", async () => {
    // The wire format requires span-role (`dial9.role`) annotations to precede
    // any event of their type (docs/design/single-event-spans.md). A role frame
    // that arrives AFTER the event is a malformed trace: the decoder classifies
    // spans in a single pass at decode time and does not re-resolve later, so
    // the event stays an ordinary custom event. (Metadata annotations like
    // unit/kind may still trail — that is exercised separately above.)
    const bytes = Uint8Array.from([
      0x54, 0x52, 0x43, 0x00, 1,
      ...durationSpanSchemaFrame(),
      ...annotationFrame("unit", "ns"),
      0x02,
      ...u16(TYPE_ID),
      10, 0, 0,
      4,
      ...annotationFrame("dial9.role", "span.duration"),
    ]);

    const trace = await parseTrace(bytes);
    expect(trace.customEvents).toHaveLength(1);
    expect(trace.customEvents[0]!.singleEventSpan).toBeNull();
    // The trailing metadata annotation still attaches (metadata may follow).
    expect(trace.customEvents[0]).toMatchObject({ units: { duration: "ns" } });

    // Columnar path: with no span recognized at decode time, nothing routes to
    // the sink and the event stays fat.
    const projected: unknown[] = [];
    const spanEventSink = {
      pushIfSpan(
        _name: string,
        _timestamp: number,
        _fields: Record<string, unknown>,
        span: unknown,
      ): boolean {
        if (span == null) return false;
        projected.push(span);
        return true;
      },
    };
    const columnarTrace = await parseTrace(bytes, { spanEventSink });
    expect(columnarTrace.customEvents).toHaveLength(1);
    expect(projected).toEqual([]);
  });

  it("routes a timestamp-less span using its projected end", async () => {
    // Annotations precede the event, as the format requires.
    const bytes = Uint8Array.from([
      0x54, 0x52, 0x43, 0x00, 1,
      ...timestampLessSpanSchemaFrame(),
      ...annotationFrame("dial9.role", "span.start"),
      ...annotationFrame("dial9.role", "span.duration", 1),
      0x02,
      ...u16(TYPE_ID),
      30,
      5,
    ]);
    const projectedTimestamps: number[] = [];
    const spanEventSink = {
      pushIfSpan(
        _name: string,
        timestamp: number,
        _fields: Record<string, unknown>,
        span: unknown,
      ): boolean {
        if (span == null) return false;
        projectedTimestamps.push(timestamp);
        return true;
      },
    };

    const trace = await parseTrace(bytes, { spanEventSink });

    expect(trace.customEvents).toHaveLength(0);
    expect(projectedTimestamps).toEqual([35]);
  });
});
