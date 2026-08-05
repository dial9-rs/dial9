// Per-runtime scheduler-metrics model: turns the flat `trace.runtimeMetrics`
// side-channel (one `RuntimeMetricsSample` per runtime per flush cycle) into
// (a) a per-runtime series the lanes' summary lane plots (global-queue depth as
// a filled step area, alive tasks as a step line), and (b) the summed
// process-wide global-queue timeline the queue track plots.
//
// Pure: no store, no DOM, no canvas — built once per trace and cached
// (derived.ts), never per frame.

import type { ParsedTrace, RuntimeMetricsSample } from "../../types/trace.js";
import { sumGlobalQueueByCycle } from "./analysis.js";

/** One runtime's metrics series, keyed in `RuntimeMetrics.byRuntime` by the
 *  runtime name (empty string for the unnamed default runtime). */
export interface RuntimeSeries {
  /** Samples for this runtime, in wire (time) order. */
  samples: RuntimeMetricsSample[];
  /** Peak alive-task count across the trace. */
  maxAliveTasks: number;
  /** Peak global-queue depth across the trace (the chart's y autoscale). */
  maxGlobalQueue: number;
}

export interface RuntimeMetrics {
  /** runtime name -> its series. Empty when the trace predates RuntimeMetrics. */
  byRuntime: Map<string, RuntimeSeries>;
  /** Process-wide global-queue timeline: per cycle timestamp, the sum across
   *  all runtimes. Sorted by t. Empty when the trace carries no RuntimeMetrics
   *  (the caller then falls back to the legacy QueueSample series). */
  summedGlobalQueue: { t: number; global: number }[];
  /** True when the trace carried at least one RuntimeMetricsEvent. */
  present: boolean;
}

export const EMPTY_RUNTIME_METRICS: RuntimeMetrics = {
  byRuntime: new Map(),
  summedGlobalQueue: [],
  present: false,
};

/**
 * Build the per-runtime metrics model from a parsed trace's `runtimeMetrics`
 * side-channel. Returns {@link EMPTY_RUNTIME_METRICS} when the trace has none
 * (old format) so callers can cleanly fall back to the legacy global series.
 */
export function computeRuntimeMetrics(trace: ParsedTrace | null): RuntimeMetrics {
  const samples = trace?.runtimeMetrics;
  if (!samples || samples.length === 0) return EMPTY_RUNTIME_METRICS;

  const byRuntime = new Map<string, RuntimeSeries>();
  for (const s of samples) {
    let series = byRuntime.get(s.runtimeName);
    if (series === undefined) {
      series = {
        samples: [],
        maxAliveTasks: 0,
        maxGlobalQueue: 0,
      };
      byRuntime.set(s.runtimeName, series);
    }
    series.samples.push(s);
    if (s.aliveTasks > series.maxAliveTasks) series.maxAliveTasks = s.aliveTasks;
    if (s.globalQueue > series.maxGlobalQueue) series.maxGlobalQueue = s.globalQueue;
  }

  // The process-wide global-queue timeline is the per-cycle sum across runtimes,
  // computed by the shared frozen-core helper so the viewer and the skill
  // scripts derive the identical series.
  const summedGlobalQueue = sumGlobalQueueByCycle(samples);

  return { byRuntime, summedGlobalQueue, present: true };
}

/**
 * The runtime-GROUP names that have a metric series, so a summary lane is
 * emitted for them. Keyed by group name (not wire name) because that is what
 * `laneRowLayout` checks; the inferred default group ("main") maps to the empty
 * wire name the unnamed default runtime uses.
 *
 * Shared by every consumer that must agree on the lane stack — the renderer's
 * data and both hit-test data paths (lanes, overlay) — so a summary lane can
 * never exist for the draw but not the click.
 */
export function metricsRuntimeNames(
  groups: readonly { name: string; inferred: boolean }[],
  metrics: RuntimeMetrics,
): ReadonlySet<string> {
  const names = new Set<string>();
  for (const g of groups) {
    const series = metrics.byRuntime.get(g.inferred ? "" : g.name);
    if (series !== undefined && series.samples.length > 0) names.add(g.name);
  }
  return names;
}
