// The metrics.rs source, end to end through the viewer's own parser and chart
// machinery. Asserts that the viewer picks those events up and charts them

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { buildFieldChartCatalog } from "../../src/pages/viewer/field-chart-catalog.js";
import { materializeFieldChartSeries } from "../../src/pages/viewer/field-chart-model.js";

const require = createRequire(import.meta.url);
const { parseTrace } = require("../../trace_parser.js") as {
  parseTrace: (bytes: Buffer) => Promise<{ customEvents?: unknown[] }>;
};

const fixture = fileURLToPath(
  new URL("../../test-traces/metrics-rs.bin", import.meta.url),
);

// eslint-disable-next-line @typescript-eslint/no-explicit-any
let events: any[] = [];

beforeAll(async () => {
  const trace = await parseTrace(readFileSync(fixture));
  events = (trace.customEvents ?? []) as any[];
});

function sourcesFor(eventName: string) {
  const catalog = buildFieldChartCatalog(events);
  return catalog.annotated.find((group) => group.eventName === eventName);
}

describe("metrics.rs source", () => {
  it("puts every metric in the catalogue as its own annotated group", () => {
    const catalog = buildFieldChartCatalog(events);
    const names = catalog.annotated
      .map((group) => group.eventName)
      .filter((name) => name.startsWith("metricsrs:"))
      .sort();

    expect(names).toEqual([
      "metricsrs:bytes_read",
      "metricsrs:errors{kind=timeout}",
      "metricsrs:pool_available",
      "metricsrs:query_ms",
      "metricsrs:requests_total{route=/owners}",
      "metricsrs:requests_total{route=/pets}",
    ]);
  });

  it("labels and braces survive as an addressable event name", () => {
    // The catalogue rejects names it cannot put in a deep link. Braces, `=`
    // and `/` are all fine; only commas are not, which is why labels join
    // with `;`.
    expect(sourcesFor("metricsrs:requests_total{route=/pets}")).toBeDefined();
  });

  it("carries the kind annotation so charting needs no user input", () => {
    const counter = sourcesFor("metricsrs:requests_total{route=/pets}");
    expect(counter?.fields).toEqual([
      { eventName: counter!.eventName, fieldName: "value", kind: "counter" },
    ]);

    const gauge = sourcesFor("metricsrs:pool_available");
    expect(gauge?.fields[0]?.kind).toBe("gauge");
  });

  it("groups a histogram's fields under one pickable source", () => {
    const histogram = sourcesFor("metricsrs:query_ms");
    const byName = Object.fromEntries(
      (histogram?.fields ?? []).map((field) => [field.fieldName, field.kind]),
    );

    expect(byName).toEqual({
      count: "counter",
      sum: "counter",
      p50: "gauge",
      p90: "gauge",
      p99: "gauge",
    });
  });

  it("charts a counter as per-interval deltas, not the running total", () => {
    // The source writes cumulative values precisely so the viewer can do this.
    const series = materializeFieldChartSeries(events, {
      id: "fc1",
      eventName: "metricsrs:requests_total{route=/pets}",
      fieldName: "value",
      kind: "counter",
    });

    const deltas = series.samples
      .map((sample) => sample.value)
      .filter((value): value is number | bigint => value !== null)
      .map(Number);

    expect(deltas.length).toBeGreaterThan(3);
    // Increments come in threes, and a readout that caught no increment
    // carries the unchanged total, which differences to zero. Neither a
    // negative delta nor a non-multiple would be reachable from a cumulative
    // series.
    for (const delta of deltas) {
      expect(delta).toBeGreaterThanOrEqual(0);
      expect(delta % 3).toBe(0);
    }
    expect(deltas.some((delta) => delta > 0)).toBe(true);
  });

  it("leaves a gap where a histogram reported no samples", () => {
    // Percentiles go absent rather than repeating the last latency, so the
    // series has to show a hole rather than a flat line.
    const series = materializeFieldChartSeries(events, {
      id: "fc2",
      eventName: "metricsrs:query_ms",
      fieldName: "p99",
      kind: "gauge",
    });

    expect(series.samples.some((sample) => sample.value === null)).toBe(true);
    expect(series.samples.some((sample) => sample.value !== null)).toBe(true);
  });

  it("picks up described units for axis formatting", () => {
    const series = materializeFieldChartSeries(events, {
      id: "fc3",
      eventName: "metricsrs:query_ms",
      fieldName: "p50",
      kind: "gauge",
    });
    expect(series.unit).toBe("ms");
  });
});
