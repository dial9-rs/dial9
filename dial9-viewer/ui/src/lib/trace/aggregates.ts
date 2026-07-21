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
  tree: ApiFlamegraphNode;
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

/** Query params for GET /api/flamegraph (FlamegraphParams). */
export const TOKIO_STATS_ENDPOINT = "/api/tokio-stats";

/** Append scope params common to both endpoints. */
function appendScope(search: URLSearchParams, scope: AggregateScope): void {
  if (scope.bucket !== undefined) search.set("bucket", scope.bucket);
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
