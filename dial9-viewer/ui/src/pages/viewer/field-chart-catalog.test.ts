import { describe, expect, it } from "vitest";
import type { CustomTraceEvent } from "../../lib/trace/index.js";
import {
  buildFieldChartCatalog,
  fieldChartCatalogSize,
  filterFieldChartCatalog,
} from "./field-chart-catalog.js";

function event(
  name: string,
  fields: CustomTraceEvent["fields"],
  fieldKinds: Record<string, string> | null = null,
): CustomTraceEvent {
  return { name, timestamp: 1, fields, units: null, fieldKinds };
}

describe("buildFieldChartCatalog", () => {
  it("puts annotated sources first and groups other numeric fields by event", () => {
    const catalog = buildFieldChartCatalog([
      event("Request", { task_id: 4, latency: 12 }, { latency: "gauge" }),
      event("Queue", { depth: 3, state: "ready" }),
      event("Request", { task_id: 5, latency: 20 }, { latency: "gauge" }),
    ]);

    expect(catalog).toEqual({
      annotated: [
        {
          eventName: "Request",
          fields: [
            { eventName: "Request", fieldName: "latency", kind: "gauge" },
          ],
        },
      ],
      other: [
        {
          eventName: "Queue",
          fields: [{ eventName: "Queue", fieldName: "depth", kind: null }],
        },
        {
          eventName: "Request",
          fields: [
            { eventName: "Request", fieldName: "task_id", kind: null },
          ],
        },
      ],
    });
    expect(fieldChartCatalogSize(catalog)).toBe(3);
  });

  it("keeps an annotated optional field even when this trace has no value", () => {
    expect(
      buildFieldChartCatalog([
        event("Request", {}, { latency: "counter" }),
      ]).annotated,
    ).toEqual([
      {
        eventName: "Request",
        fields: [
          { eventName: "Request", fieldName: "latency", kind: "counter" },
        ],
      },
    ]);
  });

  it("does not guess conflicting annotations", () => {
    const catalog = buildFieldChartCatalog([
      event("Metric", { value: 1 }, { value: "gauge" }),
      event("Metric", { value: 2 }, { value: "counter" }),
    ]);

    expect(catalog.annotated).toEqual([]);
    expect(catalog.other).toEqual([
      {
        eventName: "Metric",
        fields: [{ eventName: "Metric", fieldName: "value", kind: null }],
      },
    ]);
  });

  it("rejects non-numeric fields, unknown annotations, and unsupported names", () => {
    expect(
      buildFieldChartCatalog([
        event("Event", { state: "ready", unknown: null }, { unknown: "rate" }),
        event("Bad,Event", { value: 1 }, { value: "gauge" }),
        event("Good", { "bad,field": 2 }),
      ]),
    ).toEqual({ annotated: [], other: [] });
  });

  it("sorts and deduplicates sources independently of event order", () => {
    const catalog = buildFieldChartCatalog([
      event("Zulu", { z: 1, a: 2 }),
      event("Alpha", { b: 3 }),
      event("Zulu", { a: 4 }),
    ]);

    expect(catalog.other.map((group) => group.eventName)).toEqual([
      "Alpha",
      "Zulu",
    ]);
    expect(catalog.other[1]?.fields.map((field) => field.fieldName)).toEqual([
      "a",
      "z",
    ]);
  });
});

describe("filterFieldChartCatalog", () => {
  const catalog = buildFieldChartCatalog([
    event("HttpRequest", { latency_us: 1, status: 200 }, { latency_us: "gauge" }),
    event("QueueDepth", { depth: 3 }),
  ]);

  it("matches an event name and keeps all of that event's fields", () => {
    const filtered = filterFieldChartCatalog(catalog, "http");
    expect(fieldChartCatalogSize(filtered)).toBe(2);
    expect(filtered.other[0]?.fields[0]?.fieldName).toBe("status");
  });

  it("matches individual fields case-insensitively", () => {
    const filtered = filterFieldChartCatalog(catalog, "DEPTH");
    expect(filtered.annotated).toEqual([]);
    expect(filtered.other).toEqual([
      {
        eventName: "QueueDepth",
        fields: [
          { eventName: "QueueDepth", fieldName: "depth", kind: null },
        ],
      },
    ]);
  });

  it("returns the existing catalogue for an empty filter", () => {
    expect(filterFieldChartCatalog(catalog, "   ")).toBe(catalog);
  });
});
