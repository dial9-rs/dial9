// Unit tests for single-event span schema compilation and timing resolution in
// trace_parser.js.
//
// A span has three timing quantities — start, duration, end — related by
// `end = start + duration`. A schema needs any TWO; the decoder derives the
// third. The packed event timestamp counts as an end. These tests pin that
// matrix and the validation rules from `docs/design/single-event-spans.md`,
// and they mirror the Rust-side tests in
// `dial9-viewer/src/ingest/decode/events.rs` so the two decoders cannot drift.

/* eslint-disable @typescript-eslint/no-explicit-any */

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

const { compileSingleEventSpanSchema, resolveSpanTiming } = require(
  "../../trace_parser.js",
) as {
  compileSingleEventSpanSchema: (schema: any) => any;
  resolveSpanTiming: (
    timing: any,
    values: Record<string, unknown>,
    packedEnd: number | null,
  ) => { start: number; end: number } | null;
};

const ROLE = "dial9.role";
const SPAN_TYPE_KEY = "dial9.span.type";
const VARINT = 1;
const STRING = 4;

interface Field {
  name: string;
  fieldType: number;
}
interface Annotation {
  fieldIndex: number;
  key: string;
  value: string;
}

/** Build a minimal schema object shaped like the parser's decoded schema. */
function schema(
  name: string,
  fields: Array<[string, number]>,
  annotations: Annotation[],
  hasTimestamp = true,
): any {
  return {
    name,
    hasTimestamp,
    fields: fields.map(([n, fieldType]): Field => ({ name: n, fieldType })),
    annotations,
  };
}

const ann = (fieldIndex: number, key: string, value: string): Annotation => ({
  fieldIndex,
  key,
  value,
});

describe("single-event span schema compilation", () => {
  it("treats a schema with no timing role as an ordinary custom event", () => {
    // No span.start/duration/end anywhere: not a span, and not an error.
    const result = compileSingleEventSpanSchema(
      schema("app:Plain", [["count", VARINT]], []),
    );
    expect(result.kind).toBe("not-span");
  });

  it("does not recognize spans by the metrique: schema prefix alone", () => {
    // The spec is explicit: decoders must not match on the schema name.
    const result = compileSingleEventSpanSchema(
      schema("metrique:Unannotated", [["Latency", VARINT]], []),
    );
    expect(result.kind).toBe("not-span");
  });

  it("compiles span.duration against the packed end timestamp", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "metrique:Work",
        [
          ["dur", VARINT],
          ["display", STRING],
        ],
        [
          ann(0, ROLE, "span.duration"),
          ann(0, "unit", "ns"),
          ann(0, SPAN_TYPE_KEY, "metrique"),
          ann(1, ROLE, "span.name"),
        ],
      ),
    );
    expect(result.kind).toBe("layout");
    expect(result.timing.duration).toEqual({ field: "dur", multiplier: 1 });
    expect(result.timing.start).toBeNull();
    expect(result.timing.packedEnd).toBe(true);
    expect(result.spanType).toBe("metrique");
    expect(result.nameField).toBe("display");
  });

  it("compiles span.start against the packed end timestamp", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:Legacy",
        [["begin", VARINT]],
        [ann(0, ROLE, "span.start"), ann(0, "unit", "ms")],
      ),
    );
    expect(result.kind).toBe("layout");
    expect(result.timing.start).toEqual({ field: "begin", multiplier: 1_000_000 });
    expect(result.timing.duration).toBeNull();
    // No dial9.span.type annotation -> the documented default.
    expect(result.spanType).toBe("single-event");
  });

  it("compiles an explicit span.end field", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:ExplicitEnd",
        [
          ["start", VARINT],
          ["finish", VARINT],
        ],
        [ann(0, ROLE, "span.start"), ann(1, ROLE, "span.end")],
      ),
    );
    expect(result.kind).toBe("layout");
    expect(result.timing.start).toEqual({ field: "start", multiplier: 1 });
    expect(result.timing.end).toEqual({ field: "finish", multiplier: 1 });
  });

  it("accepts start + duration with no packed end timestamp", () => {
    // Two explicit quantities are enough even when nothing packs an end.
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:StartDur",
        [
          ["start", VARINT],
          ["dur", VARINT],
        ],
        [ann(0, ROLE, "span.start"), ann(1, ROLE, "span.duration")],
        /* hasTimestamp */ false,
      ),
    );
    expect(result.kind).toBe("layout");
    expect(result.timing.packedEnd).toBe(false);
  });

  it("rejects a lone duration with no end available", () => {
    // One quantity cannot place a span.
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:OnlyDuration",
        [["dur", VARINT]],
        [ann(0, ROLE, "span.duration")],
        /* hasTimestamp */ false,
      ),
    );
    expect(result.kind).toBe("invalid");
  });

  it("defaults an absent unit annotation to nanoseconds", () => {
    const result = compileSingleEventSpanSchema(
      schema("producer:NoUnit", [["dur", VARINT]], [ann(0, ROLE, "span.duration")]),
    );
    expect(result.kind).toBe("layout");
    expect(result.timing.duration.multiplier).toBe(1);
  });

  it("rejects an unsupported unit", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:BadUnit",
        [["dur", VARINT]],
        [ann(0, ROLE, "span.duration"), ann(0, "unit", "fortnights")],
      ),
    );
    expect(result.kind).toBe("invalid");
  });

  it("rejects a duplicate timing role", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:DupDuration",
        [
          ["a", VARINT],
          ["b", VARINT],
        ],
        [ann(0, ROLE, "span.duration"), ann(1, ROLE, "span.duration")],
      ),
    );
    expect(result.kind).toBe("invalid");
  });

  it("rejects a non-integer timing field", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:StringDuration",
        [["dur", STRING]],
        [ann(0, ROLE, "span.duration")],
      ),
    );
    expect(result.kind).toBe("invalid");
  });

  it("rejects a non-string span.name field", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:IntName",
        [
          ["dur", VARINT],
          ["label", VARINT],
        ],
        [ann(0, ROLE, "span.duration"), ann(1, ROLE, "span.name")],
      ),
    );
    expect(result.kind).toBe("invalid");
  });

  it("ignores an unknown role for forward compatibility", () => {
    // An unknown role must not invalidate an otherwise-good span schema.
    const result = compileSingleEventSpanSchema(
      schema(
        "producer:FutureRole",
        [
          ["dur", VARINT],
          ["mystery", VARINT],
        ],
        [ann(0, ROLE, "span.duration"), ann(1, ROLE, "span.cost_center")],
      ),
    );
    expect(result.kind).toBe("layout");
    // The unknown-role field stays an ordinary attribute.
    expect(result.attributeFields).toContain("mystery");
  });

  it("keeps structural fields out of attributes but keeps span.name in", () => {
    const result = compileSingleEventSpanSchema(
      schema(
        "metrique:Req",
        [
          ["dur", VARINT],
          ["tid", VARINT],
          ["task", VARINT],
          ["Operation", STRING],
          ["MetricName", STRING],
        ],
        [
          ann(0, ROLE, "span.duration"),
          ann(1, ROLE, "thread_id"),
          ann(2, ROLE, "tokio.task_id"),
          ann(3, ROLE, "span.name"),
        ],
      ),
    );
    expect(result.kind).toBe("layout");
    expect(result.attributeFields).not.toContain("dur");
    expect(result.attributeFields).not.toContain("tid");
    expect(result.attributeFields).not.toContain("task");
    expect(result.attributeFields).toContain("MetricName");
    // span.name is the documented exception: it stays a normal attribute too.
    expect(result.nameField).toBe("Operation");
  });
});

describe("single-event span timing resolution", () => {
  const field = (name: string, multiplier = 1) => ({ field: name, multiplier });

  it("derives start from the packed end and a duration", () => {
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: 120 }, 500)).toEqual({
      start: 380,
      end: 500,
    });
  });

  it("saturates start to zero when duration exceeds the end", () => {
    // Duration is unsigned, so start > end is unrepresentable: clamp, don't
    // go negative. Matches the Rust decoder's saturating_sub.
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: 999 }, 100)).toEqual({
      start: 0,
      end: 100,
    });
  });

  it("treats a zero duration as a valid instantaneous span", () => {
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: 0 }, 700)).toEqual({
      start: 700,
      end: 700,
    });
  });

  it("uses the packed end with an explicit start", () => {
    const timing = {
      start: field("begin"),
      duration: null,
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { begin: 300 }, 450)).toEqual({
      start: 300,
      end: 450,
    });
  });

  it("prefers an explicit span.end field over the packed timestamp", () => {
    const timing = {
      start: field("begin"),
      duration: null,
      end: field("finish"),
      packedEnd: true,
    };
    // Packed end is 9999 and must be ignored in favor of the field.
    expect(resolveSpanTiming(timing, { begin: 300, finish: 450 }, 9999)).toEqual({
      start: 300,
      end: 450,
    });
  });

  it("derives end from start + duration with no end available", () => {
    const timing = {
      start: field("begin"),
      duration: field("dur"),
      end: null,
      packedEnd: false,
    };
    expect(resolveSpanTiming(timing, { begin: 300, dur: 25 }, null)).toEqual({
      start: 300,
      end: 325,
    });
  });

  it("scales a timing field by its declared unit", () => {
    const timing = {
      start: null,
      duration: field("dur", 1_000_000), // ms
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: 2 }, 10_000_000)).toEqual({
      start: 8_000_000,
      end: 10_000_000,
    });
  });

  it("rejects a start after its end", () => {
    const timing = {
      start: field("begin"),
      duration: null,
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { begin: 900 }, 100)).toBeNull();
  });

  it("rejects a negative timing value", () => {
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: -5 }, 500)).toBeNull();
  });

  it("rejects a non-numeric timing value", () => {
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: "banana" }, 500)).toBeNull();
  });

  it("returns null when an optional timing value is absent at runtime", () => {
    // The schema promised two quantities but this event carries only one.
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, {}, 500)).toBeNull();
  });

  it("returns null when no end is available at runtime", () => {
    const timing = {
      start: null,
      duration: field("dur"),
      end: field("finish"),
      packedEnd: false,
    };
    expect(resolveSpanTiming(timing, { dur: 10 }, null)).toBeNull();
  });

  it("reads a bigint timing value", () => {
    // Varint fields decode to BigInt for large values; Number() must apply.
    const timing = {
      start: null,
      duration: field("dur"),
      end: null,
      packedEnd: true,
    };
    expect(resolveSpanTiming(timing, { dur: 120n }, 500)).toEqual({
      start: 380,
      end: 500,
    });
  });
});
