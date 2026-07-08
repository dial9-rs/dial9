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
| T01 | pending | - | pilot, serial |
| T02 | pending | - | pilot, serial, after T01 |
| T03-T48 | pending | - | per waves above |

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
