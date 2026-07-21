// Segment-windowed loading, the tier-2 detail mechanism.
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
// CLOCK DOMAIN: all extents handed to the orchestrator and every viewport
// passed to setViewport must share ONE clock domain. deriveSegmentExtents
// produces wall-clock ns (that is all a listing knows); once a first parse
// yields clockOffsetNs, mapExtentToMonotonic converts extents into the
// trace-monotonic domain the viewer's viewport lives in. The decision
// functions are deliberately domain-agnostic.
//
// Block-in-place gap detection runs INSIDE each segment's parse
// (deriveBlockInPlaceGaps over that segment's events only), so a window edge
// is a trace edge as far as gap detection is concerned - nothing here can
// fabricate park/unpark continuity across segments.

import { EVENT_TYPES } from "../../../trace_parser.js";
import {
  computeSegmentEdgePolls,
  computeWindowBoundaryPolls,
  segmentInvariants,
} from "./segment-boundary-polls.js";
import { createRawByteCache } from "./raw-byte-cache.js";
import {
  BUDGET_EVICTION_THRESHOLD_FRACTION,
  GZIP_EXPANSION_ESTIMATE,
  RAW_GZIP_CACHE_BUDGET_BYTES,
  RESIDENT_RAW_BUDGET_BYTES,
  capToBudget,
  computeNeedSet,
  computePrefetchSet,
  evictionTriggerBytes,
  planEviction,
  extentDistance,
  extentsOverlap,
} from "./segment-budget.js";
import type {
  AdmissionCandidate,
  AdmissionPlan,
  EvictionPlan,
  EvictionPlanInput,
  ResidentSegment,
} from "./segment-budget.js";
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

// ── 2. Listing -> extent derivation ──────────────────────────────────────

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
  /** Sorted by extent start, ends tiled. */
  segments: ListedSegment[];
  skipped: SkippedListing[];
}

// A segment whose end is not strictly after its start still gets a 1s span
// instead of vanishing.
const MIN_SEGMENT_NS = 1e9;

// The `{epoch}-{index}.bin[.gz]` basename pattern: extent derivation needs
// only the filename epoch, so it works even for directory layouts parseKey
// cannot attribute service/host fields to.
const BASENAME_EPOCH_RE = /^(\d+)-\d+\.bin/;

/**
 * Derive per-segment time extents from listing metadata: start = the epoch
 * in the `{epoch}-{index}.bin.gz` filename (via keys.ts parseKey, falling
 * back to the basename pattern for unrecognized directory layouts), end =
 * listing last_modified, floored to a 1s span, then each end clamped to the
 * next segment's start so upload-lag overlaps tile instead of
 * double-covering.
 *
 * Extents are wall-clock EPOCH NANOSECONDS; see the clock-domain note in the
 * module header. Entries with no derivable epoch are returned in `skipped`
 * with a reason - never silently mislabeled.
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
  // Tile: clamp only on real overlap, never past the start.
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

// ── 6. Worker parse driver ───────────────────────────────────────────────

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
 * Parse one segment's raw (possibly still-gzipped) bytes in a worker via the
 * parse-buffer request: one worker per parse, terminated on settle (worker
 * spawn is negligible next to a segment parse; a persistent pool remains
 * open on the TraceWorkerFactory seam if profiling ever asks for one). The
 * bytes are CLONED across the boundary (see protocol.ts): the caller's copy
 * - a raw-cache entry - stays live.
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
  // Fire-and-forget friendliness: pre-mark handled.
  done.catch(() => {});

  /** Settle exactly once; always terminates (no live worker after). */
  const settle = (fn: () => void): void => {
    if (settled) return;
    settled = true;
    fn();
    port.terminate();
  };

  port.onMessage((message: TraceWorkerResponse) => {
    // Late messages (queued behind an abort) are dropped.
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
 * The store surface the segment window writes into; structurally satisfied
 * by the ViewerStore (compile-checked in tests) without lib/trace depending
 * on src/store. This orchestrator is the slice's ONLY writer: entries are
 * replaced through a fresh Map per update.
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
  /** Same-origin credential headers. */
  headers?: Record<string, string>;
  residentBudgetBytes?: number;
  gzipCacheBudgetBytes?: number;
  thresholdFraction?: number;
  /**
   * Idle scheduler for prefetch starts; defaults to requestIdleCallback
   * (setTimeout 0 fallback). Injectable.
   */
  idle?: (callback: () => void) => void;
  /** Per-segment fetch/parse progress. */
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
  /** Drive the window from the viewport. Idempotent per view. */
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
 * A parse whose decompressed size exceeds the resident budget moves the
 * entry to the terminal "oversized" state instead of "parsed": never
 * resident, never re-admitted, tier-1 rendering only.
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
    // "oversized" is terminal: the real size is known and can never fit -
    // re-parsing it would restart the loop.
    if (entry.state === "oversized") return;
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
        if (rawByteLength > residentBudget) {
          // The segment's real decompressed size can NEVER fit the resident
          // window. Writing it as "parsed" would hand planEviction's hard
          // clamp a mandatory eviction and the next admission a
          // force-re-admit - a permanent parse -> evict loop with a
          // ~2x-budget resident spike per viewport tick. Instead the parse
          // is dropped on the spot and the entry moves to the honest
          // terminal state: the learned size, invariants and edge evidence
          // are kept (lane stability and boundary-poll continuity work
          // exactly as across an eviction); the trace is not, so it never
          // enters the resident accounting.
          writeEntries((next) => {
            const e = next.get(key);
            if (e === undefined) return;
            next.set(key, {
              ...e,
              state: "oversized",
              rawByteLength,
              invariants: segmentInvariants(trace),
              edgePolls: computeSegmentEdgePolls(trace),
            });
          });
          recomputeResident();
          // Its reservation just freed: deferred work may now be admitted.
          reconcile();
          return;
        }
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

  /**
   * Budget admission for a viewport (window limit): need first, prefetch
   * only into what remains. Estimates use real decompressed sizes when
   * known. Shared by reconcile and the idle prefetch callbacks - the latter
   * must re-run ADMISSION, not just geometry, before starting work.
   *
   * Segments that can NEVER fit (real size learned oversized) are excluded
   * from admission entirely - capToBudget's first-round force-admit clause
   * must only ever see estimates or fitting sizes, or the parse -> evict
   * loop returns. They still render via tier 1. Geometric sets are computed
   * FIRST so an oversized need segment keeps contributing its neighbors to
   * the prefetch geometry.
   */
  const planAdmission = (
    currentView: TimeRange
  ): { needPlan: AdmissionPlan; prefetchPlan: AdmissionPlan } => {
    const geometricNeed = computeNeedSet(segments, currentView);
    const geometricPrefetch = computePrefetchSet(
      segments,
      geometricNeed,
      currentView
    );
    const admissible = (key: string): boolean =>
      entriesNow().get(key)?.state !== "oversized";
    const needKeys = geometricNeed.filter(admissible);
    const prefetchKeys = geometricPrefetch.filter(admissible);

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
    return { needPlan, prefetchPlan };
  };

  const schedulePrefetch = (key: string): void => {
    if (pendingPrefetch.has(key)) return;
    pendingPrefetch.add(key);
    idle(() => {
      pendingPrefetch.delete(key);
      if (disposed || view === null) return;
      // Re-verify at idle time: the viewport may have moved on, and the
      // latest reconcile may have budget-DEFERRED this key even though it is
      // still a geometric neighbor. Geometry alone is not admission:
      // starting anyway would burn a fetch + parse that lands unprotected
      // and is discarded by the next eviction pass, after transiently
      // pushing resident past the trigger.
      const { needPlan, prefetchPlan } = planAdmission(view);
      if (
        !needPlan.admitted.includes(key) &&
        !prefetchPlan.admitted.includes(key)
      ) {
        return;
      }
      startJob(key);
    });
  };

  // ── reconcile: the whole policy, one pass ───────────────────────────────

  const reconcile = (): void => {
    if (disposed || view === null) return;
    const currentView = view;
    const { needPlan, prefetchPlan } = planAdmission(currentView);
    const wanted = new Set([...needPlan.admitted, ...prefetchPlan.admitted]);

    // Stale-fetch cancellation: viewport jumps abort whatever is no longer
    // wanted, fetch and parse phase alike.
    for (const [key, job] of [...inflight]) {
      if (!wanted.has(key)) abortJob(key, job);
    }

    // Failure marks clear once a key leaves the wanted set, so re-entry
    // retries naturally without a hot loop.
    for (const key of [...failed]) {
      if (!wanted.has(key)) failed.delete(key);
    }

    // Eviction: fires past the trigger; need/prefetch protected up to the
    // hard budget. Admitted-but-unparsed work is RESERVED so the room is
    // freed BEFORE those parses land - resident bytes never have to
    // overshoot the budget waiting for a completion-time pass (the
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
          // still-resident neighbors). Raw gzip bytes stay in the cache
          // under its own budget.
          const { trace: _trace, ...kept } = entry;
          next.set(key, { ...kept, state: "evicted" });
          stats.evictions += 1;
        }
      });
    }
    recomputeResident();

    // Start need jobs now; prefetch waits for idle. A key the hard clamp
    // JUST evicted is not restarted in the same pass (it would refetch
    // straight back over the budget). By the next pass its learned real
    // size drives admission: segments that only COLLECTIVELY overshoot
    // (estimates undershot) are deferred by the capacity check, and a
    // segment that can never fit at all was parked in "oversized" at parse
    // completion and filtered out above - neither shape can loop.
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
