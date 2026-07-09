# HANDOFF - T04 (Dev loops: HMR + single-server)

## STATUS

DONE. All DoD items demonstrated headlessly; the one item that inherently
needs a browser (observing the HMR websocket patch apply without reload) is
recorded as pending-manual with the closest headless evidence attached.

Branch: `ticket/T04-dev-loops` (base 94f2632, the T03 tip).
Worktree: `/Users/facundo/code/wye/dial9-tokio-telemetry/.claude/worktrees/T04`

## COMPLETED (commit shas)

- 8346987 feat(viewer-ui): add HMR proxy dev loop and dev:embedded watch loop
  - `ui/package.json`: `"dev:embedded": "vite build --watch"`.
  - `ui/vite.config.ts`: `server.proxy` `/api` -> `http://localhost:3001`;
    inline `watchLegacyFiles` plugin registering the statically-copied legacy
    pages/scripts via `this.addWatchFile`. Needed because verified fact:
    vite-plugin-static-copy v3.2.0 watches NOTHING in build mode (chokidar is
    dev-server-only; build mode copies once per build on writeBundle), and the
    copied files are not in Rollup's module graph - without the plugin,
    editing viewer.html under `vite build --watch` does nothing.
- 8e163f9 refactor(viewer-ui): move demo-trace.bin and flamegraph.css to ui/public/
  - `git mv` both into `ui/public/` (public/.gitkeep dropped - dir now has
    committed content); `legacyPageAssets` list + T02 ruling comment removed
    from vite.config.ts; the two assets added to the watch-plugin list
    (verified: each rebuild re-copies public/ into dist, so the embedded
    loop covers them).
  - Disk readers updated (full sweep evidence below):
    - 13 `ui/test_*.js` + `bench_parse.js`: `path.join(__dirname, "public",
      "demo-trace.bin")`. Path constants only, zero test-logic changes (H2).
    - Rust: `tests/decode_test.rs`, `tests/parser_parity_test.rs` (path fn +
      the stale-fixture regen-hint string; line 340's `"demo-trace.bin"` is a
      display name arg, not a path - left), `benches/decode_bench.rs`,
      `src/server/tokio_stats.rs` (unit test), `src/ingest/decode.rs` (unit
      test), `src/bin/dev_server.rs` (S3 seed path),
      `dial9-trace-format/examples/format_comparison.rs` (3 CWD candidates).
    - `scripts/regenerate_demo_trace.sh` (DEMO_DEST, no-node hint, git add
      hint), `compose.yml` (header comment, `/out` volume now maps
      `./dial9-viewer/ui/public`, install source path),
      `.github/workflows/stress-test.yml` (artifact `cp`; the `node
      test_trace_integrity.js` invocations pass no path / an artifact path,
      unaffected), `dial9-viewer/benchmarks/trace-diagnosis/setup.sh`.
    - Docs that instruct disk reads: `AGENTS.md` (manual regen steps),
      `dial9-viewer/skills/dial9-html-report/SKILL.md` (cp lines; the
      `{html,css}` brace expansion also referenced a never-existent
      viewer.css - splitting into .html and public/flamegraph.css lines
      fixes that incidentally).
    - `aggregate_test.rs` mentions demo-trace.bin in comments with no path
      stated - left per spec.
  - NOT touched: `index.html` / `viewer.html` `?trace=demo-trace.bin` URL
    refs and `<link href="flamegraph.css">` (served URLs, unchanged because
    public/ lands at the dist/dev-server root; also page code = out of scope).
  - Post-move sweep: `grep -rn "ui/demo-trace.bin|ui/flamegraph.css"` across
    all tracked source/doc types -> zero hits outside ui/public paths.
- 945649c feat(viewer): serve ui/dist from disk in dev mode
  - `src/bin/dev_server.rs`: `with_dev_ui_dir(ui/dist)` (was raw `ui/`);
    startup `tracing::warn!` when `dist/index.html` is missing (unbuilt).
    Warns rather than bails: in proxy mode the dev-server is only the /api
    backend and an empty dist is legitimate.
  - `src/lib.rs` (`dial9 serve --dev`): candidates `ui/dist` /
    `dial9-viewer/ui/dist` (was `ui` / `dial9-viewer/ui`), same
    missing-index warning, bail message updated.
- 92d1b05 docs(viewer-ui): document both dev loops; drop obsolete serve.py
  - `ui/README.md`: "Dev loops" section (proxy + embedded commands, public/
    asset location, dial9 serve --dev note).
  - `dial9-viewer/serve.py` deleted. DISCREPANCY vs ticket: the spec's NOTE
    claimed serve.py "does NOT exist in the tree today" as a verified fact,
    but it DOES exist on this branch (tracked since 8454811, confirmed via
    `git log -- dial9-viewer/serve.py`). The primary instruction ("delete,
    obsolete") is unambiguous and both readings converge on "absent after
    T04", so I deleted it rather than stopping. Nothing referenced it
    (grep: only T03's historical HANDOFF text).
- (this commit) docs(T04): HANDOFF

## H5 DISPOSITION (cargo-only checkout after the repoint)

Behavior: on a checkout without Node, `ui/dist` contains only the committed
`.gitkeep`. `dev-server` and `dial9 serve --dev` still start and serve /api
normally, but UI requests 404; both now log a startup WARN naming the fix
(`npm run build` / `npm run dev:embedded`, or browse the Vite server in
proxy mode). Demonstrated: with `dist/index.html` moved aside, dev-server
logged the warning and still bound (log excerpt in EVIDENCE 4).

Why this does not violate H5: H1's accepted consequence (constraint doc,
"or the repo documents that UI work requires Node") and ADR-0004 section 3
("a cargo-only checkout still compiles (committed dist/.gitkeep, empty UI
until built)") already fixed this trade in T02/T03: since the embed narrowed
to `ui/dist`, the embedded (non-dev) UI on a cargo-only checkout is equally
empty, and `ui/README.md` documents that UI work requires Node. H5's target
is the dev LOOP for UI work (edit -> refresh without a cargo rebuild), which
by H1's resolution presumes Node; cargo-only END USERS get populated dists
from release CI. What T04 adds is that the failure mode is now explained at
startup instead of a silently blank page. Serving raw `ui/` instead was not
an option: after the asset move it 404s /flamegraph.css and /demo-trace.bin
(and it stops being the servable set at all once pages migrate to built
entries).

## DoD EVIDENCE

Ports used: Rust dev-server :3001 (spec default; :3002 once for the
unbuilt-dist demo), Vite dev server :5173 (default). All servers killed
after; verified no LISTEN sockets remained.

### 1. Embedded mode: serving + edit-refresh loop

Serving from `ui/dist` via dev-server (after `npm run build`) - all
byte-identical (curl + cmp against source files):

```
OK byte-identical: /index.html /viewer.html /flamegraph.html
  /tokio_stats.html /decode.js /panel_layout.js        (vs ui/<file>)
OK byte-identical: /demo-trace.bin /flamegraph.css     (vs ui/public/<file>)
GET /api/config -> {"default_bucket":"demo-traces","default_prefix":"traces",
  "aggregation_enabled":true,"supports_byo_credentials":true,
  "supports_assume_role":false}
```

(Full 18-file dist-vs-source cmp sweep also run after the build: 16 copied
files + 2 public/ assets, zero mismatches.)

Edit-refresh loop with `npm run dev:embedded` running (temporary edits,
reverted before committing; page-HTML edits are out of scope as landed
changes, these landed nothing):

```
append "<!-- T04-embedded-loop-demo -->" to ui/viewer.html
  -> ~2s later curl :3001/viewer.html | grep -c marker  => 1
git checkout ui/viewer.html
  -> curl :3001/viewer.html | grep -c marker            => 0
append "/* T04-embedded-css-demo */" to ui/public/flamegraph.css
  -> curl :3001/flamegraph.css | grep -c marker         => 1
git checkout; re-curl                                   => 0
```

Also proves the seed path: dev-server logged
`seeded full demo trace ... size=11081465` reading from ui/public/.

### 2. Proxy mode: HMR loop + /api

With dev-server on :3001 and `npm run dev` on :5173:

```
GET :5173/viewer.html         -> served (legacy page from ui/ root)
GET :5173/api/config          -> same JSON as :3001 (proxy works)
GET :5173/demo-trace.bin      -> byte-identical to ui/public/demo-trace.bin
GET :5173/flamegraph.css      -> byte-identical to ui/public/flamegraph.css
```

HMR headless evidence (closest possible without a browser):

```
GET :5173/src/pages/dev-probe.ts  -> "export const uiScaffoldVersion = 1;"
edit src/pages/dev-probe.ts (1 -> 2)
GET (2s later)                    -> "export const uiScaffoldVersion = 2;"
revert; GET                       -> back to version 1
```

Vite served the edited module content immediately after each edit (its
transform + module-graph invalidation path, which is what feeds HMR).
PENDING-MANUAL: observing the websocket patch apply in a live browser tab
without a full reload. Note the probe module is not yet referenced by any
served page (placeholder input), so a browser observation only becomes
meaningful with T13/T14's real page entries.

### 3. `/api/*` works in both

Shown above: :3001/api/config directly (embedded) and :5173/api/config
through server.proxy (proxy mode) return identical config JSON.

### 4. Unbuilt-dist behavior (H5 evidence)

With dist/index.html temporarily absent, dev-server (PORT=3002) logged:

```
WARN dev_server: ui/dist has no built UI - run `npm run build` or
  `npm run dev:embedded` in dial9-viewer/ui (or use `npm run dev` and
  browse the Vite server instead)
  path=.../dial9-viewer/ui/dist
INFO dev_server: dial9-viewer dev server listening on http://localhost:3002
```

## GATES

- `npm ci` -> ok; `npx tsc --noEmit` -> exit 0; `npm run build` ->
  "Copied 16 items, built in 34ms" (was 18: the 2 assets now ship via
  public/).
- Affected JS suites (`node <file>`, all 13 edited test files):
  test_all_skills_snippets, test_fetch_traces, test_directory_scale,
  test_directory_parse, test_flamegraph_export, test_stream_parse,
  test_slice, test_parse_yield_throttle, test_trace_properties,
  test_trace_integrity, test_trace_analysis, test_time_range -> PASS.
  test_flamegraph_recipes -> 1 failing case ("Recipe polls > 5ms: expected
  >0 samples for long polls"), PRE-EXISTING: fails identically on the
  untouched main checkout (fb81219) with the un-moved trace; data-dependent
  on the committed demo capture, unrelated to the path change. Not fixed
  (scope rule); flagged in OBSERVATIONS.
- `cargo fmt --check` -> OK.
- `cargo clippy --all-targets --features __nonlinux_all_features` (macOS) ->
  no errors; pre-existing warnings in untouched crates only
  (dial9-perf-self-profile rate_limit dead code x3,
  dial9-tokio-telemetry unused import) - none in files this ticket touched.
- `cargo nextest run` -> 810/810 passed (includes the re-pathed decode_test
  + parser_parity_test).
- `cargo nextest run --stress-duration 20s` -> 2 iterations, 810 passed
  each, no flakes.
- `cargo build -p dial9-viewer --features dev-server` -> clean.

## REMAINING

- Pending-manual: browser-level HMR observation (see EVIDENCE 2).
- Not run (out of local reach): compose.yml / stress-test.yml paths execute
  only in their Docker/CI environments; changes are path-only and mirror the
  verified regenerate_demo_trace.sh flow.

## BLOCKERS

None.

## OBSERVATIONS (out of scope, not acted on)

- Pre-existing test failure: test_flamegraph_recipes.js "Recipe polls > 5ms"
  fails on the current committed demo-trace.bin (also on main). Likely wants
  a demo regen on a perf-capable host or a threshold revisit.
- Pre-existing clippy warnings on the __nonlinux_all_features set (listed in
  GATES).
- `vite build` deletes the committed `ui/dist/.gitkeep` from the working
  tree (emptyOutDir) - pre-existing since T02; restored via git checkout
  each time, never committed as deleted. A build.emptyOutDir exception
  could stop the churn if it annoys.
- The published crate includes `/benches/**` (decode_bench.rs) but not the
  demo trace it reads - running benches from the crates.io archive was
  already broken before this ticket (archive carries ui/dist only) and
  remains so; harmless for git checkouts.
