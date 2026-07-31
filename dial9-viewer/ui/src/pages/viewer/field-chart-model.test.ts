import { describe, expect, it } from "vitest";
import type {
  CustomTraceEvent,
  DecodedFieldValue,
  ParsedTrace,
} from "../../lib/trace/index.js";
import type { FieldChartSpec } from "../../types/state.js";
import { createViewerStore } from "./store.js";
import {
  addFieldChart,
  closeFieldChart,
  createFieldChartSeriesCache,
  fieldChartTrackSpecs,
  isChartableNumericValue,
  materializeFieldChartSeries,
} from "./field-chart-model.js";

function event(
  name: string,
  timestamp: number,
  value: DecodedFieldValue | undefined,
  unit: string | null = "bytes",
): CustomTraceEvent {
  return {
    name,
    timestamp,
    fields: value === undefined ? {} : { value },
    units: unit === null ? null : { value: unit },
    fieldKinds: null,
  };
}

function spec(kind: FieldChartSpec["kind"] = "gauge"): FieldChartSpec {
  return {
    id: "fc-1",
    eventName: "Metric",
    fieldName: "value",
    kind,
  };
}

describe("numeric field eligibility", () => {
  it.each([
    0,
    -1.25,
    9_007_199_254_740_993n,
    "0",
    "-42",
    "+12.50",
    ".75",
    "1e9",
    "  123456789012345678901234567890  ",
  ] as DecodedFieldValue[])("accepts finite decimal value %s", (value) => {
    expect(isChartableNumericValue(value)).toBe(true);
  });

  it.each([
    null,
    true,
    "",
    " ",
    "12px",
    "0x10",
    "NaN",
    "Infinity",
    [1, 2],
    { n: "3" },
  ] as DecodedFieldValue[])("rejects non-decimal value %s", (value) => {
    expect(isChartableNumericValue(value)).toBe(false);
  });
});

describe("materializeFieldChartSeries", () => {
  it("sorts stably by timestamp and preserves exact bigints", () => {
    const series = materializeFieldChartSeries(
      [
        event("Other", 1, 99),
        event("Metric", 30, "9007199254740995"),
        event("Metric", 10, 9_007_199_254_740_993n),
        event("Metric", 20, "9007199254740994"),
        event("Metric", 20, "9007199254740996"),
      ],
      spec(),
    );

    expect(series.samples.map((sample) => sample.timestamp)).toEqual([
      10,
      20,
      20,
      30,
    ]);
    expect(series.samples.map((sample) => sample.value)).toEqual([
      9_007_199_254_740_993n,
      9_007_199_254_740_994n,
      9_007_199_254_740_996n,
      9_007_199_254_740_995n,
    ]);
    expect(series.samples.every((sample) => sample.endTimestamp === null)).toBe(
      true,
    );
    expect(series.unit).toBe("bytes");
  });

  it("does not apply a unit when concatenated schemas disagree", () => {
    const series = materializeFieldChartSeries(
      [event("Metric", 1, 10, "bytes"), event("Metric", 2, 20, "ns")],
      spec(),
    );

    expect(series.unit).toBeNull();
  });

  it("uses explicit null gaps for absent/non-numeric values", () => {
    const series = materializeFieldChartSeries(
      [
        event("Metric", 1, 10),
        event("Metric", 2, undefined),
        event("Metric", 3, "not numeric"),
        event("Metric", 4, 12),
      ],
      spec(),
    );

    expect(series.samples).toEqual([
      { timestamp: 1, endTimestamp: null, value: 10n, gap: null },
      { timestamp: 2, endTimestamp: null, value: null, gap: "missing" },
      { timestamp: 3, endTimestamp: null, value: null, gap: "missing" },
      { timestamp: 4, endTimestamp: null, value: 12n, gap: null },
    ]);
  });

  it("materializes monotonic increments as intervals and resets the baseline", () => {
    const series = materializeFieldChartSeries(
      [
        event("Metric", 1, "100"),
        event("Metric", 2, "110"),
        event("Metric", 3, "4"),
        event("Metric", 4, "9"),
      ],
      spec("counter"),
    );

    expect(series.samples).toEqual([
      { timestamp: 1, endTimestamp: 2, value: 10n, gap: null },
      { timestamp: 3, endTimestamp: null, value: null, gap: "reset" },
      { timestamp: 3, endTimestamp: 4, value: 5n, gap: null },
    ]);
  });

  it("materializes signed up/down counter deltas over their intervals", () => {
    const series = materializeFieldChartSeries(
      [event("Metric", 1, 10), event("Metric", 2, 3)],
      spec("updown-counter"),
    );
    expect(series.samples).toEqual([
      { timestamp: 1, endTimestamp: 2, value: -7n, gap: null },
    ]);
  });

  it("keeps large integer counter deltas exact", () => {
    const series = materializeFieldChartSeries(
      [
        event("Metric", 1, "900719925474099300000"),
        event("Metric", 2, "900719925474099300007"),
      ],
      spec("counter"),
    );

    expect(series.samples).toEqual([
      { timestamp: 1, endTimestamp: 2, value: 7n, gap: null },
    ]);
  });

  it("does not bridge a missing counter observation", () => {
    const series = materializeFieldChartSeries(
      [
        event("Metric", 1, 10),
        event("Metric", 2, undefined),
        event("Metric", 3, 20),
        event("Metric", 4, 25),
      ],
      spec("counter"),
    );

    expect(series.samples).toEqual([
      { timestamp: 2, endTimestamp: null, value: null, gap: "missing" },
      { timestamp: 3, endTimestamp: 4, value: 5n, gap: null },
    ]);
  });

  it("keeps exact integers in a mixed integer/decimal series", () => {
    const series = materializeFieldChartSeries(
      [event("Metric", 1, "10"), event("Metric", 2, "10.5")],
      spec(),
    );
    expect(series.samples.map((sample) => sample.value)).toEqual([10n, 10.5]);
  });
});

describe("field-chart lifecycle", () => {
  it("uses only the field name as the short track title", () => {
    expect(fieldChartTrackSpecs([spec()])).toEqual([
      { id: "fc-1", label: "value", height: 112 },
    ]);
  });

  it("creates unique short ids and closing removes order/collapse references", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const first = addFieldChart(store, "Metric", "value", "gauge");
    store.update("uiPrefs", {
      trackOrder: ["events", first.id, "cpu"],
      collapsed: { [first.id]: true, cpu: true },
    });
    const second = addFieldChart(store, "Metric", "other", "counter");

    expect([first.id, second.id]).toEqual(["fc-1", "fc-2"]);
    closeFieldChart(store, first.id);
    expect(store.getState().view.fieldCharts).toEqual([second]);
    expect(store.getState().uiPrefs.trackOrder).toEqual(["events", "cpu"]);
    expect(store.getState().uiPrefs.collapsed).toEqual({ cpu: true });
  });

  it("continues generated ids in base 36", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const charts = Array.from({ length: 10 }, (_, index) =>
      addFieldChart(store, "Metric", `value_${index}`, "gauge"),
    );

    expect(charts.at(-1)?.id).toBe("fc-a");
  });

  it("rejects event and field names containing the URL separator", () => {
    const store = createViewerStore({ scheduler: () => {} });

    expect(() =>
      addFieldChart(store, "Metric,Other", "value", "gauge"),
    ).toThrow("cannot contain commas");
    expect(() =>
      addFieldChart(store, "Metric", "value,total", "gauge"),
    ).toThrow("cannot contain commas");
    expect(store.getState().view.fieldCharts).toEqual([]);
  });

  it("drops the materialized list as soon as its panel is reconciled away", () => {
    const cache = createFieldChartSeriesCache();
    const trace = {
      customEvents: [event("Metric", 1, 10)],
    } as ParsedTrace;
    cache.get(trace, spec());
    expect(cache.entryCount()).toBe(1);

    cache.reconcile(trace, []);
    expect(cache.entryCount()).toBe(0);
  });

  it("drops materialized lists from a replaced trace before repaint", () => {
    const cache = createFieldChartSeriesCache();
    const firstTrace = {
      customEvents: [event("Metric", 1, 10)],
    } as ParsedTrace;
    const nextTrace = {
      customEvents: [event("Metric", 2, 20)],
    } as ParsedTrace;
    cache.get(firstTrace, spec());

    cache.reconcile(nextTrace, [spec()]);

    expect(cache.entryCount()).toBe(0);
  });
});
