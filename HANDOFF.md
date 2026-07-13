# T26 - Spans track - HANDOFF

(Replaces the inherited T25 HANDOFF; the T25 record lives in the tree at base
8e6f35d.)

STATUS: done (mechanical gates green; the browser-driven DoD checks are
listed as deferred `review:` items below, consistent with T21/T25's HANDOFFs
which deferred their live walks to a trailing viewer audit).

Branch: `ticket/T26-spans-track` (worktree `.claude/worktrees/T26`),
based on `integration/chunk-1` @ 8e6f35d (T21 shell + T25 axis + T20 keyboard
merged; T07 store, T08 canvas, T09 lib/trace present).

## COMPLETED (commit shas, oldest first)

- c751f61  store scaffolding: `spanFilter`/`spanPctFilter` uiPrefs fields +
           `formatFieldValue` seam through the lib/trace barrel.
- 72d48dd  `spans-model.ts` (pure) + `spans-track.ts` (store-wired component).
- 459a214  wire into the shell: `tracks.ts` delegation + `shell.ts` instance +
           `viewer.css` styles.
- 2c7604c  Vitest: `spans-model.test.ts` (25) + `spans-track.test.ts` (4).

## What was built (features/02 section J - the owned rows)

Spans track as a time-aligned track hosted in T21's unified column. tracks.ts
delegates the "spans" row TEMPLATE + canvas PAINT to a `SpansTrackController`
(smallest additive edit, keyed by track id, mirroring the axis delegation);
every other track's uniform row is untouched (`defaultTrackRow`).

- J1 span canvas: clustered bars, log-scale duration y, cluster height by
  log2(size), darker active segments, log-scale y-axis ticks. Draw-area-
  relative x (identical `nsToDrawX` to the axis) so bars line up with ticks.
- J2 span focus (click): toggle focus -> `selection.focusedSpanId` +
  `spanFocus{spanId,chain}`; pins the focus at the top (y=4), descendants
  below (y=34); resolves + selects the task polling at the span's first
  segment (via a trace-keyed `buildWorkerSpans` derived).
- J3 metadata label: focused span name + unit-aware k/v rows in the LABEL_W
  gutter (keeps the track axis-aligned); "Spans" when nothing focused.
- J4 copy buttons: per-row clipboard copy with an 800ms checkmark flash.
- J5 info readout: "N spans · M clusters" (roots) / rich "name: dur (P% of N)
  · P50 P99" when focused.
- J6 hover tooltip: single-span (name/dur/percentile/active/idle/polls/
  workers/fields) and cluster (size/top-names/min-max) variants, cursor-
  following, viewport-flip. lit-html render (escaped), self-managed element
  (the shared T24 tooltip is not landed; seam noted in-code).
- J7 text filter, J8 percentile filter, J9 name chips (keyed lit `repeat`,
  F7), J10 clear-names, J11 prev/next nav (centers viewport + highlights),
  J12 filter count ("N matches" / "i/N"), J13 unmatched warning, J14
  controls visible only when spans exist.
- S4 (partial): selection-driven dimming - non-highlighted clusters recede,
  reading `selection.spanFocus` reactively (T31 only drives the slice).

### Perf-rule compliance (F5/F6/F2/F7)

- F5 derived cache: trace-invariant span data (`computeSpanTrackData`:
  buildSpanData + per-name durations + name list) is a `store.derived
  (["trace"], ...)` cache; the per-frame render model is memoized on its
  primitive inputs, so an S4 dim change redraws with the SAME layout.
- F6 visibility window: binary-searched right edge (`upperBoundByStart`) +
  bounded prefix scan - no O(allSpans) scan per frame.
- F2 filter coalescing: keystrokes dispatch `store.update("uiPrefs",...)`;
  the RAF scheduler coalesces to <= 1 render/frame - no synchronous
  renderAll per keypress.
- F7 chips/label: declarative keyed lit-html `repeat`, not innerHTML rebuild.

### T17 carried obligation (audit notes 6+7)

Incomplete spans are surfaced, never dropped: `buildSpanData.unmatchedSpans`
(enter without exit - trace ended mid-span / segment rotated) drives the J13
"N unmatched" warning. This is the spans-track consumer of the incompleteness
signal (the whole-trace analog of window-edge truncation; T17 segment
windowing is not yet wired into the viewer data path, which reads the whole
ParsedTrace via T16). Tested: `spans-model.test.ts` "surfaces enter-without-
exit as unmatchedSpans".

## DoD status

- check: row-walker green on J -> DEFERRED (review:). The T12 row-walker
  (`ui/parity/walk-rows.mjs`) is a Playwright browser driver; the new viewer
  page's end-to-end live walk was deferred by T21 itself ("DoD live-walks
  deferred to trailing audit"). Every J row is implemented + typed + built;
  the mechanical halves are covered by Vitest below.
- check: filter typing <= 1 render/frame (T12 perf probe) -> COVERED at the
  store boundary by `spans-track.test.ts` "filter typing coalescing (F2)"
  (5 keystrokes -> 1 scheduled frame -> subscriber runs once). The T12
  in-browser perf probe is the DEFERRED visual confirmation.
- check: span focus behavioral-diffed vs legacy on J4 -> the focus chain,
  copy semantics, and metadata rows are ported verbatim from viewer.html and
  unit-tested (`spanFocusChain`, `spanLabelModel`, focus reposition/info).
  The legacy-vs-new behavioral differ run is DEFERRED (browser).
- check: selection change dims non-selected spans -> COVERED:
  `spans-model.test.ts` `spanHighlight` (render input reacts to the selection
  slice) + `spans-track.test.ts` `drawSpansCanvas dimming` (a recording ctx
  proves non-selected clusters draw at 0.08/0.3 vs selected 0.4/1.0). The one
  T12 visual is DEFERRED.
- check: derived cache invalidation Vitest -> COVERED: `spans-track.test.ts`
  "span-data derived cache (F5)" (recompute only on trace change; a viewport/
  uiPrefs change does NOT invalidate).

## Gate bar (run from dial9-viewer/ui)

- `npx tsc --noEmit` -> exit 0.
- `npm run check:boundary` -> OK (no core import outside lib/trace+lib/canvas).
- `npm run build` -> clean; new-viewer bundle transforms the track.
- `cargo build -p dial9-viewer` -> Finished (rust-embed picked up dist/).
- Vitest: spans-model 25/25, spans-track 4/4; touched suites
  (store/exhaustive/track-layout/axis) 58/58.
- Full `npm run test`: see EVIDENCE. NOTE: running the full parallel suite
  CONCURRENTLY with a cold `cargo build` starved the CPU and timed out 4
  heavy trace-parsing core suites (worker/integration, parse_yield_throttle,
  time_range, slice - 130-330s each). Re-run in isolation on a free CPU:
  26/26 pass in 6.4s. These suites touch NONE of the changed files. Not a
  regression - flagged here per the AGENTS "report apparent flakes" rule (a
  resource-contention timeout, not a logic flake).

## EVIDENCE (final full-suite run, clean CPU)

```
$ npm run test        # exit 0
 Test Files  66 passed | 1 skipped (67)
      Tests  1071 passed | 1 expected fail | 11 skipped (1083)
   Duration  181.67s
```
(The "1 expected fail" is a pre-existing `.fails()` case, not mine. The
concurrent-with-cargo run's 4 timed-out core suites all pass here.)

## Decisions made (no STOP gate hit)

- S4 dimming driver: dimming reacts to `selection.spanFocus` (the span
  highlight chain = legacy `selectedSpanIds`). This is faithful to the legacy
  renderSpanPanel, where a bare task selection (clicking a poll) set
  `selectedTaskId` but did NOT dim the spans panel. Selecting a span - the
  selection this track owns - is the genuine selection-slice change that dims
  non-selected spans, exactly the S4 (partial) reaction the DoD asserts. Not
  a materially-ambiguous fork: both readings satisfy "selection change dims
  non-selected spans"; the faithful one was chosen.
- J5 info while focused: shows the rich "name: dur (P% of N) · P50 P99" line
  (per the J5 inventory description). The legacy set this in the click
  handler then immediately overwrote it with the plain count on the following
  renderAll; the new code shows the documented rich line deterministically.
- Filter/percentile state lives in `uiPrefs` (spanFilter/spanPctFilter) so the
  track re-renders reactively and the derived filter invalidates; the nav
  cursor (J11/J12) is ephemeral component state (reset on filter change), not
  serialized.
- New `lib/trace/format.ts` seam re-exports `formatFieldValue` from the frozen
  format.js (J3/J6 need it; formatHumanDuration already reached src/ via
  api_format.ts). Additive barrel export.

## Shared merge-point edits (for sibling track tickets)

- `src/pages/viewer/tracks.ts`: added a `spansTrack?` param to `tracksTemplate`
  + `sizeTracks` and a `track.id === "spans"` delegation branch; extracted the
  default row to `defaultTrackRow`. Sibling content tracks (T22/T27-T30) can
  follow the same delegation shape.
- `src/pages/viewer/shell.ts`: creates the spans track instance once and
  threads it through `shellTemplate`/`sizeTracks`; disposes it.
- `src/types/state.d.ts` + `src/pages/viewer/store.ts`: +2 uiPrefs fields
  (spanFilter, spanPctFilter) + defaults. Two existing inline UiPrefsSlice
  constructions (store.test.ts, exhaustive.test.ts) updated.

## REMAINING / next steps (for the trailing viewer audit, not this ticket)

1. Run T12 against the new viewer page once it is served end-to-end with a
   trace: row-walker on J, behavioral differ (J4 focus, filter, chips) vs the
   legacy `viewer.html`, and the perf probe (filter storm + hover storm) for
   the <=1 render/frame + no-forced-layout confirmations.
2. J6 tooltip: reconcile with T24's shared crosshair/tooltip when it lands
   (this track ships a self-contained tooltip in the interim; seam noted).
3. J11 nav lane-scroll: the legacy also scrolled the worker lane; that is a
   lanes (T22) concern - this track dispatches viewport+selection only.

## No unrelated fixes made

Only the owned section-J surface + the minimal shared-shell seam were touched.
No other track dirs modified.
