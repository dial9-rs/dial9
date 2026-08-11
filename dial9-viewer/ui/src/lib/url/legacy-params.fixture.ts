// Test-only inventory of recorded flamegraph query parameters, used to verify
// that codec changes preserve the established URL contract.

/** How a param reaches the URL. */
export type LegacyParamMechanism =
  | "load-only" // read at load, never written by the page
  | "replaceState" // rewritten in place on state change (no history entry)
  | "pushState"; // pushed on change (Back walks the history)

export interface LegacyParamRecord {
  /** Query-param name, exactly as on the wire. */
  param: string;
  /** Page mode that owns it. */
  mode: "exact" | "api" | "both";
  mechanism: LegacyParamMechanism;
  /** Inventory anchor. */
  row: string;
  /** What it carries. */
  role: string;
  /** True for the zoom-state params the codec unifies. */
  viewState: boolean;
}

/**
 * The flamegraph page's URL params when the codec landed.
 *
 * Only the two `viewState: true` rows are the codec's business. Everything
 * else is LOAD SCOPE (what data to show), stays query-based, page-owned,
 * and must be preserved verbatim by any URL rewrite - the codec
 * never touches them.
 */
export const FLAMEGRAPH_LEGACY_PARAMS: readonly LegacyParamRecord[] = [
  // -- exact mode (client decode) --
  { param: "trace", mode: "exact", mechanism: "load-only", row: "F1", role: "trace component URL (repeatable)", viewState: false },
  { param: "start", mode: "exact", mechanism: "load-only", row: "F19", role: "time-range filter start (ns)", viewState: false },
  { param: "end", mode: "exact", mechanism: "load-only", row: "F19", role: "time-range filter end (ns)", viewState: false },
  { param: "svc", mode: "exact", mechanism: "load-only", row: "F24", role: "service name for the title", viewState: false },
  { param: "host", mode: "exact", mechanism: "load-only", row: "F24", role: "host name for the title", viewState: false },
  { param: "segs", mode: "exact", mechanism: "load-only", row: "F26", role: "segment count for the stats bar", viewState: false },
  { param: "from", mode: "exact", mechanism: "load-only", row: "F27", role: "human time-range start for the stats bar", viewState: false },
  { param: "to", mode: "exact", mechanism: "load-only", row: "F27", role: "human time-range end for the stats bar", viewState: false },
  {
    param: "worker-zoom",
    mode: "exact",
    mechanism: "replaceState",
    row: "F148",
    role: "worker-tree zoom path, TAB-joined frame names root->target; set when non-empty, deleted when empty; restore gated on timeRangeMatched (F151); cleared by Esc reset (F152)",
    viewState: true,
  },
  {
    param: "offworker-zoom",
    mode: "exact",
    mechanism: "replaceState",
    row: "F149",
    role: "off-worker-tree zoom path, same format and lifecycle as worker-zoom",
    viewState: true,
  },
  // -- aggregated api mode (?api=1) --
  // Scope + facet params, rebuilt and PUSHED on Apply/facet change so Back
  // walks the filter history. Canvas zoom is deliberately NOT URL-synced
  // in api mode - there is no view-state param to unify here.
  { param: "api", mode: "api", mechanism: "load-only", row: "F168", role: "mode switch (api=1)", viewState: false },
  { param: "data_dir", mode: "api", mechanism: "pushState", row: "F168", role: "local-dir scope (alternative to bucket/prefix)", viewState: false },
  { param: "bucket", mode: "api", mechanism: "pushState", row: "F168", role: "S3 scope bucket", viewState: false },
  { param: "prefix", mode: "api", mechanism: "pushState", row: "F168", role: "S3 scope key prefix", viewState: false },
  { param: "service", mode: "api", mechanism: "pushState", row: "F168", role: "scope service name", viewState: false },
  { param: "host", mode: "api", mechanism: "pushState", row: "F168", role: "scope host (repeatable)", viewState: false },
  { param: "start_ns", mode: "api", mechanism: "pushState", row: "F168", role: "scope window start (ns; seeds the UTC picker)", viewState: false },
  { param: "end_ns", mode: "api", mechanism: "pushState", row: "F168", role: "scope window end (ns; seeds the UTC picker)", viewState: false },
  { param: "source", mode: "api", mechanism: "pushState", row: "F168", role: "facet filter (default cpu)", viewState: false },
  { param: "thread_class", mode: "api", mechanism: "pushState", row: "F168", role: "facet filter", viewState: false },
  { param: "spawn_location", mode: "api", mechanism: "pushState", row: "F168", role: "facet filter", viewState: false },
  { param: "max_files", mode: "api", mechanism: "pushState", row: "F178", role: "refinement fold ceiling", viewState: false },
];

/**
 * Recorded legacy fixture URLs (query + hash only; origin-independent).
 * Shapes taken from the recorded legacy access paths; the zoom paths are
 * synthetic here (codec-level tests need the FORMAT, not demo-trace truth).
 * The demo-trace-real twin lives in the parity journeys.
 */
export const LEGACY_FIXTURE_URLS: readonly string[] = [
  // Single-level worker zoom.
  "?trace=demo-trace.bin&worker-zoom=tokio%3A%3Aruntime%3A%3Apark",
  // Multi-level TAB-joined path (%09 = \t).
  "?trace=demo-trace.bin&worker-zoom=main%09poll%09do_work",
  // Off-worker zoom alongside a worker zoom.
  "?trace=demo-trace.bin&worker-zoom=main%09poll&offworker-zoom=blocking%09read",
  // Zoom params embedded in the full S3-browser context params.
  "?trace=t/a.bin&trace=t/b.bin&start=1000&end=9000&svc=api&host=h1&segs=2&from=12%3A00&to=12%3A05&worker-zoom=main",
  // API-mode pushState URL (no view-state params by design).
  "?api=1&bucket=b&prefix=p&service=svc&host=h1&host=h2&start_ns=1&end_ns=2&source=cpu&max_files=64",
];
