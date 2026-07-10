// lib/trace barrel - THE typed boundary around the frozen core (T09;
// architecture 2.7). This directory is the only place in src/ permitted
// to import the frozen core (decode.js, trace_parser.js,
// trace_analysis.js) directly - everything else consumes this barrel
// (lib/canvas re-exports the core's drawing math through its own barrel
// under the same rule; scripts/check-core-imports.mjs enforces both).
// Explicit named re-exports, not `export *`: a name collision between
// modules must be a compile error, never a silently-omitted export.

// keys.ts - S3 key parsing with the known/unknown layout discriminant.
export { formatEpoch, parseKey } from "./keys.js";
export type {
  EpochFormatOptions,
  KnownTraceKey,
  ParsedTraceKey,
  UnknownTraceKey,
} from "./keys.js";

// title.ts - shared viewer/flamegraph header metadata.
export { traceTitleParams } from "./title.js";

// load.ts - load orchestration + the trace_parser.js surface.
export {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  canStreamDecode,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
  formatFrame,
  loadTrace,
  loadTraceBuffered,
  loadTraceInWorker,
  loadTraceStreamed,
  objectTraceUrls,
  parseTraceBuffer,
  symbolizeChain,
} from "./load.js";
export type {
  AllocEvent,
  BlockInPlaceGap,
  CallframeSymbols,
  ClockSyncAnchor,
  CpuSample,
  CustomTraceEvent,
  DecodedFieldValue,
  FetchOptions,
  FreeEvent,
  LoadTraceOptions,
  LoadedTrace,
  MemoryOverflowEvent,
  ParseOptions,
  ParseProgress,
  ParsedTrace,
  SampleGroup,
  SymbolFrame,
  TaskDump,
  TraceEvent,
  TraceSliceStore,
  WorkerLoadOptions,
  WorkerLoadResult,
  WorkerTraceLoad,
} from "./load.js";

// worker/protocol.ts - the worker-boundary message vocabulary pages and
// transport adapters consume (the worker entry itself is not exported;
// load.ts's factory owns it).
export type {
  TraceWorkerFactory,
  TraceWorkerLoadMode,
  TraceWorkerPort,
  TraceWorkerProgress,
  TraceWorkerRequest,
  TraceWorkerResponse,
  TraceWorkerTiming,
} from "./worker/protocol.js";

// segments.ts - segment-windowed loading (tier 2, architecture 2.8):
// budget constants (N19), extent derivation, window decision functions,
// the raw-gzip cache, boundary-poll stitching/truncation, and the
// viewport-driven orchestrator.
export {
  BUDGET_EVICTION_THRESHOLD_FRACTION,
  GZIP_EXPANSION_ESTIMATE,
  RAW_GZIP_CACHE_BUDGET_BYTES,
  RESIDENT_RAW_BUDGET_BYTES,
  capToBudget,
  computeNeedSet,
  computePrefetchSet,
  computeSegmentEdgePolls,
  computeWindowBoundaryPolls,
  createRawByteCache,
  createSegmentWindow,
  deriveSegmentExtents,
  evictionTriggerBytes,
  extentDistance,
  extentsOverlap,
  mapExtentToMonotonic,
  parseSegmentInWorker,
  planEviction,
  segmentInvariants,
} from "./segments.js";
export type {
  AdmissionCandidate,
  AdmissionPlan,
  DerivedExtents,
  EvictionPlan,
  EvictionPlanInput,
  ListedSegment,
  RawByteCache,
  ResidentSegment,
  SegmentBytesFetcher,
  SegmentListing,
  SegmentParseJob,
  SegmentParseOptions,
  SegmentParseResult,
  SegmentParser,
  SegmentWindow,
  SegmentWindowOptions,
  SegmentWindowStats,
  SegmentsSliceStore,
  SkippedListing,
} from "./segments.js";

// reparse.ts - Set/Clear-Range in-memory windowed re-parse.
export { isRangeActive, reparseWithRange } from "./reparse.js";
export type { ReparseRange } from "./reparse.js";

// query.ts - per-interaction read helpers.
export {
  SPAN_ANCESTRY_CYCLE_LIMIT,
  enclosingSpans,
  findContainingSpan,
  findSpanAt,
  spanAncestryAt,
  spansById,
  taskAt,
} from "./query.js";
export type { SpanAncestry } from "./query.js";

// analysis.ts - the trace_analysis.js facade.
export {
  analyzeAllocations,
  attachCpuSamples,
  buildActiveTaskTimeline,
  buildFgData,
  buildFlamegraphTree,
  buildProcessCpuUsageSeries,
  buildRuntimeFilterData,
  buildSpanData,
  buildWorkerSpans,
  collectDescendants,
  computePollWakes,
  computeRuntimeGroups,
  computeSchedulingDelays,
  computeSpanLayout,
  filterPointsOfInterest,
  flattenFlamegraph,
  getTraceTimeRange,
  hasCpuProfileSamples,
  selectSpanRenderSet,
} from "./analysis.js";
export type {
  ActiveSpan,
  AllocationAnalysis,
  AllocationSite,
  FlamegraphNode,
  FlamegraphSampleInput,
  FlatFlamegraphNode,
  ParkSpan,
  PointOfInterest,
  PointOfInterestType,
  PollSpan,
  ProcessCpuUsageInterval,
  ProcessCpuUsageSample,
  RuntimeFilterData,
  RuntimeGroup,
  SchedDelay,
  SpanData,
  SpanLayoutBucket,
  SpanSegment,
  TaskWake,
  TracingSpan,
  UnmatchedSpan,
  WorkerLane,
  WorkerSpansResult,
  WorkerWake,
} from "./analysis.js";
