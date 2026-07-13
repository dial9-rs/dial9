# T27 - Custom events track - HANDOFF

(Supersedes the T24 HANDOFF inherited through the branch chain.)

## STATUS: DoD met (mechanical gates green; live-browser row-walk + the
T31-dependent sidebar-KV diff deferred per the T24/T26 precedent). Ready for
review.

The custom-events panel (features/02 section K) landed as a time-aligned track
in the unified column: per-pixel marker ticks (K1), the `N events · M markers`
info (K2), name-chip legend + clear (K5/K6), click-to-pin (K4), the hover guide
line (K3/I4), and task/poll resolution (K7). The T24/T31 seams are honored
strictly: T27 DISPATCHES `transient.hoverEventTs` (the orange guide) and
`selection.pinnedEvent` (the marker + sidebar); T24's overlay draws the marks,
T31's inspector will render the event KV. The legacy per-frame O(all generic
events) visibility scan (03 F5) is replaced by a BOTH-EDGES binary-searched
window (03 F6's binary-search-helpers rule) - the perf fix this ticket owns.

## COMPLETED (commits, on branch ticket/T27-events-track)

- `00f54a3` feat(viewer): custom-events track - markers, legend, click-to-pin,
  hover guide (T27)
  - NEW `pages/viewer/events-model.ts`: pure derivation + logic.
    - `computeEventTrackData` (F5 tier-1, trace-invariant): generic (non-span)
      custom events sorted by ts + sorted unique names for the legend. Filters
      out SpanEnter:/SpanExit: prefixes + SpanEnter/Exit/CloseEvent (legacy
      viewer.html:2100-2119).
    - `lowerBoundByTimestamp` / `upperBoundByTimestamp` / `filterVisibleEvents`:
      the visible window BINARY-SEARCHED at BOTH edges (point events, unlike
      spans, need no back-scan) - O(log N + window), never O(N). THE 03 F5 -> F6
      fix.
    - `buildEventRenderModel` (K1/K2): per-pixel clustering, tick width
      `max(3, min(3+log2(size)*2, 10))`, base alpha `min(0.4+size*0.15, 1)`,
      mixed-name second stripe, `>=12px` hit padding, `N events · M markers`.
      Does NOT read the selection (the draw applies the S4 fade -> a selection
      change is a cheap redraw over cached geometry).
    - `resolveTaskForEvent` / `resolvePollForEvent` / `resolveClusterTask` (K7):
      ported verbatim from legacy `taskForEvent`/`pollForEvent`.
    - `eventHighlightTask` (S4): `selectedTaskId ?? pinnedEvent.taskId` - the
      selection-slice-driven dim key, same mechanism as T26's `spanHighlight`.
    - `buildPinnedEvent` / `isSameCluster` (K4): the PinnedCustomEvent contract
      builder + the same-cluster (toggle-off) test by representative-event
      identity.
  - NEW `pages/viewer/events-track.ts`: the store-wired controller
    (mirrors `spans-track.ts`). Derived caches (event data + worker lanes, F5),
    a WeakMap-memoized per-event task resolver (legacy `_taskForEventCache`), a
    render-model memo NOT keyed on selection (S4), the K5/K6 legend template
    (keyed `repeat`, F7), the K3 hover tooltip, and the exported dispatch
    functions `dispatchEventPin` (K4) / `dispatchHoverEvent` (I4) + the pure
    `drawEventsCanvas`. Surfaces a "partial window" badge when a segment is
    `oversized` (T17 carried obligation; point markers have no continuous edge
    to hatch, so only the oversized state is surfaced).
  - Shell wiring (smallest additive edits, mirroring the spans track):
    `tracks.ts` (+`eventsTrack?` param on `tracksTemplate`/`sizeTracks` + the
    "events" delegation branches), `shell.ts` (create/thread/dispose
    `createEventsTrack`), `track-layout.ts` (events height 44 -> 70 for the
    legend strip + tick canvas), `styles/viewer.css` (`d9-events-*`).

- `91c5f4e` test(T27): events-model + events-track Vitest (K rows + DoD checks)
  - `events-model.test.ts` (20 tests): extraction, the BOUNDED-SCAN proof
    (access-counting Proxy over a 50k-event array: `< 100` element reads for a
    10-event window vs 50k linear), name filter, clustering geometry, info,
    task/poll/cluster resolution, S4 key.
  - `events-track.test.ts` (8 tests): click-to-pin dispatch (contract +
    toggle-off + prior-selection clear), hover dispatch + no-op-when-unchanged,
    dim-on-selection render input, K2 info paint + resting message, and
    derived-cache invalidation.

- `bcdfa18` feat(viewer): surface the K2 events info readout
  - Paints `N events · M markers` top-right on the events canvas (legacy
    `#ce-panel-info`) and mirrors it to `canvas.dataset.eventsInfo` for the
    row-walker / behavioral differ (CPU-track pattern). +2 draw-input tests.

## DoD status

- check: row-walker green on K -> DEFERRED (live browser + T12 row-walker, not
  runnable headless here; same deferral as T24/T25/T26). All K rows (K1-K8; K8
  is the legacy DEAD "inline tick label", preserved as not-implemented)
  are ported - see events-model.ts / events-track.ts anchors above.
- check: click-to-pin dispatches the selection slice (Vitest) -> DONE
  (`events-track.test.ts` > `dispatchEventPin (K4)`).
  - sidebar-KV behavioral-diff vs legacy -> DEFERRED-UNTIL-T31 (T31 owns the
    event-KV inspector surface; T27 dispatches `selection.pinnedEvent` with
    `detailEvent`, verified in the same test). Re-run this diff once T31 lands.
- check: bounded-scan Vitest -> DONE (`events-model.test.ts` >
  "scans O(log N + window), NOT O(N)").
- check: dim-on-selection as T26 -> DONE (`events-track.test.ts` >
  `drawEventsCanvas dimming (S4)`); mechanism mirrors T26's `spanHighlight`
  (selection-slice-driven, cheap redraw over cached geometry).

## EVIDENCE (gate bar, run in dial9-viewer/ui unless noted)

- `npx tsc --noEmit` -> exit 0.
- `npm run check:boundary` -> "OK (no core imports outside lib/trace +
  lib/canvas)".
- `npm run test` (full Vitest) -> Test Files 73 passed | 1 skipped (74);
  Tests 1199 passed | 1 expected fail | 11 skipped (1211). 0 unexpected
  failures. (The events suites: 28 passed.)
- `npm run build` -> clean, "built in 394ms", 17 static-copy items.
- `cargo build -p dial9-viewer` (repo root) -> Finished (rust-embed picks up
  the rebuilt `ui/dist`).

## REMAINING / deferred

- T12 row-walker on features/02 section K against the running new-UI viewer
  (headless browser env required).
- Sidebar-KV behavioral diff vs legacy: activates once T31 lands and renders
  `selection.pinnedEvent`.

## BLOCKERS / QUESTIONS

None. No STOP-gate hit: every K row had a legacy source of truth, the T24
overlay contract (`transient.hoverEventTs`, `selection.pinnedEvent`) was already
consumed by the merged `components/overlay/crosshair.ts`, and the spans track
(T26) supplied the exact track-wiring + dim pattern to mirror.

## SCOPE NOTES (owned K rows only; no unrelated changes)

- Did NOT draw T24's overlay marks (I4 guide / I5 marker) - only dispatched the
  slices; `crosshair.ts` already draws both.
- Did NOT build T31's sidebar - only dispatched `selection.pinnedEvent`.
- `track-layout.ts` events height 44 -> 70 is the one shared-shell geometry
  edit, needed to seat the legend strip above the (legacy 40px) tick canvas.
