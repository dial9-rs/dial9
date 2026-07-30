// Pure model and lifecycle actions for URL-defined custom-event field charts.
//
// A chart materializes one event-name stream into timestamp order exactly once
// per (trace, panel definition). Gauges retain observations; counters retain
// deltas over explicit time intervals. Numeric values stay as bigint when
// possible, so counters larger than Number.MAX_SAFE_INTEGER retain exact reset
// detection and deltas. The canvas layer converts only the visible range to
// pixels.

import type { ViewerStore } from "../../store/store.js";
import type {
  FieldChartKind,
  FieldChartSpec,
} from "../../types/state.js";
import type {
  CustomTraceEvent,
  DecodedFieldValue,
  ParsedTrace,
} from "../../lib/trace/index.js";
import {
  FIELD_CHART_TRACK_ID_PREFIX,
  isFieldChartTrackId,
  type FieldChartTrackId,
  type TrackSpec,
} from "../../lib/canvas/track-layout.js";

export const FIELD_CHART_TRACK_HEIGHT = 112;

export type FieldChartNumeric = number | bigint;
export type FieldChartGap = "missing" | "reset";

export interface FieldChartSample {
  /** Observation time for gauges; interval start for counters. */
  timestamp: number;
  /** Counter interval end; null for gauge observations and gap markers. */
  endTimestamp: number | null;
  value: FieldChartNumeric | null;
  /** Why this timestamp breaks the rendered path. Null on numeric samples. */
  gap: FieldChartGap | null;
}

export interface FieldChartSeries {
  samples: readonly FieldChartSample[];
  /** Unit annotation recovered from the matching event schema, if present. */
  unit: string | null;
}

// Strict decimal syntax: no hexadecimal, Infinity, NaN, separators, or
// partially numeric strings. Varints decoded as decimal strings take the exact
// bigint path; decimal/exponent strings take the finite-number path.
const DECIMAL = /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/;
const INTEGER = /^[+-]?\d+$/;

function numericValue(value: DecodedFieldValue): FieldChartNumeric | null {
  if (typeof value === "bigint") return value;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return null;
    return Number.isSafeInteger(value) ? BigInt(value) : value;
  }
  if (typeof value !== "string") return null;
  const text = value.trim();
  if (text === "" || !DECIMAL.test(text)) return null;
  if (INTEGER.test(text)) {
    try {
      return BigInt(text);
    } catch {
      return null;
    }
  }
  const parsed = Number(text);
  return Number.isFinite(parsed) ? parsed : null;
}

/** True when a decoded field can be graphed as a finite decimal number. */
export function isChartableNumericValue(value: DecodedFieldValue): boolean {
  return numericValue(value) !== null;
}

function lessThan(a: FieldChartNumeric, b: FieldChartNumeric): boolean {
  return typeof a === "bigint" && typeof b === "bigint"
    ? a < b
    : Number(a) < Number(b);
}

function subtract(
  current: FieldChartNumeric,
  previous: FieldChartNumeric,
): FieldChartNumeric | null {
  if (typeof current === "bigint" && typeof previous === "bigint") {
    return current - previous;
  }
  const delta = Number(current) - Number(previous);
  return Number.isFinite(delta) ? delta : null;
}

interface OrderedEvent {
  event: CustomTraceEvent;
  sourceIndex: number;
}

/**
 * Filter one event type, materialize it in timestamp order, parse the selected
 * field, and insert explicit null gaps for missing values. Gauges keep the
 * observations. Counters become deltas over [previous timestamp, current
 * timestamp); a monotonic decrease emits a reset marker and establishes a new
 * baseline instead of producing a negative delta. Sorting is stable for equal
 * timestamps through `sourceIndex`.
 */
export function materializeFieldChartSeries(
  events: readonly CustomTraceEvent[],
  spec: FieldChartSpec,
): FieldChartSeries {
  const ordered: OrderedEvent[] = [];
  for (let i = 0; i < events.length; i++) {
    const event = events[i]!;
    if (event.name === spec.eventName) ordered.push({ event, sourceIndex: i });
  }
  ordered.sort(
    (a, b) =>
      a.event.timestamp - b.event.timestamp ||
      a.sourceIndex - b.sourceIndex,
  );

  let unit: string | null = null;
  let previous: { timestamp: number; value: FieldChartNumeric } | null = null;
  const samples: FieldChartSample[] = [];
  for (const { event } of ordered) {
    unit ??= event.units?.[spec.fieldName] ?? null;
    const value = numericValue(event.fields?.[spec.fieldName] ?? null);
    if (value === null) {
      samples.push({
        timestamp: event.timestamp,
        endTimestamp: null,
        value: null,
        gap: "missing",
      });
      previous = null;
      continue;
    }

    if (spec.kind === "gauge") {
      samples.push({
        timestamp: event.timestamp,
        endTimestamp: null,
        value,
        gap: null,
      });
      continue;
    }

    if (previous === null) {
      previous = { timestamp: event.timestamp, value };
      continue;
    }

    if (spec.kind === "counter" && lessThan(value, previous.value)) {
      samples.push({
        timestamp: event.timestamp,
        endTimestamp: null,
        value: null,
        gap: "reset",
      });
      previous = { timestamp: event.timestamp, value };
      continue;
    }

    if (event.timestamp > previous.timestamp) {
      const delta = subtract(value, previous.value);
      if (delta === null) {
        samples.push({
          timestamp: event.timestamp,
          endTimestamp: null,
          value: null,
          gap: "missing",
        });
      } else {
        samples.push({
          timestamp: previous.timestamp,
          endTimestamp: event.timestamp,
          value: delta,
          gap: null,
        });
      }
    }
    previous = { timestamp: event.timestamp, value };
  }

  return {
    samples,
    unit,
  };
}

interface CacheEntry {
  trace: ParsedTrace;
  spec: FieldChartSpec;
  series: FieldChartSeries;
}

export interface FieldChartSeriesCache {
  get(trace: ParsedTrace, spec: FieldChartSpec): FieldChartSeries;
  /** Drop every materialized list whose panel no longer exists. */
  reconcile(specs: readonly FieldChartSpec[]): void;
  clear(): void;
  /** Diagnostic/test seam proving close/dispose releases retained series. */
  entryCount(): number;
}

function sameDefinition(a: FieldChartSpec, b: FieldChartSpec): boolean {
  return (
    a.id === b.id &&
    a.eventName === b.eventName &&
    a.fieldName === b.fieldName &&
    a.kind === b.kind
  );
}

/** Create the per-view materialization cache owned by the dynamic-track UI. */
export function createFieldChartSeriesCache(): FieldChartSeriesCache {
  const entries = new Map<string, CacheEntry>();
  return {
    get(trace, spec) {
      const cached = entries.get(spec.id);
      if (
        cached !== undefined &&
        cached.trace === trace &&
        sameDefinition(cached.spec, spec)
      ) {
        return cached.series;
      }
      const series = materializeFieldChartSeries(trace.customEvents ?? [], spec);
      entries.set(spec.id, { trace, spec, series });
      return series;
    },
    reconcile(specs) {
      const live = new Set(specs.map((spec) => spec.id));
      for (const id of entries.keys()) {
        if (!live.has(id)) entries.delete(id);
      }
    },
    clear() {
      entries.clear();
    },
    entryCount() {
      return entries.size;
    },
  };
}

export const FIELD_CHART_KINDS: readonly FieldChartKind[] = [
  "gauge",
  "counter",
  "updown-counter",
];

export function nextFieldChartKind(kind: FieldChartKind): FieldChartKind {
  const index = FIELD_CHART_KINDS.indexOf(kind);
  return FIELD_CHART_KINDS[(index + 1) % FIELD_CHART_KINDS.length]!;
}

/** Convert dynamic definitions to the ordinary track catalogue shape. */
export function fieldChartTrackSpecs(
  charts: readonly FieldChartSpec[],
): TrackSpec[] {
  return charts
    .filter((chart): chart is FieldChartSpec & { id: FieldChartTrackId } =>
      isFieldChartTrackId(chart.id),
    )
    .map((chart) => ({
      id: chart.id,
      // Intentionally short: the 100px gutter also hosts management controls.
      label: chart.fieldName,
      height: FIELD_CHART_TRACK_HEIGHT,
    }));
}

function nextFieldChartId(usedIds: ReadonlySet<string>): FieldChartTrackId {
  for (let n = 1; ; n++) {
    const id =
      `${FIELD_CHART_TRACK_ID_PREFIX}${n.toString(36)}` as FieldChartTrackId;
    if (!usedIds.has(id)) return id;
  }
}

/** Add one dynamic chart and return its generated definition. */
export function addFieldChart(
  store: ViewerStore,
  eventName: string,
  fieldName: string,
  kind: FieldChartKind,
): FieldChartSpec {
  const state = store.getState();
  const used = new Set([
    ...state.view.fieldCharts.map((chart) => chart.id),
    ...state.uiPrefs.trackOrder,
    ...Object.keys(state.uiPrefs.collapsed),
  ]);
  const spec: FieldChartSpec = {
    id: nextFieldChartId(used),
    eventName,
    fieldName,
    kind,
  };
  store.update("view", {
    fieldCharts: [...state.view.fieldCharts, spec],
  });
  return spec;
}

/**
 * Close a dynamic chart and remove every URL-state reference to its id. The
 * cache owner observes the view update and drops the materialized list.
 */
export function closeFieldChart(store: ViewerStore, id: string): void {
  if (!isFieldChartTrackId(id)) return;
  const state = store.getState();
  const fieldCharts = state.view.fieldCharts.filter((chart) => chart.id !== id);
  if (fieldCharts.length !== state.view.fieldCharts.length) {
    store.update("view", { fieldCharts });
  }

  const trackOrder = state.uiPrefs.trackOrder.filter((trackId) => trackId !== id);
  const collapsed = { ...state.uiPrefs.collapsed };
  const hadCollapsed = Object.hasOwn(collapsed, id);
  if (hadCollapsed) delete collapsed[id];
  if (
    trackOrder.length !== state.uiPrefs.trackOrder.length ||
    hadCollapsed
  ) {
    store.update("uiPrefs", { trackOrder, collapsed });
  }
}
