# T01 HANDOFF - Refresh feature inventories against HEAD

## STATUS

COMPLETE (docs-only; no code changed). All DoD checks run; evidence below.
Resumed from checkpoint `cfdf926` (previous agent killed mid-work).

## COMPLETED (commits on `ticket/T01-inventory-refresh`)

- `cfdf926` (inherited checkpoint): features/01 refresh complete; features/03
  header + section A/E rows done.
- `91e2c0f` docs(T01): features/03 finished - the missing section P
  (aggregated `?api=1` mode, rows F168-F185), F146 stale-anchor +
  runtimeWorkers fix, F165 applySpawnFilter->applyFilters, section C
  api-mode cross-ref.
- `2f77c0d` docs(T01): features/02 refreshed - #596/#600 rows (B12 rewrite,
  new B18, G21-G24, F10/W5/W6/G1/G13/G17/G18/B8 amendments), anchors
  remapped with the CORRECT base (see Key Finding), trace_parser.js anchors
  re-derived by hand post-#582 reindent.
- `ffa1fc7` docs(T01): 2026-07-08 verification sections appended to
  features/02 and features/03; test-suite inventory added to
  `docs/ui-inventory/02-architecture.md` section 3 (grounds T10/T11).
- (this commit) chunk-1 ownership updates + HANDOFF.

## KEY FINDING (for anyone re-deriving anchors later)

features/02's ORIGINAL anchors match commit `1f257f1` (#564), NOT the
pre-drift tip `544afd2` (#581) that the interrupted agent's `tmp/linemap.py`
assumed. Only `viewer.html` differs between those two trees, so the
features/01 and features/03 remaps done with base 544afd2 were unaffected
(spot-checks confirm). features/02 was re-remapped from base `1f257f1`;
`tmp/linemap.py` BASE was updated accordingly (tmp/ is deliberately
uncommitted).

## DRIFT-SET DISPOSITIONS (DoD: one per commit)

| Commit | PR | Disposition |
| --- | --- | --- |
| 97cc9fa | #582 | Rows updated: features/01 F2 (rewritten `/api/browse`), F3/G7 (last-24h sample window), G1 (30-day raw window), F20 (truncation banner, NEW), I7. Whole-file `trace_parser.js` reindent -> all its anchors in features/02/03 re-derived. |
| ede024f | #570 | Rows updated/added: features/01 A5 (`aggregation_enabled`), H3 (two-mode Flamegraph), H6 (Tokio Stats button, NEW), I8 (`extractPrefix`, NEW); features/03 section P F168-F185 (NEW aggregated mode) + module list. `tokio_stats.html` page itself = T40 (out of T01 scope). `trace_properties.js` is a decoder-parity test oracle loaded by NO page -> no feature rows (recorded in the test-suite inventory). |
| 5847e6f | #585 | Rows updated/added: features/01 A6/A7 (URL state, NEW) + B3/D6/D8/E1 amendments; `test_url_state.js` inventoried. |
| ea899f4 | #587 | No change to features/01-03 surfaces (XSS fix in `tokio_stats.html` only; that page's inventory is T40). |
| 2df3285 | #586 | No new UI surface (server-side browse overflow attribution fix); referenced in features/01 F20's row text. |
| c0a97e1 | #600 | Rows updated/added: features/01 H4 (200 MB cap); features/02 B8, B12 (rewritten), B18 (NEW), W5; features/03 F1/F4-F6/F9 amendments + F166 (NEW). Tests `test_parse_yield_throttle.js` (new), `test_fetch_traces.js`/`test_heatmap.js` (extended) inventoried. |
| 86270d9 | #596 | Rows updated/added: features/02 G1/G13/G17/G18 amendments + G21-G24 (NEW runtime grouping), F10, W5/W6; features/03 F167 (NEW runtime filter) + F146/F165 amendments + section E header. `test_runtime_groups.js` inventoried. |
| a89e0cc | #597 | No UI-visible change on inventoried pages: server-side assume-role credential resolution; `/api/config` gained `supports_assume_role` but no UI file consumes it (grepped index.html/creds.js); observed `false` on the dev-server (features/01 refresh notes). |
| 84a21e5 | #607 | Rows updated/added: features/01 A5/A6/A7, C6 (show-all toggle - resolves 2026-06-30 Finding 2), C7, C11 (region auto-detect, NEW), D1/D10, I7; HTTP 421 WrongRegion referenced in C11. |

## EVIDENCE (DoD checks)

- Dev-server walk (`CARGO_TARGET_DIR=.../target PORT=3001 cargo run -p
  dial9-viewer --bin dev-server --features dev-server`, gate `/api/config`
  JSON ok): `aggregation_enabled:true`, `supports_byo_credentials:true`,
  `supports_assume_role:false`. `GET /api/flamegraph?bucket=demo-traces&
  prefix=traces` read-only -> instant empty tree, `files_folded:0`, generic
  `metadata.facets` array present; `&refine=true` -> folds demo trace
  (`total_samples:147`, coverage 1/1 files, 1/1 hosts, total_bytes 4336378,
  facets populated: source[cpu,sched], thread_class[worker], host[local],
  spawn_location x4); subsequent read-only -> folded tree persists. Matches
  section P rows F168/F170/F173/F176/F177.
- NOT-TRIGGERABLE justifications: demo trace has ZERO named runtimes
  (`trace.runtimeWorkers` empty Map via Node against `trace_parser.js`;
  `computeRuntimeGroups` -> one inferred `main` group, 2 workers) -> all
  multi-runtime rows (02 G21-G24/F10, 03 F167) marked NOT-TRIGGERABLE with
  that reason. F183 (no-coverage response) NOT-TRIGGERABLE: dev-server always
  returns `coverage`.
- Unit runs (local, all green): `test_flamegraph_api.js` (48 passed),
  `test_fetch_traces.js`, `test_parse_yield_throttle.js`,
  `test_runtime_groups.js`, `test_url_state.js`, `test_heatmap.js`.
- Anchor spot-checks: features/01 I2/I3/I4 = `index.html:1006/1686/1713` at
  HEAD (the three named in the chunk ticket - already corrected by the
  checkpoint, re-confirmed). features/02: `esc()` 1019-1021, LABEL_W/LANE_H
  1394-1395, addLegend 6646, sidebar-Escape 6282-6287, POI select 853-859,
  eventDetailHtml 3976, relatedHtml 4044, showStackPopup 5455, showSchedPanel
  6008, showIdleTimeFlamegraph 6765, showHeapFlamegraph 6825, showFlamegraph
  6989. features/03: createFlamegraph 128, onSearchInput 520-525, zoomTo
  527-533, handleEscape 752, applyFilters 779-820, setData 829, setTreeDirect
  982, buildRuntimeFilterData trace_analysis.js:1601, runtimeWorkers
  trace_parser.js:665/943. All land on their cited functions.
- Snapshot date noted in each touched file (REFRESHED 2026-07-08 headers +
  refresh sections in features/01/02/03; dated subsection in
  02-architecture.md).

## REMAINING

None for T01. Follow-ups belong to other tickets (below).

## BLOCKERS

None.

## OPEN QUESTIONS (maintainer input wanted; defaults recorded, not assumed)

1. Section P ownership: chunk-1's "features/03: T13 (all rows)" now implies
   T13 also migrates the aggregated `?api=1` mode (F168-F185) - a materially
   larger scope than the drafted "165 rows". Alternative reading: group the
   aggregated-mode migration with the aggregation tickets (T41 adjacency,
   chunk 3). Current text keeps it in T13; the chunk-1 summary + T13 body now
   flag the question. Decide before T13 starts.
2. T15's C6 amendment target may be moot: #607 already shipped the bucket
   "Show all" toggle (features/01 Finding 2 marked RESOLVED). Same for the
   known-defect placement bullet in `02-architecture.md` section 4
   ("dial9 bucket-filter lockout") - left untouched (T15's scope), flagged
   here.
3. `flamegraph_api.js` exports `sourceFacetOptions`/`threadFacetOptions`
   that NO page consumes at HEAD (only unit tests) - dead-helper candidates,
   noted CODE-ONLY in features/03 section P.

## NOTES

- tmp/ (linemap.py, remap_md.py) committed at the maintainer's request
  (the ticket originally said to leave them out). CAUTION: BASE in
  linemap.py is hardcoded to `1f257f1`, which is correct for features/02
  re-runs ONLY; features/01/03 originals were anchored at `544afd2`.
- A dev-server may still be listening on :3001 from this session's
  verification (background task); kill it if it lingers.
