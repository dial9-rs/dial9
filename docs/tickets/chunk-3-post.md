# Migration tickets - chunk 3 (fourth page, post-migration, enablement)

Continues `chunk-1-foundation.md` / `chunk-2-viewer.md` (same conventions).
This chunk closes the gaps outside the original three-page partition and the
deliberately-deferred work.

**Discovery driving this chunk:** `tokio_stats.html` is a FOURTH page (added
by #570 after the inventory snapshot; opened via the browser page's "Tokio
Stats" button, backed by `/api/tokio-stats` + `/tokio-stats` route). It is
absent from the inventories and from chunks 1-2. T40/T41 own it. T01's scope
is hereby bounded to rows in the three EXISTING inventories; the new page
gets its own inventory (T40).

Ownership summary (chunk 3):
- tokio_stats.html: T40 (inventory) + T41 (migration, all its rows)
  (T40 2026-07-10: `docs/ui-inventory/features/04-tokio-stats-html.md`
  landed - 69 rows in sections A-K, incl. the `/api/tokio-stats` backend
  contract (section J) and a findings list. "All its rows" for T41 = those
  69. One row is DEAD at HEAD: exemplar deep links target `/api/trace`,
  removed by #582 - T41 must ledger fix-vs-preserve; see finding 1 there.)
- No other feature rows; T42-T48 are enablement/process/deferred work.
- Chunk-1's ownership summary does not reference features/04 (checked
  2026-07-10), so no update there.

---

## T40 - Inventory: tokio_stats.html (features/04)

**Goal:** `docs/ui-inventory/features/04-tokio-stats-html.md` - the fourth
page's functional contract, same method and format as features/01..03.

**Context:** the established format (any features/*.md "How to read"
section); the page: `dial9-viewer/ui/tokio_stats.html`; its backend:
`dial9-viewer/src/server/tokio_stats.rs` (+ `aggregation_enabled` gate at
`dial9-viewer/src/server/config.rs:14,30`); entry point: the browser page's
`#health-btn` "Tokio Stats" button (`dial9-viewer/ui/index.html:357`,
`viewTokioStats()` at :1785) - its features/01 rows are T01's to add; if
T40 runs first, document the entry point from the source directly and
cross-link when T01 lands. Validation: walk every row against the
dev-server; populated data needs the shared-decisions aggregation recipe
(dev-server constructs AppState with agg=None - rows unreachable without
the recipe are marked NOT-TRIGGERABLE with the reason, and the recipe work
item lands here or in T18, whichever goes first).

**Owns:** the features/04 contract.

**DoD:** every control/interaction/URL-param/conditional state of the page
has a row with verified access path + `file:line` source; live-walked against
the dev-server; ownership summaries in this file + chunk-1 updated.

**Deps:** T12 (row-walker used for validation). Blocks: T41.

(T40 2026-07-10: DONE - 69 rows. Two ticket caveats superseded by the walk:
(1) the dev-server's `/api/tokio-stats` IS functional against the seeded
demo bucket (cold poll empty, `refine=true` folds 94212 polls / 5 spawn
locations) - the agg=None recipe work item was unnecessary (T18 verified the
same); (2) what remains NOT-TRIGGERABLE locally is narrower: time-windowed
scopes (the demo key's filename-epoch vs date-path mismatch 404s every
window - features/01 finding 3), off-CPU class-0 polls (demo max poll ~1ms
< the 10ms confidence bound), cap-plateau refinement (1 matched file), and
multi-host/-service data - all T42 fixture targets. No `features04` walker
registry was added (per-page walkers are T41's implementation-time
deliverable); DOM verdicts are CODE-READ, API verdicts curl-VERIFIED.)

---

## T41 - Migrate tokio_stats.html

**Goal:** Fourth page onto the new stack, behavior-preserving, same treatment
as T13/T14.

**Context:** features/04 (T40); `02-architecture.md` 2.1/2.4 (component
shapes); T18's aggregates client (this page is the primary consumer of
`/api/tokio-stats` - reuse, do not duplicate the typed client); XSS history
note: #587 fixed an XSS here - the declarative-template rule (N17) is the
structural guard; keep the escaping semantics under test.

**Owns:** features/04 ALL rows.

**Work:** `src/pages/tokio-stats/` + components; Vite entry under the new-UI
path (T38); legacy stays servable with the switch injected; URL contract
preserved; unified keyboard model (T20) integrated.

**DoD:** check: T12 full pass on features/04 (row-walker, census diff vs
legacy page (+switch, ledger), behavioral differ on the stats numbers -
exact match); check: switch round-trip preserves the page's full query
string (bucket, prefix, service, host (repeatable), start_ns, end_ns,
refine - all of it, per the T38 rule); check: axe clean; check: XSS
regression test (hostile service/host strings render inert).

**Deps:** T40, T02-T09, T12, T18, T20, T38.

(T40 2026-07-10 heads-up for T41: features/04 landed with 69 rows; four
carry behavior-vs-fix ledger decisions - H4 exemplar deep links are DEAD at
HEAD (`/api/trace` removed by #582; fix candidate: link through
`/api/object?bucket&key=<source_key>`), D4 coverage is fetched but never
displayed (no refinement progress UI), E2/H2 mixed+unknown classes and
their exemplars are wire-present but invisible, and G3 diff view throws
when P1 is unloaded. The "behavioral differ on the stats numbers - exact
match" gate maps to rows E1-E5/K3 (pure client recompute from cached
responses).)

---

## T42 - Synthetic trace fixtures (unblock NOT-TRIGGERABLE rows)

**Goal:** Generate trace fixtures the demo trace cannot provide, so parity
gates stop carrying NOT-TRIGGERABLE holes: multi-host/multi-service,
boot-id transitions, multi-runtime (#596 grouping), coverage gaps,
segment-boundary polls, and a large (>=100 MB raw) multi-segment set.

**Context:** the NOT-TRIGGERABLE lists in features/01 "Live validation
results" (F5/F7/F8/F9, H4, C7, D4...) and T12's out-of-scope note.
Generation tools and their limits: `scripts/regenerate_demo_trace.sh`
produces a single-host single-segment trace only (NOT sufficient here); the
writer API (`dial9-trace-format` encoder + the dial9 writer's
SegmentMetadata) is the real fixture producer - the trace-format skills
(`dial9-viewer/skills/dial9-trace-loading`, `dial9-trace-recipes`) document
event/field semantics for a generator; the S3 key layout convention is
documented in features/01 I2 (#225 layout). Dev-server seeding precedent:
`dial9-viewer/src/bin/dev_server.rs`. T17's segment fixtures need
multi-segment sets with controlled boundary polls. Multi-runtime (#596)
fixture: read the runtime-grouping test (`test_runtime_groups.js`) for the
event shape; if unclear, `gh issue view 596`.

**Owns:** none (test infrastructure).

**Work:** a fixtures generator (script or Rust bin) producing: multi-host
S3 layouts (heatmap gap/boot/multi-row cases), boundary-spanning polls,
multi-runtime events, a big windowing set; wire into T12 so the row-walker
can request the fixture a row needs; document in `ui/parity/README`.

**DoD:** previously NOT-TRIGGERABLE rows from features/01 validation now walk
green against the fixtures (list them in the PR); T17's simulated scenario
upgraded from repeated-demo to real fixtures; T39's "large trace" budget run
has a reproducible input.

**Deps:** T12, T17. Feeds: T39.

---

## T43 - Agent-driveability: the URL contract as an API (#303)

**Goal:** Issue #303: agents cannot drive the UI (set time range, highlight,
open flamegraph). T19's URL view-state codec IS the mechanism - finish it
into a documented, stable contract and update the agent skills to emit it.

**Context:** #303; T19 (codec, versioned schema); the skills that produce
viewer links: `dial9-viewer/skills/dial9-html-report` (viewer deep-links in
reports), `dial9-zoom-window` (says "zoom in at +6953ms" - could emit a URL);
`docs/agents/domain.md` for skill conventions; N10 (old params stay valid).

**Owns:** the deep-link contract doc + skill updates (no inventory rows; adds
rows for any new param via ledger).

**Work:** document the full URL schema in `dial9-viewer/ui/README.md` as a
stability-promised contract, with the split made explicit: QUERY params =
trace source + legacy window params (`from`/`to` etc., N10 set); HASH =
T19's versioned view-state (viewport/selection/POI/time mode). Reconcile
the dial9-html-report skill's `start=`/`end=` params (SKILL.md:94 - not in
the N10 set) to the documented contract, keeping old links resolving.
Update the two skills to emit view-state URLs; add a T12 contract test
pinning the schema version. Closing criterion for #303: read the issue via
`gh issue view 303` at implementation time; the doc + a demonstrated
agent-constructed URL covering its named asks (set time range, highlight,
open flamegraph) is the bar.

**DoD:** an agent (or curl-constructed URL) can open the viewer at an exact
window with a selection, verified by a T12 script; skills emit working links;
#303 closeable with a link to the doc.

**Deps:** T19, T13/T14 (pages honoring the codec). Viewer-window params
complete after T21-T23 land.

---

## T44 - Issue housekeeping: close what the migration fixes

**Goal:** The F5 finding's process half: shipped capabilities must leave a
trace. Map landed tickets to the open issues they resolve; close with
evidence links as each lands.

**Context:** the issue->ticket seed mapping below was assembled during the
UX phase from the LIVE issue tracker (it is NOT in 04-ux-findings.md; only
#137/#281/#282 appear there). This ticket re-derives and verifies the
mapping against `gh issue view <n>` for each: #55 (select-to-zoom - already
exists + T23), #191 (horizontal scroll - WASD/pan T23), #281 (load section
- T34), #282 (queue bar - T29), #137 (time legibility - T19/T25),
#421/#523 (large traces - T17), #480 partial (events pin - landed #552),
#593 (flamegraph search % - T48), #68 (segment metadata - T45), #248
(flamegraph text search - verify against T13), #303 (T43). Evidence =
merged PR links (+ ticket doc anchors where useful). Draft close-comments
live in `docs/tickets/issue-closures.md`; sign-off = the shared ledger
convention (maintainer approves the PR adding the draft/mapping; actual GH
posting only after that approval).

**Owns:** none (process). NOTE: closing issues = outward-facing; each close
needs maintainer OK per repo norms - this ticket produces the mapping and
drafts the close comments; a human (or explicitly-authorized agent) posts.

**DoD:** a mapping table (issue -> ticket -> evidence) committed to
`docs/tickets/`; draft close-comments prepared; issues actually closed only
with maintainer sign-off recorded.

**Deps:** rolling (per landed ticket); final sweep after T39.

---

## T45 - Surface segment metadata in the viewer (#68)

**Goal:** SegmentMetadata (service, host) is parsed but never shown (#68,
open since early). Show it in the viewer's file-info area - closes a
years-old ask with one small contract addition.

**Context:** #68; parse side: SegmentMetadata handling in
`trace_parser.js` (frozen - read-only; the data reaches ParsedTrace via
T05/T06 types). SegmentMetadata is a free-form KV map: the "service"/"host"
key names are a writer-side convention, not a documented contract - first
work item: confirm the emitted key names against the writer
(dial9 SegmentMetadata usage) and pin the read contract in types; also
confirm whether demo-trace.bin carries them (if not, the walk needs a T42
fixture). Display target: toolbar file info (features/02 C rows, T33's
component). Distinct sources, reconcile explicitly: the URL `svc`/`host`
params derive from S3 KEY parsing (`traceTitleParams`), while this ticket
surfaces TRACE-EMBEDDED metadata; when both present and disagreeing, show
metadata and tooltip the key-derived value.

**Owns:** new inventory row (added in-diff, ledger addition).

**DoD:** viewer shows service/host from trace metadata for file-drop loads;
URL-param and metadata sources reconciled (metadata wins when both present,
documented); row walked; #68 closeable via T44.

**Deps:** T33.

---

## T46 - End-user documentation refresh

**Goal:** Docs catch up with the new UI: keyboard map, staged-rollout switch,
windowed loading semantics (partial-data badges), dev workflow.

**Context:** `dial9-viewer/ui/README.md` (rewrite: dev loops from T04,
parity tooling from T12, test conventions from T10/T11 - the "CI does NOT
auto-discover" warning DIES with Vitest, AGENTS.md testing section must be
updated in the same PR); `dial9-viewer/README_TELEMETRY.md` (note: its
content mirrors the top-level README, which is a symlink to
dial9-tokio-telemetry/README.md - update BOTH or de-duplicate, decide and
document); the switch (T38 "dual-UI raw switch", `?ui=new` per the shared
decision) documented for users; help-overlay content (T20/T21) is the
in-app source of truth - docs link to it rather than duplicating.

**Owns:** none.

**DoD:** check: AGENTS.md testing rules match reality (Vitest, no hand
registration); check: ui README covers dev/test/parity/URL-contract; check:
switch documented for users; check: stale instructions removed
(registration warnings; grep the owned docs for dead references before
closing - do not assume a fixed list).

**Deps:** T04, T11, T20, T38 states final wording.

---

## T47 - Author the core-reshape ADR (deferred work, activated)

**Goal:** The ADR-0004-section-6 deferral comes due after the viewer parity
gates pass: author ADR-0004 for the frozen-core data-model reshape (columnar
storage, string interning, monomorphic records) + the batched micro-fixes.

**Context:** ADR-0004 section 6 (decision + trigger); measured
justification: `03-performance-findings.md` F4's mechanism catalog
(polymorphic events[], triple poll materialization, decoder churn, dead
fields, un-interned callchains - the committed record; the raw audit notes
were ephemeral); micro-fix batch list: one-`TextDecoder` reuse
(`decode.js:50`), `getTraceTimeRange` N-array (`trace_analysis.js:40`);
constraints: H2/H4 (Node-tested source form, wire compat), the `lib/trace`
seam (2.7) is the swap boundary; shuttle/stress test obligations (AGENTS.md)
for core touches. ADR number: the next free at authoring time (0005 if
nothing else lands first - 0004 is the migration ADR). Micro-fixes are
partition entries INSIDE the new ADR's ticket set, not separate.

**Owns:** the ADR document + its own ticket partition (implementation is NOT
this ticket).

**DoD:** check: the new ADR drafted with candidates, measured baseline,
compat plan (old-trace leniency preserved - the JS decoder rules in
AGENTS.md), test plan (existing suites as the behavioral lock), and a
ticket partition; review: partition completeness; process note:
implementation tickets are filed only after the maintainer accepts the ADR
(acceptance itself is outside this ticket's diff).

**Deps:** T39 (trigger: viewer parity gates passed). Explicitly NOT before.

---

## T48 - Flamegraph search-count mismatch (#593)

**Goal:** Open bug: reported match percentage does not match the visibly
highlighted samples. T13 ports the page behavior-preserving (bug included);
this ticket fixes it after parity is established.

**Context:** #593 (read the issue body via `gh issue view 593` first); the
count logic is VERIFIED core-located: `countSearchMatches`
(`flamegraph.js:106-118`) and `updateSearchStats` (`:485-518`) - so expect
the core-side branch. The investigation must also decide the intended
semantic: percentage = self-sample share (current F38 math) vs highlighted
frame area (what users see, F40 dimming includes descendants) - pick the
one matching the issue's expectation, with evidence.

**Owns:** amended features/03 search rows (in-diff) IF a page-side remedy
exists; else the T47 line item.

**DoD:** check: root cause + chosen semantic documented in
`docs/tickets/issue-closures.md` (T44's file) with the draft #593 comment;
check: page-side fix (if any) lands with regression test + amended rows;
else check: T47's partition gains the line item with the analysis attached.

**Deps:** T13.
