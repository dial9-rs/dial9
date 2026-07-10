# T40 HANDOFF - Inventory: tokio_stats.html (features/04)

(Replaces the FIX-T38 HANDOFF inherited through the branch chain; FIX-T38's
record lives at commit b5864e5.)

## STATUS

DONE - no STOP-gate hit. Docs only (scope fence respected: no code changes
anywhere, features/01-03 untouched, no push/PR). The dev-server started for
the walk was killed (port 3071 verified free).

## COMPLETED (commits on `ticket/T40-tokio-stats-inventory`, on top of c1d923e)

- `7bcedc9` docs(viewer): add features/04 inventory for tokio_stats.html -
  `docs/ui-inventory/features/04-tokio-stats-html.md`, 69 rows, sections A-K,
  snapshot date 2026-07-10, full verdict table + reproduce recipe.
- `930fea5` docs(tickets): ownership summaries updated in
  `docs/tickets/chunk-3-post.md` (chunk-3 summary + T40 completion note +
  T41 heads-up). Chunk-1's ownership summary does NOT reference features/04
  (grep-checked), so per the DoD's conditional it was not edited.
- (this commit) HANDOFF.

## ROW / VERDICT COUNTS

69 rows: A (bootstrap/URL contract) 10, B (periods) 6, C (toolbar) 4,
D (loading/refinement) 6, E (client stats) 5, F (single-period view) 4,
G (tabs/diff) 9, H (exemplar links) 4, I (XSS #587) 3, J (backend
`/api/tokio-stats` contract) 15, K (cross-cutting) 3.

Verdicts (per-row table in the doc's "2026-07-10 validation" section):
- VERIFIED component (live curl walk / served bytes / fix-diff / T38 unit):
  22 rows (A1, A3, A8, A10, C1, D1, D2, D3, D5, H4, I1, J1-J3, J5-J7,
  J10-J15).
- CODE-READ (no browser driver; re-derivable by the T12 row-walker once T41
  registers features04 walkers): the remaining 47 rows.
- NOT-TRIGGERABLE sub-cases recorded with reasons (all T42 fixture targets):
  time-windowed scopes (demo-key epoch vs date-path catch-22 - BOTH window
  directions 404, reproduced live), off-CPU class 0 (demo max poll ~1ms <
  10ms confidence bound), cap-plateau refinement (1 matched file < baseline
  4), multi-host/-service data, live multi-period diff, `--agg`-server gate
  variant.
- Status tags: 1 DEAD row (H4, see findings), rest OK/CONDITIONAL.

## EVIDENCE - dev-server walk log (2026-07-10, port 3071)

Build + launch (post-T04 the dev-server serves ui/dist):
`npm ci && npm run build` in dial9-viewer/ui (green, 17 items static-copied),
then `CARGO_TARGET_DIR=<repo>/target PORT=3071 cargo run -p dial9-viewer
--bin dev-server --features dev-server`.

- `/api/config` -> `{aggregation_enabled:true, supports_byo_credentials:true,
  supports_assume_role:false, default_bucket:"demo-traces",
  default_prefix:"traces"}`.
- `GET /tokio_stats.html` -> 200 text/html, 19483 bytes, byte-identical to
  `ui/tokio_stats.html` (diff -q); `ui-switch.js` + `creds.js` 200 and
  byte-identical.
- Cold poll `?bucket=demo-traces&prefix=traces` -> instant
  `{time_span_ns:1, total_polls:0, by_spawn_loc:[], coverage:{files_matched:1,
  files_folded:0, samples_folded:0, total_bytes:4336378, hosts_matched:1,
  hosts_folded:0}}`.
- `&refine=true` -> `total_polls:94212`, `time_span_ns:4143811668`, folded
  1/1 files + 1/1 hosts, 5 spawn locations (top:
  `examples/metrics-service/src/axum_traced.rs:243:33`, 3319 notable polls).
  Asserted on the wire with node: durations desc-sorted, all >= 100000 ns,
  classes aligned + values {1,2,3} only, locations sorted by notable count
  desc, one zero-notable location present, per-class exemplars with
  host:"local" + raw source_key.
- Warm read-only poll -> identical folded counts (frozen terminator).
- `prefix=no-such-prefix` -> 404 `no source files match this scope`.
- no `bucket` param -> 404 `tokio-stats requires aggregation (start with
  --agg or supply a bucket)`.
- `refine=1` -> 400 (strict serde bool; page always sends literal "true").
- `service=demo-service` 200; `host=local` 200; `host=local&host=nonexistent`
  200 (OR semantics); `host=host-0` 404 (demo key's host component is
  `local`).
- Time windows: `start_ns/end_ns` at the data's row timestamps (June 2026)
  -> 404; at the key's date-path hour (2026-04-09 19:00 UTC) -> 404 (listing
  finds the file, `scope_matches` rejects on the filename epoch 1744224000 =
  2025-04-09). Catch-22 documented as features/01 finding 3 biting the
  aggregate listing.
- `GET /api/trace?...` -> 404 - confirms finding 1 (exemplar deep links dead
  at HEAD; `/api/trace` removed by #582 (`git show 97cc9fa`) while #570's
  page still targets it).

Doc integrity: markdown tables render (pipe-balanced), anchors spot-checked
against the tree - page anchors from the numbered read of tokio_stats.html
(escapeHtml 93, syncUrl 147, exemplarLink 166, computeStats 177,
renderFromCache 226, renderSinglePeriod 272, renderDiffView 302, loadPeriod
371, auto-load 428), backend anchors re-greped (get_tokio_stats
tokio_stats.rs:71, classify_poll 224, scope_matches aggregate.rs:236,
time_scoped_prefixes refine.rs:381, route mod.rs:448, gate config.rs:30);
the features/01 H6 cross-link notes the +1 line shift T38 introduced in
index.html.

## NOTABLE FINDINGS (detailed in the doc)

1. H4: exemplar deep links broken at HEAD (`/api/trace` removed by #582);
   fix candidate is one line (`/api/object?bucket&key`). T41 ledger decision.
2. D4: coverage fetched but never displayed (no refinement progress UI,
   unlike flamegraph F174/F176-F179); silent plateau at the sampling cap.
3. E2/H2: mixed (2) + unknown (3) classes and their exemplars are on the
   wire but invisible in the UI.
4. G3: diff view throws (TypeError on `first.rate`) when P1 failed to load
   while 2+ later periods loaded. Code-read.
5. A4/A7: URL restores at most 10 periods; sync writes all of them.

## OPEN QUESTIONS

None blocking. Two notes for the maintainer/next tickets:
- The T40 DoD's "T12 row-walker used for validation" was satisfied in the
  sanctioned fallback mode (hand-walk + curl in the features/01-refresh
  style, each verdict recording its method): `ui/parity/walkers/` contains
  only `features01.mjs`, and features/04 walkers are T41's deliverable. A
  minimal features04 registry was considered and skipped as not-cheap: the
  page's rows are dominated by API-contract and multi-period states the
  walker lib has no fixtures for yet.
- Whether to fix finding 1 (dead exemplar links) in legacy before T41, or
  only in the migrated page, is a ledger call outside T40's docs-only fence.
