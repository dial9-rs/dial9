# T22 - Worker lanes track - HANDOFF

(Supersedes the T21 HANDOFF inherited through the branch chain; T21's record
lives in the tree at base e0fa98e.)

STATUS: **partial** - code-complete on the owned G rows + all four gates
green; the live-browser DoD items (row-walker, behavioral differ, pan-storm
perf) are PENDING because the environment cannot sustain a Playwright
dev-server. Recorded below, not faked.

Branch: `ticket/T22-lanes-track` off `integration/chunk-1` (e0fa98e).

## COMPLETED (with shas)

- **d3acc6c** feat: lanes render + data + resolvers.
  - `src/components/canvas/lanes/render.ts` - `renderLanes(ctx, input, layout)`
    pure draw of G1-G12 + the G6-G11 highlights, ported from viewer.html
    `renderLane` (:2957). Pixel-bounded fills via the frozen core
    `pixelDownsampleSpans` + `makeBarCoalescer`; the local-queue step line
    (G12) and every dashed marker (G5 open-ended, G2 block_in_place outline)
    go through the **T08 stroke batcher** - one path per style, dash state
    hoisted (03 F1). N worker rows stack into the one lanes canvas; band
    geometry scales from the 60px legacy reference.
  - `src/components/canvas/lanes/data.ts` - `deriveLaneData(trace)`
    (buildWorkerSpans + attachCpuSamples + buildSpanData + span-id index),
    derived ONCE per trace and store-cached over the `trace` slice (03 F5).
  - `src/components/canvas/lanes/click.ts` - `resolveLaneClick` pure resolver
    (G13 select-task/toggle, G14 outermost-span focus, G15 Poll-Detail
    signal) over the T09 query helpers (03 F6).
  - `src/components/canvas/lanes/hover.ts` - `assembleLaneHover` pure G16
    readout via binary-search/indexed helpers; carries `hasClickableStack`
    (the K8 affordance signal T24 turns into a pointer cursor).
  - `src/pages/viewer/track-renderers.ts` - the renderer registry
    (claim/skip-placeholder seam). Shell seam edits: `tracks.ts` skips claimed
    tracks; `main.ts` mounts `mountLanes` AFTER the shell.
- **cf4b6d8** test + legend: `src/components/canvas/lanes/lanes.test.ts`
  (9 tests, green) + the G19/04 F3 legend ribbon + CSS.
- **a807040** docs: ledger G20 retired / G17-G19 amended; features/02 G rows
  annotated (contract-amendment rule).

## Owned-row disposition (features/02 G)

- G1-G5, G9, G10, G12 rendered (grid, bg states, CPU tint, poll bars,
  open-ended markers, CPU ticks, sched triangles, queue step line).
- G6, G7, G8, G11 selection/hover highlights (task, span chain, waker, wakes)
  driven by the `selection` slice - redraw lanes only (03 F2).
- G13/G14/G15 click **semantics** as a pure resolver (T23 owns the gesture
  layer that drives it - see SEAMS).
- G16 hover **data** assembler (T24 owns the tooltip component - see SEAMS).
- G17/G18 amended by the unified-column scroll model (ledger).
- G19 amended -> lanes legend ribbon covering all marks + q:NN (04 F3, ledger).
- **G20 retired** (dead `selectedEvent` indicator; ledger).

## EVIDENCE (gates, run in dial9-viewer/ui)

- `npx tsc --noEmit` -> clean (no output).
- `node scripts/check-core-imports.mjs` -> OK (no core imports outside
  lib/trace + lib/canvas).
- `npx vitest run` -> **1026 passed | 1 expected-fail | 11 skipped** (65 files),
  incl. the 9 new T22 tests. No regressions.
- `npm run build` -> dist built; `new-viewer` bundle 32.59 kB (lanes
  included); 17 static-copy items.
- `CARGO_TARGET_DIR=.../target cargo build -p dial9-viewer` -> Finished
  (rust-embed picks up the new dist).

### Vitest coverage of the DoD "downsample/coalesce usage" item

- 1,000,000 contiguous polls, drawW 200 -> `fillRect` <= drawW+5
  (pixel-bounded, not O(polls)).
- 500,000 queue samples, one lane -> `stroke()` **<= 2** (queue + separator
  styles), `setLineDash` 0 - the F1 fix (legacy stroked per-sample).
- 10,000 open-ended markers -> `setLineDash` <= 2 (dash hoisted), one dashed
  stroke - not one per marker.
- `resolveLaneClick`: select-task, same-task toggle-off, span ancestor walk,
  Poll-Detail-only-with-samples.
- `assembleLaneHover`: polling (task/cpu/sched/queue/active-task +
  hasClickableStack), parked (kernel sched delay).

## PENDING (live-browser DoD - no dev-server in this environment)

Not faked; run once a Playwright dev-server is available:
1. **Row-walker green on G rows** (`dial9-viewer/ui/parity/`). NOTE: there is
   no `parity/walkers/features02.mjs` yet - the features/02 walker itself does
   not exist; running the G-row walk needs that walker written PLUS a live
   `PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server`.
   G20 is ledger-retired (listed, not gated).
2. **Behavioral differ vs legacy on J2/J4** (identical poll/task selection).
   The pure `resolveLaneClick` is unit-covered; the end-to-end click needs
   T23's gesture wiring to drive it against the running page.
3. **Pan-storm perf (x8 repeated demo):** record renders <=1/frame (N2),
   lanes script time, and stroke() self-time share vs legacy's 76%. Needs the
   dev-server + `parity/perf-probe.mjs`. The stroke-batch Vitest bounds are
   the code-level proxy (stroke() is now O(styles), not O(samples)).

## SEAMS (coordinate with dependent tickets)

- **T23 (interaction):** owns the pointer gesture layer (3px drag-intent,
  x->ns, y->worker). It should call `resolveLaneClick(...)`, dispatch the
  returned `selection` patch, and route `openStackFor` to T31. T22
  deliberately does NOT wire a raw `click`/pan listener (it would fight T23's
  pan drag) - hence the live behavioral-differ is PENDING, not done here.
- **T24 (hover/tooltip):** consumes `assembleLaneHover(...)` for the G16
  tooltip and sets the K8 pointer cursor from `hasClickableStack`.
- **T27 (custom events):** dispatches `selection.pinnedEvent`; the in-lane
  pinned-poll highlight resolves the worker by value-matching the poll in the
  lane (`selection.pinnedEvent.poll` carries no workerId). If T27 adds a
  workerId, the match can tighten.
- **Runtime groups (G21-G24, #596):** NOT in T22's owned set (G1-G19) and
  NOT-TRIGGERABLE on the demo (zero named runtimes). `deriveWorkerIds` already
  orders lanes by `computeRuntimeGroups`, but group HEADERS / collapse /
  per-group hit-testing are unbuilt and belong with track management (T36) /
  the shell. Flagged so they are not silently dropped.

## INTEGRATION NOTES (shell edits - minimal, additive)

- `tracks.ts sizeTracks` skips claimed tracks (no placeholder, no resize that
  would clear the renderer's draw). Non-lanes tracks unchanged.
- `main.ts` mounts `mountLanes(shell.trackColumn, store)` after the shell so
  its subscription runs after the shell's chrome render each frame; the shell
  never clobbers the lanes canvas, and a uiPrefs-only change (which the lanes
  do not subscribe to) leaves the canvas untouched (claim keeps the
  placeholder off).
- Lanes track height stays the shell's 130px; N worker rows divide it evenly.
  Very large worker counts make rows thin (legacy scrolled per-lane) - a
  track-height/scroll refinement for T36, noted not fixed (scope fence).

## SCOPE-FENCE / UNRELATED

- No `.rs`, no trace-format, no frozen-core edits. JS/TS + docs only, so the
  Rust nextest/stress suites are not required (AGENTS.md).
- No unrelated bugs found or fixed.

## Open questions
None blocking. The G13-G16 seam split (T22 semantics/data vs T23/T24 wiring)
follows the ticket's stated seams; if the maintainer wants T22 to also land a
throwaway click listener for a self-contained live differ, that is a small
add - but it would be replaced by T23 and risks fighting T23's pan.
