# T41 - Migrate tokio_stats.html - HANDOFF

(Supersedes the T36 HANDOFF inherited through the branch chain.)

## STATUS: DoD met (implementation complete, all runnable gates green)

Behavior-preserving migration of `tokio_stats.html` onto the new stack, same
treatment as T13/T14. The full mechanical gate bar is green; the live T12
Playwright parity layers are delivered (features04 walker registry + switch
registration) and their properties are proven offline (Vitest exact-number
behavioral tests, the XSS regression, the switch round-trip logic + a live
Playwright smoke). The live parity RUNS themselves need the seeded DDB
dev-server, which is environment-gated (as the T40 inventory itself notes for
features/04) - see REMAINING.

## COMPLETED (commits on `ticket/T41-migrate-tokio-stats`)

- `83c1013` feat: migrate tokio_stats.html to the new stack
  - `src/pages/tokio-stats/`: `format.ts` (converters), `stats.ts`
    (computeStats + diff math + refine-termination), `exemplar.ts`
    (deep-link builder), `url.ts` (URL contract), `render.ts` (lit-html
    declarative templates - N17 XSS guard), `keys.ts` (T20 `?` help),
    `dom.ts`, `main.ts` (wiring + refine loop via T18's `fetchTokioStats`).
  - `new/tokio_stats.html` Vite entry (new-UI path); legacy page stays
    servable. Registered `tokio_stats.html` in `ui-switch.js` NEW_UI_ENTRIES
    and `vite.config.ts` input; extended the pinned `ui_switch.test.ts`
    registry test.
- `e69c37f` test: behavioral parity, XSS regression, URL + switch round-trip
  (`stats/render/format/exemplar/url.test.ts` + the tokio_stats switch
  round-trip case in `ui_switch.test.ts`).
- `e7ab280` test(parity): `parity/walkers/features04.mjs` (9 gated-row
  walkers) + registration in `walk-rows.mjs`; T41 ledger entries.
- `6edd8c8` fix: keep pathname explicit in `syncUrl` under `<base href>`
  (a real URL-contract break caught by the Playwright smoke).

## DoD CHECKS

- **XSS regression test (hostile strings render inert)** - DONE.
  `src/pages/tokio-stats/render.test.ts` proves the #587 sinks (spawn_loc,
  the exemplar URL, the diff-% cell) are interpolated lit-html VALUES (inert),
  never baked into static HTML, plus a source guard against the `innerHTML` /
  `unsafeHTML` class. NOTE: `service`/`host` URL params are never rendered by
  this page (they only build the fetch query), so they are inert by
  construction; spawn_loc is the actual attacker-influenceable sink the #587
  fix and this test target.
- **Switch round-trip preserves the FULL query string** - DONE.
  Logic-level in `ui_switch.test.ts` (bucket, prefix, service, REPEATABLE
  host, per-period bounds) AND a live Playwright smoke (repeatable host
  preserved across legacy->new). The page's own `syncUrl` keeps all scope +
  `p{i}_*` params (`url.test.ts`).
- **Behavioral differ (exact numbers)** - proven offline in `stats.test.ts`
  against the recorded refine fixture (total_polls 94212, notable 3379,
  per-location long/p50/p99/max, class buckets, rates) + the diff model
  (G3-G9). The LIVE old-vs-new differ needs the dev-server (REMAINING).
- **T12 row-walker on features/04** - registry DELIVERED
  (`parity/walkers/features04.mjs`), covering exactly the 9 gated rows
  (A8, A10, D3, D5, I1, J3, J5, J6, J11 - verified == the inventory's gated
  set), registered in `walk-rows.mjs`. LIVE run env-gated (REMAINING).
- **Census diff == switch delta** - analyzed switch-only and ledgered; the
  seed's default single-period state renders no tabs/exemplar-links (the
  onclick-vs-@click affordances never appear on either side) and the help
  overlay content is not census-selected. LIVE run env-gated.
- **axe clean** - the help overlay uses axe-considered semantics
  (role=dialog, aria-modal, h2/h3 heading order). LIVE axe-scan env-gated.

## GATE BAR EVIDENCE (all green)

- `npx tsc --noEmit` -> exit 0.
- `npm run test` (full Vitest, single process) -> **1492 passed, 1 expected
  fail, 11 skipped; 97 files passed, 1 skipped. 0 unexpected.** (The known
  stragglers did not time out this run.)
- `npm run build` -> clean; emits `dist/new/tokio_stats.html` +
  `new-tokio-stats` chunk (12.76 kB, gzip 4.81 kB). The `<script
  src="/ui-switch.js"> can't be bundled` line is a benign warning identical
  for all four new pages (the copied plain script is intentionally external).
- `cargo build -p dial9-viewer` -> exit 0 (rust-embed picks up the new
  `dist/new/tokio_stats.html`).
- `npm run check:boundary` -> OK (no core imports outside lib/trace).
- Playwright smoke (`vite preview` + built dist) -> GREEN: new page boots with
  zero console errors, renders the shell/one period, threshold label
  "1.00ms", switch "Switch to legacy UI"; + Add period adds a row and the
  remove (x) buttons; `?` opens / Esc closes the help overlay; legacy page
  now renders "Switch to new UI" -> `/new/tokio_stats.html`; full-query
  (repeatable host) round-trip lands on `/new/tokio_stats.html` with all
  params preserved.

## DECISIONS (recorded for maintainer sign-off)

- **H4 (dead `/api/trace` exemplar link) - PRESERVED**, not fixed. This is a
  behavior-preserving port (T13/T14 treatment: defects carried); the DoD
  requires the census/behavioral differ to show ONLY the switch delta, so
  repointing to `/api/object` would introduce a second, non-switch delta.
  Ledgered (`docs/tickets/ledger.md`, `features/04 H4 | preserved`). The
  one-line fix (`exemplarLink` -> `/api/object?bucket&key`) is a follow-up if
  the maintainer wants it live. Other preserved defects: D4 (no coverage UI),
  G3 (diff crash when P1 unloaded - the null deref is kept and pinned by a
  test), E2 (mixed/unknown classes computed but invisible).
- **`?` help (T20)** integrated as the ONLY keyboard binding (features/04 K1:
  the legacy page had no keyboard; "existing bindings unchanged" holds
  vacuously). Ledgered `features/04 K1 | amended`.
- **Test DOM env**: briefly added `happy-dom` for the XSS render test, then
  REMOVED it (it carries a critical VM-escape/RCE advisory - a bad fit for a
  published library, especially a test that feeds hostile scripts). The XSS
  test is structural instead (TemplateResult value-vs-static-HTML + source
  guard), matching the repo's deliberate "no DOM env; lit-html exercised by
  browser tooling" convention. `package.json`/`package-lock.json` are
  byte-identical to the base - zero dependency residue.

## REMAINING (environment-gated; NOT blockers)

Run the live T12 parity layers against a seeded DDB dev-server (the recipe is
the features/04 inventory's Reproduce block):

```
cd dial9-viewer/ui && npm ci && npm run build
PORT=3071 cargo run -p dial9-viewer --bin dev-server --features dev-server
# row-walker (new + legacy sides):
node parity/walk-rows.mjs --inventory ../../docs/ui-inventory/features/04-tokio-stats-html.md --url http://localhost:3071/new/tokio_stats.html
node parity/walk-rows.mjs --inventory ../../docs/ui-inventory/features/04-tokio-stats-html.md --url http://localhost:3071/tokio_stats.html
# census + behavioral differ (legacy vs new), axe: see ui/README parity section
node parity/axe-scan.mjs http://localhost:3071/new/tokio_stats.html
```

The walkers are correct-by-construction against the inventory's recorded seed
facts; time-window rows stay NOT-TRIGGERABLE on the seed (the epoch/date
catch-22 - unblocked by T42's synthetic fixtures).

## BLOCKERS / QUESTIONS

None.
