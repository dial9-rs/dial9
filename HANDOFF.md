# T15 HANDOFF - Index/browser page contract amendments (4 defect fixes)

(Replaces the T40 HANDOFF inherited through the branch chain; that record
lives in the history of the integrated tip 2b0c5f1.)

Branch `ticket/T15-index-amendments`, based on the integrated tip 2b0c5f1
(includes T13 flamegraph + T14 browser migrations, parity tooling, docs).
Contract: docs/ui-inventory/features/01-index-html.md rows G8, C6, I2
(+F4/I3 display consequences), F10-axis - amended consciously per ADR-0004
section 1, AFTER T14's faithful port, so parity and amendment never share a
diff. Legacy page untouched; frozen core untouched; F3 legend (chunk 2)
untouched. No STOP-gate hit (the ticket pre-decided the bucket-filter
mechanism and sort semantics; no materially forked readings encountered).

## STATUS

COMPLETE. All four amendments landed on the MIGRATED page only
(src/pages/browser/ + lib/trace), each with Vitest coverage; the parse_key
suite is re-pointed at lib/trace/keys.ts; the inventory rows are amended
in-branch with a superseding "2026-07-10 T15 amendments" verdict table;
ledger entries appended; row-walker GREEN on BOTH pages (per-side walks);
census diff and J6 behavioral differ unchanged from T14's baseline; all
formal gates pass including the full Rust set (.rs files changed for C6).

## COMPLETED (commits on top of 2b0c5f1)

- 5a05082 I2 amendment: unknown-layout keys render RAW. keys.ts unknown
  variant gains layout-independent filename epoch/segIndex; extractPrefix
  moves into keys.ts; legacy-keys.ts + its test RETIRED (T14's marked
  seam); raw table shows the full key across Service/Host/Boot
  (td.rawkey[colspan=3], pure model raw-rows.ts); heatmap groups/labels
  unknown keys by raw directory path; titles via lib/trace/title.ts (svc/
  host from known layouts only; from/to window keeps unknown filename
  epochs); aggregation scope params (H3/H6) from known layouts only.
  Parity: walk-rows.mjs gains the per-side `side` context; I2/F4 walkers
  branch on it.
- 4cc82e6 suite re-point: tests/core/parse_key.test.ts targets
  lib/trace/keys.ts (closes T10's interim note; same four T10 cases).
- a7ed1ea G8 amendment: sortable raw-results table. Ticket-decided
  semantics: ALL columns sortable, numeric for Trace Start/Seg #/Size,
  lexical otherwise; repeat-click flips direction; indicator in the legacy
  .sort-arrow slot + aria-sort; sort rebuild PRESERVES checkbox selection
  (search/TZ rebuilds still drop it - recorded legacy behavior);
  getSelectedKeys follows the active sort. Pure sort model in raw-rows.ts.
- 00ed079 C6 amendment: config-driven bucket filter. Rust: AppState gains
  bucket_filter (default "dial9") + with_bucket_filter(impl Into<String>)
  following the existing with_* pattern; /api/config advertises it. Page:
  resolution = ?bucket_filter= page-URL override (empty allowed = no
  filtering; re-appended by syncUrl so it survives replaceState - shared
  url_state.js untouched) > config value > "dial9" fallback (pure module
  bucket-filter.ts); picker predicate/messages render the active filter
  (default strings byte-identical to legacy); #607 toggle preserved.
- a851073 F10-axis amendment: axis ticks carry the date (YYYY-MM-DD
  HH:MM:SS) when the visible span crosses a calendar-day boundary in the
  active TZ mode (crossesDayBoundary + fmtTick withDate). Tick COUNT
  unchanged; selection-count readout (H1) unchanged.
- (final commit) inventory amendments + ledger entries + README per-side
  note + this HANDOFF.

## KEY DECISIONS

1. **Per-side walker convention** (the ticket's open design point): the
   registry stays ONE registry serving both pages; walk-rows.mjs derives
   `side` from the page URL (/new/ = migrated) and passes it in the walker
   context. Only the amended rows' walkers branch (G8, C6, I2, F4, F10);
   everything else is side-blind. Chosen over "amended-row walk against
   the new page only" because the README self-test (legacy walk must stay
   green) and T14's identical-verdict precedent both want the FULL
   inventory walkable on both sides. The amended rows' verdicts live in a
   new "| Row | Verdict |" table the inventory parser already treats as a
   superseding refresh (later entries win), so no parser change was
   needed.
2. **UnknownTraceKey carries filename epoch/segIndex.** The
   {epoch}-{index}.bin[.gz] filename convention is independent of the
   directory layout, so parsing it is not a positional guess - and
   without it, unknown-layout segments would vanish from the heatmap
   (start>0 filter), the window trim, and the epoch sort: a "lose
   nothing" violation. The mislabel defect was only ever about
   directory-derived Service/Host/Boot.
3. **Heatmap grouping for unknown keys = raw directory path** (service ""
   , host = dir path into the frozen groupByHost; HeatmapSegment gains a
   `layout` discriminant so rebuildLabels renders the path without the
   "svc / host" split). Granularity >= legacy (legacy grouped by the two
   trailing dir components).
4. **title.ts from/to now includes unknown-key filename epochs** (T09
   wrote "unknown keys contribute only to segs", decided when the variant
   had no epoch). The legacy title DID carry the window for these keys;
   dropping it would lose behavior unrelated to the mislabel. svc/host
   stay known-only. Documented in title.ts, tested.
5. **C6 became a GATED row** (amendment table records VERIFIED): the new
   C6 walker drives the default-filter toggle path on both sides and the
   ?bucket_filter=demo override + single-match auto-select on the new
   side. C6 was previously CODE-READ/not gated - this ADDS walker
   coverage.
6. **No CLI flag for bucket_filter.** The ticket decides the /api/config
   field + query-param mechanism; the AppState setter makes it server-
   configurable for library users. Wiring a --bucket-filter CLI flag is
   left open (one-liner in cli.rs if wanted).
7. **Sort indicator glyphs are plain ASCII** ("^"/"v" in the .sort-arrow
   span) and only render after a sort click, so the default-state census
   is untouched (th elements are not censused anyway).
8. **J6 differ + census stay clean by construction:** the F10 amendment
   changes tick TEXT not tick COUNT (the readout schema compares counts);
   no new censused affordances were added.

## EVIDENCE (dev-server on :3091 serving ui/dist, playwright chromium)

### Per-fix Vitest (all green inside the full run below)
- I2: src/lib/trace/keys.test.ts (unknown carries epoch/segIndex;
  extractPrefix), src/lib/trace/title.test.ts (no svc/host leak; window
  kept), src/pages/browser/raw-rows.test.ts (raw display model).
- G8: raw-rows.test.ts sorting block (numeric epoch/size/seg# incl. the
  10-vs-3 lexical trap, lexical service, unknown-row sort key, direction
  toggle, default order).
- C6: src/pages/browser/bucket-filter.test.ts (predicate, override
  resolution incl. empty-string override) + Rust
  server_test.rs::config_returns_defaults (bucket_filter="dial9") and
  ::config_reports_custom_bucket_filter (with_bucket_filter honored).
- F10: src/pages/browser/format.test.ts (crossesDayBoundary UTC/local,
  fmtTick withDate).
- Suite re-point: tests/core/parse_key.test.ts 4/4 against keys.ts.

### Row-walker on the amended rows (--rows G8,C6,I2,F4,F10)
- NEW page: 5/5 VERIFIED, GREEN. Evidence lines: G8 "Service header
  sorts: asc -> desc toggle with indicator; rows intact"; C6 "toggle path
  ok; ?bucket_filter=demo surfaced + auto-selected demo-traces"; I2
  "unknown-layout key rendered raw (full key across Service/Host/Boot);
  epoch kept"; F4 raw-dir label; F10 "9 date-carrying ticks rendered".
- LEGACY page: 5/5 VERIFIED, GREEN. G8 dead click, I2 mislabel, F4
  "host-0 / abcd", F10 HH:MM:SS ticks, C6 toggle path - the preserved
  recorded behaviors re-derive.

### Full-inventory walks (both sides)
- NEW: 75 rows - 43 VERIFIED, 32 NOT-TRIGGERABLE, zero FAILED -> GREEN.
- LEGACY: 75 rows - 43 VERIFIED, 32 NOT-TRIGGERABLE, zero FAILED -> GREEN.
- Split moved from T14's 42/33 to 43/32: C6 is newly gated on both sides.
  Full tables: parity/out/walk-{new,legacy}-t15.{md,json} (parity/out is
  gitignored by design; reproduce per ui/README.md).

### Other parity layers (not DoD-required; run as sanity)
- Census diff legacy-vs-new: 1 entry - the ledger-justified #d9-ui-switch
  CHANGED line only (identical to T14's baseline).
- Behavioral differ J6: ZERO DIFF (both checkpoints identical, 9 fields).

### Formal gates
- npx tsc --noEmit: clean.
- npm run test (full Vitest; pretest boundary check): 50 files passed,
  1 skipped; 891 passed, 1 expected fail, 11 skipped.
- npm run build: green (new/index.html + assets emitted; legacy pages
  static-copied); node scripts/check-core-imports.mjs: OK.
- Rust (this ticket touches .rs): cargo fmt --check clean; cargo clippy
  --all-targets --features __nonlinux_all_features (macOS): dial9-viewer
  clean - pre-existing warnings in dial9-perf-self-profile and
  dial9-tokio-telemetry libs (unused rate_limited/time_since_epoch etc.,
  untouched crates; reported, not fixed per AGENTS.md scope rule); cargo
  nextest run: 811/811 passed; cargo nextest run --stress-duration 20s:
  2 iterations, all 811 passed each, no flakes. Shuttle suite not run (no
  #[cfg(all(test, shuttle))] / flush/source paths touched).
- Dev-server killed after the evidence runs.

### Untouched surfaces
- git diff 2b0c5f1..HEAD touches NO frozen-core file and NO legacy page
  (index.html/viewer.html/flamegraph.html/tokio_stats.html unchanged);
  url_state.js and ui-switch.js unchanged. Rust changes are confined to
  dial9-viewer/src/server/{config.rs,mod.rs} + tests/server_test.rs
  (additive, builder-pattern, backwards compatible).

## REMAINING / OPEN

- Optional follow-up (out of fence): a --bucket-filter CLI flag in
  dial9-viewer feeding AppState::with_bucket_filter, and a legacy-page
  carry of the ?bucket_filter= override if anyone wants it there (the
  legacy page is fenced off from T15 by design).
- The aggregated-mode scope change (H3/H6 params from known layouts only)
  means a selection made ONLY of unknown-layout keys sends no service/host
  narrowing to /api/flamegraph - honest behavior (previously it sent
  WRONG names), noted here for the chunk-2 viewer tickets.
