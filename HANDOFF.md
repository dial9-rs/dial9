# HANDOFF - T28 Process CPU usage track

(Replaces the T25 HANDOFF inherited through the branch chain; T25's record
lives in git history at the T25 merge.)

## STATUS: DoD MET

The CPU usage track (features/02 section L) is implemented as
`src/pages/viewer/cpu.ts`, wired into the T21 shell's "cpu" track slot,
tested (62 Vitest cases incl. a behavioral diff vs the verbatim legacy
logic), and verified LIVE in a real browser where the migrated readout
matches the legacy viewer TO THE DIGIT on the demo trace. All mechanical
gates are green.

## What the track renders (features/02 L)

- **L1 CPU bar chart:** avg-cores-over-time fillRect bars, bg `#111b2e`,
  y-axis `0..max(1, visible max cores, capacity)`, 25/50/75% grid, dashed
  orange capacity line (`rgba(255,207,153,.65)` + "N core capacity"), bars
  coloured by load (blue -> pink/red ramp, verbatim). Render discipline
  (03 F1): **bars go through the T08 run-length coalescer** (`makeBarCoalescer`);
  the **grid + dashed capacity line go through the T08 batched-stroke path**
  (`makeStrokeBatcher`/`drawStrokeBatches`, one `stroke()` per style). The
  two paths are never mixed.
- **L2 info readout:** `avg X cores [· avg Y%] · max Z` ported EXACTLY
  (`cpuReadoutText`/`visibleCpuStats`/`fmtCpuCores`/`fmtCpuPercent`). The
  render returns it; `tracks.ts` mirrors it into `canvas.dataset.cpuReadout`
  (a DOM-queryable stand-in for the legacy `#cpu-panel-info`).
- **L5 data source:** `buildProcessCpuUsageSeries` (frozen core, via the
  `lib/trace` seam) from `ProcessResourceUsageEvent` custom events +
  `process.available_parallelism` segment metadata, memoized on the trace
  identity (`cpuSeriesFor`, WeakMap).
- **L6 (DEAD click):** no click handler is wired - the legacy row is DEAD,
  so "no effect" is its expected behavior (re-derives VERIFIED per the
  shared verdict mapping).

### Layout seam (same as the T25 axis)
Legacy drew full-panel-width with an inline `LABEL_W` offset. The new track
canvas is `drawW` wide and sits after the shared `LABEL_W` DOM label gutter,
so the chart draws draw-area-relative (`nsToDrawX`, no LABEL_W added) and is
aligned pixel-exact with the axis/lanes by construction (A13). The y-axis
scale numbers, which the legacy drew in the LABEL_W gutter, now sit at the
draw area's left edge (no in-canvas gutter exists) - the identical
gutter->draw-area shift the axis track made. The old 92px/24px fold heights
are NOT hardcoded; the track height (74) comes from the T21 track catalogue,
and the chart margins are taken relative to it.

### Deliberate render-discipline consequence (not a feature-row change)
The legacy also stroked a 1px top edge on EACH bar (`rgba(...,0.9)` over the
`0.42` fill). The ticket's explicit render model lists only the capacity +
grid as stroked primitives and routes "the bars" through the coalescer
(fills). Per 03 F1 (eliminate per-primitive strokes), the per-bar top edge
is folded into the coalesced fill (each bar's fill top IS its edge); it is
not re-added as a separate per-interval stroke. This is a sub-row rendering
nuance (alpha of a 1px top edge), not one of the L1 inventory feature
elements (bg / y-axis / grid / load colours / capacity line are all
preserved), so no ledger row is warranted. Flagging it for the reviewer.

## L3 / L4 (hover tooltip + crosshair): the T24 seam
L3 (CPU hover tooltip) and L4 (crosshair sync + lanes-tooltip suppression)
belong to the T24 overlay/tooltip system (features/02 sections I + V; V2
names the CPU panel as a tooltip-content provider). T24 is NOT a T28 dep and
has NOT landed. Per the chunk-2 seam pattern (e.g. T27's "deferred-until-T31"),
T28 owns the CPU CONTENT and provides it as pure, tested functions:
`cpuIntervalAt` (binary-search lookup, legacy `findProcessCpuIntervalAt`) and
`cpuIntervalTooltip` (structured label/value rows, legacy
`cpuIntervalTooltipHtml`). **Deferred until T24:** wiring these into the
rendered tooltip + crosshair overlay. No overlay/crosshair code was added
here (that is T24's dir).

## T17 carried obligation (windowing) - discharged
`renderCpuTrack` consumes a `CpuWindow` descriptor (`truncatedAt:
"start"|"end"|"both"|null` + `oversized`) and SURFACES it - a translucent
edge hatch at each truncated edge (so missing data reads as "not loaded",
not "idle CPU") and a "partial window" badge when a needed segment is
`oversized` - rather than painting a truncated window as complete. Tested
directly (both-edge hatch, oversized badge, clean complete window).
`deriveCpuInputs` surfaces `oversized` from the segments slice today;
`truncatedAt` derivation from a live resident window
(`computeWindowBoundaryPolls`) is the downstream wiring seam (T34/T35 feed
windowed data into the viewer) - the viewer shell currently loads WHOLE
traces (empty segments slice), so it resolves to `null` while the renderer
consumes it regardless. NOTE: `docs/tickets/reviews/T17-audit.md` does not
exist in the tree; the contract was taken from the encoded types
(`truncatedAt` on `WindowEdgePoll`, `oversized` on `SegmentLifecycle`) and
the segments Vitest suites, which define notes 6+7's semantics.

## COMPLETED (commits, off base 8e6f35d)
- `f37bc6c` feat(T28): cpu.ts track module (render + coalescer + batched
  stroke + readout + hover content + T17 window) and wiring into the shell
  (`tracks.ts` render case + `TracksViewModel.cpu`; `shell.ts`
  `cpu: deriveCpuInputs(state)`) - the minimal additive edits, mirroring
  exactly how T25's axis was wired.
- `824d45a` test(T28): `cpu.test.ts` - 62 cases.

Files: `src/pages/viewer/cpu.ts` (new), `src/pages/viewer/cpu.test.ts`
(new), `src/pages/viewer/tracks.ts` (+import, +`cpu` VM field, +`cpu`
render case), `src/pages/viewer/shell.ts` (+import, +`cpu:
deriveCpuInputs(state)`).

## EVIDENCE (gates)
- `npx tsc --noEmit`: clean (exit 0).
- FULL Vitest (`vitest run --no-file-parallelism`, machine quiet): 65 files
  passed / 1 skipped; **1104 passed / 1 expected-fail** (pre-existing xfail,
  also noted in T25's HANDOFF) / 11 skipped; **0 unexpected failures**. The
  +33 over the prior baseline is exactly this ticket's behavioral-diff
  cases. Serialized because the heavy `worker_threads`/gunzip trace-parse
  suites (worker/*, load, slice, fetch_traces - none touched here) mutually
  starve and time out when run in parallel on a loaded box; run in isolation
  they pass 50/50, and the earlier quiet parallel `npm run test` also passed
  them (0 failed). This is an environmental parallelism/timeout constraint,
  NOT a regression - flagged so a reviewer on a clean box sees `npm run
  test` (parallel) go green.
- `cpu.test.ts` in isolation: 62 passed.
- `npm run check:boundary`: OK (no core imports outside lib/trace +
  lib/canvas).
- `npm run build` (vite): success (only the pre-existing
  os/child_process/fs/path/zlib externalization warnings from the frozen
  trace_parser.js; new-viewer bundle 27.53 kB incl. cpu.ts).
- `cargo build -p dial9-viewer`: success (rust-embed rebuilt from the new
  dist).

### DoD check 1 - row-walker green on L
No `parity/walkers/features02.mjs` exists yet (a viewer features02 registry
is T12/viewer-parity infra, out of T28 scope - exactly as T25's HANDOFF
recorded for section F). The row-walker's FUNCTION for L was satisfied by
(a) the Vitest suite exercising each row's logic and (b) a live browser walk
of the L access paths (below). L1/L2/L5 VERIFIED; L6 DEAD -> VERIFIED
(expected no-op); L3/L4 = T24 seam (content provided + tested; rendered
overlay deferred-until-T24, listed above).

### DoD check 2 - avg/max readouts behavioral-diffed EXACT vs legacy
Two independent proofs:
1. **Mechanical (CI-runnable):** `cpu.test.ts` runs the LEGACY readout logic
   copied verbatim from viewer.html (`fmtCpuCores`/`fmtCpuPercent`/
   `visibleCpuStats`/info-string) against the ported logic over 60 intervals
   x 8 viewports x 4 capacities + the empty series - identical to the digit
   in every case.
2. **Live on real data:** the legacy `viewer.html` and the migrated
   `new/viewer.html`, both on the demo trace (which DOES carry
   ProcessResourceUsageEvent data), both report
   **`avg 0.48 cores · avg 4.4% · max 1.52`** - an exact match. Live check
   also confirmed CPU track `drawW=980 == timeline drawW=980` (A13 exact in
   the browser), CPU_BG painted (`rgb(17,27,46)` = `#111b2e`), and ZERO
   console/page errors. (Playwright + `vite preview` on dist; ephemeral, not
   committed.)

## BLOCKERS: none

## Seam notes for downstream
- **T24 (overlay/tooltip):** consume `cpuIntervalAt` + `cpuIntervalTooltip`
  from `cpu.ts` for the L3 hover tooltip; the L4 crosshair/mouseNs live in
  the `transient` slice T24 owns.
- **T34/T35 (windowing into the viewer):** populate `CpuWindow.truncatedAt`
  from the resident window (`computeWindowBoundaryPolls`) and make the shell
  subscribe to `segments` so a truncated/oversized CPU window re-renders;
  the renderer already surfaces the descriptor.
- **T36 (track collapse):** `renderCpuTrack` guards `chartH <= 0` (paints bg
  + readout only), so a collapsed CPU track degrades gracefully.
