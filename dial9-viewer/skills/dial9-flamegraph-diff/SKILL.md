---
name: dial9-flamegraph-diff
description: Compare two dial9 on-CPU profiles and report what got hotter or colder — as a ranked text/JSON "top movers" list and/or a red/blue differential flamegraph. Use to compare two time periods (before/after a deploy, peak vs trough, incident vs baseline), two hosts/nodes, or one node vs the fleet. Builds on dial9-flamegraph's folded stacks.
---

# dial9 differential flamegraph

Answers "**what changed** between these two captures." Works on any two sets of
segments folded the same way — the two sides can be:

- **Two time periods** — before vs after a deploy, peak vs trough, during-incident
  vs baseline-hour, today vs last-week. (The single most useful comparison.)
- **Two hosts / nodes** — is one box behaving differently from another?
- **One node vs the fleet** — is this host an outlier?

Output is either a **ranked mover list** (text or JSON — what an agent reasons
over) or a **red/blue differential flamegraph SVG** (what a human eyeballs).
Red = hotter in B (candidate), blue = hotter in A (baseline).

## Prerequisites

Same as `dial9-flamegraph` (dial9 toolkit + segments). `diff_folded.js` itself is
**dependency-free** — it only needs Node — so the ranked report works with no
renderer installed. The SVG path additionally needs `inferno` (`inferno-flamegraph`).

## Workflow

### 1. Fold each side identically
Use `fold.js` / `merge_folded.js` from the `dial9-flamegraph` skill. **Both sides
MUST be folded at the same granularity** (both full, or both `--max-depth N`) or
stacks won't line up and everything reads as "changed."

```bash
# Example: TWO TIME PERIODS for the same service.
#   A = baseline window, B = the window you're investigating.
node fold.js /tmp/traces/2026-06-18_1000/  > A.folded     # baseline hour
node fold.js /tmp/traces/2026-06-18_1600/  > B.folded     # suspect hour
```
Other framings are just different segment sets on each side:
- **host vs host:** `A` = all of host-1's segments, `B` = all of host-2's.
- **node vs fleet:** `A` = the whole fleet's segments, `B` = the one node.

(For big sets, pre-fold each segment with `fold.js --json` once and assemble each
side with `merge_folded.js` — see the dial9-flamegraph skill.)

### 2a. Ranked movers (for agents / quick read)
```bash
node diff_folded.js A.folded B.folded            # top 25 movers, normalized
node diff_folded.js --json --top 50 A.folded B.folded
```
Reports each stack's share-of-profile delta in percentage points (+ = hotter in
B). Profiles are normalized to equal weight first, so you compare **shape**, not
raw sample volume. By default it drops stacks whose leaf is an unsymbolized `0x…`
address (they never match across captures and would dominate the ranking); it
prints how many it dropped, and `--raw` keeps them.

### 2b. Differential flamegraph SVG (for humans)
```bash
node diff_folded.js --inferno A.folded B.folded > diff.folded
inferno-flamegraph --title "DIFF: B vs A (normalized)" \
  --subtitle "red = hotter in B, blue = hotter in A" diff.folded > diff.svg
```
`--inferno` emits inferno's differential-folded format (`stack baseline candidate`,
with A scaled to B's total). Add `--negate` to `inferno-flamegraph` to flip hues.

### 3. Deliver
Write `diff.svg` to a durable path. **Always state which side is A (baseline) and
which is B, and what red/blue mean** — misreading the direction is the #1 diff
mistake.

## Reading it
- **Red tower** = B spends more on-CPU time there → new/grown cost (regression, or
  higher load on that path in the candidate window/node).
- **Blue tower** = A spent more → shrunk/removed cost.
- **Mostly flat/grey** = same shape. For two hosts in one fleet this is the
  expected, healthy result — evidence they're interchangeable.
- One-color-everywhere usually means the two sides weren't normalized or were
  folded at different depths.

## Gotchas
- **Order matters:** `diff_folded.js A B` ⇒ A is baseline, B is candidate. Be deliberate.
- **Cross-capture time predicates are invalid.** Timestamps come from different
  monotonic clocks per trace, so you can't filter a union by absolute time across
  segments — diff works because it aggregates by callchain, not time. To diff two
  *time periods*, fold each period's segments separately (as above), don't try to
  time-slice one merged fold.
- On-CPU only (see dial9-flamegraph) — a diff can't reveal changes in blocking
  time on captures with schedule profiling off.

## Related
- `dial9-flamegraph` — produce the folded inputs and single-profile flamegraphs.
- `dial9-s3-analysis` — discover/download the segments for each side.
