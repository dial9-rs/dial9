// lib/trace barrel - THE typed boundary around the frozen core. This
// directory is the only place in src/ permitted to import the frozen core
// (decode.js, trace_parser.js, trace_analysis.js) directly; everything else
// consumes this barrel (lib/canvas re-exports the core's drawing math
// through its own barrel under the same rule). Explicit named re-exports,
// not `export *`: a name collision must be a compile error, never a
// silently-omitted export.

// keys.ts - S3 key parsing with the known/unknown layout discriminant.
export { extractPrefix, formatEpoch, parseKey } from "./keys.js";
export type {
  EpochFormatOptions,
  KnownTraceKey,
  ParsedTraceKey,
  UnknownTraceKey,
} from "./keys.js";

// title.ts - shared viewer/flamegraph header metadata.
export { traceTitleParams } from "./title.js";

// segment-metadata.ts - trace-embedded service/host identity and its
// reconciliation against the key-derived svc/host URL params.
export {
  SEGMENT_HOST_KEY,
  SEGMENT_SERVICE_KEY,
  readKeyDerivedIdentity,
  readSegmentIdentity,
  readSegmentMetadataEntries,
  reconcileIdentity,
} from "./segment-metadata.js";
export type { IdentityField, ReconciledIdentity } from "./segment-metadata.js";

// format.ts - frozen format.js field-value formatter (span metadata/tooltip).
export { formatFieldValue } from "./format.js";

// prefixes.ts - S3 prefix-discovery heuristics (frozen prefix_detect.js).
export { isDateLayer, lastSegment, preferredPrefix } from "./prefixes.js";

// load.ts - load orchestration + the trace_parser.js surface.
export {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  canStreamDecode,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
  fetchTraceBytes,
  formatFrame,
  loadTrace,
  loadTraceBuffered,
  loadTraceInWorker,
  loadTraceOnMainThread,
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
  SingleEventSpan,
  SymbolFrame,
  TaskDump,
  TidWorkerBinding,
  TraceEvent,
  TraceSliceStore,
  WorkerLoadOptions,
  WorkerLoadResult,
  WorkerTraceLoad,
} from "./load.js";

// load-perf.ts - opt-in User Timing instrumentation for the load path
// (`?perf=1` / localStorage "dial9.viewer.perf"). Pages read the flag to decide
// whether to log loader-reported timings alongside these marks.
export { isLoadPerfEnabled, startLoadPerf, measureSpan } from "./load-perf.js";
export type { LoadPerfRecorder, LoadPerfStats, LoadPhase } from "./load-perf.js";

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

// segments.ts - segment-windowed loading: budget constants, extent
// derivation, window decision functions, the raw-gzip cache, boundary-poll
// stitching/truncation, and the viewport-driven orchestrator.
export {
  createSegmentWindow,
  deriveSegmentExtents,
  mapExtentToMonotonic,
  parseSegmentInWorker,
} from "./segments.js";

// segment-boundary-polls.ts - the truncation hard edge: what a segment left
// open, what the next one closed, and the stitched whole polls.
export {
  computeSegmentEdgePolls,
  computeWindowBoundaryPolls,
  segmentInvariants,
} from "./segment-boundary-polls.js";

// raw-byte-cache.ts - the compressed lower level of the two-level cache.
// segment-budget.ts - residency budgets and the pure need/prefetch/admit/
// evict decisions computed from them.
export {
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
export type {
  AdmissionCandidate,
  AdmissionPlan,
  EvictionPlan,
  EvictionPlanInput,
  ResidentSegment,
} from "./segment-budget.js";

export { createRawByteCache } from "./raw-byte-cache.js";
export type { RawByteCache } from "./raw-byte-cache.js";
export type {
  DerivedExtents,
  ListedSegment,
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
  enclosingSpansColumnar,
  findContainingSpan,
  findSpanAt,
  spanAncestryAt,
  spansById,
  taskAt,
} from "./query.js";
export type { SpanAncestry, SpanList } from "./query.js";

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
  globalQueueSeries,
  hasCpuProfileSamples,
  selectSpanRenderSet,
  sumGlobalQueueByCycle,
} from "./analysis.js";
export type {
  ActiveSpan,
  AllocationAnalysis,
  AllocationSite,
  FlamegraphNode,
  FlamegraphSampleInput,
  FlamegraphTreeOptions,
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

// The lane union: a lane is fat objects on small traces and a columnar view on
// big ones, so consumers take LaneSpans and narrow with isColumnarLane.
export { fatLanes, isColumnarLane, laneSource } from "./columnar-worker-spans.js";
export type {
  LaneSource,
  LaneSpans,
  LaneWorkerSpans,
  ParkLike,
  WorkerLaneView,
} from "./columnar-worker-spans.js";

// creds.ts - the frozen creds.js store, typed. Trace fetches carry its
// x-dial9-aws-* headers.
export { Dial9Creds } from "./creds.js";
export type {
  BucketInfo,
  CredentialCheckResult,
  Dial9CredsApi,
  SetCredentialsInput,
  StoredCredentials,
} from "./creds.js";

// session.ts - opaque tab-scoped request correlation for same-origin APIs.
export { Dial9Session } from "./session.js";
export type { Dial9SessionApi, SessionStorageLike } from "./session.js";

// api_format.ts - aggregated-mode (`?api=1`) display/format helpers,
// re-exported from flamegraph_api.js plus formatHumanDuration from format.js so
// consumers share one implementation.
export {
  coveragePercent,
  decodeFlamegraphTree,
  foldErrorNotice,
  formatCoverageBadge,
  formatHumanDuration,
  hostFacetOptions,
  msToNs,
  nextMaxFiles,
  nsToMs,
  nsToPickerUtc,
  pickerUtcToNs,
  refinementWorkDepth,
  shouldAdoptRefinementSnapshot,
} from "./api_format.js";
export type {
  DecodedFlamegraphNode,
  FacetOption,
  LegacyCoverage,
} from "./api_format.js";

// sse.ts - the fetch-based Server-Sent Events client for the streamed
// aggregation endpoints (the server owns the refine/stop loop).
export { openSse } from "./sse.js";
export type { SseOptions } from "./sse.js";

// trace_scope.ts - the compact, stateless scope codec (bucket/prefix/service/
// host-set + time window) that keeps a large selection's viewer/flamegraph/
// tokio-stats deep link under CloudFront's 8192-byte request-URI cap.
export {
  encodeAggregationParams,
  encodeScope,
  hasScope,
  readScope,
  resolveScope,
  scopeFromKeys,
} from "./trace_scope.js";
export type { EncodeScopeOptions, EncodedScope, TraceScope } from "./trace_scope.js";

// source-scope.ts - canonical bucket+region+credential identity and the safe
// projections used by browser URLs, built-in sharing, and API requests.
export {
  AMBIENT_CREDENTIALS,
  EMPTY_LITERAL_CREDENTIALS,
  EMPTY_SOURCE_SCOPE,
  applyToCreds,
  credentialHeadersForSource,
  credentialMode,
  isLiteralConfigured,
  isSourceShareable,
  makeSourceScope,
  readNamespacedSourceScope,
  readPlainSourceScope,
  sourceScopeFromStored,
  toShareableSourceScope,
  toUrlSourceScope,
  writeNamespacedParams,
  writeNamespacedUrlParams,
  writeRequestParams,
  writeShareableParams,
  writeUrlParams,
} from "./source-scope.js";
export type {
  AmbientCredentials,
  LiteralCredentials,
  RoleCredentials,
  Shareability,
  ShareableSourceScope,
  SourceCredentials,
  SourceScope,
  SourceScopeCredentials,
  StoredSourceCredentials,
  UrlCredentials,
  UrlSourceScope,
} from "./source-scope.js";

// exemplar-link.ts - the sole viewer deep-link builder for poll and span
// exemplars. It owns object-key normalization, safe credentials, and focus.
export { buildExemplarViewerUrl } from "./exemplar-link.js";
export type {
  ExemplarViewerLink,
  ViewerExemplar,
} from "./exemplar-link.js";

// aggregates.ts - server aggregate wire types (/api/flamegraph +
// /api/tokio-stats), the tokio-stats URL builder, and the coverage
// full/partial/none fallback signal. (Both endpoints stream over SSE now, so
// the old client-side fetch/refine loop is gone.)
export {
  TOKIO_STATS_ENDPOINT,
  coverageSignal,
  isCoverageFrozen,
  tokioStatsUrl,
} from "./aggregates.js";
export type {
  AggregateScope,
  ApiFlamegraphNode,
  ApiFlamegraphTree,
  AttributeFacet,
  CompositionBucket,
  Coverage,
  CoverageSignal,
  Exemplar,
  ExemplarAttribute,
  FacetResult,
  FlatApiFlamegraphNode,
  FlatApiFlamegraphTree,
  FlamegraphMetadata,
  FlamegraphCoverageEvent,
  FlamegraphResponse,
  FlamegraphStreamEvent,
  InternedApiFlamegraphNode,
  InternedApiFlamegraphTree,
  PollDurationBar,
  PollExemplar,
  SchedulingDelay,
  SchedulingDelayCoverage,
  SchedulingDelayKind,
  ScopeEcho,
  SpanDurationBucket,
  SpanStatsResponse,
  SpanTypeStats,
  SpawnLocStats,
  TimeComposition,
  TokioStatsQuery,
  TokioStatsResponse,
  WorkerStats,
} from "./aggregates.js";

// span_explorer.ts - shared Span Explorer helpers: catalog sorting, the
// log-duration histogram geometry + percentile estimation, the five-way time
// composition, attribute filters, and the flamegraph deep-link builder.
export {
  TIME_CATEGORIES,
  addAttrFilter,
  classifyExemplarSnapshot,
  collectExemplarAttributeKeys,
  columnIsDegenerate,
  completeExemplarRefresh,
  computeTimeComposition,
  countInBand,
  durationAtPercentile,
  exemplarAttrValue,
  exemplarRequestMatches,
  exemplarsInBand,
  flamegraphUrl,
  fmtNs,
  fmtPercentile,
  formatAttrFilterParams,
  hasAttrFilter,
  mergeSelectedExemplarSnapshot,
  normalizeSpanHistogram,
  parseAttrFilterParams,
  percentileForDuration,
  removeAttrFilter,
  setMaxFilesParam,
  shouldAdoptCatalogSnapshot,
  sortSpanTypes,
  spanBrushToBand,
  spanHistogramLayout,
  spanNsToPx,
  spanTypeLabel,
  spanTypeQuality,
} from "./span_explorer.js";
export type {
  AttrFilter,
  CompositionCategory,
  DurationBand,
  HistogramBarLike,
  SpanExplorerState,
  SpanHistogramBar,
  SpanHistogramColumn,
  SpanHistogramLayout,
  StreamMode,
  TimeCompositionView,
} from "./span_explorer.js";
