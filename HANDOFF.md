# T42 HANDOFF - Synthetic trace fixtures (unblock NOT-TRIGGERABLE rows)

(Replaces the T40 HANDOFF inherited through the branch chain; T40's record
lives at commit bd1503b.)

## STATUS

DONE - no STOP-gate hit. All three DoD legs delivered (flipped rows walked
green, T17 upgraded off repeated-demo, T39 large input reproducible). Scope
fence respected: frozen JS core untouched, page HTML untouched,
features/01-04 inventory files untouched (flipped verdicts live in the walk
output + this HANDOFF; re-recording is T39's final gate). No push, no PRs.
All dev-servers started during the work were killed.

## COMPLETED (commits on `ticket/T42-synthetic-fixtures`, on top of bd1503b)

- `626f68b` feat(viewer): synthetic trace fixture generator (gen-fixtures
  bin) - `dial9-viewer/src/bin/gen_fixtures.rs`, feature-gated behind
  `dev-server`, excluded from the published package. Deterministic (seeded
  jitter, fixed clock anchors; regenerating without a code change is a
  byte-identical no-op, verified by hash diff across runs).
- `98a4ff9` feat(viewer): dev-server fixture seeding - `DIAL9_SEED_DIR`
  (mtime-preserving bucket-tree seed; mtimes ARE the S3 last_modified the
  heatmap uses as segment end) + `DIAL9_DEFAULT_PREFIX` (empty = no default
  prefix, required by D4/#471). Env-gated; default behavior unchanged.
- `a46a297` test(viewer): committed small fixtures
  (`dial9-viewer/ui/parity/fixtures/segments/`: window-00..09.bin.gz,
  multi-runtime.bin.gz, manifest.json; ~120 KB total) + new vitest suite
  `src/lib/trace/segments.fixtures.test.ts` (7 tests) + the T17 upgrade in
  `src/lib/trace/segments.window.test.ts`. `parity/fixtures/generated/`
  gitignored.
- `05e2853` feat(parity): `walk-rows.mjs --fixtures` mode + fixture walker
  registry `parity/walkers/features01.fixtures.mjs` (family preflight, exit 2
  with generate/serve instructions when the seed is missing).
- `69e4d93` style(viewer): rustfmt + clippy polish on gen_fixtures (fixture
  bytes verified unchanged).
- `b21da7e` docs(viewer): fixture-walk section in `dial9-viewer/ui/README.md`
  "Parity gate tooling" (the parity docs location this tree uses; there is no
  `ui/parity/README`).
- (this commit) HANDOFF.

## FIXTURE FAMILIES PRODUCED

Generator: `cargo run --release -p dial9-viewer --features dev-server --bin
gen-fixtures` (~3 s). Wire format: real `dial9-trace-format` encoder, same
event/field names the runtime emits (PollStart/PollEnd/WorkerPark/
WorkerUnpark/QueueSample/TaskSpawn/ClockSync/SegmentMetadata with
`runtime.<name>` entries); keys conform to the #225 layout with filename
epoch matching the date path (unlike the demo key, features/01 finding 3).

1. `dial9-fixtures` bucket (browse-layout scenarios, all on 2026-04-09 so
   the parity clock's "Last 24hr" reaches them): svc-alt/host-z (multi-
   service row, multi-runtime content), svc-fix/boots (3 boot ids),
   svc-fix/gap (10-minute coverage hole), svc-fix/seam (upload-lag mtime
   overlap, size-identical segments by construction - asserted in the
   generator), svc-fix/window (the 10-segment set, also committed).
2. `dial9-fixtures-dates` bucket: date partitions at the bucket ROOT (#471).
3. `dial9-fixtures-large` bucket: 8 x 28.0 MB stored-gzip segments
   (224.1 MB listed; >200 MiB selection for H4; >=100 MB-raw set for T39).
   Skippable via `--skip-large`.
4. Committed small set: 10 window segments sharing one monotonic clock with
   two planted boundary polls (adjacent seg0->seg1; seg3->seg5 with a fully
   silent interior - the T17-audit N-segment chain) + a multi-runtime (#596)
   segment (`runtime.journal` = workers 64..67 over an unnamed main 0..3) +
   `manifest.json` recording every planted fact.

## EVIDENCE - DoD leg 1: flipped rows (fixture walk GREEN)

Invocation (documented in ui/README.md):

```
cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures
DIAL9_SEED_DIR=dial9-viewer/ui/parity/fixtures/generated/s3 \
  DIAL9_DEFAULT_PREFIX= PORT=3022 \
  cargo run -p dial9-viewer --features dev-server --bin dev-server
node parity/walk-rows.mjs --inventory ../../docs/ui-inventory/features/01-index-html.md \
  --url http://localhost:3022/index.html --fixtures
```

Result: `Summary: 8 rows - 8 VERIFIED / GREEN: zero FAILED`. Rows flipped
from recorded NOT-TRIGGERABLE to walker-VERIFIED (evidence strings from the
run):

- C7 select bucket -> region detect: "chip filled bucket, region check
  fired, status 'Using dial9-fixtures', prefixes re-discovered".
- D4 date-layer auto-empty (#471): "prefix auto-emptied with placeholder
  '(no prefix - dates at root)'".
- F5 boot-count annotation: "boots row annotated '3 boots'; single-boot
  rows unannotated".
- F7 seam tiling: "uniform density across the seam (ref rgb(220,105,142),
  max channel deviation 0)" - canvas-pixel census; an untiled overlap would
  double the second half.
- F8 coverage-gap hatching: "no-data band across [19:05,19:15] (299/299
  band columns), start tick + crisp end boundary (band terminus x=456),
  density control distinct" - canvas-pixel census.
- F9 boot-change dividers: "2 dashed cyan dividers at the boot transitions
  (x=153,305); none on single-boot rows" - positions match the page's own
  timeToX mapping of the planted transition times.
- F20 truncation banner: "truncated:true; banner 'Some traces were
  omitted...'" - the range-truncation path (99-day window > 2000 hourly
  prefixes); needs no seeded data. The recorded dev-data verdict concerned
  the per-prefix cap; noted in the walker.
- H4 selection size cap (amended #570/#600): "8 segments, 224.1 MB
  selected: View disabled; Flamegraph exempt; warning suppressed
  (aggregation mode)".

NOT flipped, with reason: H5 (the red warning text) renders only when
aggregation is disabled, and `aggregation_enabled = agg.is_some() ||
allow_byo_creds` - any BYO-creds dev-server (which C7 requires) reports
aggregation enabled. C11 (cross-region 421) remains out of reach on a
single-region fake S3. Both stay honest NOT-TRIGGERABLE.

Regression: the STANDARD walk against an unseeded dev-server (port 3021)
re-ran green after the walk-rows changes: 75 rows - 42 VERIFIED /
33 NOT-TRIGGERABLE, zero FAILED. `--fixtures` against the unseeded server
exits 2 at preflight with generation instructions.

## EVIDENCE - DoD leg 2: T17 upgrade

`segments.window.test.ts`'s real-parse anchor no longer serves two copies of
the demo trace: it drives the ten distinct fixture segments through
`createSegmentWindow` with real gz bytes + the real frozen-core parser,
keeping the existing accounting/budget assertions (recorded rawByteLength ==
actual decompressed size per segment, resident <= RESIDENT_RAW_BUDGET_BYTES,
gzip cache totals) and adding end-to-end `boundaryPolls()` assertions: both
planted polls come out stitched with task identity, zero truncated. The
mock-based 10-segment budget scenario is untouched. New
`segments.fixtures.test.ts` additionally pins edge extraction, the planted
silence, the honest truncation when the silent interior is not resident, and
#596 runtime grouping - all over real wire bytes.

## EVIDENCE - DoD leg 3: T39 large input

`dial9-fixtures-large`: 8 segments x 28.0 MB raw (stored gzip, listed size
~= raw size), deterministic, scripted, NOT committed
(`parity/fixtures/generated/` is gitignored; size policy documented in
ui/README.md). Regeneration: the same `gen-fixtures` invocation; runtime
~3 s release.

## GATES

- `npx tsc --noEmit`: clean.
- FULL `npm run test`: 47 files passed, 1 skipped (pre-existing); 846 tests
  passed, 1 expected-fail, 11 skipped (all pre-existing).
- `npm run build`: green; `cargo build -p dial9-viewer`: green (rust-embed
  picks up the tree).
- `cargo fmt --check`: clean.
- `cargo clippy --all-targets --features __nonlinux_all_features` (macOS
  form): zero warnings in touched crates; PRE-EXISTING warnings reported in
  untouched crates (dial9-perf-self-profile: unused rate_limited macro/
  import, dead time_since_epoch; dial9-tokio-telemetry: unused
  poll_start_ts_monotonic import/fn, unused `shared` variable, dead
  TaskDumpEvent struct). Not fixed per the scope rule.
  `cargo clippy -p dial9-viewer --all-targets --features dev-server`
  (covers the gated bins): zero warnings.
- `cargo nextest run -p dial9-viewer` (scoped sanity per ticket budget):
  144/144 passed. NOTE: full-workspace nextest + the 20s stress run were
  NOT run (Rust changes are dev-only bins: gen-fixtures is new, and the
  dev_server seeding extension was exercised live end-to-end instead -
  fixture mtimes verified through /api/browse, object GET, and the 8-row
  walk).

## OPEN QUESTIONS / NOTES FOR T39 AND SIBLINGS

- The fixture walk's H4 evidence reflects the amended #570 aggregation-mode
  contract (View disabled, Flamegraph exempt, warning suppressed). If T39
  wants the warning-text branch (H5) walked, it needs a no-BYO-creds,
  no-agg dev-server profile; that conflicts with C7 in the same run.
- The fixture-walk output is written to `parity/out/fixture-walk.{json,md}`
  (gitignored, like all parity reports); re-run the three commands above to
  reproduce.
- features/04's NOT-TRIGGERABLE list (multi-host/-service, off-CPU class-0
  polls, cap-plateau refinement) now has raw material in these buckets;
  wiring features04 walkers to them is T41's implementation-time work, not
  done here (per ownership). Off-CPU class-0 specifically may need a
  long-poll (>10 ms) fixture variant - a one-line cadence tweak in
  gen_fixtures if T41 asks.
