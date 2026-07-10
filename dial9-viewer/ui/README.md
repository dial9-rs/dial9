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

### `/api/trace` (deprecated)

`GET /api/trace?bucket=&keys=a&keys=b` fetches every key, gunzips each
server-side, and returns one concatenated **uncompressed** blob. This is
**deprecated and slated for removal**: it transfers far more bytes (the merged,
decompressed trace) and serializes the work on the backend. The UI no longer
links to it; it remains only for out-of-tree callers (e.g. the
`dial9-trace-loading` skill). New code should fetch individual objects via
`/api/object` and let `fetchTraces` merge them.

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
