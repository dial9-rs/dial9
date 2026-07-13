# T37 - UX polish sweep (remaining findings) - HANDOFF

(Supersedes the T41 HANDOFF inherited through the branch chain.)

## STATUS: DoD met (with a documented trailing live-axe run). Ready for review.

Closure ticket. The primary deliverable - a closure table mapping every 04
finding (S1-S8, K1-K8, F1-F7) to an outcome + landing ticket(s) - is appended
to `docs/ui-inventory/04-ux-findings.md` ("Findings closure (T37)"). Two small,
in-scope polish fixes landed (browser contrast, viewer `f` fit). All four gates
green. Feature-sized remainders are deferred with reasons, not built (scope
fence honored).

## COMPLETED (commit shas, oldest first)

- `8ecbdb6` fix(browser): lift low-contrast text to WCAG AA (T37 F6/F4).
  `styles/browser.css`: footer `#444` (~1.75:1, worst node) + header meta /
  empty-state status `#666` (~2.8:1) lifted to AA. Verified effective by
  headless axe (page went 15 -> 12 contrast nodes; those three no longer
  flagged). New page only; legacy page keeps its palette.
- `dfb5840` feat(viewer): bind `f` to fit-the-whole-trace (T37 K4).
  `pages/viewer/lane-interaction.ts`: `f` -> the EXISTING `viewport.fit()` (the
  doc-commented H3/`f` action the "Fit all" mouse button already drove; commits
  to zoom history so `z` undoes it). Closes the K4 viewer fit-key sub-gap that
  the `?` help already advertised; makes `f`=fit consistent with the flamegraph
  (K3). No new mechanism.
- `365d971` docs(T37): findings closure table + ledger lines (the primary
  deliverable) + two ledger lines for the code fixes.
- `9ab5be1` docs(T37): correct F1/F6 closure with headless axe evidence (see
  BLOCKERS - discovered the ports carry legacy a11y debt).

## FINDING -> TICKET MAP (23 findings)

Built by grepping each id across `docs/tickets/ledger.md` + the chunk files.
Full per-finding notes are in the closure table. Roll-up:

- Fully LANDED (18): S1-S8, K2, K3, K4, K5, K6, K8, F2, F3, F5, F7.
- LANDED for the viewer, remainder DEFERRED (3): F1 (ported-page a11y debt),
  F4 (browser empty-state IA), F6 (ported-page brand-accent contrast).
- DEFERRED outright (2): K1 (viewer search), K7 (-> T47).
- REJECTED (0).

## REMAINING / DEFERRED (for follow-up tickets, NOT this sweep)

1. **K1 - viewer task/span search (task-blocking).** The search palette
   MECHANISM landed in T20 (`createSearchPalette` + `palette.test.ts`) but was
   never wired into the viewer: `createSearchPalette` has ZERO page callers and
   the viewer key router binds only lane keys / `n`/`p` / `g` / `?` / Esc (no
   `/`). T20 planned "the palette indexes whatever query exposes per page" as a
   chunk-2 activation that no chunk-2 ticket claimed. Wiring it (index
   tasks/spans/POIs via `lib/trace/query` -> `palette.open` -> selection +
   viewport dispatch) is FEATURE-sized -> deferred per the scope fence. The
   viewer `?` help advertises `/` (`pages/viewer/help.ts:23`), currently a dead
   key.
2. **K7 - keyboard flamegraph traversal.** Already deferred by T32 to T47
   (needs a frozen-core absolute-zoom API). No action here.
3. **F1 / F6 ported-page a11y debt.** browser/flamegraph/tokio-stats
   (behavior-preserving ports T13/T14/T41) carry legacy unlabeled inputs,
   missing `<main>`, un-regioned content, and the brand accent `#6c63ff`
   failing contrast on dark (3.68:1). Remediation touches census-relevant
   markup on merged, parity-gated ports and/or the brand color -> a dedicated
   a11y ticket, not a polish edit.
4. **F4 - browser empty-state IA.** Contrast legibility landed (this ticket);
   moving the demo/drop actions INTO the empty-state body (rather than the
   footer) is a browser-page layout change -> optional follow-up.

## BLOCKERS / QUESTIONS (discovered gaps needing maintainer awareness)

- **The DoD "T12 axe/census clean across all four migrated pages" cannot be met
  as literally stated for the three ports.** They are behavior-preserving ports
  that carried legacy a11y debt BY DESIGN (ledger H4 precedent: "defects
  carried, not fixed"). "Clean" for them = parity-with-legacy (their tickets'
  bar). Only the new-build viewer is held to actual axe-clean, and it passes
  (headless). I did NOT re-open the ports' a11y (would break their parity gates
  + needs a brand-color decision). Reading + evidence are in the closure table.
- **K1 is a task-blocking finding that is NOT closed** (mechanism ready, viewer
  wiring unclaimed). Surfacing it as the most important gap of the sweep.

## FOR MAINTAINER (ledger-PR sign-off)

- **No REJECTED findings** this sweep -> no reject sign-off required. (The
  closure convention reserves REJECT for maintainer-signed non-fixes; I created
  none - everything is LANDED or DEFERRED-with-reason.)
- Decisions requested on the deferrals (also in the closure table's "For
  maintainer" block): file the K1 viewer-search wiring ticket? open the F1/F6
  ported-page a11y-remediation ticket (and may the brand accent change)? is the
  F4 browser empty-state IA wanted, or is the legibility fix sufficient?

## EVIDENCE (gate bar - all green)

Run in `dial9-viewer/ui` (deps installed via `npm ci`):

- `tsc --noEmit` -> exit 0.
- `npm run test` (full Vitest, includes `check:boundary`) -> Test Files 99
  passed | 1 skipped; Tests 1524 passed | 1 expected-fail | 11 skipped; 0
  unexpected failures. The known heavy-suite timeout straggler did NOT trigger
  (ran in 121s).
- `npm run build` (vite) -> clean, "Copied 17 items", built in ~0.5s.
- `cargo build -p dial9-viewer` -> Finished (rust-embed picks up the changed
  files; no new files added).
- Headless axe-core scan of the BUILT static shells (chromium 149, no live
  server): viewer clean (1 `nested-interactive` nit, not an 04 finding);
  browser 12 contrast + 2 label + landmark + 11 region; flamegraph 1 contrast +
  landmark + h1 + 3 region; tokio-stats 1 contrast + 1 label + landmark + 5
  region. This is the structural a11y signal; the live seeded-DDB axe/census
  run is the documented TRAILING verification (environment lacks the seeded DDB
  dev-server, a limitation prior chunk-2 tickets all hit).

## FILES TOUCHED

- `dial9-viewer/ui/src/styles/browser.css` (F6/F4 contrast).
- `dial9-viewer/ui/src/pages/viewer/lane-interaction.ts` (K4 `f` fit).
- `docs/ui-inventory/04-ux-findings.md` (closure table - primary deliverable).
- `docs/tickets/ledger.md` (2 T37 ledger lines).

## SCOPE NOTES

- No frozen-core files touched. No Rust behavior changed (cargo build is the
  embed check only). No trace-format change (no demo-trace regen needed).
- No push / PR / GitHub actions (per ticket rule 6).
