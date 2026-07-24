# dial9-viewer UI

Static HTML/JS frontend for the trace viewer, embedded into the `dial9-viewer`
binary via `rust-embed` and served by the server (`../src/server/`). In dev,
the assets are served from disk by `../src/bin/dev_server.rs`.

## UI development requires Node

The served assets are the BUILT output in `dist/` (gitignored), not the
sources in this directory. Working on the UI requires Node (CI uses Node 24):

```bash
npm ci            # lockfile-pinned install (never `npm install` in CI)
npm run build     # vite build -> dist/ (what rust-embed embeds)
npm run test      # vitest (passes with no tests until suites migrate, T10)
npx tsc --noEmit  # typecheck
```

A cargo-only checkout still compiles (`dist/.gitkeep` keeps the folder
present) but serves an empty UI until `npm run build` runs. End users never
need Node: release CI builds `dist/` before publishing, so the crates.io
archive and the prebuilt binaries carry the built assets.

## Dev loops

Two ways to work on the UI (ADR-0004 section 3). Both use the Rust
dev-server as the API backend:

```bash
PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server
```

**Proxy mode (primary, HMR):**

```bash
npm run dev       # Vite dev server, prints its URL (default :5173)
```

Browse the Vite URL: `/api/*` is proxied to the dev-server on :3001
(`server.proxy` in `vite.config.ts`), edits to `src/` modules hot-reload
in the browser, and the legacy pages/scripts are served from this
directory as-is.

**Embedded mode (single-server, edit -> refresh):**

```bash
npm run dev:embedded   # vite build --watch -> dist/
```

Browse the dev-server directly (http://localhost:3001): it serves `dist/`
from disk, and the watch build rewrites `dist/` on every edit - including
edits to the statically-copied legacy pages/scripts and the `public/`
assets, which a config plugin registers as watch files. Edit, refresh,
done. `dial9 serve --dev` serves the same `dist/` from disk (run it from
the repo root or `dial9-viewer/`).

Root-served verbatim assets (`demo-trace.bin`, `flamegraph.css`) live in
`public/`: Vite serves `public/` at `/` in dev mode and copies it into the
`dist/` root at build, so the pages' root-relative references keep working
in both modes. The Node tests read the demo trace from
`public/demo-trace.bin`.

Key files:

- `index.html` — landing page / S3 browser. Emits one `trace=/api/object?…`
  component per selected file and opens the viewer or flamegraph.
- `viewer.html` — main trace viewer.
- `flamegraph.html` — standalone CPU-profile flamegraph view.
- `decode.js` — low-level binary trace-frame decoder (`TraceDecoder`).
- `trace_parser.js` — higher-level parser (`parseTrace`, `fetchTraces`, …)
  built on `decode.js`. Works in both the browser and Node.

## Dual-UI switch (`ui-switch.js`)

During the migration (ADR-0004 section 8) every page can exist in two
versions: the legacy one at its canonical URL and a migrated Vite entry at its
own dist path. `ui-switch.js` (plain browser JS at the `ui/` root, shipped via
the static-copy list; NOT frozen core) is the whole rollout mechanism:

- **Routing convention:** `?ui=new` on the canonical page URL selects the new
  version. Precedence: explicit `?ui=new`/`?ui=legacy` param > stored
  preference (localStorage key `dial9-ui-preference`) > default (legacy until
  the flip - a single commented line in `ui-switch.js`, `DEFAULT_UI`).
- **Raw switch:** the query string (trace source) is preserved across every
  switch, minus the `ui` param which the script owns; the hash (view state) is
  always dropped. No view state ports in either direction.
- **The control:** a small fixed bottom-right pill (`id="d9-ui-switch"`, the
  id the T12 census asserts), always visible on BOTH versions. It only renders
  when there is somewhere to go: a legacy page with no registered new version
  shows nothing.
- **Legacy pages** load it via one `<script src="ui-switch.js"></script>` line
  in `<head>` - the only edit legacy pages ever receive.

**Registering a migrated page (T13/T14/T41):** add ONE line to
`NEW_UI_ENTRIES` in `ui-switch.js` mapping the canonical page to your entry's
served dist path, e.g. `"flamegraph.html": "new/flamegraph.html"` (and update
the registry expectation in `tests/ui_switch.test.ts`). Your new entry should
load `ui-switch.js` and call `window.D9UiSwitch.mount({ side: "new" })` to
render the "Switch to legacy UI" control (pass `page: "x.html"` if your entry
path is not the registered value verbatim).

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

## URL contract (stable deep-link API)

The pages' URLs are an API, for humans sharing links and for agents driving
the UI without a browser (issue #303): a report generator, a skill, or a
`curl`-style script can construct a URL from the tables below and know the
page will honor it. This section is the normative contract; the skills that
emit viewer links (`../skills/dial9-html-report`, `../skills/dial9-zoom-window`)
and the contract tests (see Enforcement below) follow it.

**Stability promise (architecture NFR N10): old params stay valid forever.**
Evolution is additive-only: a param documented here never changes meaning,
never gets removed, and never stops being honored by the page that owns it.
New capability means a NEW query param or hash key, and adding one requires
updating this section, the ledger (`docs/tickets/ledger.md`), and the
contract tests in the same PR.

The split, made explicit:

- **QUERY string = load scope + page-owned state.** What data to load
  (`trace=`, time-range filters) and per-page state that predates the hash
  codec (browser-page search state, flamegraph zoom, api-mode facets).
  Query params can reach the server and are preserved verbatim by every
  view-state rewrite.
- **HASH = versioned view state** (`#v=1&...`), what the reader is LOOKING
  at, never what is loaded. Owned exclusively by the T19 codec on migrated
  pages; never reaches the server; deliberately dropped by the dual-UI raw
  switch. Normative schema: `docs/ui-inventory/05-url-view-state.md`
  (codec: `src/lib/url/view-state.ts`).

### Query params - viewer.html and flamegraph.html (exact mode)

Both trace-rendering pages read the same load-scope vocabulary (the browser
page emits it via `traceTitleParams`/`objectTraceUrls`):

| Param | Value | Meaning |
|-------|-------|---------|
| `trace` | URL, **repeatable** | Trace component to fetch and gunzip client-side; N values parse as one trace (see "The `trace=` query parameter" above). Relative or absolute; must be same-origin-fetchable. |
| `start` | absolute monotonic ns (integer) | Parse-time time-range filter, inclusive; either optional. The page re-writes it via `history.replaceState` when the user sets/clears a range (viewer "Set Range", features/02 E3-E5; flamegraph F19). |
| `end` | absolute monotonic ns (integer) | Filter end; same semantics as `start`. |
| `svc` | string | Service name, display label only. |
| `host` | string | Host name, display label only. |
| `segs` | integer as string | Segment COUNT for the header/stats display (index.html sets `String(keys.length)`); NOT a list of segment keys. |
| `from` | string | Human-readable wall-clock range start, display only. |
| `to` | string | Human-readable wall-clock range end, display only. |
| `worker-zoom` | TAB-joined frame names, root -> target | Flamegraph only: worker-tree zoom path (features/03 F148/F150). Legacy view-state param, kept live forever; the migrated page also mirrors zoom state here so copied links open on both page generations. |
| `offworker-zoom` | same | Flamegraph only: off-worker-tree zoom path (F149). |
| `prof` | `1` | Viewer only, debug: enables the render profiler (features/02 A14). Honored but not part of any UI's emitted links. |

`start`/`end` are ABSOLUTE monotonic nanoseconds, the same values carried by
`event.ts`/`trace.minTs` from `TraceParser.parseTrace()`, NOT offsets from
trace start. On the legacy exact-mode pages `start`/`end` are parse filters;
on `/new/viewer.html` they are viewport bounds and the additive
`data-start`/`data-end` pair carries the parse filter. The legacy pages do not
honor the new-viewer state table below; the migrated viewer does.

### Query params - new/viewer.html durable view state

Only the migrated trace viewer owns these additive parameters. They are
canonicalized with `history.replaceState` after each settled store update;
defaults are omitted. Values are semantic anchors where possible, so agents
can construct them directly. `start`/`end` are the visible viewport here;
`data-start`/`data-end` are the distinct parse-time Set Range filter. For
`v1:` lists, percent-encode the complete query value once in the normal URL
way (`TAB` becomes `%09`, newline `%0A`); commas inside names need no special
list escaping. Previously emitted comma/pre-encoded list values remain readable.

| Param | Value | Meaning |
|-------|-------|---------|
| `start` | monotonic ns | Visible viewport start (does not discard data). |
| `end` | monotonic ns | Visible viewport end. Valid only with `start < end`. |
| `task` | integer or `0x` hex | Selected task. |
| `span-filter` | string | Span text filter. |
| `track-order` | comma-separated track ids | Analysis-track order. |
| `collapsed` | comma-separated track ids | Collapsed analysis tracks. |
| `span` | span id | Lane-highlighted span (ancestor chain is re-derived). |
| `span-focus` | span id | Span-panel subtree root. |
| `poll` | `<startNs>:<taskId>` | Poll-detail anchor. |
| `event` | monotonic ns | Pinned custom-event cluster timestamp. |
| `region` | `<startNs>-<endNs>` | Retained analysis region. |
| `spawned` | `<startNs>-<endNs>` | Queue-track spawned-task range. |
| `issue` | POI detector id | Issues filter. |
| `issue-sort` | `<worker\|kind\|time\|duration>,<asc\|desc>` | Issues ordering. |
| `issue-index` | non-negative integer | Current issues cursor. |
| `span-pct` | `50` \| `90` \| `95` \| `99` | Span percentile floor. |
| `span-names` | `v1:` + TAB-joined names | Enabled span legend chips. |
| `event-names` | same | Enabled custom-event legend chips. |
| `rail` | `issues` \| `tasks` | Visible rail tab. |
| `task-sort` | `<id\|loc\|polls\|total\|longest\|lifetime>,<asc\|desc>` | Tasks ordering. |
| `task-index` | non-negative integer | Current Tasks cursor. |
| `runtime-collapsed` | `v1:` + TAB-joined names | Folded runtime groups. |
| `inspector-width` | positive CSS pixels | Inspector width. |
| `lanes-height` | positive CSS pixels | Worker-lanes viewport height. |
| `lanes-scroll` | non-negative CSS pixels | Worker-lanes vertical position. |
| `stack-view` | `list` \| `flame` | Poll/blocking stack presentation. |
| `inspector` | `task` \| `poll` \| `event` \| `related` \| `stack` | Visible inspector tab. |
| `poll-section` | `cpu` \| `sched` | Poll flamegraph sample family. |
| `poll-expanded` | `v1:` + TAB-joined group ids | Expanded poll list groups. |
| `poll-worker-zoom` | TAB-joined frame path | Poll worker-tree flamegraph zoom. |
| `poll-offworker-zoom` | same | Poll off-worker-tree flamegraph zoom. |
| `related-collapsed` | `v1:` + TAB-joined titles | Collapsed Related sections. |
| `related-expand` | `v1:` + newline-joined `<title><TAB><before><TAB><after>` entries | Related load-more counts. |
| `related-key` | string | Correlation field key. |
| `related-value` | string | Correlation field value; active with `related-key`. |
| `analysis` | `cpu` \| `blocking` \| `heap` | Region-analysis mode. |
| `heap-weight` | `bytes` \| `count` | Heap flamegraph weighting. |
| `blocking-group` | `leaf` \| `full` | Blocking-list grouping. |
| `analysis-worker-zoom` | TAB-joined frame path | Region worker-tree flamegraph zoom. |
| `analysis-offworker-zoom` | same | Region off-worker-tree flamegraph zoom. |
| `span-index` | non-negative integer | Current filtered-span navigation cursor. |
| `data-start` | monotonic ns | Parse-time Set Range lower bound. |
| `data-end` | monotonic ns | Parse-time Set Range upper bound. |

Clock mode (`tm`) and timezone (`tz`) remain in the versioned hash. Unknown
query params are preserved. Invalid known values are ignored rather than
coerced. Hover, in-flight drag, temporary search/help modals, toasts, and load
progress are intentionally transient and are not deep-linked.

### Query params - flamegraph.html aggregated API mode (`?api=1`)

`api=1` switches the flamegraph page to server-aggregated mode; scope and
facet params are rebuilt and `history.pushState`ed on every Apply/facet
change (features/03 F168/F180), so Back walks the filter history:

| Param | Value | Meaning |
|-------|-------|---------|
| `api` | `1` | Mode switch. |
| `data_dir` | path | Local-directory scope (alternative to bucket/prefix). |
| `bucket` | string | S3 scope bucket. |
| `prefix` | string | S3 scope key prefix. |
| `service` | string | Scope service name. |
| `host` | string, **repeatable** | Scope host filter. |
| `start_ns` | epoch ns | Scope window start (seeds the UTC picker). |
| `end_ns` | epoch ns | Scope window end. |
| `source` | `cpu` (default) etc. | Facet filter: sample source. |
| `thread_class` | string | Facet filter. |
| `spawn_location` | string | Facet filter. |
| `max_files` | integer | Refinement fold ceiling. |

Canvas zoom is deliberately NOT URL-synced in api mode (F180); there are no
view-state params here.

### Query params - index.html (trace browser)

Owned by `url_state.js` (its header is the field-level contract); serialized
with the History API on state changes, defaults omitted. Service-tab changes
push history entries so Back/Forward restores the focused service:

| Param | Value | Meaning |
|-------|-------|---------|
| `bucket` | string | S3 bucket name. |
| `aws_region` | string | Region the bucket lives in (cross-region buckets). |
| `prefix` | string | User-entered key prefix. |
| `service` | string | Focused Browse service tab and exact backend filter. |
| `tab` | `raw` | Active tab; `browse` is the default and omitted. |
| `tz` | `local` | Timezone toggle; `utc` is the default and omitted. |
| `last` | positive number | Relative quick range in hours ("last N hours from now"). Mutually exclusive with `from`/`to`; wins when both present. |
| `from` | epoch seconds | Precise window start. NOTE: same NAME as the viewer's display-only `from`, different page, different meaning - both stable. |
| `to` | epoch seconds | Precise window end. |
| `q` | string | Raw-search prefix query. |

### Query params - every page

| Param | Value | Meaning |
|-------|-------|---------|
| `ui` | `new` \| `legacy` | Dual-UI switch (owned by `ui-switch.js`, see above). Preserved-minus-`ui` across switches; the hash is dropped by design. |

### Hash - versioned view state (`#v=1`)

The hash carries a form-encoded payload with a leading integer version:
`#v=1&fg.w=<tab-joined path>&tm=abs`. Full grammar, precedence against the
legacy zoom params, tolerant-reader and version rules:
`docs/ui-inventory/05-url-view-state.md`. The v1 key registry, with honest
status:

| Key | Status | Meaning |
|-----|--------|---------|
| `v` | live | Schema version, currently `1`. Required; a hash without a well-formed integer `v` is foreign and left alone. |
| `fg.w` | live (flamegraph) | Worker-tree zoom path; overrides `worker-zoom` per field, legacy fills gaps. |
| `fg.o` | live (flamegraph) | Off-worker-tree zoom path. |
| `fg.i` | live (flamegraph) | Inspect (butterfly) focus display name; overrides legacy `inspect`. |
| `fg.if` | live (flamegraph) | Inspect focus symbol; overrides legacy `inspect_full`. Emitted only when it differs from `fg.i`. |
| `fg.s` | live (flamegraph) | Frames-search query; overrides legacy `search`. |
| `fg.sp` | live (flamegraph) | Spawn-location filter value (exact mode); overrides legacy `spawn`. |
| `fg.rt` | live (flamegraph) | Runtime filter value (exact mode); overrides legacy `runtime`. |
| `tm` | live (new viewer) | Clock display mode (`rel`\|`abs`). |
| `tz` | live (new viewer) | Timezone (`utc`\|`local`) for absolute timestamps. |
| `vp` | reserved hash name | Not honored in hash; new viewer uses query `start`/`end`. |
| `sel.*` | reserved hash names | Not honored in hash; new viewer uses readable selection query params. |
| `poi` | reserved hash name | Not honored in hash; rail cursors are page-owned query params. |

Reserved hash keys claim the NAME only. Emitting them does nothing; the
new-viewer query implementation does not activate or reinterpret them.

### Deep-link recipes for agents (issue #303)

The three asks from #303, in contract terms:

1. **Open the new viewer at an exact window, optionally with Set Range:**

   ```
   new/viewer.html?trace=<trace-url>&start=<visible-start-ns>&end=<visible-end-ns>
   new/viewer.html?trace=<trace-url>&data-start=<parse-start-ns>&data-end=<parse-end-ns>&start=<visible-start-ns>&end=<visible-end-ns>
   ```

   All values are absolute monotonic nanoseconds. Omit `data-start`/`data-end`
   to keep the full trace zoomable; include them to reproduce a Set Range
   reparse exactly. Legacy `viewer.html` continues to interpret `start`/`end`
   as its parse filter.

2. **Open a flamegraph, optionally pre-zoomed to a subtree:**

   ```
   flamegraph.html?trace=<trace-url>&start=<ns>&end=<ns>&worker-zoom=<f1>%09<f2>
   ```

   Emit the legacy `worker-zoom`/`offworker-zoom` QUERY form for maximum
   compatibility: both page generations honor it (the migrated page reads
   it as the hash's fallback). The hash form
   `#v=1&fg.w=<f1>%09<f2>` is equivalent on migrated pages and wins per
   field when both are present. Zoom restore is gated on the time-range
   filter reproducing the shared tree (F151), so carry the same
   `start`/`end` the zoomed view had (or none).

3. **Select an analysis target and exact surface:**

   ```
   new/viewer.html?trace=<trace-url>&task=0x2a&start=<ns>&end=<ns>
   new/viewer.html?trace=<trace-url>&region=<a>-<b>&analysis=heap&heap-weight=count&inspector=stack
   new/viewer.html?trace=<trace-url>&poll=<poll-start>:<task-id>&inspector=poll&stack-view=flame&poll-section=sched&poll-worker-zoom=<f1>%09<f2>
   ```

   Selection anchors that depend on trace content are validated after load;
   an anchor absent from that trace is dropped without disturbing the rest of
   the link. The full parameter table above is the source of truth for agents.

### Enforcement

- `src/lib/url/url-contract.test.ts` (vitest) pins this section against the
  code: schema version, hash-key registry behavior (live keys round-trip,
  reserved keys preserved-not-honored), and the param tables against the
  recorded fixtures (`src/lib/url/legacy-params.fixture.ts`, `url_state.js`).
  Renaming this section's headings or table param names breaks that test by
  design.
- `parity/url-contract.mjs` (T12-style, live server) constructs the recipe
  URLs above and asserts the pages honor them end-to-end; see "Parity gate
  tooling" below.

## Parity gate tooling (`parity/`)

The migration's "lose nothing" machinery (ticket T12; ADR-0004 section 7):
plain Node scripts, dev-only — `parity/` is not a Vite input and never enters
`dist/` or the crate package. Every page-migration ticket's DoD invokes these
against a live server (the dev-server, or `dial9 serve`; readiness gate:
`GET /api/config` returns JSON):

```bash
npm run build     # the dev-server serves ui/dist from disk
PORT=3021 cargo run -p dial9-viewer --bin dev-server --features dev-server
```

All tools run headless Chromium (playwright devDependency; run
`npx playwright install chromium` once) at a fixed 1440x900 viewport.
Tools that hit the browser page pin the page clock to the dev seed's date
(`lib/browser.mjs`) so the seeded April segment stays reachable through
relative time windows.

**(a) Row-walker** — drives every feature-inventory row's access path, emits
`VERIFIED` / `FAILED` / `NOT-TRIGGERABLE` per the shared verdict mapping
(chunk-1 tickets header). Green = zero FAILED; exit 0 only when green.
Amended rows (T15: features/01 G8/C6/I2/F4/F10) walk PER-SIDE: the walker
derives `side` from the URL (`/new/` = the migrated page) and those rows'
walkers assert the amended contract on the new page and the preserved
recorded behavior on the legacy page, so both sides gate green against the
one inventory.

```bash
node parity/walk-rows.mjs \
  --inventory ../../docs/ui-inventory/features/01-index-html.md \
  --url http://localhost:3021/index.html \
  [--rows A1,F12] [--json parity/out/walk.json] [--md parity/out/walk.md]
```

**(a2) Fixture walk** (ticket T42) — the demo seed is a single segment on a
single host/boot, so some features/01 rows are recorded `NOT-TRIGGERABLE`
(boot transitions, seams, coverage gaps, the 200 MB cap, ...). The synthetic
fixture generator produces what the demo cannot, and `--fixtures` walks
exactly those rows against a fixture-seeded dev-server:

```bash
# 1. Generate (deterministic; ~4 s in release). Writes the committed small
#    fixtures under parity/fixtures/segments/ (a no-op diff unless the
#    generator changed) and the UNCOMMITTED seed tree under
#    parity/fixtures/generated/s3/ (~224 MB; --skip-large omits the
#    dial9-fixtures-large family and with it the H4 row).
cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures

# 2. Serve it (DIAL9_DEFAULT_PREFIX= empties the default prefix — the
#    D4/#471 date-root scenario needs discovery to see the date layer).
DIAL9_SEED_DIR=dial9-viewer/ui/parity/fixtures/generated/s3 \
  DIAL9_DEFAULT_PREFIX= PORT=3022 \
  cargo run -p dial9-viewer --features dev-server --bin dev-server

# 3. Walk. Default row set = the fixture-backed rows; the runner preflights
#    every fixture family the selected rows need and exits 2 with these
#    instructions when one is missing.
node parity/walk-rows.mjs \
  --inventory ../../docs/ui-inventory/features/01-index-html.md \
  --url http://localhost:3022/index.html --fixtures \
  [--json parity/out/fixture-walk.json]
```

Fixture-backed rows (registry: `parity/walkers/features01.fixtures.mjs`;
fixture geometry mirrors `src/bin/gen_fixtures.rs` — change them together):
C7, D4, F5, F7, F8, F9, F20, H4. H5's warning text renders only with
aggregation disabled, and any BYO-creds dev-server (which C7 needs) reports
aggregation enabled — it stays `NOT-TRIGGERABLE`. Recorded inventory verdicts
are NOT edited by the fixture walk; re-recording them is T39's final gate.

The seed tree is `<dir>/<bucket>/<key...>`; file **mtimes are load-bearing**
(they become S3 `last_modified`, the heatmap's segment end — the seam/gap
scenarios exist entirely in mtimes), which is why the tree is regenerated
rather than committed. Size policy: only the small fixtures under
`parity/fixtures/segments/` (tens of KB: the 10-segment boundary-poll set +
the multi-runtime #596 trace + `manifest.json`) are committed — the vitest
suites (`src/lib/trace/segments.fixtures.test.ts`, the real-parse anchor in
`segments.window.test.ts`) consume them hermetically. Everything under
`parity/fixtures/generated/` (incl. the >200 MB large family, which is also
T39's reproducible large-trace budget input) is gitignored and regenerable
byte-identically.

**Journeys J1-J8** (`parity/journeys.mjs`) — the eight expert journeys from
`docs/ui-inventory/04-ux-findings.md` as declarative step lists
(`lib/steps.mjs` is the vocabulary). Smoke-run one or all, printing the
readouts captured at each checkpoint:

```bash
node parity/run-journey.mjs --base http://localhost:3021 [--journey J2]
```

**(b) Affordance census** — dump one page's interactive-control census, or
diff two pages' censuses (exit 0 only on ZERO diff):

```bash
node parity/census.mjs --url http://localhost:3021/index.html [--json p] [--md p]
node parity/census.mjs --a <pageUrlA> --b <pageUrlB> [--json p] [--md p]
```

**(c) Behavioral differ** — same journey on two URLs, field-level exact diff
of the data readouts at every checkpoint. The readout schema is the fixture
`parity/fixtures/readout-schema.mjs` (owned by the parity tool; extending it
is a parity change, not a page change). Bare origins get each journey's
default path appended. Exit 0 only on zero field diffs:

```bash
node parity/behavior-diff.mjs --a http://host1 --b http://host2 [--journey J6] [--json p]
```

**(d) axe + contrast scan** — violation list (impact, rule, node count,
example target) in the shape 04-ux-findings cites, plus a contrast summary
line. Report producer (exit 0); `--fail-on <impact>` turns it into a gate:

```bash
node parity/axe-scan.mjs --url <pageUrl> [--json p] [--md p] [--fail-on serious]
```

**(e) Perf probe** — runs a journey to a representative state (defaults
J6/J1/J5 per page kind), then drives an interaction storm and records painted
frames, long tasks, total/forced layouts (per frame), and render invocations
per frame via a pluggable source (`lib/render-sources.mjs`; the default
`stub` reports unavailable — chunk 2 wires the store scheduler's
`devRenderAssertStats()` hook as the `store` source):

```bash
node parity/perf-probe.mjs --url <pageUrl> [--journey J3] [--render-source stub] [--json p]
```

**(f) URL-contract check** - the live half of the URL contract's
enforcement (the codec-level pin is `src/lib/url/url-contract.test.ts`):
constructs the contract section's deep-link recipe URLs in plain Node (no
browser) and asserts real pages honor them - the legacy viewer opens at an
exact `?start/?end` window, the recorded zoom link restores on both page
generations (query form on legacy, `#v=1` hash form on migrated), reserved
hash keys are inert, a foreign version restores nothing. Exit 0 only when
all legs pass:

```bash
node parity/url-contract.mjs --base http://localhost:3021 [--json p]
```

Self-tests (run whenever the tools themselves change): census and behavioral
differ legacy-vs-legacy on the same URL must emit ZERO diff; the row-walker
against the legacy browser page must stay green (zero FAILED).

## Tests — IMPORTANT for agents

Vitest is the single test runner (ADR-0004 section 7); the legacy
`node test_*.js` runner was retired when the last suite migrated (T11):

- **Vitest suites** (`tests/core/**/*.test.ts` for suites over the frozen
  core, `src/**/*.test.ts` for new TS modules) run with `npm run test` and
  are auto-discovered — no registration needed. The `ui` job in
  `.github/workflows/ci.yml` runs them against the committed demo trace.
- **Trace-dependent suites** additionally run against a freshly regenerated
  demo trace: the `trace-integrity` CI job runs
  `../../scripts/e2e-trace-tests.sh`, which regenerates
  `public/demo-trace.bin` (needs DynamoDB Local) and then runs a filtered
  `vitest run` over its `TRACE_SUITES` list. If a new suite's assertions must
  hold against regenerated traces (not just the committed one), add it to
  that list.
- **Env-var overrides** (formerly argv): `D9_TRACE_FILE` points
  `trace_integrity.test.ts` at another trace file, `DIAL9_TRACE_PATH` does
  the same for `time_range.test.ts`, `D9_SCALE_LARGE=1` runs
  `directory_scale.test.ts` with 200 files, and `D9_DIAGNOSTIC_TRACES`
  points `diagnose_setup.test.ts` at generated diagnostic traces (suite
  skips when the directory is absent; run
  `scripts/generate_diagnostic_traces.sh` first).
- **Exception:** `test_parser.js` stays a plain Node script at the ui/ root —
  the Rust integration test `dial9-tokio-telemetry/tests/js_parser.rs`
  invokes it by filename as `node test_parser.js <trace.bin>
  <expected.jsonl>`. Do not migrate or rename it without updating that test.
- `bench_parse.js` is a benchmark, not a test.
