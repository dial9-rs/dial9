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

// aggregates.ts - tier-1 server aggregate endpoints client (T18):
// /api/flamegraph + /api/tokio-stats typed requests, the refine-polling
// helper, and the coverage full/partial/none fallback signal.
export {
  AggregatesRequestError,
  DEFAULT_MAX_REFINE_POLLS,
  FLAMEGRAPH_ENDPOINT,
  TOKIO_STATS_ENDPOINT,
  coverageSignal,
  fetchFlamegraph,
  fetchTokioStats,
  flamegraphUrl,
  isCoverageFrozen,
  refineUntilFrozen,
  tokioStatsUrl,
} from "./aggregates.js";
export type {
  AggregateResult,
  AggregateScope,
  AggregateUnavailableReason,
  AggregatesRequestOptions,
  ApiFlamegraphNode,
  Coverage,
  CoverageSignal,
  FacetResult,
  FetchLike,
  FlamegraphMetadata,
  FlamegraphQuery,
  FlamegraphResponse,
  PollExemplar,
  RefineUntilFrozenOptions,
  ScopeEcho,
  SpawnLocStats,
  TokioStatsQuery,
  TokioStatsResponse,
} from "./aggregates.js";
