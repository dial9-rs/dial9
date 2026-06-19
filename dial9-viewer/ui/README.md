# dial9-viewer UI

Static HTML/JS frontend for the trace viewer, embedded into the `dial9-viewer`
binary via `rust-embed` and served by the server (`../src/server/`). In dev,
the assets are served from disk by `../src/bin/dev_server.rs`.

Key files:

- `index.html` — landing page / S3 browser. Builds `/api/trace` URLs and opens
  the viewer or flamegraph.
- `viewer.html` — main trace viewer.
- `flamegraph.html` — standalone CPU-profile flamegraph view.
- `decode.js` — low-level binary trace-frame decoder (`TraceDecoder`).
- `trace_parser.js` — higher-level parser (`parseTrace`, `fetchTraces`, …)
  built on `decode.js`. Works in both the browser and Node.

## The `trace=` query parameter

`trace=` is **repeatable**. Each value is fetched independently and may be
individually gzipped (unlike `/api/trace`, which gunzips server-side before
returning a single response). `TraceParser.fetchTraces()` fetches every
component, runs each through `maybeGunzip`, and concatenates the raw bytes.
The decoder treats a concatenated stream as multiple segments — a mid-stream
`TRC\0` header resets the frame parser — so the combined buffer parses as one
trace. Read all values with `params.getAll('trace')`, never `params.get`.

## Tests — IMPORTANT for agents

Tests are plain Node scripts named `test_*.js` (run with `node test_foo.js`),
most using the shared `test_harness.js`.

**CI does NOT auto-discover these tests.** They are listed explicitly in
`../../scripts/e2e-trace-tests.sh`, which the `trace-integrity` job in
`.github/workflows/ci.yml` runs. If you add a new `test_*.js`, you MUST add a
line for it in `scripts/e2e-trace-tests.sh` or it will never run in CI — adding
the file alone is not enough.
