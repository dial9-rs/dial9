// Segment residency budgets and the pure decisions that follow from them:
// which segments are needed, which to prefetch, what to admit, what to evict.
//
// Every function here is referentially transparent over plain data -- no
// fetching, no parsing, no store. That is deliberate: residency policy is the
// part with the awkward edge cases (the parse->evict loop, the ~2x-budget
// resident spike, never evicting the segment just admitted), and it is far
// easier to pin down in tests when it cannot touch the world.
//
// Split out of segments.ts, which orchestrates these decisions against the
// live viewport.

import type { TimeRange } from "../../types/trace.js";
import type { SegmentEntry, SegmentsSlice } from "../../types/state.js";
import type { ListedSegment } from "./segments.js";

// ── 1. Budget constants ──────────────────────────────────────────────────

/**
 * Resident raw window: the sum of DECOMPRESSED byte sizes of all parsed
 * segments (the proxy for their ~10x parsed heap). A window limit, not a
 * load-time rejection.
 */
export const RESIDENT_RAW_BUDGET_BYTES = 128 * 1024 * 1024;

/** Raw-gzip byte cache budget (the two-level cache's lower level). */
export const RAW_GZIP_CACHE_BUDGET_BYTES = 256 * 1024 * 1024;

/**
 * Eviction triggers at this fraction of each budget: the 10% headroom is
 * the hysteresis margin - GC lags eviction, so waiting for the hard
 * budget would overshoot it while freed heap is still uncollected.
 */
export const BUDGET_EVICTION_THRESHOLD_FRACTION = 0.9;

/**
 * Planning estimate for a segment's decompressed size before its first
 * fetch, as a multiple of the gzipped listing size. Used ONLY for budget
 * admission (how many unfetched segments to admit at once); the real
 * decompressed size replaces it after the first parse. Deliberately
 * conservative: the demo trace expands ~3.3x.
 */
export const GZIP_EXPANSION_ESTIMATE = 4;

/** The byte level at which eviction fires for a given budget. */
export function evictionTriggerBytes(
  budgetBytes: number,
  thresholdFraction: number = BUDGET_EVICTION_THRESHOLD_FRACTION
): number {
  return Math.floor(budgetBytes * thresholdFraction);
}

// ── 3. Pure decision functions ───────────────────────────────────────────

/** Closed-interval overlap: extents touching the view edge count (a poll
 * AT the edge belongs to both sides; fetching one segment early beats
 * missing one). */
export function extentsOverlap(a: TimeRange, b: TimeRange): boolean {
  return a.startNs <= b.endNs && a.endNs >= b.startNs;
}

/** Gap between an extent and the view; 0 when they overlap. */
export function extentDistance(extent: TimeRange, view: TimeRange): number {
  if (extentsOverlap(extent, view)) return 0;
  return extent.endNs < view.startNs
    ? view.startNs - extent.endNs
    : extent.startNs - view.endNs;
}

/**
 * The need set: keys of segments overlapping the viewport, in listing
 * (extent) order. `segments` must be ordered by extent start (the
 * deriveSegmentExtents output order).
 */
export function computeNeedSet(
  segments: readonly ListedSegment[],
  view: TimeRange
): string[] {
  return segments
    .filter((s) => extentsOverlap(s.extent, view))
    .map((s) => s.key);
}

/**
 * The prefetch set: +/-1 segment beyond both edges of the need set (the
 * boundary preload, fetched at idle priority so panning across a boundary
 * feels instant). With an EMPTY need set (viewport inside a coverage gap
 * or outside the listing), the nearest segment on each side is prefetched
 * instead, so panning back toward data stays warm.
 */
export function computePrefetchSet(
  segments: readonly ListedSegment[],
  needKeys: readonly string[],
  view: TimeRange
): string[] {
  const out: string[] = [];
  if (needKeys.length > 0) {
    const first = segments.findIndex((s) => s.key === needKeys[0]);
    const last = segments.findIndex((s) => s.key === needKeys[needKeys.length - 1]);
    if (first > 0) out.push(segments[first - 1]!.key);
    if (last >= 0 && last < segments.length - 1) out.push(segments[last + 1]!.key);
    return out;
  }
  let before: ListedSegment | null = null;
  let after: ListedSegment | null = null;
  for (const s of segments) {
    if (s.extent.endNs < view.startNs) before = s; // ordered: last such wins
    if (s.extent.startNs > view.endNs) {
      after = s; // ordered: first such wins
      break;
    }
  }
  if (before) out.push(before.key);
  if (after) out.push(after.key);
  return out;
}

/** A budget-admission candidate (need or prefetch member). */
export interface AdmissionCandidate {
  key: string;
  extent: TimeRange;
  /** rawByteLength when known (prior parse), else gzip size x estimate. */
  estimatedRawBytes: number;
}

export interface AdmissionPlan {
  /** Admitted keys, nearest-to-viewport-center first. */
  admitted: string[];
  /** Candidates deferred to tier-1 rendering. */
  deferred: string[];
}

/**
 * Cap a candidate set to the budget's eviction trigger: admit candidates
 * nearest the viewport center first while their estimated raw sizes fit
 * within `capacityBytes` on top of `baseBytes` (bytes already committed by
 * earlier admission rounds). The nearest candidate of the FIRST round
 * (baseBytes === 0) is always admitted, even when its ESTIMATE exceeds the
 * capacity: the minimum useful window is one segment, and the budget is a
 * window limit, not a load-time rejection - estimates are guesses, so the
 * segment gets its one real fetch + parse.
 *
 * Candidates whose REAL decompressed size is known to exceed the hard budget
 * must never reach this function: the orchestrator parks them in the
 * "oversized" state at parse completion and filters them out before
 * admission, so the force-admit clause cannot re-admit a segment that
 * provably cannot fit.
 */
export function capToBudget(
  candidates: readonly AdmissionCandidate[],
  view: TimeRange,
  capacityBytes: number,
  baseBytes = 0
): AdmissionPlan {
  const viewCenter = (view.startNs + view.endNs) / 2;
  const byDistance = [...candidates].sort((a, b) => {
    const da = Math.abs((a.extent.startNs + a.extent.endNs) / 2 - viewCenter);
    const db = Math.abs((b.extent.startNs + b.extent.endNs) / 2 - viewCenter);
    return da - db || a.extent.startNs - b.extent.startNs;
  });
  const admitted: string[] = [];
  const deferred: string[] = [];
  let total = baseBytes;
  for (const c of byDistance) {
    if (
      total + c.estimatedRawBytes <= capacityBytes ||
      (admitted.length === 0 && baseBytes === 0)
    ) {
      admitted.push(c.key);
      total += c.estimatedRawBytes;
    } else {
      deferred.push(c.key);
    }
  }
  return { admitted, deferred };
}

/** A currently parsed (resident) segment, as the eviction planner sees it. */
export interface ResidentSegment {
  key: string;
  extent: TimeRange;
  /** Decompressed size - the segment's budget cost. */
  rawBytes: number;
}

export interface EvictionPlanInput {
  resident: readonly ResidentSegment[];
  /** Never evicted below the hard budget (the viewport needs them). */
  needKeys: ReadonlySet<string>;
  /** Evicted only to get back under the HARD budget (before need keys). */
  prefetchKeys: ReadonlySet<string>;
  view: TimeRange;
  budgetBytes: number;
  thresholdFraction?: number;
  /**
   * Estimated bytes of admitted-but-not-yet-parsed work (in-flight or
   * about to start). The planner acts as if these bytes were already
   * resident, so eviction happens BEFORE a new parse lands rather than
   * after - the resident total never has to overshoot the budget while
   * waiting for a completion-time eviction pass.
   */
  reservedBytes?: number;
}

export interface EvictionPlan {
  /** Keys to evict, in eviction order (farthest from the viewport first). */
  evict: string[];
  residentBytesBefore: number;
  residentBytesAfter: number;
  /** True when the threshold was crossed and eviction fired. */
  triggered: boolean;
}

/**
 * Eviction policy: once resident raw bytes cross the trigger (90% of
 * budget, hysteresis margin), drop parsed segments in
 * order of distance from the viewport - farthest first - until back at or
 * under the trigger. Need/prefetch members are protected at this stage.
 *
 * Hard clamp: if unprotected evictions cannot bring the total under the
 * HARD budget (admission estimates can undershoot real decompressed
 * sizes), prefetch members are evicted next, then need members farthest
 * from the viewport center - the window limit wins over window width;
 * dropped need segments fall back to tier-1 rendering.
 *
 * Ties (equal distance, e.g. the two immediate neighbors) break toward
 * evicting the EARLIER extent first - arbitrary but deterministic.
 */
export function planEviction(input: EvictionPlanInput): EvictionPlan {
  const threshold = input.thresholdFraction ?? BUDGET_EVICTION_THRESHOLD_FRACTION;
  const reserved = input.reservedBytes ?? 0;
  // Reservations shrink both watermarks (never below 0): the reserved
  // bytes will land shortly, so the plan must leave room for them.
  const trigger = Math.max(
    0,
    evictionTriggerBytes(input.budgetBytes, threshold) - reserved
  );
  const hardBudget = Math.max(0, input.budgetBytes - reserved);
  const before = input.resident.reduce((sum, r) => sum + r.rawBytes, 0);
  let total = before;
  const evict: string[] = [];
  if (total <= trigger) {
    return { evict, residentBytesBefore: before, residentBytesAfter: total, triggered: false };
  }

  const byDistanceDesc = (a: ResidentSegment, b: ResidentSegment): number => {
    const da = extentDistance(a.extent, input.view);
    const db = extentDistance(b.extent, input.view);
    return db - da || a.extent.startNs - b.extent.startNs;
  };
  const evictFrom = (
    pool: ResidentSegment[],
    stopAtBytes: number
  ): void => {
    for (const seg of pool.sort(byDistanceDesc)) {
      if (total <= stopAtBytes) return;
      evict.push(seg.key);
      total -= seg.rawBytes;
    }
  };

  const unprotected = input.resident.filter(
    (r) => !input.needKeys.has(r.key) && !input.prefetchKeys.has(r.key)
  );
  evictFrom(unprotected, trigger);
  if (total > hardBudget) {
    const prefetch = input.resident.filter((r) => input.prefetchKeys.has(r.key));
    evictFrom(prefetch, hardBudget);
  }
  if (total > hardBudget) {
    const viewCenter = (input.view.startNs + input.view.endNs) / 2;
    const need = input.resident
      .filter((r) => input.needKeys.has(r.key))
      .sort((a, b) => {
        const da = Math.abs((a.extent.startNs + a.extent.endNs) / 2 - viewCenter);
        const db = Math.abs((b.extent.startNs + b.extent.endNs) / 2 - viewCenter);
        return db - da || a.extent.startNs - b.extent.startNs;
      });
    for (const seg of need) {
      if (total <= hardBudget) break;
      evict.push(seg.key);
      total -= seg.rawBytes;
    }
  }
  return { evict, residentBytesBefore: before, residentBytesAfter: total, triggered: true };
}
