# dial9-viewer UI

Static HTML/JS frontend for the trace viewer, embedded into the `dial9-viewer`
binary via `rust-embed` and served by the server (`../src/server/`). In dev,
the assets are served from disk by `../src/bin/dev_server.rs`.

Key files:

- `index.html` — landing page / S3 browser. Emits one `trace=/api/object?…`
  component per selected file and opens the viewer or flamegraph.
- `viewer.html` — main trace viewer.
- `flamegraph.html` — standalone CPU-profile flamegraph view.
- `decode.js` — low-level binary trace-frame decoder (`TraceDecoder`).
- `trace_parser.js` — higher-level parser (`parseTrace`, `fetchTraces`, …)
  built on `decode.js`. Works in both the browser and Node.

## The `trace=` query parameter

`trace=` is **repeatable**. Each value is fetched independently and may be
individually gzipped. The decoder treats a concatenated stream as multiple
segments — a mid-stream `TRC\0` header resets the frame parser — so N components
parse as one trace. Read all values with `params.getAll('trace')`, never
`params.get`.

The viewer and flamegraph **stream** the components whenever the runtime
supports it (`DecompressionStream` + a readable `fetch` body):
`TraceParser.fetchTracesStream()` dispatches every component's `fetch()` up
front (so downloads run concurrently) and yields their gunzipped chunks
back-to-back, in order, into a single `parseTraceStream`. Parsing the first
segment then overlaps the in-flight downloads of the rest, so total load time is
~`max(download, parse)` instead of `download_all + parse` — the same win the
single-URL path already had, now for N components too (issue #595).

`TraceParser.fetchTraces()` is the non-streaming fallback (no
`DecompressionStream`, e.g. some Node test runtimes): it awaits every component
in parallel, runs each through `maybeGunzip`, concatenates the raw bytes, and
hands the whole buffer to `parseTrace`. Same bytes, but no fetch/parse overlap.

For S3-backed traces, `index.html` points each `trace=` at
`/api/object?bucket=&key=`, which serves one file's raw (still-gzipped) bytes.
The browser thus downloads the files in parallel and decompresses them
client-side — far less network transfer than a single merged response.

## The `s_*` scope parameters (large selections)

One `trace=` per file means a large heatmap selection produces a very long URL.
Opening the viewer/flamegraph is a navigation (a GET), so the whole list rides
in the URL — and past ~8 KB it exceeds CloudFront's hard request-URI limit, so
the new tab gets a **414** before it can load. For S3-backed selections the S3
browser instead emits a compact **scope** (`trace_scope.js`):

- `s_bucket`, `s_prefix`, `s_svc` — where to look
- `s_host` — repeatable host set (empty = all hosts in the window)
- `s_from`, `s_to` — time window, epoch seconds

The viewer/flamegraph re-list the matching files from the scope via `/api/browse`
(the same listing the S3 browser uses) and feed the resulting `/api/object` URLs
into `fetchTraces`. A scope is bounded by *host count*, not *file count*, so it
stays short; and because it is **stateless** (no per-browser storage), a shared
deep link re-resolves in any browser — this is what keeps the userscript's
"Copy deep link" feature working for large selections. A pathological host set
that still wouldn't fit degrades to time-range-only (all hosts in the window);
the UI warns when that happens. Consumers read a scope via
`Dial9TraceScope.readScope(params)` and fall back to inline `trace=` for non-S3
sources (locally-dropped files, `blob:` URLs, the demo trace).

Re-listing means a scope opened later may pick up files that landed in the
window since it was shared. For a finished trace that is nil; it is the trade
for a portable, length-safe link.

`trace_scope.js` owns the **Scope** concept end-to-end: `parseKey` /
`extractPrefix` (the single source of truth — `index.html` delegates to them),
`scopeFromKeys` (derive a scope from a selection), and two sibling encoders for
its two URL dialects. `encodeScope` writes the namespaced `s_*` form above (it
rides in the viewer page URL alongside unrelated `host`/`from`/`to`/`start`/`end`
params). `encodeAggregationParams` writes the **un-namespaced** form the server
aggregation endpoints expect — `bucket`/`prefix`/`service`/repeatable `host`,
window as `start_ns`/`end_ns` in **nanoseconds** — used by the demand-driven
flamegraph (`?api=1`) and `/api/tokio-stats`. A box spanning more than one
service sends *no* service filter (all services in the box), consistent across
exact and aggregation modes.

### `/api/trace` (deprecated)

`GET /api/trace?bucket=&keys=a&keys=b` fetches every key, gunzips each
server-side, and returns one concatenated **uncompressed** blob. This is
**deprecated and slated for removal**: it transfers far more bytes (the merged,
decompressed trace) and serializes the work on the backend. The UI no longer
links to it; it remains only for out-of-tree callers (e.g. the
`dial9-trace-loading` skill). New code should fetch individual objects via
`/api/object` and let `fetchTraces` merge them.

## Tests — IMPORTANT for agents

Tests are plain Node scripts named `test_*.js` (run with `node test_foo.js`),
most using the shared `test_harness.js`.

**CI does NOT auto-discover these tests.** They are listed explicitly in
`../../scripts/e2e-trace-tests.sh`, which the `trace-integrity` job in
`.github/workflows/ci.yml` runs. If you add a new `test_*.js`, you MUST add a
line for it in `scripts/e2e-trace-tests.sh` or it will never run in CI — adding
the file alone is not enough.
