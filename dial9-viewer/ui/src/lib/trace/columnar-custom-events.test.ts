// The columnar store has to hand back exactly what the fat array held: the
// parser writes to one or the other, so any divergence reaches consumers
// unnoticed. Parity runs over the demo trace, and the unit cases cover the
// wire types the demo trace happens not to exercise.

import { readFileSync } from "node:fs";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTrace } from "../../../trace_parser.js";
import type { ParsedTrace } from "../../../trace_parser.js";
import { ColumnarCustomEvents } from "./columnar-custom-events.js";
import { ColumnarEvents } from "./columnar-events.js";

const tracePath = new URL("../../../public/demo-trace.bin", import.meta.url);

/** Wire field types, mirroring decode.js FieldType. */
const F = {
  I64: 1, F64: 2, Bool: 3, String: 4, Bytes: 5, PooledStackFrames: 6,
  PooledString: 7, StackFrames: 8, Varint: 9, StringMap: 10, U8: 11,
  U16: 12, U32: 13, DynamicList: 14, DynamicMap: 15,
};
const OPTIONAL = 0x80;

describe("ColumnarCustomEvents parity with the fat array", () => {
  let fat: ParsedTrace;
  let columnar: ParsedTrace;
  let sink: ColumnarCustomEvents;

  beforeAll(async () => {
    const bytes = readFileSync(tracePath);
    fat = await parseTrace(bytes, { eventSink: new ColumnarEvents() } as never);
    sink = new ColumnarCustomEvents();
    columnar = await parseTrace(bytes, {
      eventSink: new ColumnarEvents(),
      customEventSink: sink,
    } as never);
  });

  it("holds the same events, in the same order", () => {
    expect(sink.length).toBe(fat.customEvents.length);
    expect(columnar.customEvents).toBe(sink);
  });

  it("every row round-trips field for field", () => {
    for (let i = 0; i < fat.customEvents.length; i++) {
      expect(sink.at(i)).toEqual(fat.customEvents.at(i));
    }
  });

  it("resolves units and fieldKinds from the schema", () => {
    const withUnits = fat.customEvents.filter((e) => e.units !== null);
    // The demo trace annotates at least one schema; without that this test
    // would pass vacuously.
    expect(withUnits.length).toBeGreaterThan(0);
    for (let i = 0; i < fat.customEvents.length; i++) {
      expect(sink.at(i)!.units).toEqual(fat.customEvents.at(i)!.units);
      expect(sink.at(i)!.fieldKinds).toEqual(fat.customEvents.at(i)!.fieldKinds);
    }
  });
});

describe("ColumnarCustomEvents column typing", () => {
  const schema = (fields: { name: string; fieldType: number }[]) => ({
    name: "S",
    fields,
    units: { a: "ns" },
    fieldKinds: null,
  });

  it("keeps u64 varints exact past 2^53", () => {
    const s = new ColumnarCustomEvents();
    const sch = schema([{ name: "v", fieldType: F.Varint }]);
    const big = "18446744073709551615"; // u64::MAX, unrepresentable as a double
    const small = "173801472";
    s.pushCustom("E", 1, { v: small }, sch, null);
    s.pushCustom("E", 2, { v: big }, sch, null);
    expect(s.at(0)!.fields["v"]).toBe(small);
    expect(s.at(1)!.fields["v"]).toBe(big);
  });

  it("reads i64 back as the BigInt the decoder produced", () => {
    const s = new ColumnarCustomEvents();
    const sch = schema([{ name: "v", fieldType: F.I64 }]);
    const values = [-5n, 0n, 7n, -(1n << 63n), (1n << 63n) - 1n];
    values.forEach((v, i) => s.pushCustom("E", i, { v }, sch, null));
    expect(values.map((_, i) => s.at(i)!.fields["v"])).toEqual(values);
  });

  it("round-trips each fixed-width numeric type", () => {
    const s = new ColumnarCustomEvents();
    const sch = schema([
      { name: "b", fieldType: F.Bool },
      { name: "u8", fieldType: F.U8 },
      { name: "u16", fieldType: F.U16 },
      { name: "u32", fieldType: F.U32 },
      { name: "f", fieldType: F.F64 },
    ]);
    s.pushCustom("E", 1, { b: true, u8: 255, u16: 65535, u32: 4294967295, f: 1.5 }, sch, null);
    s.pushCustom("E", 2, { b: false, u8: 0, u16: 0, u32: 0, f: 0 }, sch, null);
    expect(s.at(0)!.fields).toEqual({
      b: true, u8: 255, u16: 65535, u32: 4294967295, f: 1.5,
    });
    expect(s.at(1)!.fields["b"]).toBe(false);
  });

  it("interns strings and keeps the exotic types boxed", () => {
    const s = new ColumnarCustomEvents();
    const sch = schema([
      { name: "p", fieldType: F.PooledString },
      { name: "m", fieldType: F.StringMap },
      { name: "l", fieldType: F.StackFrames },
    ]);
    const map = { k: "v" };
    const frames = ["0x1", "0x2"];
    s.pushCustom("E", 1, { p: "repeated", m: map, l: frames }, sch, null);
    s.pushCustom("E", 2, { p: "repeated", m: map, l: frames }, sch, null);
    expect(s.at(0)!.fields["p"]).toBe("repeated");
    expect(s.at(1)!.fields["p"]).toBe("repeated");
    expect(s.at(0)!.fields["m"]).toEqual(map);
    expect(s.at(0)!.fields["l"]).toEqual(frames);
  });

  it("reports an absent optional as null, not as a zero", () => {
    const s = new ColumnarCustomEvents();
    const sch = schema([{ name: "opt", fieldType: F.U32 | OPTIONAL }]);
    s.pushCustom("E", 1, { opt: null }, sch, null);
    s.pushCustom("E", 2, { opt: 7 }, sch, null);
    expect(s.at(0)!.fields["opt"]).toBeNull();
    expect(s.at(1)!.fields["opt"]).toBe(7);
  });

  it("interleaves rows of different schemas without crossing their columns", () => {
    const s = new ColumnarCustomEvents();
    const a = schema([{ name: "x", fieldType: F.U32 }]);
    const b = schema([{ name: "y", fieldType: F.U8 }]);
    s.pushCustom("A", 1, { x: 100 }, a, null);
    s.pushCustom("B", 2, { y: 5 }, b, null);
    s.pushCustom("A", 3, { x: 200 }, a, null);
    expect(s.at(0)!.fields).toEqual({ x: 100 });
    expect(s.at(1)!.fields).toEqual({ y: 5 });
    expect(s.at(2)!.fields).toEqual({ x: 200 });
    expect([...s].map((e) => e.name)).toEqual(["A", "B", "A"]);
  });

  it("carries a row with no wire schema by name and timestamp alone", () => {
    const s = new ColumnarCustomEvents();
    s.pushCustom("E", 9, { ignored: 1 }, null, null);
    expect(s.at(0)).toEqual({
      name: "E", timestamp: 9, fields: {}, units: null,
      fieldKinds: null, singleEventSpan: null,
    });
  });
});
