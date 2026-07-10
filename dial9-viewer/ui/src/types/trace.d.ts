// App-level trace vocabulary (T06; docs/ui-inventory/02-architecture.md 2.6).
//
// This is the single import surface for trace-domain types in `src/`: it
// re-exports the frozen core's shapes (declared in the ambient wildcard
// modules of src/types/*.d.ts, T05) and adds the app-level shapes the core
// does not return -- most importantly the kind-discriminated RuntimeEvent
// union that makes event switches compiler-exhaustive.
//
// FORM: unlike the T05 declarations (ambient `declare module "*/x.js"`
// blocks describing real .js files), this is an importable module .d.ts.
// There is NO backing .js module at runtime, so this file must contain
// TYPES ONLY -- no `export const`, no functions. Consumers must use
// `import type { ... } from "../types/trace.js"`; `verbatimModuleSyntax`
// (tsconfig) turns any accidental value import into a compile error
// (TS1484), so a runtime import of the nonexistent module cannot ship.
//
// ADR-0002 (block-in-place gap is unknowable): gap-related absence is
// encoded EXPLICITLY, never silently optional. `tid` on park/unpark
// variants is a required `number | undefined` -- constructors must
// acknowledge old traces that predate the field rather than forget them.

// ── Re-exported core shapes (T05 declarations) ──────────────────────────
//
// The parsed-trace shape per architecture 2.6: workers, polls, spans,
// custom events, CPU/heap samples, sched events, queue series,
// block-in-place gaps. Type-only re-exports; erased at build time.

export type {
  ParsedTrace,
  TraceEvent,
  CpuSample,
  CustomTraceEvent,
  AllocEvent,
  FreeEvent,
  MemoryOverflowEvent,
  TaskDump,
  ClockSyncAnchor,
  BlockInPlaceGap,
  SymbolFrame,
  CallframeSymbols,
  ParseProgress,
  ParseOptions,
} from "../../trace_parser.js";

export type {
  PollSpan,
  ParkSpan,
  ActiveSpan,
  WorkerLane,
  WorkerSpansResult,
  TaskWake,
  WorkerWake,
  RuntimeGroup,
  SchedDelay,
  PointOfInterest,
  PointOfInterestType,
  ProcessCpuUsageSample,
  ProcessCpuUsageInterval,
  TracingSpan,
  SpanSegment,
  UnmatchedSpan,
  SpanData,
  FlamegraphNode,
  FlatFlamegraphNode,
  AllocationAnalysis,
  AllocationSite,
} from "../../trace_analysis.js";

// ── Shared primitives ───────────────────────────────────────────────────

/**
 * A half-open-agnostic time interval in trace-monotonic nanoseconds
 * (the clock of `TraceEvent.timestamp`). Used for the retained sidebar
 * range, segment extents, and re-parse windows. Invariant: start <= end.
 */
export interface TimeRange {
  startNs: number;
  endNs: number;
}

// ── Segment-window boundary polls (architecture 2.8; T17) ───────────────
//
// A poll can straddle a segment boundary (segment rotation is not
// poll-aligned): its PollStart lands in one S3 object and its PollEnd in
// the next. The frozen core, parsing one segment at a time, DISCARDS the
// open poll at the segment's end (trace_analysis.js #194 - "segment
// rotated mid-poll, not a long poll") and IGNORES the dangling PollEnd at
// the next segment's start. Under segment-windowed loading these shapes
// carry that boundary evidence through the parsed output instead:
//
// - When BOTH neighbors are resident, the two halves stitch back into a
//   complete poll (StitchedBoundaryPoll).
// - When the neighbor is beyond the resident window (evicted/unfetched),
//   the poll surfaces explicitly truncated at the window edge
//   (WindowEdgePoll) - the features/02 G5 open-ended marker extended to
//   window edges, never a long-poll false positive and never silently
//   dropped data.
//
// Additive only (T06 precedent): nothing in the core's PollSpan changes.

/**
 * A poll left open at a segment's END: its PollStart was parsed but no
 * closing event (PollEnd / WorkerPark / next PollStart) followed within
 * the segment. Extracted per segment at parse time
 * (lib/trace/segments.ts `computeSegmentEdgePolls`).
 */
export interface SegmentEdgeOpenPoll {
  workerId: number;
  /** PollStart timestamp (trace-monotonic ns). */
  start: number;
  /** 0 when the trace has no task tracking (core convention). */
  taskId: number;
  spawnLocId: string | null;
  spawnLoc: string | null;
}

/**
 * A dangling poll CLOSE at a segment's START: the first poll-lifecycle
 * event for the worker in this segment is a PollEnd, so the poll began
 * before the segment. (A WorkerPark-first segment start is ambiguous -
 * no poll may have been running - and is deliberately NOT captured:
 * absence is never fabricated, ADR-0002 spirit.)
 */
export interface SegmentEdgeDanglingClose {
  workerId: number;
  /** PollEnd timestamp (trace-monotonic ns). */
  end: number;
}

/** Both edges of one parsed segment, extracted in a single event scan. */
export interface SegmentEdgePolls {
  openAtEnd: readonly SegmentEdgeOpenPoll[];
  closeAtStart: readonly SegmentEdgeDanglingClose[];
}

/**
 * A poll truncated by a resident-window edge. `start`/`end` never extend
 * past RESIDENT observed data (`end` is clamped to the last resident
 * event the poll provably spans for "end"-truncated polls, `start` to
 * the first for "start"-truncated ones), so the span is a LOWER bound on
 * the real poll - renderers must show the G5 truncation marker, not a
 * duration. A poll provably in flight across one or more whole resident
 * segments (its start AND end both beyond the resident window, continuity
 * proven by worker silence plus retained edge evidence - T17-audit
 * finding 1) is truncated at "both" edges.
 */
export interface WindowEdgePoll {
  workerId: number;
  start: number;
  end: number;
  /**
   * null when task identity is genuinely unknown: the PollStart (the only
   * carrier of task identity) was never parsed AND no edge evidence
   * retained from an earlier parse of the neighbor identifies it.
   */
  taskId: number | null;
  spawnLocId: string | null;
  spawnLoc: string | null;
  /** Which window edge(s) truncate this poll. */
  truncatedAt: "start" | "end" | "both";
  /** Always true: duration unknown, G5 marker semantics. */
  openEnded: true;
}

/**
 * A complete poll reconstructed across internal boundaries of a run of
 * extent-adjacent RESIDENT segments: PollStart in one segment, PollEnd in
 * a later one, every interior segment resident and provably silent for
 * the worker (a PollEnd/Park/PollStart there would have closed the poll).
 * The two-segment case is the k = 2 degenerate chain; polls spanning 3+
 * segments assemble the same way (T17-audit finding 1). Field-compatible
 * with the core's PollSpan.
 */
export interface StitchedBoundaryPoll {
  workerId: number;
  start: number;
  end: number;
  taskId: number;
  spawnLocId: string | null;
  spawnLoc: string | null;
}

/**
 * The window-level boundary-poll view derived from the segments slice
 * (lib/trace/segments.ts `computeWindowBoundaryPolls`): recompute whenever
 * the set of parsed segments changes (a store `derived()` fits).
 */
export interface WindowBoundaryPolls {
  truncated: readonly WindowEdgePoll[];
  stitched: readonly StitchedBoundaryPoll[];
}

// ── Kind-discriminated runtime-event union (architecture 2.6) ───────────
//
// The frozen core normalizes every runtime event into a flat `TraceEvent`
// with a numeric `eventType` and zero/null defaults for fields the type
// doesn't carry (trace_parser.js processFrame). This union is the
// app-level view: one variant per EVENT_TYPES entry, carrying ONLY the
// fields that event type actually populates, discriminated by a string
// `kind` so switches are exhaustive (see src/types/exhaustive.test.ts).
// The flat-to-union refinement lives in lib/trace (typed core boundary,
// architecture 2.7) -- this file only defines the vocabulary.

/** Common fields present on every runtime event. */
interface RuntimeEventBase {
  /** Trace-monotonic nanoseconds. */
  timestamp: number;
}

/** EVENT_TYPES.PollStart (0): a task poll began on a worker. */
export interface PollStartEvent extends RuntimeEventBase {
  kind: "poll-start";
  workerId: number;
  /** 0 when the trace has no task tracking. */
  taskId: number;
  /** Interned spawn-location id; null when unknown / untracked. */
  spawnLocId: string | null;
  /** Human-readable spawn location; null when unknown / untracked. */
  spawnLoc: string | null;
  /** Worker-local queue depth at poll start. */
  localQueue: number;
}

/** EVENT_TYPES.PollEnd (1): the current poll on a worker finished. */
export interface PollEndEvent extends RuntimeEventBase {
  kind: "poll-end";
  workerId: number;
}

/** EVENT_TYPES.WorkerPark (2): a worker went to sleep. */
export interface WorkerParkEvent extends RuntimeEventBase {
  kind: "worker-park";
  workerId: number;
  localQueue: number;
  /** CLOCK_THREAD_CPUTIME_ID reading (ns) on the parking thread. */
  cpuTime: number;
  /**
   * OS thread id the park happened on. Explicitly `undefined` (not
   * optional) on traces that predate the field: ADR-0002 gap detection
   * needs park/unpark tids and must SKIP events without one instead of
   * fabricating attribution -- constructors are forced to acknowledge
   * the absence.
   */
  tid: number | undefined;
}

/** EVENT_TYPES.WorkerUnpark (3): a worker woke up. */
export interface WorkerUnparkEvent extends RuntimeEventBase {
  kind: "worker-unpark";
  workerId: number;
  localQueue: number;
  /** CLOCK_THREAD_CPUTIME_ID reading (ns) on the unparking thread. */
  cpuTime: number;
  /** Kernel scheduling wait (ns) between wakeup request and running. */
  schedWait: number;
  /** See WorkerParkEvent.tid (ADR-0002). */
  tid: number | undefined;
}

/** EVENT_TYPES.QueueSample (4): periodic global injection-queue depth. */
export interface QueueSampleEvent extends RuntimeEventBase {
  kind: "queue-sample";
  globalQueue: number;
}

/** EVENT_TYPES.WakeEvent (9): task A woke task B onto a target worker. */
export interface WakeEvent extends RuntimeEventBase {
  kind: "wake";
  wakerTaskId: number;
  wokenTaskId: number;
  targetWorker: number;
}

/**
 * The app-level runtime event, discriminated on `kind`. One variant per
 * trace_parser.js EVENT_TYPES entry. When adding a variant here, the
 * exhaustive switches over `RuntimeEvent` / `RuntimeEventKind` stop
 * compiling until every consumer handles it (that is the point).
 */
export type RuntimeEvent =
  | PollStartEvent
  | PollEndEvent
  | WorkerParkEvent
  | WorkerUnparkEvent
  | QueueSampleEvent
  | WakeEvent;

/** "poll-start" | "poll-end" | "worker-park" | "worker-unpark" | "queue-sample" | "wake" */
export type RuntimeEventKind = RuntimeEvent["kind"];
