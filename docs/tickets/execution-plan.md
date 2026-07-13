# Ticket execution plan and live state

How the 48 migration tickets get implemented, and the CURRENT execution
state. This file is the single source of truth for resuming work: any fresh
session (human or agent) reads this + the chunk files and continues without
re-deriving anything. Update the state table on every transition.

## Execution model (decided 2026-07-08)

Per ticket: **1 implementer -> mechanical gates -> expert reviewers -> maintainer merge.**

- **Implementer**: one cold-start agent per ticket, in a git WORKTREE on
  branch `ticket/T<nn>-<slug>`. Prompt = `templates/implementer.md` with the
  ticket section pasted in. The ticket is the full spec by design (verified
  by the cold-start pass; see `verifier-findings.md`).
- **Mechanical gates** (run before any review): `tsc --noEmit`, `vitest run`,
  `npm run build`, repo lint rules (e.g. T09's no-core-imports check), T12
  parity tooling once it exists, plus AGENTS.md Rust gates when Rust files
  are touched.
- **Expert reviewers**: adversarial per-PR passes, lens depends on ticket
  class (see template `templates/reviewer-*.md`): contract (owned rows +
  DoD honesty), constraints (frozen core / store discipline / perf rules),
  correctness. Trivial tickets (docs/process) skip agent review.
- **Orchestrator**: the main planning session (or its successor) routes
  escalations; genuine decisions go to the maintainer. Implementers STOP on
  ambiguity/decision gates - never invent (standing rule).
- **Maintainer**: merge authority; nothing merges without them.

## Token-resilience rules (all work must survive a dead session)

1. **Commit early, commit often.** Implementers commit to their worktree
   branch at every coherent step with descriptive messages. A dead agent
   leaves a resumable branch, never lost context.
2. **HANDOFF.md on stop.** Any implementer that stops before DoD (limit,
   blocker, escalation) writes `HANDOFF.md` at the worktree root: done /
   remaining / blockers / next command to run. The resume prompt
   (`templates/resume.md`) consumes it.
3. **Reviews are files.** Reviewer output lands in the PR body or
   `docs/tickets/reviews/T<nn>.md` - never only in a conversation.
4. **This file is the ledger of progress.** Every state transition
   (started / gates-passed / review-done / merged / blocked) updates the
   table below in the same commit or immediately after.
5. **Cheap before expensive.** Mechanical gates run before any reviewer
   agent; small tickets before big ones inside a wave; never START a large
   ticket when close to a known limit - park at a wave boundary instead.
6. **Templates are files** (`docs/tickets/templates/`), so spawning costs a
   file-read, not a re-derivation.

## Waves (dependency order; parallelism cap 3, maintainer review is the bottleneck)

- Wave 0 (serial PILOT, validates the loop): T01 -> T02.
- Wave 1: T03, T04, T05 (parallel, all post-T02).
- Wave 2: T06 -> T07/T08 (T06 first, then parallel), T10 (post-T03).
- Wave 3: T09, T12, T11.
- Wave 4: T13 (end-to-end pipeline proof; gate before widening), T38.
- Wave 5: T14 -> T15; T16 -> T17; T18; T19; T20.
- Wave 6+ (chunk 2): T21 -> T22/T23/T24/T25 -> remaining tracks (T26-T30)
  -> T31/T32/T33 -> T34/T35/T36 -> T37. Chunk 3 (T40-T48) slots by its own
  deps; T39 last.

## Resume protocol (for a fresh session after token exhaustion)

1. Read this file's state table.
2. `git worktree list` + `git branch --list 'ticket/*'` for in-flight work.
3. For any in-progress branch: read its `HANDOFF.md`, spawn
   `templates/resume.md` with it.
4. For review-pending tickets: check `docs/tickets/reviews/`.
5. Continue the wave table; update states.

## State table

Status values: `pending` | `in-progress(<who/branch>)` | `gates-passed` |
`review(<lens list>)` | `changes-requested` | `merged` | `blocked(<reason>)`.

| Ticket | Status | Branch | Notes |
|---|---|---|---|
| T01 | implemented; docs-lineage ticket | ticket/T01-inventory-refresh @ c1fccc5 | push+PR = maintainer; 3 HANDOFF questions still await maintainer ruling |
| T02-T20, T38, T43, T48 + audit fixes | MERGED into integration/chunk-1 @ d4d67c8 | (ticket/* branches retained) | **CHUNK 1 CLOSED 2026-07-12**: T20 merge gate-verified by orchestrator inline (tsc clean, 1001 passed + 1 sanctioned xfail, build clean, 17 static-copy items). Chunk 2 (T21 viewer shell) now unlocked, plus T41/T46. |
| T43 | MERGED into integration (clean) | ticket/T43-url-contract-api @ 346a284 | all 4 parity legs green, 12/12 codec pin, skill-link e2e PASS, full gate run clean; #303 disposition: URL contract covers set-time-range + open-flamegraph; "highlight" reserved until chunk 2 (T21-T23); issue's server-push/event-API asks are OUT of migration scope - recommend #303 stays OPEN with a partial-progress comment, not closed |
| T15, T19-T37, T39, T41, T42, T44-T48 | pending/in-flight per executor | - | chunk-2 renderer obligations from T17 audit attached; T20+T48 in flight |

**CONTROL MODEL (decided by maintainer 2026-07-10, supersedes the freeze):**
Executor drives; orchestrator audits.
- The long-running executor agent owns implementation dispatch per the wave
  plan (T12 -> T13/T14/T40 -> onward), keeping all implementer hard rules:
  no push/GitHub, HANDOFF + commit discipline, stop-on-gate escalation to
  the maintainer for genuine decisions.
- The planning session (or successor) owns AUDIT: adversarial reviews of the
  integrated tree, risk-targeted (T07 store/scheduler and T17 windowing
  first - largest blast radius; T12 parity tooling on landing - it is the
  future verification substrate; others as budget allows). Findings file to
  `docs/tickets/reviews/` ; blocking findings halt the affected wave until
  resolved.
- Maintainer: merge authority, gate rulings, pushes/PRs.
- Review-before-build is replaced by build-with-trailing-audit; the
  integration tree's full gate run (tsc + all tests) remains the merge bar
  for every ticket branch entering integration/chunk-1.

**DISPATCH MODEL (maintainer 2026-07-12, chunk 2 onward):** orchestrator
dispatches ONE bounded short-lived agent per ticket/phase from disk state,
audits in the same session, merges under the gate bar. No long-lived
executor (the old one stall-looped on bloated context and is retired). Each
bounded agent gets: its ticket spec + shared decisions + carried
obligations + an explicit STOP fence (finish, HANDOFF, stop - no next
ticket). Chunk-2 sequencing: fix(ui-switch) T38-href FIRST, then T21 (viewer
shell), then the wave. The Fable-credit stream cuts are resolved (switched
to Opus).

**Chunk-2 track progress (2026-07-13):** MERGED into integration/chunk-1
(tip 3e4b03f): T21, T25, T22 (lanes), T28 (cpu), T26 (spans), T24 (crosshair/
tooltip/transient slice). Each merged under the gate bar; conflicts were all
mechanical (tracks.ts import-union, viewer.css block-union, state.d.ts distinct
regions auto-merged, HANDOFF took-theirs). tsc clean after every merge; full
4-merge Vitest gate running.
DOC-TOPOLOGY FIX: `docs/tickets/reviews/` (audit docs incl. T17-audit.md) lived
only on docs/ui-migration, NOT on integration/chunk-1 where agents branch from -
so T24/T26/T28 all flagged the T17-audit path missing (handled it from types
instead). Copied reviews/ onto integration/chunk-1 (commit 3e4b03f) so
T27/T29/T30 can read it. LESSON: anything an implementer is told to read must be
on integration/chunk-1, not just the planning branch.
T30 (task detail) MERGED into integration/chunk-1 2026-07-13 (tip bdaa422, clean
merge - base==tip, additive only; tsc clean; full-suite validation folded into
the next batch gate). T30 note: caught a ticket-vs-code discrepancy - the waker-
hover field lives in the `selection` slice (selection.hoveredWakerTaskId), NOT
`transient` as the prose said; dispatched to the field the merged lanes actually
consume (state.d.ts:154, lanes/index.ts). Good judgment, documented.
MERGED SINCE: T23 (6c7a902, lane interaction; only HANDOFF conflict) and T27
(6d022e0, events track; N-way union merge of tracks.ts+shell.ts+viewer.css
delegations - T27's base predated T30 so both the task-detail and events track
delegations had to be unioned by hand in tracksTemplate/sizeTracks/shellTemplate/
mount/dispose). tsc clean after each. Build clean on the 9-merge tree.
MERGE PATTERN for the shell hot-files: each track/chrome ticket adds one
delegate (import + template branch + sizeTracks branch + create/thread/dispose in
shell.ts). When a ticket's base predates a sibling's merge, git conflicts on the
shared param lists + delegate chains - resolution is always UNION (keep every
sibling's delegate), never pick-a-side. viewer.css conflicts are block-union.
HANDOFF.md always take-theirs.
T29 (queue track + S6 fix) MERGED 2026-07-13 (44aa32b). N-way union again (queue
delegate joined task-detail+events in the param lists; bodies auto-merged).
CROSS-TICKET TSC BREAK caught + fixed at integration: T29 made selection.
spawnedTasksRange REQUIRED, but T23's selection-overlay.test.ts fixture (merged
earlier) built a SelectionSlice literal without it -> tsc error 2322. Each branch
was green ALONE; only the combination breaks. Fixed by adding spawnedTasksRange:
null to the fixture, amended into the merge commit. LESSON: the integration tsc
gate is load-bearing - per-worktree green does NOT imply integration green when
one ticket adds a required slice field and another has an independent full-slice
fixture. Always tsc the integrated tree after every merge (already doing).
T34 (load chrome) MERGED 2026-07-13 (aeb724b). shell.ts + viewer.css auto-merged
(T34's sourceLabel getter + New File button coexisted with T29's queue wiring);
only ledger (append-union M-rows + B-rows) + HANDOFF conflicted. tsc clean.
FLAG for downstream merges: T34 changed ShellDeps.sourceLabel from `string` to
`() => string` and added optional onNewFile - tsc will catch any T33/T31/T35
collision at their merge (they share ShellDeps).
T33 (toolbar + issues rail) MERGED 2026-07-13 (e9b05ff, amended with getter fix).
This was the biggest semantic merge yet: T33 refactored the shell (moved file-
info out of viewModel into toolbar.ts, changed shellTemplate signature to add
state/toolbar/rail, made ShellDeps extend ToolbarDeps). Resolved by taking T33's
slim ShellViewModel + unioning T29's queueTrack into the reconstructed shellTemplate
call, and unioning T34's loadChrome/onNewFile + T33's onOpenAnalysis/onSetRange/
onClearRange + notify helper in main.ts. THEN the predicted cross-ticket break
fired: T34 made sourceLabel `() => string` but T33's toolbar.fileInfoTemplate/
analysisTemplate take `string` -> tsc 2345. Fixed by resolving the getter at the
call sites (deps.sourceLabel() per render). Second cross-ticket tsc break this
session; integration tsc gate is doing its job.
T31 (inspector) MERGED 2026-07-13 (45640e6). Heaviest merge - T31 replaces the
inspector placeholder with an empty mount-host aside + mountInspector, while T33
had refactored the shell; auto-merge correctly took T31's empty-aside
inspectorTemplate (lit-html leaves imperative children alone, toastRegion pattern)
and unioned keyBindings (T33) + inspectorRegion (T31) in MountedShell + the return
object, and mountLoadChrome + mountInspector imports in main.ts (boot body
auto-merged both). tsc clean. Additive selection.pollDetail wired from the lane
click. T31 deferred R3-R9 (region blocking panel) to T32 per the merged-code seam.
GIT-COMMIT-DENY FINDING: T31's agent hit a `Bash(git commit *)` deny and left ALL
work UNCOMMITTED in its worktree (gates were green there). Orchestrator salvage-
committed it (30851a7) then merged. Most earlier agents committed fine via
`git -C <wt> commit`; T31's likely used `cd <wt> && git` (denied). MITIGATION:
added the `git -C` (not `cd && git`) note to T32's prompt; VERIFY every agent's
worktree `git status` on completion and salvage-commit if uncommitted.
T35 (minimap+status) MERGED (c29141a) + T36 (track collapse+reorder) MERGED
(d307614), 2026-07-13. 16 CHUNK-2 TICKETS NOW INTEGRATED: T21-T31, T33-T36.
T35 was another big semantic merge: T33 removed file-info from ShellViewModel
(toolbar owns), T35 independently removed selection/range fields (status-bar
component owns) - union reduced ShellViewModel to just TracksViewModel; MountedShell
gained all 4 hosts (keyBindings+inspectorRegion+minimapRegion+statusRegion). T36
added trackOrder/collapsed to the vm (track column reads them) - kept those, dropped
T35-already-removed status labels. tsc clean after each.
IN FLIGHT: T32 (in-viewer flamegraph + region; S7 count investigation; renders into
T31's Stack tab; frozen core NOT edited, K7 defers to T47).
FULL-SUITE BATCH GATE: GREEN 2026-07-13. 1446 passed + 1 xfail + 11 skipped, exit 0
(ran concurrent with T32, no starvation this time). The 9 tsc-only merges
(T30/T23/T27/T29/T34/T33/T31/T35/T36) are now full-suite validated - no runtime
cross-ticket breaks. Chunk-2 (T21-T31, T33-T36) is HEALTHY. Build clean (167KB).
T40/T42 FINDING: both were already DONE + MERGED into integration/chunk-1 in the
earlier (pre-compaction) session - features/04 inventory + gen_fixtures.rs + parity
fixtures all present in the tree, branch tips are ancestors of integration. Their
stale worktrees remain on disk (harmless). Do NOT re-dispatch them.
T32 (flamegraph+region) MERGED 2026-07-13 (6585f8c). main.ts+viewer.css auto-merged;
only ledger+HANDOFF conflicted. tsc+build clean (full-suite validation folded into
next batch gate). S7 concluded (units mismatch, not a bug - reconciled on-screen +
F20 frame->timeline link added). K7 deferred to T47; F17/F18 (task-scoped fgs)
deferred (need T30/T22 dispatch). ALL 16 CHUNK-2 FEATURE TICKETS (T21-T36) NOW
INTEGRATED. Only T37 (polish) + T39 (legacy removal) remain in chunk 2.
T37 BLOCKED on T41: its DoD needs axe/census across all FOUR pages incl. tokio-stats
(T41). Dispatch T37 after T41 merges (removed its premature worktree).
T46 (end-user docs) MERGED clean (docs-only). Most of its scope was already landed by
dependency tickets (AGENTS.md testing section already Vitest-rewritten, ui/README
already current); net change = end-user "Using the viewer" section in the canonical
README (symlinked -> all 3 surfaces). NOTE: AGENTS.md line ~80 has pre-existing
em-dashes (dependency-authored, f754bbe) - flagged, out of T46 scope; clean up later.
T45 (segment metadata #68) MERGED (9c8a2b8) + T41 (tokio_stats migration) MERGED
(2c1f84f), 2026-07-13. Both only ledger+HANDOFF conflicts. T45: confirmed writer
keys literally "service"/"host", restored structured-metadata display T33's port
dropped (C1a). T41: ALL FOUR PAGES now migrated (browser/viewer/flamegraph/tokio-
stats); caught+fixed a `<base href>` replaceState bug; H4/D4/G3/E2 defects PRESERVED
(census-delta rule) - ledgered for maintainer; happy-dom added-then-removed (RCE
advisory), package.json byte-identical. tsc clean after each.
FULL-SUITE BATCH GATE: GREEN (1524 passed + 1 xfail + 11 skipped, exit 0, clean
no-agent run). T32/T41/T45 validated; tree at 2c1f84f healthy. Build clean.
T37 (UX-findings closure sweep - LAST chunk-2 ticket) DISPATCHED off 2c1f84f (deps
T21-T36 + T41 all met). Builds the finding->ticket map, closes every 04 finding
(landed/deferred/reject-pending), small polish only.
REMAINING after T37: T39 (legacy removal, TRUE FINAL chunk-2 - needs parity gates) ->
T44 (issue housekeeping, draft-only: gh closes are maintainer) + T47 (core-reshape
ADR), both after T39.
REMAINING: T37 (after T41) -> T39 (legacy removal, LAST, needs parity) -> T44 (issue
housekeeping) + T47 (core-reshape ADR), both after T39. T43+T48+T40+T42 DONE.
DEFERRED BATCH GATE: full Vitest for T30/T23/T27 folded forward - run ONE clean
full-suite gate after T29/T33/T34 land (never concurrent with a running agent's
own suite; T27 proved concurrent Vitest starves timing-sensitive suites -> 13
spurious timeouts).
QUEUED: T31 (inspector, needs T24+T30+T27+T29 contracts - dispatch after T29 so
M7/M8 queue-list contract is in-tree) -> T35 (minimap+status) -> T36 (track mgmt)
-> T37 (UX polish) -> chunk 3 (T40/T41/T44/T46) -> T39 (legacy removal) last.
KNOWN STRAGGLER: tests/core/all_skills_snippets.test.ts (+ heavy worker_threads/
gunzip parse suites) time out under full-parallel load, pass in isolation
(flagged in vite.config). NOT a regression - re-run in isolation to confirm when
a batch gate reports it failing.

**Audit log:**
- T22 worker lanes track (CORE viz): MERGED into integration/chunk-1 2026-07-13
  (tip 1ec89e3). Gate GREEN: tsc clean, build clean (lanes bundled), full
  Vitest 1051 passed + 1 xfail + 11 skipped (T22's own worktree suite had 1026
  + 9 new lane tests; INTEG total higher with T25/T21 suites). Merge added only
  import-union in tracks.ts + doc appends, no logic change. Merge conflicts were
  all trivial: tracks.ts import
  block (kept BOTH renderTimeAxis from T25 + isTrackClaimed from T22; bodies
  auto-merged), ledger append-union, HANDOFF took-theirs. Introduced the
  track-renderer registry (track-renderers.ts / isTrackClaimed) so a mounted
  track owns its own canvas draw+size and the shell stops painting the
  placeholder. TRAILING OBLIGATION (per T22 HANDOFF): live-browser DoD items
  PENDING - G-row row-walker (needs a features02.mjs walker that does not exist
  yet), J2/J4 behavioral differ, x8 pan-storm perf capture. Stroke-batch Vitest
  bounds stand as the code-level proxy. G20 retired, G17/G18/G19 amended (ledger
  annotated). Deliberate seams: T22 ships click/hover as pure resolvers/data
  only (no raw listener - T23 owns gestures); G21-G24 runtime-group headers
  unbuilt (T36/shell).
- T25 timeline axis: MERGED 2026-07-12. Gate green (1042 tests, tsc clean
  after npm install in INTEG - T21's lit-html dep needed installing there).
  Live alignment check passed (timeline drawW == lanes drawW). Filled a T21
  gap: added uiPrefs.timeMode/tz store fields (legacy rel/utc defaults; T33
  drives them). NOTE for future merges: when a merge brings a new npm dep,
  run `npm install` in the INTEG worktree before tsc/build gates.
- T21 viewer shell: MERGED into integration 2026-07-12 (opens chunk 2). Gate
  bar green (tsc, 1016 tests + 1 xfail, build). Orchestrator finished the
  last 3 commits after the implementer stall-looped on connection drops (2
  test fixes: toasts-fake Proxy identity bug + ui-switch registry extension;
  no shell impl authored by orchestrator). TRAILING OBLIGATION: DoD live
  walks (row-walker on A/T/U rows, axe on shell, tab-order dump) were
  DEFERRED - run them as a trailing audit before relying on the shell's a11y
  claims. Placeholder track slots by design (content = T22-T30).
- T07 store/scheduler: FINDINGS (non-blocking), 5 defects filed
  (`reviews/T07-audit.md`) -> ALL FIXED via fix(store), merged into
  integration/chunk-1 BEFORE any subscriber code exists (sequencing
  obligation met). Core mechanics + coverage claim verified sound.
- T17 segment windowing: BLOCKING (`reviews/T17-audit.md`) -> FIXED +
  AUDIT-VERIFIED 2026-07-10: `fix/T17-segment-stitching-budget` (4 commits
  off 3004ca2) resolves findings 1-4 + the coverage item with failing-first
  regression tests; audit re-ran the suites (72/72) and inspected the
  probe-mirror assertions. MERGED into integration/chunk-1 (tip 851b02e;
  gate: tsc clean, 824 passed + 1 sanctioned xfail) - block LIFTED
  2026-07-10. Chunk-2 obligations stand: renderers must consume
  `truncatedAt:"both"` and surface the `oversized` state (audit notes 6+7);
  carried in the executor's dispatch notes for the owning tickets.
- T38 dual-UI switch: FINDINGS (`reviews/T38-audit.md`, commit 254cd69):
  (1) MEDIUM boot-time href goes stale vs live query rewrites -> trace loss
  on switch; MUST fix in ui-switch.js before/with T13's first page
  registration; (2)+(3) LOW. RESOLVED 2026-07-12: all three were already
  fixed during T13/T14 page registration (liveControlHref click-time
  resolution reading location.search + pinWouldBounce + live-href loop
  guard), regression-tested in tests/ui_switch.test.ts (48 pass, findings
  1+2 named in the suite). The audit had reviewed a pre-T13 tip; the
  redundant fix dispatch was discarded. Fundamentals also clean (repeated
  trace=, hash drop, precedence, one-line injection, 404-inert,
  default=legacy).
- T48 flamegraph search fix (SANCTIONED frozen-core touch): audit-verified
  inline 2026-07-11 - diff minimal + semantically correct (topmost-match
  inclusive union = lit-area share, no nested double-count), guard dropped
  per ruling, blast radius confirmed (feeds updateSearchStats only), parity
  probe reported%==lit% across queries. MERGED into integration 2026-07-11 (HANDOFF-only conflict). Non-blocking note stands: "N frames" counts tree nodes (unchanged
  by design, flagged in issue-closures.md).
- Queue: T12 parity tooling on landing; T02 embed as budget allows.

## Standing risks

- Org/session token limits have fired 3x during planning; execution is
  paced by waves and the park-at-boundary rule.
- The docs corpus (everything under docs/ + ui mocks) must be committed
  BEFORE execution starts - uncommitted planning is the largest
  wasted-work exposure (step 0 below).

## Step 0 (before any ticket)

Commit the full planning corpus (docs/adr/0004-*, docs/ui-inventory/**,
docs/tickets/**) to a branch and merge/push per maintainer preference. The
prepared commit message exists from the earlier /caveman-commit pass;
extend its scope line to include tickets + execution plan.
