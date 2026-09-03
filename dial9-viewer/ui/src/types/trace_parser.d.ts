// Type declarations for the frozen-core file `trace_parser.js`.
// See src/types/decode.d.ts for the declaration-form rationale (ambient
// wildcard module matching any relative import ending in `/trace_parser.js`).
//
// Trace-format backwards compatibility: fields added to the wire format later
// than the first release are optional here (e.g. `tid` on park/unpark
// TraceEvents) because old traces won't carry them. Block-in-place gap
// nullability (the gap is unknowable) is encoded explicitly on
// `BlockInPlaceGap`, `TraceEvent.tid` and the OFF_WORKER_WORKER_ID sentinel.

declare module "*/trace_parser.js" {
  /** Numeric event type ids used in `TraceEvent.eventType`. */
  export const EVENT_TYPES: {
    readonly PollStart: 0;
    readonly PollEnd: 1;
    readonly WorkerPark: 2;
    readonly WorkerUnpark: 3;
    readonly QueueSample: 4;
    readonly WakeEvent: 9;
  };

  /**
   * Sentinel `workerId` for CPU samples that cannot be confidently attributed
   * to a specific worker (matches producer-side `WorkerId::UNKNOWN`).
   * Samples falling inside a block-in-place gap are rewritten to this value.
   */
  export const OFF_WORKER_WORKER_ID: number;

  /**
   * A normalized runtime event. The parser materializes every field with a
   * zero/null default for event types that don't carry it (see processFrame),
   * so these are non-optional except the genuinely absent-able ones.
   */
  export interface TraceEvent {
    /** One of EVENT_TYPES values. */
    eventType: number;
    /** Monotonic nanoseconds. */
    timestamp: number;
    workerId: number;
    localQueue: number;
    globalQueue: number;
    cpuTime: number;
    /** null when this park->unpark carried no sched_wait_ns (distinct from a
     * sampled 0); the parser decodes the absent field to null. */
    schedWait: number | null;
    taskId: number;
    spawnLocId: string | null;
    spawnLoc: string | null;
    /**
     * OS thread id. Only set on WorkerPark/WorkerUnpark events, and only for
     * traces new enough to carry it (`undefined` on old traces -- the
     * block-in-place gap detection skips events without it).
     */
    tid?: number | undefined;
    /** WakeEvent only. */
    wakerTaskId?: number;
    /** WakeEvent only. */
    wokenTaskId?: number;
    /** WakeEvent only. */
    targetWorker?: number;
  }

  export interface CpuSample {
    /** Monotonic nanoseconds. */
    timestamp: number;
    /**
     * Worker attribution, re-derived from park/unpark tids at parse time.
     * May be the OFF_WORKER_WORKER_ID sentinel (255) for samples that cannot
     * be attributed -- including samples inside a block-in-place gap.
     */
    workerId: number;
    tid: number;
    /** 0 = on-CPU profiling sample, 1 = scheduling (off-CPU) sample. */
    source: number;
    /** Address strings like "0x55cc6d053893". */
    callchain: string[];
    /** CPU id the sample was taken on; null when the backend couldn't tell. */
    cpu: number | null;
    /**
     * Assigned by trace_analysis.js `attachCpuSamples`: spawn location of the
     * poll the sample landed in (null when outside any poll / unknown).
     * Absent until attachCpuSamples has run.
     */
    spawnLoc?: string | null;
    /**
     * Assigned by trace_analysis.js `attachCpuSamples`: whether the sample
     * landed inside a poll span. Absent until attachCpuSamples has run.
     */
    inPoll?: boolean;
  }

  /** A resolved stack frame (from SymbolTableEntry events). */
  export interface SymbolFrame {
    symbol: string;
    /** "file.rs:123" (or bare file); null when unknown. */
    location: string | null;
  }

  /**
   * address -> resolved symbol. An array value means inlined frames,
   * ordered [outermost, ...inlined callees].
   */
  export type CallframeSymbols = Map<string, SymbolFrame | SymbolFrame[]>;

  /** A completed span projected from one annotated event schema. */
  export interface SingleEventSpan {
    start: number;
    end: number;
    name: string;
    spanType: string;
    threadId: number | null;
    taskId: number | null;
    workerId: number | null;
    /** Event fields excluding recognized structural-role fields. */
    fields: Record<string, import("*/decode.js").DecodedFieldValue>;
    /** Units for projected attribute fields. */
    units: Record<string, string> | null;
  }

  export interface TidWorkerBinding {
    timestamp: number;
    workerId: number;
  }

  /** An event type the parser doesn't recognize, captured verbatim. */
  export interface CustomTraceEvent {
    /** Schema name, e.g. "SpanEnter:{target}::{name}:{file}:{line}". */
    name: string;
    /** Monotonic nanoseconds. */
    timestamp: number;
    /**
     * Decoded wire fields. Genuinely dynamic: the on-wire schema defines
     * them, so values are the decoder's field-value union.
     */
    fields: Record<string, import("*/decode.js").DecodedFieldValue>;
    /** field name -> unit annotation ("ns", "bytes", ...), if any. */
    units: Record<string, string> | null;
    /** field name -> chart interpretation annotation, if any. */
    fieldKinds: Record<string, string> | null;
    /** Present when schema annotations project this event as a completed span. */
    singleEventSpan?: SingleEventSpan | null;
  }

  export interface AllocEvent {
    timestamp: number;
    tid: number;
    size: number;
    /** Pointer address as a decimal string (BigInt-safe). */
    addr: string;
    callchain: string[];
  }

  export interface FreeEvent {
    timestamp: number;
    tid: number;
    /** Pointer freed, as a decimal string (BigInt-safe). */
    addr: string;
    size: number;
    /** Timestamp of the matching allocation (denormalized). */
    allocTimestampNs: number;
  }

  export interface MemoryOverflowEvent {
    timestamp: number;
    droppedAllocs: number;
    droppedFrees: number;
  }

  export interface TaskDump {
    timestamp: number;
    callchain: string[];
  }

  export interface ClockSyncAnchor {
    monotonicNs: number;
    realtimeNs: number;
  }

  /**
   * One per-runtime scheduler-metrics sample, decoded from a
   * `RuntimeMetricsEvent`. Every runtime attached to the session emits one per
   * flush cycle, all sharing the cycle's timestamp `t`.
   */
  export interface RuntimeMetricsSample {
    /** Monotonic sample timestamp (ns). Shared by all runtimes in one cycle. */
    t: number;
    /** Runtime name; empty string for the unnamed default runtime. */
    runtimeName: string;
    /** Tasks pending in this runtime's global (injection) queue. */
    globalQueue: number;
    /**
     * Tasks alive (spawned, not yet completed) in this runtime. Null for old
     * RuntimeMetricsEvent schemas that predate this field.
     */
    aliveTasks: number | null;
  }

  /** Process-wide active-task sample decoded from the superseded QueueSampleEvent. */
  export interface LegacyActiveTaskSample {
    /** Monotonic sample timestamp (ns). */
    t: number;
    /** Tasks alive across all attached runtimes. */
    count: number;
  }

  /**
   * A detected block-in-place handoff interval. Worker attribution inside
   * [startNs, endNs) is unknowable: samples in the gap have workerId
   * rewritten to OFF_WORKER_WORKER_ID, and active spans crossing a gap are
   * discarded by buildWorkerSpans.
   */
  export interface BlockInPlaceGap {
    workerId: number;
    fromTid: number;
    toTid: number;
    startNs: number;
    endNs: number;
  }

  export interface ParsedTrace {
    magic: "D9TF";
    version: number;
    events: TraceEvent[];
    /** Earliest/latest event timestamp (ns); null when there are no events. */
    minTs: number | null;
    maxTs: number | null;
    /** Bounds over ALL sliceable timestamped records; null if none. */
    recordMinTs: number | null;
    recordMaxTs: number | null;
    truncated: boolean;
    timeFiltered: boolean;
    /** Time-range filter bounds (ns); null when unfiltered. */
    filterStartTime: number | null;
    filterEndTime: number | null;
    hasCpuTime: boolean;
    hasSchedWait: boolean;
    hasTaskTracking: boolean;
    /**
     * False when poll events cover only dial9-spawned tasks rather than every
     * task on the runtime.
     */
    hasFullTaskCoverage: boolean;
    /** False when per-worker queue depth has no source and records 0. */
    hasLocalQueueDepth: boolean;
    /**
     * False when the trace contains no task spawn events, so task lifetimes
     * are absent rather than zero-length.
     */
    hasTaskLifetimes: boolean;
    spawnLocations: Map<string, string>;
    /** task id -> spawn location (null if unknown). */
    taskSpawnLocs: Map<number, string | null>;
    taskSpawnTimes: Map<number, number>;
    taskTerminateTimes: Map<number, number>;
    taskInstrumented: Map<number, boolean>;
    cpuSamples: CpuSample[];
    allocEvents: AllocEvent[];
    freeEvents: FreeEvent[];
    memoryOverflows: MemoryOverflowEvent[];
    callframeSymbols: CallframeSymbols;
    threadNames: Map<number, string>;
    /** tid -> worker id (derived from park/unpark events). */
    tidToWorker: Map<number, number>;
    /** Historical TID bindings, ordered by monotonic timestamp. */
    tidBindings?: Map<number, TidWorkerBinding[]>;
    /** TIDs that mapped to exactly one worker throughout the parsed trace. */
    stableTidToWorker?: Map<number, number>;
    /** runtime name -> worker ids. */
    runtimeWorkers: Map<string, number[]>;
    /** Latest segment-metadata key -> value. */
    segmentMetadata: Map<string, string>;
    /**
     * Per-runtime scheduler-metrics samples (one per runtime per flush cycle),
     * in wire order. Low-volume, so kept as a plain array rather than routed
     * through the columnar event store. Empty for old traces that predate
     * `RuntimeMetricsEvent` (those carry a summed `QueueSample` instead).
     */
    runtimeMetrics: RuntimeMetricsSample[];
    /**
     * Process-wide active-task samples from the transitional QueueSampleEvent
     * format. Empty for traces before that field and for current traces, which
     * carry per-runtime counts in `runtimeMetrics`.
     */
    legacyActiveTaskSamples: LegacyActiveTaskSample[];
    customEvents: CustomTraceEvent[];
    /**
     * Columnar span-producing custom events (SpanEnter/Exit/Close and annotated
     * single-event spans), present only on the main-thread columnar load path
     * (spanEventSink). When set, these events are NOT in `customEvents` and
     * buildSpanDataColumnar reads them. Undefined on the fat/worker path, where
     * they remain inside `customEvents`.
     */
    spanEvents?: import("../lib/trace/columnar-span-events.js").ColumnarSpanEvents;
    /** task id -> async stack captures, sorted by timestamp. */
    taskDumps: Map<number, TaskDump[]>;
    clockSyncAnchors: ClockSyncAnchor[];
    /** Monotonic-to-wall-clock offset; null when no anchor exists. */
    clockOffsetNs: number | null;
    /** Sorted by startNs. */
    blockInPlaceGaps: BlockInPlaceGap[];
  }

  /** Progress callback payload for the buffer/stream parse paths. */
  export interface ParseProgress {
    bytesRead: number;
    /** null while streaming (total size unknown until the stream ends). */
    totalBytes: number | null;
    eventCount: number;
  }

  /** Progress callback payload for the (Node-only) directory parse path. */
  export interface DirParseProgress {
    done: number;
    total: number;
    file: string | null;
    cached?: number;
  }

  /**
   * The event-store contract a `ParseOptions.eventSink` must satisfy: the
   * parser writes with `push`, and finalizeParse + deriveBlockInPlaceGaps read
   * `length` and iterate. A plain TraceEvent[] satisfies it structurally.
   */
  export interface EventSink {
    push(event: TraceEvent): void;
    readonly length: number;
    [Symbol.iterator](): Iterator<TraceEvent>;
  }

  /**
   * The cpu-sample contract a `ParseOptions.cpuSampleSink` must satisfy.
   * `rawCallchain` holds the undecoded frame values; the sink owns how (or
   * whether) it materializes them.
   */
  export interface CpuSampleSink {
    pushSample(
      timestamp: number,
      workerId: number,
      tid: number,
      source: number,
      rawCallchain: readonly unknown[],
      cpu: number | null
    ): void;
  }

  /**
   * The span-event contract a `ParseOptions.spanEventSink` must satisfy.
   * Returns true when the event produces a span and the sink consumed it, so
   * the parser knows to keep it out of the fat customEvents array.
   */
  export interface SpanEventSink {
    pushIfSpan(
      name: string,
      timestamp: number,
      fields: Record<string, import("*/decode.js").DecodedFieldValue>,
      singleEventSpan: SingleEventSpan | null
    ): boolean;
  }

  export interface ParseOptions {
    /** Cap event count (metadata/symbols always parsed). Default Infinity. */
    maxEvents?: number;
    /** Filter events to a time range (absolute ns, inclusive). */
    startTime?: number;
    endTime?: number;
    onParseProgress?: (progress: ParseProgress & DirParseProgress) => void;
    /**
     * Optional pluggable event store, used in place of the default plain array
     * for `state.events`. The viewer passes a columnar sink
     * (src/lib/trace/columnar-events.ts ColumnarEvents) whose `.push(event)`
     * writes fields into typed-array columns and drops the object, so a large
     * trace never materializes millions of fat event objects. A plain
     * TraceEvent[] satisfies EventSink, which is the default.
     */
    eventSink?: EventSink;
    /**
     * Optional columnar cpu-sample sink (src/lib/trace/columnar-cpu-samples.ts
     * ColumnarCpuSamples). Its `.pushSample(...)` stores each sample's callchain
     * in a flat frame-index pool instead of a per-sample hex array, so a
     * perf-capture trace's cpuSamples drop from ~2.3 GB to ~0.5 GB. Defaults to
     * a plain array of fat samples.
     */
    cpuSampleSink?: CpuSampleSink;
    /**
     * Optional columnar sink for span-producing custom events (src/lib/trace/
     * columnar-span-events.ts ColumnarSpanEvents). Its
     * `.pushIfSpan(name, ts, v, singleEventSpan)`
     * routes SpanEnter/Exit/Close and annotated single-event spans into typed columns
     * (the ~2.3 GB of fat span-event objects on a heavily-instrumented trace);
     * other custom events stay in the fat customEvents array.
     * buildSpanDataColumnar reads the columns.
     */
    spanEventSink?: SpanEventSink;
    /** Directory parsing only (Node). */
    cache?: boolean;
    parallel?: boolean;
    force?: boolean;
    sample?: number;
  }

  /**
   * Parse dial9 trace data.
   * - Buffer input returns Promise<ParsedTrace> (browser path).
   * - String path/directory input (Node-only) returns an AsyncIterable
   *   yielding one ParsedTrace per file (also awaitable for a single file).
   */
  export function parseTrace(
    input: ArrayBuffer | Uint8Array,
    options?: ParseOptions
  ): Promise<ParsedTrace>;
  export function parseTrace(
    input: string,
    options?: ParseOptions
  ): AsyncIterable<ParsedTrace>;

  /**
   * Parse a trace from an async stream of raw (already-gunzipped) byte
   * chunks; decoding overlaps the download. Same result as parsing the
   * concatenated buffer.
   */
  export function parseTraceStream(
    chunks: AsyncIterable<Uint8Array>,
    options?: ParseOptions
  ): Promise<ParsedTrace>;

  /** Parse a single trace file/buffer; always a Promise (Node accepts paths). */
  export function parseOne(
    input: ArrayBuffer | Uint8Array | string,
    options?: ParseOptions
  ): Promise<ParsedTrace>;

  export interface FetchOptions {
    signal?: AbortSignal;
    /**
     * Attached to every same-origin request (bring-your-own-credentials
     * headers); withheld from cross-origin URLs.
     */
    headers?: Record<string, string>;
  }

  /**
   * Fetch one or more trace URLs, gunzip each component, and concatenate
   * into a single ArrayBuffer (the decoder treats the concatenation as
   * multiple segments).
   */
  export function fetchTraces(
    urls: string | string[],
    opts?: FetchOptions
  ): Promise<ArrayBuffer>;

  /**
   * Fetch a single trace URL as an async iterable of raw (gunzipped)
   * chunks; pair with parseTraceStream so download and decode overlap.
   */
  export function fetchTraceStream(
    url: string,
    opts?: FetchOptions
  ): Promise<AsyncIterable<Uint8Array>>;

  /**
   * Stream MULTIPLE trace URLs as one logical trace, chunks emitted in
   * `urls` order. Fetches are dispatched concurrently up front.
   */
  export function fetchTracesStream(
    urls: string | string[],
    opts?: FetchOptions
  ): AsyncIterable<Uint8Array>;

  /** Whether this runtime can stream-fetch + incrementally gunzip. */
  export function canStreamDecode(): boolean;

  /**
   * Format a stack frame for display. Accepts a resolved frame, or an
   * address string plus the symbol map.
   */
  export function formatFrame(
    frame: SymbolFrame | string,
    callframeSymbols?: CallframeSymbols
  ): { text: string; docsUrl: string | null };

  /**
   * Resolve a callchain (address strings) to frames. Leaf→root in, leaf→root
   * out: an address's inlined frames are expanded in place innermost callee
   * first, so `[0]` is always the leaf.
   */
  export function symbolizeChain(
    callchain: readonly string[],
    callframeSymbols: CallframeSymbols
  ): SymbolFrame[];

  export interface SampleGroup {
    count: number;
    frames: SymbolFrame[];
    leaf: string;
    leafRaw: string;
  }

  /** Deduplicate CPU/sched samples by symbolized stack, sorted by count desc. */
  export function deduplicateSamples(
    samples: readonly { callchain: string[] }[],
    callframeSymbols: CallframeSymbols
  ): SampleGroup[];

  /**
   * Derive block-in-place gaps from park/unpark events and rewrite
   * `cpuSamples[i].workerId` to OFF_WORKER_WORKER_ID for samples inside a
   * gap (mutates cpuSamples). Returns gaps sorted by startNs.
   */
  export function deriveBlockInPlaceGaps(
    events: readonly TraceEvent[],
    cpuSamples: CpuSample[]
  ): BlockInPlaceGap[];
}
