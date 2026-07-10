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
