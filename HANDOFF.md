# T19 HANDOFF - Viewer URL view-state + copy-link

(Replaces the T40 HANDOFF inherited through the branch chain; T40's record
lives at commit bd1503b.)

## STATUS

COMPLETE - no STOP-gate hit. All DoD items done, all gates green, no open
blockers. The hash-vs-query reconciliation followed the recorded default
(legacy params untouched and mirrored; versioned hash carries the new
unified state; hash wins per field on read) - it proved workable, no fork.

Branch: `ticket/T19-url-view-state`, based on integrated tip 03f4626.

## COMPLETED (commits)

| sha | what |
| --- | --- |
| a87564c | codec (`ui/src/lib/url/view-state.ts`) + legacy-param fixture (`legacy-params.fixture.ts`) + property/round-trip tests |
| 9f09db0 | store->URL sync binding (`ui/src/lib/url/sync.ts`) + copy-link (`ui/src/lib/url/copy-link.ts`) + barrel + tests |
| d4015e0 | flamegraph page integration (`ui/src/pages/flamegraph/view-state.ts`; exact-mode/api-mode/dom/new-html wiring) + integration tests |
| 2a212e4 | parity: journey J9 (recorded legacy zoom-link restore) + `fg.breadcrumb` readout |
| 33b5204 | schema doc `docs/ui-inventory/05-url-view-state.md` + ADR-0004 doc-index row + ledger entries |

## WHAT SHIPPED

- Versioned URL hash codec: `#v=1&fg.w=<tab path>&fg.o=...&tm=...&tz=...`
  (form-encoded payload; empty state = no hash at all). Tolerant reader:
  unknown v=1 keys preserved on rewrite; invalid values dropped; foreign
  or future-version hashes never restored from nor rewritten.
- Store-slice -> URL sync: one debounced (150ms trailing) replaceState
  per change burst; no-op writes skipped; host/timer/scheduler injectable
  for Node tests. Restore-on-load bypasses the store (the frozen widget's
  zoomToPath does not fire onZoomChange), so opening a shared link
  produces ZERO URL writes - legacy parity, gated by J9's url.query.
- Flamegraph page (first consumer): user zoom -> page-local store slice
  (`fgView`) -> one write carrying BOTH the legacy `worker-zoom` /
  `offworker-zoom` query params (exact F147-F153 semantics: address-bar
  copies still open on the legacy page) AND the versioned hash. Read
  precedence: hash wins per field, legacy fills gaps; F151
  timeRangeMatched gate kept.
- Copy-link button (`.d9-copy-link`) in the migrated page header, both
  modes: flushes the pending debounced write, copies location.href,
  flashes "Copied". API mode mounts it without a flush (its URL is
  already current via F180 pushState; canvas zoom deliberately not
  URL-synced there - legacy parity, codec stays out).
- Time mode: `tm` (`rel`|`abs`) + `tz` (`utc`|`local`) DEFINED in the v1
  vocabulary but unwritten - the flamegraph page has no clock-mode
  control; the chunk-2 viewer wires them (design addition, rationale
  documented in the schema doc).

## DoD EVIDENCE

1. check: Vitest codec round-trip property test - PASS.
   `ui/src/lib/url/view-state.test.ts`: 500 seeded-random states
   (mulberry32; adversarial frame names incl. `& = # % + ? / \ ' " space
   unicode`; tab excluded = the legacy wire format's own limitation),
   decode(encode(s)) === s, encode-stability, legacy-mirror round-trip.
   No new deps (constraint S1) - hand-rolled PRNG, no fast-check.

2. check: restore-on-load integration test on each page migrated at
   landing time (flamegraph is the only one) - PASS, done BOTH sanctioned
   ways, documented:
   - vitest-level against the page module:
     `ui/src/pages/flamegraph/view-state.test.ts` (recording fake widget:
     legacy-only / hash-only / both-precedence / F151 gate / zero writes
     on restore / write shape / Esc cleanup / F153 preservation / flush).
   - parity behavioral differ (in-browser, real page + real widget):
     journey J9 below.

3. check: recorded legacy-param fixture URLs resolve identically - PASS.
   - Fixture (the ticket's FIRST work item), recorded from reading
     flamegraph.html + flamegraph.js + features/03 M/P into
     `ui/src/lib/url/legacy-params.fixture.ts`:
     - exact mode, load scope (read-only): `trace` (repeatable), `start`,
       `end`, `svc`, `host`, `segs`, `from`, `to`;
     - exact mode, VIEW STATE (replaceState, F147-F153): `worker-zoom`,
       `offworker-zoom` (tab-joined frame paths; set when non-empty,
       deleted when empty; restore gated on timeRangeMatched F151; Esc
       clears F152; all other params preserved F153);
     - api mode (pushState on Apply/facet change, F180): `api`,
       `data_dir`, `bucket`, `prefix`, `service`, `host` (repeatable),
       `start_ns`, `end_ns`, `source`, `thread_class`, `spawn_location`,
       `max_files` - NO view-state params by design (canvas zoom not
       URL-synced in api mode; kept that way).
   - Behavioral differ (dev-server :3081 over built dist, per the DoD
     recipe), J9 fixture URL recorded from the LEGACY page itself
     (click-zoom on demo-trace, walkable prefix of the emitted path):
     `/flamegraph.html?trace=demo-trace.bin&worker-zoom=0xffff9b8cbf1c%090xffff9b862030%09Thread%3A%3Anew%3A%3Athread_start+unix.rs%3A130`
     Output: `== J9 (restore a shared zoom link) ... checkpoint restored:
     identical (6 fields) ... ZERO DIFF` (legacy /flamegraph.html vs
     migrated /new/flamegraph.html).
   - J5 re-run legacy vs new with the copy-link mounted: `ZERO DIFF`
     (rendered + searched checkpoints, 6 fields each).
   - End-to-end playwright verification on the migrated page (throwaway
     script, removed): zoom writes legacy params + v=1 hash (debounced);
     hash-only URL restores (breadcrumb populated, URL byte-stable, no
     write-back); copy-link copies href (clipboard === href, "Copied"
     flash); Esc clears both params and the hash, keeps `trace`; the
     LEGACY page loads a hash URL fine and is simply not zoomed (raw
     ui-switch policy honored - no state porting, none attempted).

4. review: schema documented for chunk-2 extension -
   `docs/ui-inventory/05-url-view-state.md`: key registry
   (live/defined/reserved), version + tolerant-reader rules, precedence,
   write mechanics, ownership boundary table, extension checklist.
   Registered in ADR-0004's doc-index table.

## GATES

- `npx tsc --noEmit`: clean.
- `npm run test` (FULL suite, includes the check:boundary pretest):
  50 files passed + 1 skipped (pre-existing), 888 passed / 1 expected
  fail / 11 skipped (pre-existing baseline). 0 unexpected failures.
- `npm run build`: clean (dist/new/flamegraph.html + bundles + 17
  static-copied items).
- `cargo build -p dial9-viewer`: clean (rust-embed embed check).
- Dev-server killed; port 3081 verified closed.
- JS/TS-only change (no .rs touched, no trace-format change): per
  AGENTS.md, cargo nextest/stress/clippy not required.

## RECONCILIATION / SCOPE FENCE

- `url_state.js` and the browser page: NOT modified (T14's surface).
  Reconciliation is documentation + codec design: url_state.js owns the
  browser page's QUERY params (`bucket`, `aws_region`, `prefix`, `tab`,
  `tz`, `last`, `from`, `to`, `q`); the codec owns the HASH on migrated
  pages. The `tz` name exists in both vocabularies deliberately (same
  values, different carrier + page - no interference). Boundary table in
  the schema doc.
- Frozen core untouched. The parity readout-schema/journey extension is a
  parity-TOOL change, explicitly allowed by the schema fixture's header.
- No chunk-2 chrome: only the minimal copy-link button the ticket owns.
- No push, no PRs.

## OPEN QUESTIONS / NOTES FOR MAINTAINER + CHUNK-2

None blocking. Notes:
- Ledger lines added (= PR sign-off items): `features/03 F147/F153
  amended (T19)` (debounced write + hash alongside unchanged legacy
  params) and `features/03 census +.d9-copy-link added (T19)`.
- The J9 fixture path is demo-trace-dependent: after a demo-trace
  regeneration, re-record it by click-zooming the legacy page and copying
  the emitted URL (comment in parity/journeys.mjs says the same).
- Chunk-2's status bar should replace `mountCopyLink` but keep the
  flush-then-read contract (`ViewStateBinding.flush()` before reading
  href).
- Sibling coordination: T14 (browser page) may adopt the codec for any
  NEW view state on its page; url_state.js's params stay query-based
  as-is (schema doc, boundary table).
