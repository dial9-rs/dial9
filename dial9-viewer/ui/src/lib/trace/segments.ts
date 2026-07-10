// lib/trace/segments.ts - segment-windowed loading, the tier-2 detail
// mechanism (T17; docs/ui-inventory/02-architecture.md 2.8; budget N19).
//
// Traces reach ~100 MB/min in S3 and parsed heap is ~10x raw, so loading
// whole traces tops out around one minute of data. The replacement keeps a
// lazy WINDOW of parsed segments around the viewport:
//
//   need set  = segments overlapping [viewStart, viewEnd]; fetched + parsed
//               (in the Web Worker) as the viewport reaches them.
//   prefetch  = +/-1 segment beyond both window edges, at idle priority.
//   eviction  = when resident raw bytes cross 90% of the budget, parsed
//               segments are dropped farthest-from-viewport first ("LRU by
//               distance"); their raw gzip bytes stay in a second, larger
//               cache so re-entry re-parses (~33 MB/s) instead of
//               re-downloading.
//
// Layout of this module:
//   1. Budget constants (shared decisions, chunk-1 ticket header).
//   2. Listing -> extent derivation (features/01 F2/I2 heatmap parity).
//   3. Pure decision functions: need set, prefetch set, budget admission,
//      eviction plan. Exhaustively unit-tested; the orchestrator only
//      wires them to fetch/worker/store.
//   4. Raw-gzip byte cache (the two-level cache's lower level).
//   5. Boundary-poll extraction/stitching (the 2.8 truncation hard edge).
//   6. Worker parse driver (parse-buffer protocol, T16 seam).
//   7. The orchestrator: createSegmentWindow.
//
// CLOCK DOMAIN: all extents handed to the orchestrator and every viewport
// passed to setViewport must share ONE clock domain. deriveSegmentExtents
// produces wall-clock ns (that is all a listing knows); once a first parse
// yields clockOffsetNs, mapExtentToMonotonic converts extents into the
// trace-monotonic domain the viewer's viewport lives in. The decision
// functions are deliberately domain-agnostic.
//
// ADR-0002 note: block-in-place gap detection runs INSIDE each segment's
// parse (deriveBlockInPlaceGaps over that segment's events only), so a
// window edge is a trace edge as far as gap detection is concerned -
// nothing here can fabricate park/unpark continuity across segments.

import { EVENT_TYPES } from "../../../trace_parser.js";
import { parseKey } from "./keys.js";
import { defaultTraceWorkerFactory } from "./load.js";
import type { ParsedTrace, TraceEvent } from "../../../trace_parser.js";
import type {
  SegmentEdgeDanglingClose,
  SegmentEdgeOpenPoll,
  SegmentEdgePolls,
  StitchedBoundaryPoll,
  TimeRange,
  WindowBoundaryPolls,
  WindowEdgePoll,
} from "../../types/trace.js";
import type {
  SegmentEntry,
  SegmentParseInvariants,
  SegmentsSlice,
} from "../../types/state.js";
import type {
  TraceWorkerFactory,
  TraceWorkerProgress,
  TraceWorkerResponse,
} from "./worker/protocol.js";

// ── 1. Budget constants (N19; shared-decisions block) ───────────────────

/**
 * Resident raw window: the sum of DECOMPRESSED byte sizes of all parsed
 * segments (the proxy for their ~10x parsed heap). A window limit, not a
 * load-time rejection - it replaces the legacy 100/200 MB open cap.
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

// ── 2. Listing -> extent derivation (features/01 F2 + I2 parity) ────────

/** One object from an S3 listing (the /api/browse response shape). */
export interface SegmentListing {
  /** The S3 object key - THE segment key everywhere in this module. */
  key: string;
  /** Gzipped object size from the listing, bytes. */
  sizeBytes: number;
  /** Listing last_modified (upload time), unix seconds. */
  lastModifiedEpochS: number;
}

/** A segment with a derived time extent, ready for the orchestrator. */
export interface ListedSegment {
  key: string;
  sizeBytes: number;
  extent: TimeRange;
}

/** A listing entry whose extent could not be derived (never guessed). */
export interface SkippedListing {
  key: string;
  reason: string;
}

export interface DerivedExtents {
  /** Sorted by extent start, ends tiled (heatmap parity). */
  segments: ListedSegment[];
  skipped: SkippedListing[];
}

// Same floor as heatmap.js MIN_SEGMENT_SECONDS: a segment whose end is not
// strictly after its start still gets a 1s span instead of vanishing.
const MIN_SEGMENT_NS = 1e9;

// The `{epoch}-{index}.bin[.gz]` basename pattern (keys.ts FILE_RE
// equivalent, applied to the basename only): extent derivation needs just
// the filename epoch, so it works even for directory layouts parseKey
// cannot attribute service/host fields to.
const BASENAME_EPOCH_RE = /^(\d+)-\d+\.bin/;

/**
 * Derive per-segment time extents from listing metadata exactly as the
 * browser page's heatmap does (features/01 F2/F4/F7): start = the epoch in
 * the `{epoch}-{index}.bin.gz` filename (via keys.ts parseKey, falling
 * back to the basename pattern for unrecognized directory layouts), end =
 * listing last_modified, floored to a 1s span, then each end clamped to
 * the next segment's start so upload-lag overlaps tile instead of
 * double-covering (heatmap.js tileSegments).
 *
 * Extents are wall-clock EPOCH NANOSECONDS; see the clock-domain note in
 * the module header. Entries with no derivable epoch are returned in
 * `skipped` with a reason - never silently mislabeled.
 */
export function deriveSegmentExtents(
  listings: readonly SegmentListing[]
): DerivedExtents {
  const segments: ListedSegment[] = [];
  const skipped: SkippedListing[] = [];
  for (const listing of listings) {
    const parsed = parseKey(listing.key);
    let epochS = 0;
    if (parsed.layout === "known" && parsed.epoch > 0) {
      epochS = parsed.epoch;
    } else {
      const basename = listing.key.split("/").at(-1) ?? "";
      const m = BASENAME_EPOCH_RE.exec(basename);
      if (m) epochS = parseInt(m[1]!, 10);
    }
    if (epochS <= 0) {
      skipped.push({
        key: listing.key,
        reason: "no {epoch}-{index}.bin[.gz] filename epoch in key",
      });
      continue;
    }
    const startNs = epochS * 1e9;
    let endNs = listing.lastModifiedEpochS * 1e9;
    if (!(endNs > startNs)) endNs = startNs + MIN_SEGMENT_NS;
    segments.push({
      key: listing.key,
      sizeBytes: listing.sizeBytes,
      extent: { startNs, endNs },
    });
  }
  segments.sort(
    (a, b) => a.extent.startNs - b.extent.startNs || (a.key < b.key ? -1 : 1)
  );
  // Tile: clamp only on real overlap, never past the start (tileSegments).
  for (let i = 0; i < segments.length - 1; i++) {
    const seg = segments[i]!;
    const next = segments[i + 1]!;
    if (
      next.extent.startNs > seg.extent.startNs &&
      next.extent.startNs < seg.extent.endNs
    ) {
      seg.extent = { startNs: seg.extent.startNs, endNs: next.extent.startNs };
    }
  }
  return { segments, skipped };
}

/**
 * Map a wall-clock extent into the trace-monotonic domain using the
 * parse-derived clock offset (ParsedTrace.clockOffsetNs = realtimeNs -
 * monotonicNs, so monotonic = realtime - offset).
 */
export function mapExtentToMonotonic(
  extent: TimeRange,
  clockOffsetNs: number
): TimeRange {
  return {
    startNs: extent.startNs - clockOffsetNs,
    endNs: extent.endNs - clockOffsetNs,
  };
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
  /** Candidates deferred to tier-1 rendering (window limit, N19). */
  deferred: string[];
}

/**
 * Cap a candidate set to the budget's eviction trigger: admit candidates
 * nearest the viewport center first while their estimated raw sizes fit
 * within `capacityBytes` on top of `baseBytes` (bytes already committed by
 * earlier admission rounds). The nearest candidate of the FIRST round
 * (baseBytes === 0) is always admitted, even oversized: the minimum useful
 * window is one segment, and the budget is a window limit, not a
 * load-time rejection (N19).
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
 * Eviction policy (architecture 2.8): once resident raw bytes cross the
 * trigger (90% of budget, hysteresis margin), drop parsed segments in
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

// ── 4. Raw-gzip byte cache (two-level cache, lower level) ────────────────

/**
 * The raw-gzip byte cache: still-compressed segment bytes retained after
 * parse (and after parsed-data eviction) so re-entering a window
 * re-parses instead of re-downloading - the S3 GET churn killer. True
 * LRU by access, its OWN budget (256 MB default), same 90% trigger.
 */
export interface RawByteCache {
  /** Lookup + LRU touch. */
  get(key: string): Uint8Array | undefined;
  /** Lookup without the LRU touch (planning). */
  has(key: string): boolean;
  /**
   * Insert (LRU-evicting past the trigger). Returns false - NOT cached -
   * for entries larger than the trigger itself: caching one would evict
   * the entire cache and still exceed the trigger.
   */
  set(key: string, bytes: Uint8Array): boolean;
  delete(key: string): boolean;
  totalBytes(): number;
  /** Keys, least-recently-used first (test introspection). */
  keys(): string[];
}

export function createRawByteCache(
  budgetBytes: number = RAW_GZIP_CACHE_BUDGET_BYTES,
  thresholdFraction: number = BUDGET_EVICTION_THRESHOLD_FRACTION
): RawByteCache {
  const trigger = evictionTriggerBytes(budgetBytes, thresholdFraction);
  // Map iteration order is insertion order; re-inserting on access makes
  // the first key the least recently used.
  const entries = new Map<string, Uint8Array>();
  let total = 0;
  return {
    get(key) {
      const bytes = entries.get(key);
      if (bytes === undefined) return undefined;
      entries.delete(key);
      entries.set(key, bytes);
      return bytes;
    },
    has: (key) => entries.has(key),
    set(key, bytes) {
      const prev = entries.get(key);
      if (prev !== undefined) {
        entries.delete(key);
        total -= prev.byteLength;
      }
      if (bytes.byteLength > trigger) return false;
      entries.set(key, bytes);
      total += bytes.byteLength;
      for (const [oldKey, oldBytes] of entries) {
        if (total <= trigger) break;
        if (oldKey === key) continue; // never evict the just-inserted entry
        entries.delete(oldKey);
        total -= oldBytes.byteLength;
      }
      return true;
    },
    delete(key) {
      const bytes = entries.get(key);
      if (bytes === undefined) return false;
      entries.delete(key);
      total -= bytes.byteLength;
      return true;
    },
    totalBytes: () => total,
    keys: () => [...entries.keys()],
  };
}

// ── 5. Boundary polls (2.8 truncation hard edge; features/02 G5) ────────

/**
 * Extract the boundary-poll evidence at a segment parse's edges in one
 * event scan (see types/trace.d.ts for the semantics):
 *
 * - openAtEnd: a PollStart with no closing event (PollEnd / WorkerPark /
 *   next PollStart) before the segment ends - exactly the polls the
 *   frozen core DISCARDS at trace end (trace_analysis.js #194).
 * - closeAtStart: the worker's first lifecycle event (PollStart, PollEnd,
 *   WorkerPark or WorkerUnpark) is a PollEnd - the poll began before this
 *   segment. A Park/Unpark-first segment start is ambiguous and captured
 *   as NOTHING (absence is never fabricated).
 */
export function computeSegmentEdgePolls(
  trace: Pick<ParsedTrace, "events">
): SegmentEdgePolls {
  const perWorker = new Map<number, TraceEvent[]>();
  for (const e of trace.events) {
    if (
      e.eventType === EVENT_TYPES.PollStart ||
      e.eventType === EVENT_TYPES.PollEnd ||
      e.eventType === EVENT_TYPES.WorkerPark ||
      e.eventType === EVENT_TYPES.WorkerUnpark
    ) {
      let list = perWorker.get(e.workerId);
      if (list === undefined) {
        list = [];
        perWorker.set(e.workerId, list);
      }
      list.push(e);
    }
  }
  const openAtEnd: SegmentEdgeOpenPoll[] = [];
  const closeAtStart: SegmentEdgeDanglingClose[] = [];
  for (const [workerId, events] of perWorker) {
    events.sort((a, b) => a.timestamp - b.timestamp);
    const first = events[0]!;
    if (first.eventType === EVENT_TYPES.PollEnd) {
      closeAtStart.push({ workerId, end: first.timestamp });
    }
    let open: TraceEvent | null = null;
    for (const e of events) {
      if (e.eventType === EVENT_TYPES.PollStart) {
        open = e;
      } else if (
        e.eventType === EVENT_TYPES.PollEnd ||
        e.eventType === EVENT_TYPES.WorkerPark
      ) {
        // Park closes an open poll WITHIN the segment (block_in_place
        // handoff, already openEnded via the core) - not a boundary case.
        open = null;
      }
    }
    if (open !== null) {
      openAtEnd.push({
        workerId,
        start: open.timestamp,
        taskId: open.taskId,
        spawnLocId: open.spawnLocId,
        spawnLoc: open.spawnLoc,
      });
    }
  }
  openAtEnd.sort((a, b) => a.workerId - b.workerId);
  closeAtStart.sort((a, b) => a.workerId - b.workerId);
  return { openAtEnd, closeAtStart };
}

/**
 * Parse-derived invariants retained across eviction (2.8: lanes/axes stay
 * stable). Worker ids come from the poll/park lifecycle events - the same
 * events that create lanes.
 */
export function segmentInvariants(
  trace: Pick<ParsedTrace, "events" | "minTs" | "maxTs">
): SegmentParseInvariants {
  const workers = new Set<number>();
  for (const e of trace.events) {
    if (
      e.eventType === EVENT_TYPES.PollStart ||
      e.eventType === EVENT_TYPES.PollEnd ||
      e.eventType === EVENT_TYPES.WorkerPark ||
      e.eventType === EVENT_TYPES.WorkerUnpark
    ) {
      workers.add(e.workerId);
    }
  }
  return {
    minTs: trace.minTs,
    maxTs: trace.maxTs,
    workerIds: [...workers].sort((a, b) => a - b),
  };
}

/**
 * Derive the window-level boundary-poll view from the segments slice.
 *
 * A poll's lifecycle can span ANY number of segments - segment rotation
 * is not poll-aligned, and a worker blocked in one poll for minutes (the
 * pathology this tool diagnoses) is silent for whole segments. The
 * derivation walks each worker's evidence chain across the listing
 * (T17-audit findings 1 and 3; formerly a pairwise-boundary matcher that
 * made 3+-segment polls vanish):
 *
 * - CONTINUITY: an open poll at a segment's end continues through the
 *   next listed segment when (a) their extents are ADJACENT - no
 *   unobserved time between them; listing order alone can hide holes,
 *   and stitching across one would fabricate a completed long poll out
 *   of two different polls' evidence (finding 3) - and (b) the worker is
 *   SILENT there: any PollEnd / WorkerPark / PollStart would have closed
 *   the poll, and silence is proven via the parse invariants' worker set
 *   (which, like edgePolls, survives eviction).
 * - STITCH: a chain whose open and close endpoints and every interior
 *   segment are RESIDENT assembles into ONE complete poll, whether it
 *   crosses one boundary or many.
 * - TRUNCATE: a chain interrupted by anything non-resident (unfetched or
 *   evicted neighbor, extent gap, evidence without retained edges)
 *   surfaces each maximal resident stretch it provably spans as a
 *   WindowEdgePoll truncated at "start"/"end"/"both", spans clamped to
 *   RESIDENT observed events - a lower bound, never an inflated long
 *   poll (the G5 marker semantics). Task identity carries whenever the
 *   PollStart was ever parsed (resident or retained edge evidence);
 *   otherwise it is explicitly null.
 * - DROP (core #194 parity): a chain meeting a boundary where the worker
 *   is ACTIVE with retained edges but no matching counterpart drops
 *   whole - the counterpart never made it to the wire, and the silence
 *   proof is only as good as the wire. Chains running off the ABSOLUTE
 *   listing ends also drop: the frozen core's trace-edge behavior,
 *   preserved for parity.
 *
 * Pure derivation over (listing order, entries): recompute when the set
 * of parsed segments changes; T07's derived() fits.
 */
export function computeWindowBoundaryPolls(
  orderedKeys: readonly string[],
  entries: ReadonlyMap<string, SegmentEntry>,
): WindowBoundaryPolls {
  const truncated: WindowEdgePoll[] = [];
  const stitched: StitchedBoundaryPoll[] = [];

  interface Spot {
    entry: SegmentEntry | undefined;
    /** Parsed data present: contributes rendered spans and stitch ends. */
    resident: boolean;
    /** Parse-derived evidence; survives eviction (worker set, bounds). */
    invariants: SegmentParseInvariants | undefined;
    /** Edge evidence; survives eviction (absent on pre-parse entries). */
    edgePolls: SegmentEdgePolls | undefined;
  }
  const spots: Spot[] = orderedKeys.map((key) => {
    const entry = entries.get(key);
    return {
      entry,
      resident:
        entry !== undefined &&
        entry.state === "parsed" &&
        entry.trace !== undefined &&
        entry.edgePolls !== undefined &&
        entry.invariants !== undefined,
      invariants: entry?.invariants,
      edgePolls: entry?.edgePolls,
    };
  });

  /** No unobserved time between two consecutively listed segments. */
  const adjacent = (left: Spot, right: Spot): boolean =>
    left.entry !== undefined &&
    right.entry !== undefined &&
    left.entry.extent.endNs >= right.entry.extent.startNs;

  const silentFor = (spot: Spot, workerId: number): boolean =>
    spot.invariants !== undefined &&
    !spot.invariants.workerIds.includes(workerId);

  // Workers with edge evidence anywhere; others cannot have chains.
  const workers = new Set<number>();
  for (const spot of spots) {
    if (spot.edgePolls === undefined) continue;
    for (const o of spot.edgePolls.openAtEnd) workers.add(o.workerId);
    for (const c of spot.edgePolls.closeAtStart) workers.add(c.workerId);
  }

  /** One maximal RESIDENT span a chain provably covers. */
  interface Stretch {
    start: number;
    end: number;
    /** The chain's PollStart lies inside this stretch (resident). */
    openObserved: boolean;
    /** The chain's PollEnd lies inside this stretch (resident). */
    closeObserved: boolean;
  }

  const emitStretches = (
    workerId: number,
    stretches: readonly Stretch[],
    identity: SegmentEdgeOpenPoll | null
  ): void => {
    for (const s of stretches) {
      truncated.push({
        workerId,
        start: s.start,
        end: s.end,
        taskId: identity !== null ? identity.taskId : null,
        spawnLocId: identity !== null ? identity.spawnLocId : null,
        spawnLoc: identity !== null ? identity.spawnLoc : null,
        truncatedAt: s.openObserved ? "end" : s.closeObserved ? "start" : "both",
        openEnded: true,
      });
    }
  };

  for (const workerId of [...workers].sort((a, b) => a - b)) {
    /** An open poll carried rightward across segment boundaries. */
    interface Carry {
      open: SegmentEdgeOpenPoll;
      /** Origin and every walked segment resident so far. */
      stitchable: boolean;
      stretches: Stretch[];
      /** The last stretch reaches the current boundary (contiguous). */
      live: boolean;
    }
    let carry: Carry | null = null;

    const openAt = (spot: Spot): SegmentEdgeOpenPoll | undefined =>
      spot.edgePolls?.openAtEnd.find((o) => o.workerId === workerId);
    const closeAt = (spot: Spot): SegmentEdgeDanglingClose | undefined =>
      spot.edgePolls?.closeAtStart.find((c) => c.workerId === workerId);

    const startCarry = (spot: Spot, open: SegmentEdgeOpenPoll): Carry => {
      if (spot.resident) {
        const maxTs = spot.invariants!.maxTs;
        return {
          open,
          stitchable: true,
          stretches: [
            {
              start: open.start,
              end: Math.max(maxTs ?? open.start, open.start),
              openObserved: true,
              closeObserved: false,
            },
          ],
          live: true,
        };
      }
      // Evicted neighbor with RETAINED edge evidence: informs continuity
      // and task identity but contributes no rendered span.
      return { open, stitchable: false, stretches: [], live: false };
    };

    /**
     * A dangling close with no live carry: the poll began before this
     * segment - and before every adjacent SILENT segment to its left (a
     * PollStart there would be a lifecycle event). Extend the provable
     * span leftward, then decide the left-edge disposition.
     */
    const resolveDanglingClose = (
      i: number,
      close: SegmentEdgeDanglingClose
    ): void => {
      const spot = spots[i]!;
      const stretchesReversed: Stretch[] = [];
      let live = false;
      if (spot.resident) {
        const minTs = spot.invariants!.minTs;
        stretchesReversed.push({
          start: Math.min(minTs ?? close.end, close.end),
          end: close.end,
          openObserved: false,
          closeObserved: true,
        });
        live = true;
      }
      let reason: "listing-start" | "gap" | "unparsed" | "active" =
        "listing-start";
      let stopper: Spot | null = null;
      for (let j = i - 1; j >= 0; j--) {
        const prev = spots[j]!;
        if (!adjacent(prev, spots[j + 1]!)) {
          reason = "gap";
          break;
        }
        if (prev.invariants === undefined) {
          reason = "unparsed";
          break;
        }
        if (!silentFor(prev, workerId)) {
          reason = "active";
          stopper = prev;
          break;
        }
        if (prev.resident) {
          const { minTs, maxTs } = prev.invariants;
          if (minTs !== null && maxTs !== null) {
            if (live) {
              const last = stretchesReversed[stretchesReversed.length - 1]!;
              last.start = Math.min(last.start, minTs);
            } else {
              stretchesReversed.push({
                start: minTs,
                end: maxTs,
                openObserved: false,
                closeObserved: false,
              });
              live = true;
            }
          }
          // A resident segment with no events keeps contiguity as-is.
        } else {
          live = false; // evidence-only interior: span breaks here
        }
      }
      // Absolute listing start: core trace-edge parity (#194) - drop.
      if (reason === "listing-start") return;
      // Active neighbor with retained edges and (necessarily) no
      // matching open - a matching open would have produced a forward
      // carry that consumed this close. Counterpart lost: drop whole.
      if (reason === "active" && stopper!.edgePolls !== undefined) return;
      // Gap, unparsed neighbor, or legacy evidence without retained
      // edges: honestly truncated; identity unknown (no PollStart seen).
      emitStretches(workerId, stretchesReversed.reverse(), null);
    };

    for (let i = 0; i < spots.length; i++) {
      const spot = spots[i]!;
      let closeConsumed = false;

      if (carry !== null) {
        const prev = spots[i - 1]!;
        if (!adjacent(prev, spot)) {
          // Extent gap (finding 3): the chain breaks - truncated
          // fragments, never a fabricated complete poll.
          emitStretches(workerId, carry.stretches, carry.open);
          carry = null;
        } else if (spot.invariants === undefined) {
          // Never-parsed neighbor (listed/fetching): window edge.
          emitStretches(workerId, carry.stretches, carry.open);
          carry = null;
        } else if (silentFor(spot, workerId)) {
          // The poll provably runs through this WHOLE segment.
          carry.stitchable &&= spot.resident;
          if (spot.resident) {
            const { minTs, maxTs } = spot.invariants;
            if (minTs !== null && maxTs !== null) {
              if (carry.live) {
                const last = carry.stretches[carry.stretches.length - 1]!;
                last.end = Math.max(last.end, maxTs);
              } else {
                carry.stretches.push({
                  start: minTs,
                  end: maxTs,
                  openObserved: false,
                  closeObserved: false,
                });
              }
              carry.live = true;
            }
            // A resident segment with no events keeps contiguity as-is.
          } else {
            carry.live = false; // evidence-only interior: span breaks
          }
          // The carry survives to the next boundary.
        } else if (spot.edgePolls === undefined) {
          // Active worker, evidence without retained edges (entries
          // evicted before edge retention existed): shape unknown -
          // surface what is provable, honestly truncated.
          emitStretches(workerId, carry.stretches, carry.open);
          carry = null;
        } else {
          const close = closeAt(spot);
          if (close === undefined) {
            // Unmatched: the counterpart never made it to the wire.
            // Core #194 parity - the whole chain drops.
            carry = null;
          } else if (spot.resident) {
            closeConsumed = true;
            if (carry.stitchable) {
              stitched.push({
                workerId,
                start: carry.open.start,
                end: close.end,
                taskId: carry.open.taskId,
                spawnLocId: carry.open.spawnLocId,
                spawnLoc: carry.open.spawnLoc,
              });
            } else {
              // The close is resident but part of the chain is not: the
              // close-bearing stretch ends AT the observed close.
              if (carry.live) {
                const last = carry.stretches[carry.stretches.length - 1]!;
                last.end = Math.max(last.end, close.end);
                last.closeObserved = true;
              } else {
                const minTs = spot.invariants.minTs;
                carry.stretches.push({
                  start: Math.min(minTs ?? close.end, close.end),
                  end: close.end,
                  openObserved: false,
                  closeObserved: true,
                });
              }
              emitStretches(workerId, carry.stretches, carry.open);
            }
            carry = null;
          } else {
            // Close observed once but its data is not resident
            // (evicted): rendered spans end at the last resident edge.
            closeConsumed = true;
            emitStretches(workerId, carry.stretches, carry.open);
            carry = null;
          }
        }
      }

      if (carry === null) {
        if (!closeConsumed) {
          const close = closeAt(spot);
          if (close !== undefined) resolveDanglingClose(i, close);
        }
        const open = openAt(spot);
        if (open !== undefined) carry = startCarry(spot, open);
      }
    }
    // A carry running off the absolute listing end: core trace-edge
    // parity (#194) - dropped, no marker.
  }
  return { truncated, stitched };
}

// ── 6. Worker parse driver (T16 parse-buffer protocol) ──────────────────

export interface SegmentParseResult {
  trace: ParsedTrace;
  /** Decompressed byte size - the segment's resident-budget cost. */
  rawByteLength: number;
}

/** Handle over one in-flight segment parse. */
export interface SegmentParseJob {
  /** Rejects with a DOMException named AbortError on abort. */
  done: Promise<SegmentParseResult>;
  /** Cancel: cooperative abort message + terminate. Idempotent. */
  abort(): void;
}

export interface SegmentParseOptions {
  /** Cap event count (metadata/symbols always parsed). */
  maxEvents?: number;
  /** Progress stream; never invoked after abort or settle. */
  onProgress?: (progress: TraceWorkerProgress) => void;
  /** Transport seam for tests; defaults to the Vite worker entry. */
  worker?: TraceWorkerFactory;
}

/** Parses segment bytes off the main thread; injectable for tests. */
export type SegmentParser = (
  bytes: Uint8Array,
  opts?: SegmentParseOptions
) => SegmentParseJob;

/**
 * Parse one segment's raw (possibly still-gzipped) bytes in a worker via
 * the parse-buffer request: one worker per parse, terminated on settle
 * (T16's per-load lifecycle, reused per-segment - worker spawn is
 * negligible next to a segment parse; a persistent pool remains open on
 * the TraceWorkerFactory seam if profiling ever asks for one). The bytes
 * are CLONED across the boundary (see protocol.ts): the caller's copy -
 * a raw-cache entry - stays live.
 */
export const parseSegmentInWorker: SegmentParser = (bytes, opts = {}) => {
  const port = (opts.worker ?? defaultTraceWorkerFactory)();
  let settled = false;
  let resolveDone!: (result: SegmentParseResult) => void;
  let rejectDone!: (error: unknown) => void;
  const done = new Promise<SegmentParseResult>((resolve, reject) => {
    resolveDone = resolve;
    rejectDone = reject;
  });
  // Fire-and-forget friendliness (T16 precedent): pre-mark handled.
  done.catch(() => {});

  /** Settle exactly once; always terminates (no live worker after). */
  const settle = (fn: () => void): void => {
    if (settled) return;
    settled = true;
    fn();
    port.terminate();
  };

  port.onMessage((message: TraceWorkerResponse) => {
    // Late messages (queued behind an abort) are dropped, T16 semantics.
    if (settled) return;
    switch (message.kind) {
      case "progress":
        opts.onProgress?.(message.progress);
        return;
      case "done":
        settle(() => {
          resolveDone({
            trace: message.trace,
            rawByteLength: message.buffer.byteLength,
          });
        });
        return;
      case "error":
        settle(() => {
          const error = new Error(message.message);
          error.name = message.name;
          rejectDone(error);
        });
        return;
    }
  });
  port.onError((error) => {
    settle(() => {
      rejectDone(error);
    });
  });

  // Pass the backing buffer when the view covers it (postMessage CLONES
  // requests - no transfer list on the port - so the cache copy
  // survives); slice out sub-views.
  const buffer =
    bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
      ? (bytes.buffer as ArrayBuffer)
      : (bytes.buffer.slice(
          bytes.byteOffset,
          bytes.byteOffset + bytes.byteLength
        ) as ArrayBuffer);
  const parse: { maxEvents?: number } = {};
  if (opts.maxEvents !== undefined) parse.maxEvents = opts.maxEvents;
  port.postMessage({ kind: "parse-buffer", buffer, parse });

  return {
    done,
    abort: (): void => {
      settle(() => {
        port.postMessage({ kind: "abort" });
        rejectDone(new DOMException("segment parse aborted", "AbortError"));
      });
    },
  };
};

// ── 7. The orchestrator ──────────────────────────────────────────────────

/**
 * The store surface the segment window writes into; structurally
 * satisfied by the T07 ViewerStore (compile-checked in tests) without
 * lib/trace depending on src/store. This orchestrator is the slice's ONLY
 * writer: entries are replaced through a fresh Map per update (slice
 * replacement contract, architecture 2.2).
 */
export interface SegmentsSliceStore {
  getState(): { segments: SegmentsSlice };
  update(
    slice: "segments",
    patch: { segments: ReadonlyMap<string, SegmentEntry> }
  ): void;
}

export type SegmentBytesFetcher = (
  url: string,
  opts: { signal?: AbortSignal; headers?: Record<string, string> }
) => Promise<ArrayBuffer>;

export interface SegmentWindowOptions {
  /**
   * Segment key -> fetch URL. Defaults to the identity (the key IS the
   * URL - the non-S3 `trace=` component case); S3 callers pass the
   * /api/object mapping (see load.ts objectTraceUrls).
   */
  urlFor?: (key: string) => string;
  /** Byte fetcher; defaults to fetch + arrayBuffer with an ok check. */
  fetchBytes?: SegmentBytesFetcher;
  /** Segment parser; defaults to the worker-based parseSegmentInWorker. */
  parser?: SegmentParser;
  /** Same-origin credential headers (features/02 B17). */
  headers?: Record<string, string>;
  residentBudgetBytes?: number;
  gzipCacheBudgetBytes?: number;
  thresholdFraction?: number;
  /**
   * Idle scheduler for prefetch starts; defaults to requestIdleCallback
   * (setTimeout 0 fallback). Injectable, T07 scheduler precedent.
   */
  idle?: (callback: () => void) => void;
  /** Per-segment fetch/parse progress (2.8 status-bar feedback edge). */
  onProgress?: (key: string, progress: TraceWorkerProgress) => void;
  /**
   * Per-segment failure surface. Failed keys are NOT hot-retried: a key
   * retries only after leaving and re-entering the wanted set. Defaults
   * to console.warn.
   */
  onError?: (key: string, error: unknown) => void;
}

export interface SegmentWindowStats {
  residentRawBytes: number;
  peakResidentRawBytes: number;
  gzipCacheBytes: number;
  /** Network fetches issued (cache misses). */
  networkFetches: number;
  /** Raw-cache hits (re-entry parses that skipped the network). */
  cacheHits: number;
  parsesStarted: number;
  abortedJobs: number;
  evictions: number;
}

export interface SegmentWindow {
  /**
   * Drive the window from the viewport (chunk 2 wires this to a
   * store subscription on the viewport slice). Idempotent per view.
   */
  setViewport(view: TimeRange): void;
  /** Current boundary-poll view (see computeWindowBoundaryPolls). */
  boundaryPolls(): WindowBoundaryPolls;
  stats(): Readonly<SegmentWindowStats>;
  /** Abort all in-flight work and detach. Further calls are no-ops. */
  dispose(): void;
}

const defaultFetchBytes: SegmentBytesFetcher = async (url, opts) => {
  const init: RequestInit = {};
  if (opts.signal !== undefined) init.signal = opts.signal;
  if (opts.headers !== undefined) init.headers = opts.headers;
  const resp = await fetch(url, init);
  if (!resp.ok) throw new Error(`HTTP ${resp.status} fetching ${url}`);
  return resp.arrayBuffer();
};

const defaultIdle = (callback: () => void): void => {
  const ric = (
    globalThis as { requestIdleCallback?: (cb: () => void) => void }
  ).requestIdleCallback;
  if (ric !== undefined) ric(callback);
  else setTimeout(callback, 0);
};

interface InflightJob {
  aborted: boolean;
  controller: AbortController;
  parseJob: SegmentParseJob | null;
  /** State to restore on abort: "evicted" iff previously parsed. */
  revertTo: "listed" | "evicted";
}

/**
 * Create the segment window over an ordered listing (deriveSegmentExtents
 * output; extents in the viewport's clock domain - module header). Seeds
 * the store's segments slice with every segment as "listed"; from then on
 * setViewport drives the state machine
 *
 *   listed -> fetching -> parsed -> evicted (-> fetching -> ...)
 *
 * where "fetching" covers the whole in-flight job (network or raw-cache
 * read + worker parse; an aborted job reverts to its pre-flight state).
 */
export function createSegmentWindow(
  store: SegmentsSliceStore,
  segments: readonly ListedSegment[],
  opts: SegmentWindowOptions = {}
): SegmentWindow {
  const urlFor = opts.urlFor ?? ((key: string) => key);
  const fetchBytes = opts.fetchBytes ?? defaultFetchBytes;
  const parser = opts.parser ?? parseSegmentInWorker;
  const idle = opts.idle ?? defaultIdle;
  const onError =
    opts.onError ??
    ((key: string, error: unknown) => {
      console.warn(`[dial9 segments] segment ${key} failed:`, error);
    });
  const residentBudget = opts.residentBudgetBytes ?? RESIDENT_RAW_BUDGET_BYTES;
  const threshold = opts.thresholdFraction ?? BUDGET_EVICTION_THRESHOLD_FRACTION;
  const cache = createRawByteCache(
    opts.gzipCacheBudgetBytes ?? RAW_GZIP_CACHE_BUDGET_BYTES,
    threshold
  );

  const byKey = new Map<string, ListedSegment>();
  for (const s of segments) byKey.set(s.key, s);
  const orderedKeys = segments.map((s) => s.key);

  const inflight = new Map<string, InflightJob>();
  const failed = new Set<string>();
  const pendingPrefetch = new Set<string>();
  let view: TimeRange | null = null;
  let disposed = false;
  const stats: SegmentWindowStats = {
    residentRawBytes: 0,
    peakResidentRawBytes: 0,
    gzipCacheBytes: 0,
    networkFetches: 0,
    cacheHits: 0,
    parsesStarted: 0,
    abortedJobs: 0,
    evictions: 0,
  };

  // ── store access (this orchestrator is the slice's only writer) ────────

  const entriesNow = (): ReadonlyMap<string, SegmentEntry> =>
    store.getState().segments.segments;

  const writeEntries = (mutate: (next: Map<string, SegmentEntry>) => void): void => {
    const next = new Map(entriesNow());
    mutate(next);
    store.update("segments", { segments: next });
  };

  const recomputeResident = (): void => {
    let total = 0;
    for (const entry of entriesNow().values()) {
      if (entry.state === "parsed" && entry.rawByteLength !== undefined) {
        total += entry.rawByteLength;
      }
    }
    stats.residentRawBytes = total;
    if (total > stats.peakResidentRawBytes) stats.peakResidentRawBytes = total;
    stats.gzipCacheBytes = cache.totalBytes();
  };

  // Seed: every listed segment enters the slice up front (tier-1 renders
  // from extents + sizes before any raw bytes exist).
  writeEntries((next) => {
    for (const s of segments) {
      next.set(s.key, { state: "listed", extent: s.extent, sizeBytes: s.sizeBytes });
    }
  });

  // ── job lifecycle ───────────────────────────────────────────────────────

  const estimateRawBytes = (key: string): number => {
    const entry = entriesNow().get(key);
    if (entry?.rawByteLength !== undefined) return entry.rawByteLength;
    // Keys only ever come from `segments`, so the lookup cannot miss.
    return byKey.get(key)!.sizeBytes * GZIP_EXPANSION_ESTIMATE;
  };

  const abortJob = (key: string, job: InflightJob): void => {
    job.aborted = true;
    job.controller.abort();
    job.parseJob?.abort();
    inflight.delete(key);
    stats.abortedJobs += 1;
    writeEntries((next) => {
      const entry = next.get(key);
      if (entry !== undefined) next.set(key, { ...entry, state: job.revertTo });
    });
  };

  const startJob = (key: string): void => {
    const entry = entriesNow().get(key);
    if (entry === undefined || entry.state === "parsed") return;
    if (inflight.has(key) || failed.has(key)) return;
    const job: InflightJob = {
      aborted: false,
      controller: new AbortController(),
      parseJob: null,
      revertTo: entry.invariants !== undefined ? "evicted" : "listed",
    };
    inflight.set(key, job);
    writeEntries((next) => {
      const e = next.get(key);
      if (e !== undefined) next.set(key, { ...e, state: "fetching" });
    });

    void (async () => {
      try {
        let bytes = cache.get(key);
        if (bytes === undefined) {
          stats.networkFetches += 1;
          const fetchOpts: Parameters<SegmentBytesFetcher>[1] = {
            signal: job.controller.signal,
          };
          if (opts.headers !== undefined) fetchOpts.headers = opts.headers;
          const buffer = await fetchBytes(urlFor(key), fetchOpts);
          if (job.aborted) return;
          bytes = new Uint8Array(buffer);
          cache.set(key, bytes);
        } else {
          stats.cacheHits += 1;
        }
        stats.parsesStarted += 1;
        const parseOpts: SegmentParseOptions = {};
        if (opts.onProgress !== undefined) {
          const forward = opts.onProgress;
          parseOpts.onProgress = (p) => {
            if (!job.aborted && !disposed) forward(key, p);
          };
        }
        const parseJob = parser(bytes, parseOpts);
        job.parseJob = parseJob;
        if (job.aborted) {
          // abortJob raced the fetch await; it could not see parseJob yet.
          parseJob.abort();
          return;
        }
        const { trace, rawByteLength } = await parseJob.done;
        if (job.aborted || disposed) return;
        inflight.delete(key);
        writeEntries((next) => {
          const e = next.get(key);
          if (e === undefined) return;
          next.set(key, {
            ...e,
            state: "parsed",
            trace,
            rawByteLength,
            invariants: segmentInvariants(trace),
            edgePolls: computeSegmentEdgePolls(trace),
          });
        });
        recomputeResident();
        // A new resident may cross the trigger, and settled need jobs may
        // unblock prefetch: reconcile once more.
        reconcile();
      } catch (error) {
        if (job.aborted || disposed) return; // abort already handled
        inflight.delete(key);
        failed.add(key);
        writeEntries((next) => {
          const e = next.get(key);
          if (e !== undefined) next.set(key, { ...e, state: job.revertTo });
        });
        onError(key, error);
      }
    })();
  };

  const schedulePrefetch = (key: string): void => {
    if (pendingPrefetch.has(key)) return;
    pendingPrefetch.add(key);
    idle(() => {
      pendingPrefetch.delete(key);
      if (disposed || view === null) return;
      // Re-verify at idle time: the viewport may have moved on.
      const need = computeNeedSet(segments, view);
      const prefetch = computePrefetchSet(segments, need, view);
      if (!prefetch.includes(key)) return;
      startJob(key);
    });
  };

  // ── reconcile: the whole policy, one pass ───────────────────────────────

  const reconcile = (): void => {
    if (disposed || view === null) return;
    const currentView = view;
    const needKeys = computeNeedSet(segments, currentView);
    const prefetchKeys = computePrefetchSet(segments, needKeys, currentView);

    // Budget admission (N19 window limit): need first, prefetch only into
    // what remains. Estimates use real decompressed sizes when known.
    const capacity = evictionTriggerBytes(residentBudget, threshold);
    const toCandidate = (key: string): AdmissionCandidate => ({
      key,
      extent: byKey.get(key)!.extent,
      estimatedRawBytes: estimateRawBytes(key),
    });
    const needPlan = capToBudget(needKeys.map(toCandidate), currentView, capacity);
    const needBytes = needPlan.admitted.reduce(
      (sum, key) => sum + estimateRawBytes(key),
      0
    );
    const prefetchPlan = capToBudget(
      prefetchKeys.map(toCandidate),
      currentView,
      capacity,
      needBytes
    );
    const wanted = new Set([...needPlan.admitted, ...prefetchPlan.admitted]);

    // Stale-fetch cancellation (2.8 hard edge): viewport jumps abort
    // whatever is no longer wanted, fetch and parse phase alike.
    for (const [key, job] of [...inflight]) {
      if (!wanted.has(key)) abortJob(key, job);
    }

    // Failure marks clear once a key leaves the wanted set, so re-entry
    // retries naturally without a hot loop.
    for (const key of [...failed]) {
      if (!wanted.has(key)) failed.delete(key);
    }

    // Eviction (2.8): fires past the trigger; need/prefetch protected up
    // to the hard budget. Admitted-but-unparsed work is RESERVED so the
    // room is freed BEFORE those parses land - resident bytes never have
    // to overshoot the budget waiting for a completion-time pass (the
    // GZIP_EXPANSION_ESTIMATE is deliberately above the observed ratio so
    // reservations over-cover; the hard clamp below is the backstop for
    // segments that expand beyond it).
    const resident: ResidentSegment[] = [];
    for (const [key, entry] of entriesNow()) {
      if (entry.state === "parsed" && entry.rawByteLength !== undefined) {
        resident.push({ key, extent: entry.extent, rawBytes: entry.rawByteLength });
      }
    }
    let reservedBytes = 0;
    for (const key of wanted) {
      if (entriesNow().get(key)?.state !== "parsed") {
        reservedBytes += estimateRawBytes(key);
      }
    }
    const plan = planEviction({
      resident,
      needKeys: new Set(needPlan.admitted),
      prefetchKeys: new Set(prefetchPlan.admitted),
      view: currentView,
      budgetBytes: residentBudget,
      thresholdFraction: threshold,
      reservedBytes,
    });
    if (plan.evict.length > 0) {
      writeEntries((next) => {
        for (const key of plan.evict) {
          const entry = next.get(key);
          if (entry === undefined) continue;
          // Drop the parsed data (the 10x cost); keep extent, sizes,
          // invariants (lanes/axes stability) AND edgePolls (tiny; polls
          // crossing this segment must stay visible as truncated in the
          // still-resident neighbors - T17-audit finding 1). Raw gzip
          // bytes stay in the cache under its own budget.
          const { trace: _trace, ...kept } = entry;
          next.set(key, { ...kept, state: "evicted" });
          stats.evictions += 1;
        }
      });
    }
    recomputeResident();

    // Start need jobs now; prefetch waits for idle. A key the hard clamp
    // JUST evicted is not restarted in the same pass (it would refetch
    // straight back over the budget); by the next pass its real
    // decompressed size is on the entry and admission defers it honestly.
    const justEvicted = new Set(plan.evict);
    for (const key of needPlan.admitted) {
      if (!justEvicted.has(key)) startJob(key);
    }
    for (const key of prefetchPlan.admitted) {
      if (justEvicted.has(key)) continue;
      const entry = entriesNow().get(key);
      if (entry !== undefined && entry.state !== "parsed" && !inflight.has(key)) {
        schedulePrefetch(key);
      }
    }
  };

  return {
    setViewport(nextView: TimeRange): void {
      if (disposed) throw new Error("segment window disposed");
      view = nextView;
      reconcile();
    },
    boundaryPolls: () => computeWindowBoundaryPolls(orderedKeys, entriesNow()),
    // gzipCacheBytes reads fresh: cache.set happens mid-job, between
    // recomputeResident calls.
    stats: () => ({ ...stats, gzipCacheBytes: cache.totalBytes() }),
    dispose(): void {
      if (disposed) return;
      for (const [key, job] of [...inflight]) abortJob(key, job);
      disposed = true;
    },
  };
}
