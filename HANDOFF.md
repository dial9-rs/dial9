# T14 HANDOFF - Migrate index.html (browser page)

(Replaces the FIX-T38 HANDOFF inherited through the branch chain; that
record lives in the history of merge c1d923e.)

Branch `ticket/T14-migrate-browser`, based on the integrated chunk-1 tip
c1d923e. Contract: docs/ui-inventory/features/01-index-html.md (all rows
A-I incl. the T01 additions), amendments fenced to T15.

## STATUS

COMPLETE. The S3 browser page is ported behavior-preserving to a Vite entry
(`new/index.html` + `src/pages/browser/`), registered in the dual-UI switch,
and green on the T12 parity stack against the dev-server: row-walker GREEN
(zero FAILED, verdict split identical to the legacy baseline), census diff
EMPTY except the ledger-justified switch control, behavioral differ ZERO
DIFF on J6, switch round-trip preserves the query/bucket/search context.
All formal gates pass. The four T15 amendment targets (G8 dead sort, C6
picker, I2 rendering, F10 time-only axis) are ported AS-IS with a marked
seam. Frozen core and legacy index.html untouched.

## COMPLETED (commits on top of c1d923e)

- 5a2a0e1 typed lib seams: lib/canvas/heatmap.ts + lib/trace/prefixes.ts
  (frozen heatmap.js / prefix_detect.js re-exported through the sanctioned
  barrels; no math re-implemented).
- e32dbc6 the page port: new/index.html (legacy markup verbatim minus
  inline onclick; ids/labels/titles/placeholders identical),
  src/styles/browser.css (legacy <style> verbatim), src/pages/browser/*
  (store-driven components: header/tz, search controls, tabs, browse view
  painter, selection overlay, pointer interactions, raw table, actions
  bar, creds panel, footer, url-restore + config bootstrap in main.ts);
  vite input `new-index`; ui-switch registration + registry-test update.
- b69c72a unit tests: parseKeyCompat property-compared byte-identical
  against a verbatim transcription of the legacy parseKey (every branch,
  incl. the Finding-1 demo-key mislabel); titleParamsCompat/extractPrefix;
  format helpers in both TZ modes.
- 5b10ab5 live-server parity fixes (see decisions 1, 3, 4 below; plus the
  missed #heatmap-reset-zoom wiring the F16 walker caught).
- (final commit) ledger entry, parity/switch-roundtrip.mjs evidence
  script, this HANDOFF.

## KEY DECISIONS (review here first)

1. **`<base href="/">` in new/index.html.** The entry serves off-root
   (`/new/index.html`) while the legacy markup's relative navigation
   targets (`viewer.html` popups, the `?trace=` passthrough redirect,
   footer link) are part of the observable contract (census raw hrefs,
   walker popup URLs). The base tag resolves them at the server root
   exactly like the root-served legacy page - byte-identical markup,
   working navigation. Consequence handled: syncUrl's history.replaceState
   passes the pathname explicitly (a bare "?qs" resolves against the base
   and would rewrite the path to "/").
2. **T15 seam: `src/pages/browser/legacy-keys.ts`.** lib/trace keys.ts
   returns `layout:"unknown"` for undocumented layouts (the ADR-0004
   defect fix); the legacy page positionally mislabels them (Finding 1).
   T14 is a faithful port, so `parseKeyCompat` consumes the typed parser
   and reproduces the legacy positional fallback for `unknown` -
   unit-tested byte-identical against the legacy algorithm. Rendering,
   heatmap grouping, and `titleParamsCompat` (popup URLs carry
   `svc=host-0&host=abcd` for the demo key, as legacy did) all go through
   it. T15 retires this module: switch rendering to the unknown
   discriminant and re-point titles at lib/trace/title.ts. Seam marked
   in-file.
3. **Store scheduler = microtask on this page** (state.ts). The RAF
   default let anything reading the DOM immediately after a click (the
   T12 walkers assert click -> effect with no settle wait, as the legacy
   synchronous handlers allowed) see a pre-flush frame; five walkers
   (B3/C5/C8/D6/G4) failed on it. queueMicrotask keeps per-task coalescing
   (one flush per event turn) and restores legacy synchronous-DOM
   observability. Renders here are cheap idempotent chrome writes plus an
   identity-memoized canvas paint (repaints only on rows/domain/tz/resize
   change - the legacy redraw triggers), so the RAF-per-frame perf
   concern (F2) does not bite; documented in-file.
4. **Leaf seam imports, not the lib barrels.** The lib/canvas + lib/trace
   barrel indexes evaluate trace_analysis.js / trace_parser.js at module
   init; in a browser bundle without the <script>-established parser
   globals that throws at boot ("TraceParser not found") - and the legacy
   browser page never loaded the parser at all. The page imports
   lib/canvas/heatmap.ts, lib/trace/keys.ts, lib/trace/prefixes.ts and
   lib/trace/object-urls.ts (moved out of load.ts with its doc comment;
   load.ts re-exports it, so its import surface and the lib/trace barrel
   are unchanged). Boundary check stays green.
5. **creds.js / url_state.js / ui-switch.js stay `<script src>` globals**
   in the new entry: window.Dial9Creds is the page's stable userscript
   contract (C10), url_state.js is shared verbatim with the legacy page,
   ui-switch.js is T38's designated include. Typed via
   src/pages/browser/globals.d.ts (type-only import of the creds .d.ts;
   .d.ts files are boundary-exempt).
6. **Census identity preserved:** legacy's id-less buttons (quick ranges,
   raw Search, Select/Deselect All) stay id-less (census keys are
   tag:label); handler hooks use data-* attributes (not censused).
7. **Known micro-divergence (parity-invisible, documented):** the Search
   button's disabled state renders continuously from state
   (serverHasPrefix && prefix empty), so for the few ms before /api/config
   resolves it reads enabled where legacy kept the HTML `disabled`
   attribute until its first updateSearchReady call. All parity tooling
   waits for bootstrap; auto-search evaluates the same readiness formula
   from state (not the DOM), so behavior is unchanged.
8. **Legacy quirks deliberately reproduced** (faithful-port rule): TZ
   toggle resets zoom + selection and re-shows stale rows after a failed
   search; TZ toggle on the raw tab drops the checkbox selection and, on
   empty results, re-fires the sample-key hint (which uses plain fetch,
   not apiFetch - the legacy G7 asymmetry); "Searching…" remains as the
   hidden status text after a successful search (the differ's
   browse.status readout depends on it).

## REMAINING / BLOCKERS

None for T14. Notes for successors:
- T15 lands the four amendments and retires legacy-keys.ts (seam marked);
  it also re-points tests/core/parse_key.test.ts per its DoD.
- T13 touches the same three shared spots additively: vite.config.ts
  rollup input, ui-switch.js NEW_UI_ENTRIES, and the registry expectation
  in tests/ui_switch.test.ts ("one line there, one here"); trivial merges.
- FIX-T38's proposed ui-param carry-through for the LEGACY pages' URL
  rebuilds (its finding-2 residual) remains open - out of T14's fence
  (legacy page untouched). The new page needs no carry (the ?ui=
  convention applies to the canonical URL only).
- T38's browser-level round-trip test obligation for run()/mountControl
  is discharged by parity/switch-roundtrip.mjs (first registered page).

## EVIDENCE (dev-server on :3061 serving ui/dist, playwright chromium)

### Gates
- `npx tsc --noEmit`: clean.
- `npm run test` (full Vitest; pretest = boundary check): 48 files passed,
  1 skipped; 866 tests passed, 1 expected fail, 11 skipped. Includes the
  new src/pages/browser suites and the updated tests/ui_switch.test.ts.
- `npm run build`: green; dist/new/index.html emitted with the plain
  script tags preserved; legacy pages still static-copied verbatim.
- `node scripts/check-core-imports.mjs`: OK.
- `cargo build -p dial9-viewer`: exit 0 (JS/HTML-only change otherwise; no
  .rs touched - per AGENTS.md no nextest/stress/clippy run).

### Row-walker (features/01, shared verdict mapping)
- NEW page (http://localhost:3061/new/index.html):
  75 rows - 42 VERIFIED, 33 NOT-TRIGGERABLE, zero FAILED -> GREEN.
  Stability re-run: GREEN again, same split.
- LEGACY page (same server, post-registration):
  75 rows - 42 VERIFIED, 33 NOT-TRIGGERABLE, zero FAILED -> GREEN.
  Identical verdict split: the new page matches the legacy-derived
  verdicts exactly ("matching or exceeding recorded verdicts").
- G8 (recorded DEAD-CONFIRMED) re-derived VERIFIED on the new page:
  header click has no effect - the dead affordance is ported as-is.
- NOT-TRIGGERABLE rows, explicitly listed (recorded verdict in parens):
  A3/D8/D10/F19/I6 (NOT-TESTED), A5/F2/F3/G1/G7/H3/I7 (VERIFIED-API,
  refresh), A6/A7/C6/I8 (CODE-READ, refresh), C7/D4/F5/F7/F8/F9/H5
  (NOT-TRIGGERABLE), C11/F20/H4 (NOT-TRIGGERABLE, refresh), C10/I5
  (CODE-ONLY), D2/H6/I1/I3 (PARTIAL), F1 (NOT-OBSERVED).
- Full verdict table: dial9-viewer/ui/parity/out/walk-new.md (+ .json;
  parity/out/ is gitignored by design - reproduce with the README
  commands against :3061).

### Affordance census diff (legacy vs new)
```
#   A: http://localhost:3061/index.html (34 affordances)
#   B: http://localhost:3061/new/index.html (34 affordances)
- CHANGED #d9-ui-switch: label: "Switch to new UI" -> "Switch to legacy UI",
  href: "/new/index.html" -> "/index.html?ui=legacy"
1 diff entries
```
EMPTY except the dual-UI switch control - exactly the ledger entry
(docs/tickets/ledger.md).

### Behavioral differ, journey J6 (S3 browse)
```
== J6 (S3 browse)
   checkpoint heatmap: identical (9 fields)
   checkpoint selected: identical (9 fields)
ZERO DIFF
```

### Switch round-trip (query/bucket/search context preserved)
`node parity/switch-roundtrip.mjs http://localhost:3061`:
```
ok: query preserved legacy->new (?bucket=demo-traces&prefix=traces&last=24)
ok: bucket/prefix/last=24 restored on the new page
ok: new side renders the way back (ui=legacy pinned on the hop)
ok: query preserved new->legacy after the legacy page's first URL sync
ROUND-TRIP OK
```

### Untouched surfaces
- `git diff c1d923e..HEAD` shows zero changes to the frozen core files and
  to legacy index.html/viewer.html/flamegraph.html/tokio_stats.html; the
  only ui-root script change is ui-switch.js's one-line registry entry
  (T38's designated registration point, not frozen core).
- Existing creds/heatmap/prefix vitest suites: untouched, green in the
  full run.
