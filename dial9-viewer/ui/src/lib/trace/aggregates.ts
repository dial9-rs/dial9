// Wire types for the tier-1 server aggregate endpoints `/api/flamegraph` and
// `/api/tokio-stats`, plus the tokio-stats URL builder and the coverage
// full/partial/none classifier. Both endpoints stream a fresh snapshot per
// folded file over SSE (see src/lib/trace/sse.ts), so the client owns no
// fetch/refine loop; it just consumes the stream and reads `coverage` to badge
// partial results. Wire field names are the JSON as served (snake_case).

// ─── Coverage: the tier-1 fallback signal ────────────────────────────────

/**
 * How much of a scope has been folded so far, reported on every
 * demand-driven query. Counts only - the client computes the
 * full/partial/none signal (coverageSignal).
 */
export interface Coverage {
  /** Source files matching the scope. */
  files_matched: number;
  /** Of those, how many the aggregation has folded so far. */
  files_folded: number;
  /**
   * Samples aggregated this response. (tokio-stats repurposes this as
   * files READ this request - see TokioStatsResponse.coverage.)
   */
  samples_folded: number;
  /** Total bytes of all matched source files in the scope. */
  total_bytes: number;
  /** Distinct hosts across the matched set (the scope's fleet breadth). */
  hosts_matched: number;
  /** Distinct hosts among the folded files. */
  hosts_folded: number;
  /**
   * Digest of the folded leaf set actually represented. Lets a partial refresh
   * confirm it is patching statistics built from exactly the same files, not
   * merely the same file COUNT. Absent from endpoints that do not track their
   * represented set, and from servers predating it.
   */
  folded_set_id?: string;
  /**
   * Digest of the full cached seed set this stream is working toward. Seed-only
   * exemplar refreshes carry it on every cumulative event so a client can
   * preview subsets only when the final target matches the catalog it is
   * patching. Absent outside those streams.
   */
  target_folded_set_id?: string;
  /**
   * Matched files in the deterministic prefix eligible for NEW folding this
   * request. Cached coverage may exceed it, which is why the refine ceiling
   * grows from this rather than from `files_folded`. Absent on older servers.
   */
  fold_work_cap?: number;
  /** Files whose fold failed this stream (fetch/decode/write error). */
  fold_errors?: number;
  /** A representative fold error message, absent when `fold_errors` is 0. */
  fold_error_sample?: string;
}

/**
 * The consumer-facing coverage signal: "none" -> fall back to
 * listing-metadata density, "partial" -> usable but badge it (never
 * silently wrong), "full" -> trust it.
 */
export type CoverageSignal = "full" | "partial" | "none";

/**
 * Classify a response's coverage:
 *  - no coverage block -> "full": the server only omits it outside
 *    demand-driven mode, where the single fetch covers everything;
 *  - nothing matched or nothing folded yet -> "none" (zero aggregate
 *    data - a cold first poll looks like this);
 *  - folded < matched on files OR hosts -> "partial" (the host check
 *    catches a folded slice that misses part of the fleet's breadth);
 *  - otherwise -> "full".
 */
export function coverageSignal(coverage: Coverage | null | undefined): CoverageSignal {
  if (coverage == null) return "full";
  if (coverage.files_matched === 0 || coverage.files_folded === 0) return "none";
  if (
    coverage.files_folded < coverage.files_matched ||
    coverage.hosts_folded < coverage.hosts_matched
  ) {
    return "partial";
  }
  return "full";
}

/**
 * Coverage is "frozen" when files_folded does not increase between two
 * consecutive polls - the refinement loop's stop condition. `prev` is the
 * previous poll's coverage (null/undefined on the first poll, which is
 * never frozen).
 */
export function isCoverageFrozen(
  prev: Coverage | null | undefined,
  curr: Coverage
): boolean {
  if (prev == null) return false;
  return curr.files_folded <= prev.files_folded;
}

// ─── Wire types: /api/flamegraph ─────────────────────────────────────────

/**
 * One node of the server-built flamegraph tree. Distinct from the
 * client-built `FlamegraphNode` (analysis.ts) - pages convert before handing
 * to the widget, hence the `Api` prefix.
 */
export interface ApiFlamegraphNode {
  name: string;
  /** Total sample count (self + descendants). */
  count: number;
  /** Self-time sample count (serde renames `self_count` to `self`). */
  self: number;
  /** Absent (not `[]`) for leaves: serde skips empty vecs. */
  children?: ApiFlamegraphNode[];
}

/** One nested interned-v1 node; `frame` indexes the enclosing frame table. */
export interface InternedApiFlamegraphNode {
  frame: number;
  count: number;
  self: number;
  children?: InternedApiFlamegraphNode[];
}

/** Negotiated compact response used by the canonical aggregate UI. */
export interface InternedApiFlamegraphTree {
  format: "interned-v1";
  frames: string[];
  root: InternedApiFlamegraphNode;
}

/** Preorder `[parent_node, frame, count, self_count]` row. */
export type FlatApiFlamegraphNode = [
  parent: number,
  frame: number,
  count: number,
  selfCount: number,
];

export interface FlatApiFlamegraphTree {
  format: "flat-v1";
  frames: string[];
  nodes: FlatApiFlamegraphNode[];
}

export type ApiFlamegraphTree =
  | ApiFlamegraphNode
  | InternedApiFlamegraphTree
  | FlatApiFlamegraphTree;

/** One facet's values. */
export interface FacetResult {
  /** Query-param / filter key name (e.g. "source", "thread_class"). */
  name: string;
  /** Human label for a toolbar selector (e.g. "Source", "Thread"). */
  label: string;
  /** Sorted distinct values seen across the folded set. */
  values: string[];
}

/**
 * The resolved query scope echoed back by the server (the selection it
 * actually applied), so headers render backend truth rather than URL params
 * the client guessed at.
 */
export interface ScopeEcho {
  service: string | null;
  /** The host filter the query was scoped to (empty = all hosts). */
  hosts: string[];
  start_ns: number | null;
  end_ns: number | null;
  /** Active facet filter values (facet name -> value, "" = all). */
  filters: Record<string, string>;
}

/**
 * One sample-weighted, log2-bucketed poll-duration histogram bar
 * (metadata.poll_duration_histogram), ascending by range. Feeds the api-mode
 * minimap. Optional on the wire: absent outside poll-aware aggregation and on
 * older servers, so read it tolerantly.
 */
export interface PollDurationBar {
  lo_ns: number;
  hi_ns: number;
  samples: number;
}

export interface FlamegraphMetadata {
  service: string | null;
  /** Distinct hosts that passed all filters. */
  hosts: number;
  /** "from-to" echo of the from/to params; null unless both were sent. */
  time_range: string | null;
  /** Min/max sample timestamps in the result (epoch ns), null when empty. */
  min_timestamp_ns: number | null;
  max_timestamp_ns: number | null;
  /** Toolbar facets, in display order. */
  facets: FacetResult[];
  scope: ScopeEcho;
  /** Sample-weighted poll-duration histogram (minimap); absent on old servers. */
  poll_duration_histogram?: PollDurationBar[];
}

export interface FlamegraphResponse {
  tree: ApiFlamegraphTree;
  total_samples: number;
  /**
   * Absent only outside demand-driven mode (a single complete fetch);
   * both aggregate endpoints currently always send it.
   */
  coverage?: Coverage;
  metadata: FlamegraphMetadata;
}

// ─── Wire types: /api/tokio-stats ────────────────────────────────────────

/** Worst poll per class, for deep-linking into the viewer. */
export interface PollExemplar {
  start_ns: number;
  end_ns: number;
  duration_ns: number;
  host: string;
  /** Source trace file key for constructing the viewer deep link. */
  source_key: string;
  /** Present on long-poll rows (refine the focus framing), absent on spawn-loc exemplars. */
  worker_id?: number;
  task_id?: number;
}

/** Long-poll stats for one spawn location. */
export interface SpawnLocStats {
  spawn_loc: string;
  total_polls: number;
  /** Poll durations above the server's 100us floor, sorted descending. */
  durations_ns: number[];
  /**
   * Classification per duration, same index as durations_ns:
   * 0=off-cpu, 1=on-cpu, 2=mixed, 3=unknown.
   */
  classes: number[];
  /** Worst exemplar per class, indexed 0=off-cpu 1=on-cpu 2=mixed 3=unknown. */
  exemplars: [
    PollExemplar | null,
    PollExemplar | null,
    PollExemplar | null,
    PollExemplar | null,
  ];
}

/**
 * One long-running poll for the "Longest polls" rollup (server `LongPoll`).
 * Extends PollExemplar so a row deep-links through the same exemplarLink seam;
 * unlike a spawn-loc exemplar the worker/task attribution is always present (the
 * server only emits a row when it can attribute the poll to a worker + task) and
 * it carries the spawn location.
 */
export interface LongPoll extends PollExemplar {
  worker_id: number;
  task_id: number;
  /** Where the future was spawned; absent when the trace didn't record it. */
  spawn_loc?: string;
}

/**
 * How a poll's ready time was established (server `SchedulingDelayKind`,
 * serialized snake_case). `"spawn"` = spawn -> first poll; `"wake"` = an idle
 * wake -> the next poll; `"wake_during_poll"` = a wake mid-poll, made effective
 * at the poll's explicit end.
 */
export type SchedulingDelayKind = "spawn" | "wake" | "wake_during_poll";

/**
 * One measured runnable-to-poll delay for the "Scheduling delay" rollup (server
 * `SchedulingDelay`). Worker/task are always present (the server only emits a
 * row when it can attribute the poll). Unlike LongPoll this does NOT extend
 * PollExemplar — the wire carries `ready_at_ns`/`poll_end_ns`, which the client
 * maps into the exemplar focus window (ready -> poll end) to deep-link.
 */
export interface SchedulingDelay {
  /** Runnable-to-poll delay (ns): poll_start_ns - ready_at_ns. */
  delay_ns: number;
  ready_at_ns: number;
  poll_start_ns: number;
  poll_end_ns: number;
  worker_id: number;
  task_id: number;
  /** How the ready time was established. */
  kind: SchedulingDelayKind;
  /** Where the future was spawned; absent when the trace didn't record it. */
  spawn_loc?: string;
  /** The task that woke this one, when the evidence is a wake. */
  waker_task_id?: number;
  host: string;
  /** Source trace file key for constructing the viewer deep link. */
  source_key: string;
}

/**
 * Measurement coverage for the scheduling-delay rollup (server
 * `SchedulingDelayCoverage`): how many polls carried usable readiness evidence
 * versus not, and why the unmeasured ones could not be inferred. Lets the UI
 * qualify the top-N with an honest denominator rather than implying every poll
 * was measured. All fields default to 0 on a server predating the rollup.
 */
export interface SchedulingDelayCoverage {
  observed_polls: number;
  unmeasured_polls: number;
  over_1ms_polls: number;
  spawn_inferred_polls: number;
  wake_observed_polls: number;
  wake_during_poll_polls: number;
  uninstrumented_unmeasured_polls: number;
  instrumentation_unknown_unmeasured_polls: number;
  missing_readiness_unmeasured_polls: number;
}

/**
 * One worker's busyness + poll distribution for the "Worker activity" rollup
 * (server `WorkerStats`). Workers are keyed per-host: worker 0 on host-A is a
 * different runtime instance from worker 0 on host-B, so consumers must group by
 * `host` before comparing ids.
 */
export interface WorkerStats {
  worker_id: number;
  host: string;
  /** All polls observed on this worker (including sub-floor). */
  total_polls: number;
  /** Sum of ALL poll durations on this worker (ns). */
  busy_ns: number;
  /**
   * The worker's OBSERVED active time (ns) — the `busy_pct` denominator, not a
   * wall-clock span. Exposed so the UI can show `busy_ns / span_ns = busy_pct`.
   */
  span_ns: number;
  /**
   * `busy_ns / span_ns * 100`. Bounded to <=100% because a worker's polls are
   * sequential, so `busy_ns <= span_ns`.
   */
  busy_pct: number;
  /** Polls above the server's duration floor on this worker. */
  notable_polls: number;
  /** Longest poll duration on this worker (drives heat-coloring). */
  worst_poll_ns: number;
  /** Exemplar of the worst poll, for deep-linking; absent when unavailable. */
  worst_exemplar?: PollExemplar;
}

export interface TokioStatsResponse {
  /** Time span covered by the data (ns), for per-minute rates. Min 1. */
  time_span_ns: number;
  total_polls: number;
  /** Source bucket (for constructing viewer deep links). */
  bucket: string;
  by_spawn_loc: SpawnLocStats[];
  /**
   * Longest individual polls across the scope, ranked descending by duration
   * (server top_long_polls, bounded to the top 100). Each row carries the
   * coordinates to deep-link its trace segment. Optional so consumers tolerate a
   * response that omits it (a server predating the rollup); read it as `[]`.
   */
  top_long_polls?: LongPoll[];
  /**
   * Longest runnable-to-poll scheduling delays, ranked descending by delay
   * (server top_scheduling_delays, bounded to the top 100). Optional so
   * consumers tolerate a response that omits it (a server predating the
   * rollup); read it as `[]`.
   */
  top_scheduling_delays?: SchedulingDelay[];
  /**
   * Coverage tallies for the scheduling-delay rollup. Optional; absent on a
   * server predating the rollup, in which case the section is hidden.
   */
  scheduling_delay_coverage?: SchedulingDelayCoverage;
  /**
   * Per-worker busyness + poll distribution. Optional so consumers tolerate a
   * response that omits it (a server predating the rollup); read it as `[]`.
   */
  worker_activity?: WorkerStats[];
  /**
   * See FlamegraphResponse.coverage. Quirk: tokio-stats reports files
   * READ this request as `samples_folded` (its folded unit is files).
   */
  coverage?: Coverage;
}

// ─── Request types ───────────────────────────────────────────────────────

/**
 * Scope params shared by both endpoints. All optional; omitted params are
 * left off the query string entirely (the server's serde defaults apply).
 */
export interface AggregateScope {
  /** S3 bucket override (bring-your-own-credentials mode). */
  bucket?: string;
  /**
   * AWS region the bucket lives in. Carried on the REQUEST URL (the server
   * reads it from the `aws_region` query param — the only place an ambient
   * cross-region read learns the region). Safe on a request URL: the server
   * tolerates region on both header and query (header wins). The reader-role
   * ARN deliberately has no field here — the role is header-only (a role on
   * both header and query is the server's ConflictingCredentials 400), restored
   * into the creds store at boot rather than put on the request URL.
   */
  aws_region?: string;
  /** S3 key prefix scoping the source segment listing. */
  prefix?: string;
  service?: string;
  /** Host filter; each entry becomes a repeated `host=` param. */
  host?: readonly string[];
  /**
   * Scope bounds, epoch nanoseconds. Accepts string because epoch-ns
   * values exceed Number.MAX_SAFE_INTEGER (~9.0e15 < ~1.7e18); pass the
   * string form (e.g. from url_state's pickerUtcToNs) to keep precision.
   */
  start_ns?: number | string;
  end_ns?: number | string;
  /**
   * Whether this poll may fold new source files. Serialized as the
   * literal `refine=true` (serde bool - "1" would 400); omitted when
   * false/absent, matching the server default.
   */
  refine?: boolean;
}

/** Query params for GET /api/tokio-stats (TokioStatsParams). */
export interface TokioStatsQuery extends AggregateScope {
  /** "Load more": raise the sampling-cap ceiling (server clamps it). */
  max_files?: number;
}

// ─── /api/span-stats ─────────────────────────────────────────────────────

/** One bucket of a span type's log-duration histogram. */
export interface SpanDurationBucket {
  lo_ns: number;
  hi_ns: number;
  count: number;
}

/**
 * The five-way time split for a span type or a single instance. `*_frac_sum`
 * fields sum each instance's own share, so dividing by `instance_count` gives
 * an EQUAL-WEIGHTED mean that a handful of very long spans cannot dominate;
 * the `*_ns` fields are the raw totals. Absent on servers that never computed
 * the breakdown.
 */
export interface TimeComposition {
  on_cpu_ns: number;
  blocked_ns: number;
  async_wait_ns: number;
  scheduler_delay_ns: number;
  unknown_ns: number;
  instance_count?: number;
  on_cpu_frac_sum?: number;
  blocked_frac_sum?: number;
  async_wait_frac_sum?: number;
  scheduler_delay_frac_sum?: number;
  unknown_frac_sum?: number;
}

/**
 * Composition bucketed by the same log-duration buckets as `histogram`, so a
 * brushed duration band sums the buckets inside it for a band-scoped split.
 */
export interface CompositionBucket extends TimeComposition {
  lo_ns: number;
  hi_ns: number;
  instance_count: number;
}

/** One span attribute on an exemplar (already stringified by the server). */
export interface ExemplarAttribute {
  key: string;
  value: string;
}

/** A slow span instance, carrying the coordinates to deep-link its segment. */
export interface Exemplar {
  elapsed_ns: number;
  span_uid: string;
  callsite_file?: string;
  callsite_line?: number;
  host: string;
  start_ns: number;
  end_ns: number;
  /** Source object key; `s3://bucket/key` or already bucket-relative. */
  source_key: string;
  composition?: TimeComposition;
  attributes: ExemplarAttribute[];
}

/** Top values observed for one attribute key across a span type. */
export interface AttributeFacet {
  key: string;
  first_seen_values: string[];
  occurrences: number;
  truncated: boolean;
  high_cardinality: boolean;
}

/** One span type's aggregated statistics. */
export interface SpanTypeStats {
  span_type_uid: string;
  kind: string;
  name: string;
  target?: string;
  callsite_file?: string;
  callsite_line?: number;
  /** Total instances observed (NOT capped by the memory bounds). */
  count: number;
  /** Streaming-approximate percentiles; absent when too few instances. */
  p50_ns?: number;
  p95_ns?: number;
  p99_ns?: number;
  max_ns?: number;
  min_ns?: number;
  histogram: SpanDurationBucket[];
  composition?: TimeComposition;
  composition_histogram?: CompositionBucket[];
  details_complete_count: number;
  partial_count: number;
  exemplars: Exemplar[];
  /** Exact instance count inside the requested duration bounds, if any. */
  selected_duration_count?: number;
  attribute_facets: AttributeFacet[];
  /** True when more distinct attribute keys exist than were tracked. */
  attribute_keys_overflow: boolean;
}

export interface SpanStatsResponse {
  span_types: SpanTypeStats[];
  coverage?: Coverage;
  /** True when more span types exist than the response carries. */
  types_truncated: boolean;
  total_span_types_tracked: number;
  types_overflow_instances: number;
}

// The /api/span-stats request URL is built by the Span Explorer itself
// (pages/span-explorer/scope.ts buildApiUrl), which owns the per-stream-mode
// param set; there is deliberately no shared builder for it here.

/** Query params for GET /api/flamegraph (FlamegraphParams). */
export const TOKIO_STATS_ENDPOINT = "/api/tokio-stats";

/** Append scope params common to both endpoints. */
function appendScope(search: URLSearchParams, scope: AggregateScope): void {
  if (scope.bucket !== undefined) search.set("bucket", scope.bucket);
  if (scope.aws_region !== undefined) search.set("aws_region", scope.aws_region);
  if (scope.prefix !== undefined) search.set("prefix", scope.prefix);
  if (scope.service !== undefined) search.set("service", scope.service);
  for (const h of scope.host ?? []) search.append("host", h);
  if (scope.start_ns !== undefined) search.set("start_ns", String(scope.start_ns));
  if (scope.end_ns !== undefined) search.set("end_ns", String(scope.end_ns));
}

/** Build the GET /api/tokio-stats URL (path + query string). */
export function tokioStatsUrl(query: TokioStatsQuery): string {
  const search = new URLSearchParams();
  appendScope(search, query);
  if (query.max_files !== undefined) search.set("max_files", String(query.max_files));
  if (query.refine) search.set("refine", "true");
  const qs = search.toString();
  return qs ? `${TOKIO_STATS_ENDPOINT}?${qs}` : TOKIO_STATS_ENDPOINT;
}
