import { describe, expect, it } from "vitest";
import { createRequire } from "node:module";
import { FieldType, TraceDecoder } from "../../../decode.js";

const require = createRequire(import.meta.url);
const { parseTrace } = require("../../../trace_parser.js") as {
  parseTrace: (bytes: Uint8Array) => Promise<{
    customEvents: Array<{
      units: Record<string, string> | null;
      fieldKinds: Record<string, string> | null;
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

function annotationFrame(key: string, value: string): number[] {
  const keyBytes = utf8(key);
  const valueBytes = utf8(value);
  return [
    0x06,
    TYPE_ID, // one-byte ULEB128
    ...u16(1),
    ...u16(0),
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
});
