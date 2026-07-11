# T48 HANDOFF - flamegraph search-count mismatch (#593)

(Replaces the T19 HANDOFF inherited through the branch chain; T19's record
lives at commit e6b95a6. This ticket's BLOCKED-ON-MAINTAINER analysis
record lives at commit 3d5ae23.)

## STATUS

done

Branch: `ticket/T48-search-count-mismatch` (base 74bab07). The analysis
phase ended BLOCKED-ON-MAINTAINER at 3d5ae23; the maintainer ruled OPTION 1
(confirmed against the #593 issue body: the reporter expects the percentage
to match the highlighted sections), and the fix phase landed it.

## DECISION AND WHICH BRANCH WAS TAKEN

Option 1: percentage = highlighted-area share (union of INCLUSIVE sample
counts of the topmost matching frames over the in-view total), and the
`matchedSelf > 0` hiding guard removed. CORE-side branch taken (sanctioned
by T48, which carries the core-vs-page fork): the edit is in frozen
`dial9-viewer/ui/flamegraph.js`, so BOTH page generations change together -
intended by the ruling. No page-side seam was added; core diff kept
minimal (one function's math + the stat's guard + a test export).

## COMPLETED (commits)

| sha | what |
| --- | --- |
| be77d29 | analysis: `docs/tickets/issue-closures.md` created with the #593 entry (root cause, three-way-cross-checked measurements, both semantics, decision point, draft comment) |
| 3d5ae23 | analysis HANDOFF (BLOCKED-ON-MAINTAINER record) |
| bb80759 | failing-first regression suite `dial9-viewer/ui/tests/core/flamegraph_search.test.ts` (14/14 red pre-fix: export missing + self-sample math) |
| 9cee429 | the core fix: `countSearchMatches` returns the topmost-match inclusive union (`matchedCount`) + frame count; `updateSearchStats` uses it and drops the hiding guard; `countSearchMatches` exported for the suite. 14/14 green |
| 9ee0b88 | features/03 F38 amended `[2026-07-11]` (T48, #593) + ledger line (shared format). F84 untouched - wording stands, F38 now cross-references it |
| ba36a0e | issue-closures.md: decision recorded (option 1, maintainer-confirmed), implementation + parity implications documented, draft close-comment finalized (draft only) |
| c167a0c | test tweak for `exactOptionalPropertyTypes` (tsc clean) |

## THE FIX

`flamegraph.js:106-127` (`countSearchMatches`): walks with an `underMatch`
flag; a matching node increments `frameCount` always, but adds its
INCLUSIVE `count` to `matchedCount` only when no ancestor matched (a nested
match lies inside its ancestor's highlighted extent - no double count).
`flamegraph.js:493-529` (`updateSearchStats`): percentage =
`matchedSamples / totalSamples` per zoom root (F35 zoom-relative totals
preserved), shown whenever the total is > 0 - mid-stack-only matches are no
longer hidden. Matching rules (name + fullName, case-insensitive, F34),
frame counting, and the no-match path (F39) unchanged. Blast radius:
`countSearchMatches` is consumed ONLY by `updateSearchStats` (grep: call
sites :505/:511 + the export for tests), so the stat text is the entire
behavioral surface.

## GATES (all green; JS-only change per AGENTS.md)

- `npx tsc --noEmit`: clean.
- FULL `npm run test` (incl. the check:boundary pretest): 55 files passed
  + 1 skipped, 947 passed / 1 expected fail / 11 skipped (pre-existing
  baseline), 0 unexpected failures. New suite 14/14.
- `npm run build`: clean (flamegraph bundle rebuilt).
- `cargo build -p dial9-viewer`: clean (rust-embed pickup).
- No `.rs` touched, no trace-format change: nextest/stress/clippy not
  required. Dev server (port 3121) killed and verified down.

## PARITY EVIDENCE (search stat is the ONLY behavioral delta)

Live probe (Playwright, fillRect interception: union of alpha-1.0 bar
extents / union of all bar extents), demo trace, BOTH generations
byte-identical:

| Query | Reported (was, pre-fix) | Reported (now) | Lit-area measured |
| --- | --- | --- | --- |
| poll | 146 frames, 2.7% of samples | 146 frames, 100.0% of samples | 100.0% |
| tokio | 230 frames, 5.4% of samples | 230 frames, 100.0% of samples | 100.0% |
| axum | 32 frames (pct hidden) | 32 frames, 92.5% of samples | 92.5% |
| dispatcher | 24 frames (pct hidden) | 24 frames, 68.7% of samples | 68.6% (canvas sub-0.1%-width draw filter; exact tree value 68.7, the export SVG agrees at 68.7) |
| framebuf | 8 frames (pct hidden) | 8 frames, 54.4% of samples | 54.4% |
| spawn | 2 frames (pct hidden) | 2 frames, 100.0% of samples | 100.0% |

Reported% == lit-area% on every query, and the previously-hidden mid-stack
cases now show the figure. The numbers equal the export SVG's embedded
`Matched:` search (option 1's semantic) measured in the analysis phase.

Behavioral differ (legacy /flamegraph.html vs migrated /new/flamegraph.html,
same server, demo trace):

- J5 (flamegraph work, includes the search step): `checkpoint rendered:
  identical (6 fields); checkpoint searched: identical (6 fields); ZERO
  DIFF` (exit 0).
- J9 (restore a shared zoom link, non-search journey): `checkpoint
  restored: identical (6 fields); ZERO DIFF` (exit 0).

Old-vs-new (against the pre-fix baseline): at J5's "searched" checkpoint
the `fg.searchStats` readout is the single changed field - "146 frames .
2.7% of samples" -> "146 frames . 100.0% of samples" (middle-dot
separator) - while every other captured field carries its pre-fix value
(`fg.title` "Flamegraph - demo-trace.bin", `fg.stats` "147 samples",
`fg.canvases` 2, `fg.breadcrumb` empty, `url.query` unchanged), and the
"rendered" checkpoint (no query active) is identical to pre-fix. Journeys
on other pages cannot observe the change (the edit is confined to the
flamegraph search-stat path).

## COLLATERAL

- `docs/ui-inventory/features/03-flamegraph-html.md`: F38 amended in-diff,
  marked `[2026-07-11]` (T48, #593), anchors re-derived, regression suite
  referenced. F84 wording unchanged.
- `docs/tickets/ledger.md`: features/03 F38 amended-line added (shared
  format; maintainer sign-off = approving the PR carrying this line).
- `docs/tickets/issue-closures.md`: #593 marked FIXED with the decision
  provenance and the FINAL draft close-comment (posting still requires
  maintainer sign-off per T44's convention).

## SCOPE FENCE

- Only the T48 option-1 fix and its listed collateral; no other tickets or
  plans acted on. No push, no PRs, no GitHub access. No AI-attribution
  trailers. Scratch probe scripts not committed (methodology recorded in
  issue-closures.md precisely enough to reproduce).

## OPEN QUESTIONS (non-blocking)

- "N frames" still counts matching TREE NODES (146 for `poll` on a
  147-sample trace) - unchanged deliberately; the ruling covered only the
  percentage + guard. Flagged in issue-closures.md for the fix PR review;
  counting distinct function names or topmost extents would read saner if
  the maintainer wants it.
- The demo-trace anchor tests are data-dependent: after a demo-trace
  regeneration, re-derive the six anchor values; the synthetic-tree tests
  are regeneration-proof.
