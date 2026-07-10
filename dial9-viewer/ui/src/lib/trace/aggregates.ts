// lib/trace/aggregates.ts - typed client for the tier-1 server aggregate
// endpoints `/api/flamegraph` and `/api/tokio-stats` (T18; architecture 2.8
// tier 1). These are the zero-raw-bytes overview sources: the Parquet
// aggregation pipeline folds raw trace segments server-side and both
// endpoints answer from the folded set, refining on demand.
//
// Response/request types are derived from the server source of truth:
// src/server/flamegraph.rs (FlamegraphParams/FlamegraphResponse),
// src/server/tokio_stats.rs (TokioStatsParams/TokioStatsResponse) and
// src/ingest/aggregate.rs (Coverage, FacetResult). Wire field names are
// kept verbatim (snake_case) - these interfaces describe JSON as served.
//
// Coverage hard edge (architecture 2.8 "Aggregate coverage"): the Parquet
// pipeline can lag or miss hosts, and the endpoints 404 when the server has
// no aggregation context at all. Consumers therefore never get a thrown
// error for "no data" - they get a `CoverageSignal` (full | partial | none)
// and fall back to listing-metadata density on `none`, badge on `partial`,
// never blocking on aggregates:
//   - config `aggregation_enabled` false -> `none`, WITHOUT a request
//     (short-circuited; note the flag is `agg.is_some() || allow_byo_creds`,
//     so true does NOT guarantee data - a 404 can still follow);
//   - HTTP 404 (no agg context, or no source files match the scope) ->
//     `none`, not an error;
//   - Coverage counts folded < matched -> `partial` (computed client-side;
//     the server only sends counts);
//   - everything folded -> `full`.
// Genuinely unexpected failures (500, auth) still throw - see
// AggregatesRequestError.
//
// Progressive refinement: both endpoints take a `refine` query param (a
// serde bool - the literal string "true", never "1"). A read-only poll
// (refine absent) returns whatever is already folded, instantly; each
// refine=true poll folds one more bounded batch. `refineUntilFrozen` drives
// that loop; termination rule documented on it.

// ─── Coverage: the tier-1 fallback signal ────────────────────────────────

/**
 * How much of a scope has been folded so far, reported on every
 * demand-driven query (src/ingest/aggregate.rs Coverage). Counts only -
 * the client computes the full/partial/none signal (coverageSignal).
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
 * The consumer-facing coverage signal (the tier-1 hard edge, architecture
 * 2.8): "none" -> fall back to listing-metadata density, "partial" ->
 * usable but badge it (never silently wrong), "full" -> trust it.
 */
export type CoverageSignal = "full" | "partial" | "none";

/**
 * Classify a response's coverage:
 *  - no coverage block -> "full": the server only omits it outside
 *    demand-driven mode, where the single fetch covers everything
 *    (the legacy page treats that as final, flamegraph.html:415-420);
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
 * never frozen). Mirrors the legacy flamegraph_api.js isCoverageFrozen
 * semantics (page-adjacent file, deliberately not imported into src/).
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
 * One node of the server-built flamegraph tree
 * (src/server/flamegraph.rs FlamegraphNode). Distinct from the frozen
 * core's client-built `FlamegraphNode` (trace_analysis.js) - pages convert
 * before handing to the widget, hence the `Api` prefix.
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

/** One facet's values (src/ingest/aggregate.rs FacetResult). */
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
 * actually applied), so headers render backend truth rather than URL
 * params the client guessed at (src/server/flamegraph.rs ScopeEcho).
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

/**
 * Worst poll per class, for deep-linking into the viewer
 * (src/server/tokio_stats.rs PollExemplar).
 */
export interface PollExemplar {
  start_ns: number;
  end_ns: number;
  duration_ns: number;
  host: string;
  /** Source trace file key for constructing the viewer deep link. */
  source_key: string;
}

/** Long-poll stats for one spawn location (src/server/tokio_stats.rs). */
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

export interface TokioStatsResponse {
  /** Time span covered by the data (ns), for per-minute rates. Min 1. */
  time_span_ns: number;
  total_polls: number;
  /** Source bucket (for constructing viewer deep links). */
  bucket: string;
  by_spawn_loc: SpawnLocStats[];
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
export type TokioStatsQuery = AggregateScope;

/** Query params for GET /api/flamegraph (FlamegraphParams). */
export interface FlamegraphQuery extends AggregateScope {
  /** Display-only time-range echo (see FlamegraphMetadata.time_range). */
  from?: string;
  to?: string;
  /** "Fetch more": raise the sampling-cap ceiling (server clamps it). */
  max_files?: number;
  /** "worker" | "off-worker" | omitted for all. */
  thread_class?: string;
  /** "cpu" (default view) | "sched" | "all". */
  source?: string;
  /** Exact-match spawn-location filter. */
  spawn_location?: string;
}

export const FLAMEGRAPH_ENDPOINT = "/api/flamegraph";
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

/** Build the GET /api/flamegraph URL (path + query string). */
export function flamegraphUrl(query: FlamegraphQuery): string {
  const search = new URLSearchParams();
  appendScope(search, query);
  if (query.from !== undefined) search.set("from", query.from);
  if (query.to !== undefined) search.set("to", query.to);
  if (query.max_files !== undefined) search.set("max_files", String(query.max_files));
  if (query.thread_class !== undefined) search.set("thread_class", query.thread_class);
  if (query.source !== undefined) search.set("source", query.source);
  if (query.spawn_location !== undefined) search.set("spawn_location", query.spawn_location);
  if (query.refine) search.set("refine", "true");
  const qs = search.toString();
  return qs ? `${FLAMEGRAPH_ENDPOINT}?${qs}` : FLAMEGRAPH_ENDPOINT;
}

/** Build the GET /api/tokio-stats URL (path + query string). */
export function tokioStatsUrl(query: TokioStatsQuery): string {
  const search = new URLSearchParams();
  appendScope(search, query);
  if (query.refine) search.set("refine", "true");
  const qs = search.toString();
  return qs ? `${TOKIO_STATS_ENDPOINT}?${qs}` : TOKIO_STATS_ENDPOINT;
}

// ─── Fetch mechanism ─────────────────────────────────────────────────────

/**
 * The fetch surface this client needs. Injectable so tests run against
 * recorded fixtures with a stub (no live server in vitest) and pages can
 * wrap fetch for instrumentation. Defaults to globalThis.fetch.
 */
export type FetchLike = (
  url: string,
  init?: { headers?: Record<string, string>; signal?: AbortSignal }
) => Promise<Response>;

/** Options shared by fetchFlamegraph / fetchTokioStats. */
export interface AggregatesRequestOptions {
  /**
   * The /api/config `aggregation_enabled` flag, caller-provided. `false`
   * short-circuits to `unavailable("disabled")` WITHOUT issuing a request.
   * Absent/true proceeds - but does not guarantee data: the flag is
   * `agg.is_some() || allow_byo_creds` (config.rs:30), so the endpoints
   * can still 404 (mapped to `unavailable("not-found")`, never thrown).
   */
  aggregationEnabled?: boolean;
  /** Injectable fetch (see FetchLike). */
  fetch?: FetchLike;
  /** Extra request headers - the BYO-credential x-dial9-aws-* set. */
  headers?: Record<string, string>;
  /** Abort signal for stale-scope cancellation. */
  signal?: AbortSignal;
}

/**
 * Why an aggregate result carries no data:
 *  - "disabled":  config said aggregation_enabled=false; no request made.
 *  - "not-found": the endpoint returned 404 - the server has no
 *    aggregation context, or no source files match the scope. Both mean
 *    "no data here", not an error.
 */
export type AggregateUnavailableReason = "disabled" | "not-found";

/**
 * The result of one aggregate request. `coverage` is the tier-1 fallback
 * signal (see module header): consumers switch on it, falling back to
 * listing metadata on "none" and badging on "partial", without having to
 * re-derive the rule from counts.
 */
export type AggregateResult<T> =
  | { kind: "data"; response: T; coverage: CoverageSignal }
  | {
      kind: "unavailable";
      reason: AggregateUnavailableReason;
      coverage: "none";
      /** The 404 response body (server's plain-text explanation), if any. */
      message?: string;
    };

/**
 * A genuinely unexpected endpoint failure: any non-ok status other than
 * the 404-means-no-data case (500 internal, 401/403 auth, 421 wrong
 * region - see src/server/error.rs). Carries the status and the server's
 * plain-text body so pages can surface an actionable message.
 */
export class AggregatesRequestError extends Error {
  readonly status: number;
  constructor(endpoint: string, status: number, body: string) {
    super(`${endpoint} failed (${status}): ${body}`);
    this.name = "AggregatesRequestError";
    this.status = status;
  }
}

/** Shared request path for both endpoints (degradation rules included). */
async function fetchAggregate<T extends { coverage?: Coverage }>(
  url: string,
  opts: AggregatesRequestOptions | undefined
): Promise<AggregateResult<T>> {
  if (opts?.aggregationEnabled === false) {
    return { kind: "unavailable", reason: "disabled", coverage: "none" };
  }
  const doFetch: FetchLike = opts?.fetch ?? globalThis.fetch;
  const init: { headers?: Record<string, string>; signal?: AbortSignal } = {};
  if (opts?.headers !== undefined) init.headers = opts.headers;
  if (opts?.signal !== undefined) init.signal = opts.signal;
  const resp = await doFetch(url, init);
  if (resp.status === 404) {
    // "No data", by design - not an error (see module header).
    const message = await resp.text();
    return { kind: "unavailable", reason: "not-found", coverage: "none", message };
  }
  if (!resp.ok) {
    const endpoint = url.split("?", 1)[0] ?? url;
    throw new AggregatesRequestError(endpoint, resp.status, await resp.text());
  }
  // The response shape is trusted to match the server structs this module
  // mirrors (same binary serves API and UI; no version skew like traces).
  const response = (await resp.json()) as T;
  return { kind: "data", response, coverage: coverageSignal(response.coverage) };
}

/** GET /api/flamegraph for one scope (one poll of the refinement loop). */
export function fetchFlamegraph(
  query: FlamegraphQuery,
  opts?: AggregatesRequestOptions
): Promise<AggregateResult<FlamegraphResponse>> {
  return fetchAggregate<FlamegraphResponse>(flamegraphUrl(query), opts);
}

/** GET /api/tokio-stats for one scope (one poll of the refinement loop). */
export function fetchTokioStats(
  query: TokioStatsQuery,
  opts?: AggregatesRequestOptions
): Promise<AggregateResult<TokioStatsResponse>> {
  return fetchAggregate<TokioStatsResponse>(tokioStatsUrl(query), opts);
}

// ─── Refine-polling helper ───────────────────────────────────────────────

/**
 * Default ceiling on refine polls. Each refine poll folds one bounded
 * server-side batch; the loop normally terminates by freezing well before
 * this. The ceiling exists so a scope where every poll folds a little more
 * (a huge fleet) still terminates deterministically instead of hammering
 * the server; consumers wanting deeper coverage raise it (or use the
 * flamegraph `max_files` "fetch more" escape hatch).
 */
export const DEFAULT_MAX_REFINE_POLLS = 30;

export interface RefineUntilFrozenOptions<T> {
  /** Refine-poll ceiling (excludes the initial read-only poll). */
  maxRefinePolls?: number;
  /**
   * Called with every poll's result as it lands (pollIndex 0 = the
   * read-only poll), so consumers re-render progressively as coverage
   * climbs, exactly like the legacy flamegraph page.
   */
  onResult?: (result: AggregateResult<T>, pollIndex: number) => void;
}

/**
 * Drive an endpoint's progressive-refinement loop to completion.
 *
 * `poll(refine)` performs one request - typically an arrow over
 * fetchFlamegraph/fetchTokioStats closing over the scope and options:
 *
 *   refineUntilFrozen((refine) => fetchTokioStats({ ...scope, refine }, opts))
 *
 * Sequence: one read-only poll (instant server-side, establishes the
 * baseline), then refine=true polls. TERMINATION RULE - stops when:
 *  - FROZEN: `files_folded` did not increase between two consecutive
 *    polls (isCoverageFrozen, mirroring flamegraph_api.js:56-61 - the
 *    server could fold nothing new, so more polls change nothing); or
 *  - CEILING: maxRefinePolls refine polls ran (default
 *    DEFAULT_MAX_REFINE_POLLS = 30); or
 *  - the endpoint reports unavailable (disabled/404), returned as-is; or
 *  - a response carries no `coverage` (non-demand-driven single fetch:
 *    nothing to refine).
 * The legacy page also auto-stops on a coverage PLATEAU
 * (shouldAutoStopRefining's diminishing-returns heuristic); that is a
 * consumer-side policy layered via onResult + an external abort, left to
 * chunk 2 - this helper is deliberately frozen-or-ceiling only.
 *
 * Pacing: none built in (the legacy page waits 800ms between polls, a UI
 * policy). Consumers pace inside `poll` if desired; each refine request
 * already takes a server-side fold long, so unpaced loops are bounded by
 * fold time, not a tight spin.
 *
 * Returns the last result (the frozen/ceiling-hit one). Throws only what
 * `poll` throws (AggregatesRequestError, network, abort).
 */
export async function refineUntilFrozen<T extends { coverage?: Coverage }>(
  poll: (refine: boolean) => Promise<AggregateResult<T>>,
  opts?: RefineUntilFrozenOptions<T>
): Promise<AggregateResult<T>> {
  const maxRefinePolls = opts?.maxRefinePolls ?? DEFAULT_MAX_REFINE_POLLS;

  let result = await poll(false);
  opts?.onResult?.(result, 0);
  if (result.kind === "unavailable") return result;
  let prev = result.response.coverage;
  if (prev === undefined) return result; // non-demand-driven: single fetch

  for (let i = 1; i <= maxRefinePolls; i++) {
    result = await poll(true);
    opts?.onResult?.(result, i);
    if (result.kind === "unavailable") return result;
    const curr = result.response.coverage;
    if (curr === undefined) return result;
    if (isCoverageFrozen(prev, curr)) return result;
    prev = curr;
  }
  return result; // ceiling hit: best coverage reached within budget
}
