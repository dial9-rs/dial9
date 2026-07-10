// Hand-written type declarations for the frozen-core file `trace_analysis.js`.
// See src/types/decode.d.ts for the declaration-form rationale.
//
// Shapes verified against trace_analysis.js (whole file), the
// dial9-trace-analysis skill (buildWorkerSpans/buildSpanData/buildFgData
// schemas), and call sites in viewer.html / flamegraph.html.
//
// ADR-0002 encoding: buildWorkerSpans takes the trace's blockInPlaceGaps and
// DISCARDS active spans that cross a gap (they are absent from `actives`,
// never split); the final still-open park span has no schedWait, hence
// `ParkSpan.schedWait?: number`.

declare module "*/trace_analysis.js" {
  import type {
    TraceEvent,
    CpuSample,
    CallframeSymbols,
    CustomTraceEvent,
    BlockInPlaceGap,
    AllocEvent,
    FreeEvent,
    MemoryOverflowEvent,
  } from "*/trace_parser.js";
  import type { DecodedFieldValue } from "*/decode.js";

  // ── Worker span reconstruction ────────────────────────────────────────

  export interface PollSpan {
    start: number;
    end: number;
    taskId: number;
    spawnLocId: string | null;
    spawnLoc: string | null;
    /**
     * True when no matching PollEnd was seen (block_in_place handoff or
     * park closed the poll); the actual duration is unknown.
     */
    openEnded?: boolean;
    /** On-CPU samples inside this poll; assigned by attachCpuSamples. */
    cpuSamples?: CpuSample[];
    /** Off-CPU samples inside this poll; assigned by attachCpuSamples. */
    schedSamples?: CpuSample[];
  }

  export interface ParkSpan {
    start: number;
    end: number;
    /**
     * Kernel scheduling wait (ns) read from the closing WorkerUnpark.
     * Absent on the synthetic park closed at trace end (no unpark seen).
     */
    schedWait?: number;
  }

  /**
   * Worker awake period (Unpark -> Park). `ratio` is CPU time / wall time.
   * ADR-0002: active spans crossing a block-in-place gap are DISCARDED
   * (the CPU-time delta mixes two threads and is meaningless), so gaps
   * appear as missing actives, never as spans with a bogus ratio.
   */
  export interface ActiveSpan {
    start: number;
    end: number;
    ratio: number;
  }

  export interface WorkerLane {
    polls: PollSpan[];
    parks: ParkSpan[];
    actives: ActiveSpan[];
    /** Timestamps of on-CPU samples on this worker (attachCpuSamples). */
    cpuSampleTimes: number[];
  }

  export interface TaskWake {
    timestamp: number;
    wakerTaskId: number;
    targetWorker: number;
  }

  export interface WorkerWake {
    timestamp: number;
    wakerTaskId: number;
    wokenTaskId: number;
  }

  export interface WorkerSpansResult {
    /** Keyed by worker id (numeric keys of a plain object). */
    workerSpans: Record<number, WorkerLane>;
    /** Per-worker events sorted by timestamp (non-queue, non-wake). */
    perWorker: Record<number, TraceEvent[]>;
    queueSamples: { t: number; global: number }[];
    workerQueueSamples: Record<number, { t: number; local: number }[]>;
    maxLocalQueue: number;
    wakesByTask: Record<number, TaskWake[]>;
    wakesByWorker: Record<number, WorkerWake[]>;
  }

  /**
   * Reconstruct poll/park/active spans from raw events.
   * Pass `blockInPlaceGaps` (from ParsedTrace) so active spans crossing a
   * gap are suppressed per ADR-0002; omitting it skips that suppression.
   */
  export function buildWorkerSpans(
    events: readonly TraceEvent[],
    workerIds: readonly number[],
    maxTs: number,
    blockInPlaceGaps?: readonly BlockInPlaceGap[] | null
  ): WorkerSpansResult;

  /**
   * Attach CPU samples to the poll spans they fall within. Mutates poll
   * objects (adds cpuSamples/schedSamples) and samples (sets spawnLoc,
   * inPoll).
   */
  export function attachCpuSamples(
    cpuSamples: readonly CpuSample[],
    workerSpans: Record<number, WorkerLane>
  ): { pollsWithCpuSamples: number; pollsWithSchedSamples: number };

  // ── Runtime grouping ──────────────────────────────────────────────────

  export interface RuntimeGroup {
    name: string;
    workerIds: number[];
    /** True for the inferred default ("main") runtime. */
    inferred: boolean;
  }

  export function computeRuntimeGroups(
    workerIds: readonly number[],
    runtimeWorkers?: Map<string, number[]> | null
  ): RuntimeGroup[];

  export interface RuntimeFilterData {
    workerRuntime: Map<number, string>;
    /** Empty when there is a single runtime (caller hides the filter). */
    options: { name: string; inferred: boolean; sampleCount: number }[];
  }

  export function buildRuntimeFilterData(
    samples: readonly { workerId: number }[],
    runtimeWorkers?: Map<string, number[]> | null,
    offWorkerId?: number
  ): RuntimeFilterData;

  // ── Task lifecycle / scheduling delays ────────────────────────────────

  export function buildActiveTaskTimeline(
    taskSpawnTimes: Map<number, number>,
    taskTerminateTimes: Map<number, number>
  ): {
    activeTaskSamples: { t: number; count: number }[];
    taskFirstPoll: Map<number, number>;
  };

  export interface SchedDelay {
    wakeTime: number;
    pollTime: number;
    delay: number;
    taskId: number;
    wakerTaskId: number;
    worker: number;
    poll: PollSpan;
  }

  export function computeSchedulingDelays(
    workerSpans: Record<number, WorkerLane>,
    workerIds: readonly number[],
    wakesByTask: Record<number, TaskWake[]>
  ): SchedDelay[];

  /**
   * For each poll of one task, the most recent wake at/before its start and
   * the effective wake time (bumped past an earlier poll containing the
   * wake). Result is parallel to `polls`; null where no wake applies.
   */
  export function computePollWakes(
    polls: readonly { start: number; end: number }[],
    wakes: readonly TaskWake[] | null | undefined
  ): ({ wake: TaskWake; effectiveWake: number } | null)[];

  export type PointOfInterestType =
    | "sched"
    | "long-poll"
    | "cpu-sampled"
    | "wake-delay"
    | "uninstrumented";

  export interface PointOfInterest {
    time: number;
    worker: number;
    type: PointOfInterestType;
    value: number;
    span: PollSpan | ParkSpan;
    schedDelay?: SchedDelay;
  }

  export function filterPointsOfInterest(
    filterType: PointOfInterestType,
    workerSpans: Record<number, WorkerLane>,
    workerIds: readonly number[],
    schedDelays: readonly SchedDelay[],
    opts?: {
      hasSchedWait?: boolean;
      sortByWorst?: boolean;
      /** Required for the "uninstrumented" filter. */
      taskInstrumented?: Map<number, boolean>;
    }
  ): PointOfInterest[];

  // ── Trace time range / CPU usage ──────────────────────────────────────

  export function getTraceTimeRange(
    events: readonly TraceEvent[],
    cpuSamples: readonly CpuSample[],
    customEvents?: readonly CustomTraceEvent[] | null
  ): { minTs: number; maxTs: number; durationNs: number } | null;

  export function hasCpuProfileSamples(
    cpuSamples: readonly CpuSample[]
  ): boolean;

  export interface ProcessCpuUsageSample {
    t: number;
    event: CustomTraceEvent;
    userCpuNs: number;
    systemCpuNs: number;
    cpuTimeNs: number;
  }

  export interface ProcessCpuUsageInterval {
    start: number;
    end: number;
    t: number;
    wallDeltaNs: number;
    userDeltaNs: number;
    systemDeltaNs: number;
    cpuDeltaNs: number;
    startCpuTimeNs: number;
    endCpuTimeNs: number;
    cores: number;
    /** Percent of available parallelism; null when capacity unknown. */
    totalPercent: number | null;
    startSample: ProcessCpuUsageSample;
    endSample: ProcessCpuUsageSample;
  }

  export function buildProcessCpuUsageSeries(
    customEvents: readonly CustomTraceEvent[] | null | undefined,
    availableParallelism?: number | string | null
  ): {
    samples: ProcessCpuUsageSample[];
    intervals: ProcessCpuUsageInterval[];
    availableParallelism: number | null;
    maxCores: number;
    avgCores: number;
  };

  // ── Flamegraphs ───────────────────────────────────────────────────────

  /** Deterministic warm color for a frame name (hsl string). */
  export function flamegraphColor(name: string): string;

  /**
   * Input sample for tree building. CpuSample satisfies this; the heap
   * views pass pseudo-samples with explicit weights.
   */
  export interface FlamegraphSampleInput {
    callchain: string[];
    /** Per-sample weight; defaults to 1 (CPU sample counting). */
    weight?: number;
    /** Secondary weight (heap views: bytes or alloc count). */
    allocWeight?: number;
  }

  export interface FlamegraphNode {
    name: string;
    children: Map<string, FlamegraphNode>;
    count: number;
    self: number;
    /** Un-shortened symbol; absent on the synthetic root. */
    fullName?: string;
    location?: string | null;
    docsUrl?: string | null;
    /** Accumulated allocWeight (heap views only). */
    allocCount?: number;
    selfAllocCount?: number;
  }

  export function buildFlamegraphTree(
    samples: readonly FlamegraphSampleInput[],
    callframeSymbols: CallframeSymbols
  ): FlamegraphNode;

  export interface FlatFlamegraphNode {
    name: string;
    depth: number;
    /** Fraction of total width, 0..1. */
    x: number;
    w: number;
    count: number;
    self: number;
  }

  export function flattenFlamegraph(
    root: FlamegraphNode,
    total: number
  ): { nodes: FlatFlamegraphNode[]; maxDepth: number };

  export function buildFgData(
    samples: readonly FlamegraphSampleInput[],
    callframeSymbols: CallframeSymbols
  ): {
    nodes: FlatFlamegraphNode[];
    maxDepth: number;
    totalSamples: number;
  } | null;

  // ── Tracing spans (SpanEnter/SpanExit custom events) ──────────────────

  export interface SpanSegment {
    start: number;
    end: number;
    workerId: number;
  }

  export interface TracingSpan {
    start: number;
    end: number;
    spanId: string;
    spanName: string;
    /** User-defined span fields (base worker/span fields excluded). */
    fields: Record<string, DecodedFieldValue>;
    /** Only set for explicit parents; null for most #[instrument] spans. */
    parentSpanId: string | null;
    segments: SpanSegment[];
    activeNs: number;
    /** 0 for roots, +1 per ancestor (computed from the parent chain). */
    depth: number;
  }

  export interface UnmatchedSpan {
    start: number;
    spanId: string;
    workerId: number;
    spanName: string;
    fields: Record<string, DecodedFieldValue>;
    parentSpanId: string | null;
  }

  export interface SpanData {
    /** All completed spans, sorted by start. */
    allSpans: TracingSpan[];
    spanMeta: Map<
      string,
      {
        spanName: string;
        fields: Record<string, DecodedFieldValue>;
        parentSpanId: string | null;
      }
    >;
    maxDepth: number;
    unmatchedSpans: UnmatchedSpan[];
    /** parent span id (null = roots) -> child span ids. */
    childrenByParent: Map<string | null, string[]>;
  }

  export function buildSpanData(
    customEvents: readonly CustomTraceEvent[]
  ): SpanData;

  /** Seeds plus all their descendants; cycle-safe. */
  export function collectDescendants(
    seedIds: readonly string[],
    childrenByParent: Map<string | null, string[]>
  ): Set<string>;

  export function selectSpanRenderSet(opts: {
    allSpans: TracingSpan[];
    focusedSpanId: string | null;
    childrenByParent: Map<string | null, string[]>;
  }): TracingSpan[];

  /**
   * Spans actively executing on the event's worker at its timestamp,
   * outermost first. Events without a worker_id field return [].
   */
  export function enclosingSpans(
    allSpans: readonly TracingSpan[],
    ev: {
      timestamp: number;
      fields?: Record<string, DecodedFieldValue>;
    } | null | undefined
  ): TracingSpan[];

  export interface SpanLayoutBucket<S> {
    spans: S[];
    representative: S;
    x1: number;
    x2: number;
    y: number;
    h: number;
  }

  export function computeSpanLayout<S extends { start: number; end: number }>(
    opts: {
      spans: readonly S[];
      viewStart: number;
      viewEnd: number;
      drawW: number;
      panelH: number;
      clusterXPx: number;
      barH: number;
    }
  ): { buckets: SpanLayoutBucket<S>[]; minDur: number; maxDur: number };

  // ── Memory allocation analysis ────────────────────────────────────────

  export interface AllocationSite {
    callchain: string[];
    totalBytes: number;
    count: number;
    estimatedBytes: number;
  }

  export interface AllocationAnalysis {
    /** Top 10 sites by estimated bytes. */
    topSites: AllocationSite[];
    leaks: {
      callchain: string[];
      size: number;
      timestamp: number;
      addr: string;
    }[];
    perTask: Map<
      number,
      { sampledBytes: number; count: number; estimatedBytes: number }
    >;
    sampleRateBytes: number;
    summary: {
      totalAllocBytes: number;
      totalAllocCount: number;
      totalFreeCount: number;
      leakedBytes: number;
      leakedCount: number;
      estimatedTotalBytes: number;
      totalDroppedAllocs: number;
      totalDroppedFrees: number;
    };
  }

  export function analyzeAllocations(
    allocEvents: readonly AllocEvent[] | null | undefined,
    freeEvents: readonly FreeEvent[] | null | undefined,
    opts?: {
      events?: readonly TraceEvent[];
      tidToWorker?: Map<number, number>;
      sampleRateBytes?: number;
      memoryOverflows?: readonly MemoryOverflowEvent[];
    }
  ): AllocationAnalysis;

  // ── Rendering helpers (poll heatmap / LOD) ────────────────────────────

  /** Continuous log-scale `#rrggbb` color for a poll duration (ns). */
  export function pollHeatmapColor(durationNs: number): string;

  /** Quantized (bucketed, cached) variant for LOD rendering. Default 24 bins. */
  export function pollHeatmapColorQuantized(
    durationNs: number,
    bins?: number
  ): string;

  /**
   * Stateful merger collapsing adjacent same-color pixel-space bars into
   * one rectangle per run. Bars MUST be pushed in ascending x1 order; call
   * flush() after the last push.
   */
  export function makeBarCoalescer(
    emit: (left: number, width: number, color: string) => void,
    minWidth?: number
  ): {
    push(x1: number, x2: number, color: string): void;
    flush(): void;
  };

  /**
   * Pixel-level LOD downsampling: at most one representative span per pixel
   * column (largest weight whose start falls in the column). `spans` MUST
   * be sorted ascending by start.
   */
  export function pixelDownsampleSpans<S extends { start: number }>(
    spans: readonly S[],
    startIdx: number,
    viewStart: number,
    viewEnd: number,
    pw: number,
    weightOf: (span: S) => number
  ): S[];

  /**
   * Per-pixel coverage histogram (0..1 per column) for a sorted,
   * non-overlapping span array.
   */
  export function pixelCoverage(
    spans: readonly { start: number; end: number }[],
    startIdx: number,
    viewStart: number,
    viewEnd: number,
    pw: number
  ): Float64Array;
}
