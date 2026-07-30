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
  nextFieldChartKind,
} from "./field-chart-model.js";

function event(
  name: string,
  timestamp: number,
  value: DecodedFieldValue | undefined,
): CustomTraceEvent {
  return {
    name,
    timestamp,
    fields: value === undefined ? {} : { value },
    units: { value: "bytes" },
  };
}

function spec(kind: FieldChartSpec["kind"] = "gauge"): FieldChartSpec {
  return {
    id: "field-chart-1",
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
  it("filters one event type, sorts by timestamp, and preserves exact bigints", () => {
    const series = materializeFieldChartSeries(
      [
        event("Other", 1, 99),
        event("Metric", 30, "9007199254740995"),
        event("Metric", 10, 9_007_199_254_740_993n),
        event("Metric", 20, "9007199254740994"),
      ],
      spec(),
    );

    expect(series.samples.map((sample) => sample.timestamp)).toEqual([10, 20, 30]);
    expect(series.samples.map((sample) => sample.value)).toEqual([
      9_007_199_254_740_993n,
      9_007_199_254_740_994n,
      9_007_199_254_740_995n,
    ]);
    expect(series.numericKind).toBe("integer");
    expect(series.unit).toBe("bytes");
    expect(series.matchingEventCount).toBe(3);
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
      { timestamp: 1, value: 10n, gap: null },
      { timestamp: 2, value: null, gap: "missing" },
      { timestamp: 3, value: null, gap: "missing" },
      { timestamp: 4, value: 12n, gap: null },
    ]);
  });

  it("breaks a monotonic counter on decrease and starts from the new baseline", () => {
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
      { timestamp: 1, value: 100n, gap: null },
      { timestamp: 2, value: 110n, gap: null },
      { timestamp: 3, value: null, gap: "reset" },
      { timestamp: 3, value: 4n, gap: null },
      { timestamp: 4, value: 9n, gap: null },
    ]);
    expect(series.resetCount).toBe(1);
  });

  it("keeps decreases continuous for an up/down counter", () => {
    const series = materializeFieldChartSeries(
      [event("Metric", 1, 10), event("Metric", 2, 3)],
      spec("updown-counter"),
    );
    expect(series.samples).toEqual([
      { timestamp: 1, value: 10n, gap: null },
      { timestamp: 2, value: 3n, gap: null },
    ]);
    expect(series.resetCount).toBe(0);
  });

  it("promotes a mixed integer/decimal series to finite numbers", () => {
    const series = materializeFieldChartSeries(
      [event("Metric", 1, "10"), event("Metric", 2, "10.5")],
      spec(),
    );
    expect(series.numericKind).toBe("float");
    expect(series.samples.map((sample) => sample.value)).toEqual([10, 10.5]);
  });
});

describe("field-chart lifecycle", () => {
  it("cycles gauge -> counter -> up/down counter -> gauge", () => {
    expect(nextFieldChartKind("gauge")).toBe("counter");
    expect(nextFieldChartKind("counter")).toBe("updown-counter");
    expect(nextFieldChartKind("updown-counter")).toBe("gauge");
  });

  it("uses only the field name as the short track title", () => {
    expect(fieldChartTrackSpecs([spec()])).toEqual([
      { id: "field-chart-1", label: "value", height: 112 },
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

    expect([first.id, second.id]).toEqual(["field-chart-1", "field-chart-2"]);
    closeFieldChart(store, first.id);
    expect(store.getState().view.fieldCharts).toEqual([second]);
    expect(store.getState().uiPrefs.trackOrder).toEqual(["events", "cpu"]);
    expect(store.getState().uiPrefs.collapsed).toEqual({ cpu: true });
  });

  it("drops the materialized list as soon as its panel is reconciled away", () => {
    const cache = createFieldChartSeriesCache();
    const trace = {
      customEvents: [event("Metric", 1, 10)],
    } as ParsedTrace;
    cache.get(trace, spec());
    expect(cache.entryCount()).toBe(1);

    cache.reconcile([]);
    expect(cache.entryCount()).toBe(0);
  });
});
