import { describe, expect, it } from "vitest";
import { FieldType, TraceDecoder } from "../../../decode.js";

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

describe("TraceDecoder schema annotations", () => {
  it("accumulates unit and kind from separate annotation frames", () => {
    const name = utf8("Metric");
    const field = utf8("value");
    const bytes = Uint8Array.from([
      0x54, 0x52, 0x43, 0x00, 1,
      0x01,
      ...u16(TYPE_ID),
      ...u16(name.length),
      ...name,
      1,
      ...u16(1),
      ...u16(field.length),
      ...field,
      FieldType.Varint,
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
});
