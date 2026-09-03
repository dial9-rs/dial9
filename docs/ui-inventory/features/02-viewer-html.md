# UI Feature Inventory: `viewer.html` (Trace timeline viewer, panels, sidebar & flamegraph)

> Companion to `01-index-html.md`. Purpose: capture every existing functionality of the trace viewer precisely enough that (a) each one can be validated in the running UI and (b) it can be re-implemented without losing anything. Derived from the code; source refs are `file:line` snapshots whose anchor is the function/handler name.
>
> REFRESHED 2026-07-08 against HEAD (of the drift set, only #596 group-by-runtime
> and #595/#600 multi-trace load pipelining touched this page; anchors
> re-derived from commit 84a21e5's tree - the original anchors were found to
> match commit 1f257f1's tree, and `trace_parser.js` anchors were re-derived by
> hand because #582 reindented that file). Rows added or behavior-updated in
> the refresh are marked `[2026-07-08]`; their verification statuses are in the
> "2026-07-08 refresh" section at the end.

## What this surface is

The main trace viewer. It loads a dial9 D9TF binary trace (a dropped `.bin`/`.bin.gz` file, the bundled demo, or a fetched URL), then renders per-worker-thread timelines plus stacked analysis panels (spans, custom events, CPU usage, queue depth, per-task detail) and a right-hand sidebar with event/related detail and CPU/heap/idle-time flamegraphs. It offers zoom/pan/region-selection, points-of-interest navigation, blocking-call analysis, and time-range re-parsing.

- Entry file: `dial9-viewer/ui/viewer.html` (markup + inline `<style>` + inline `<script>`)
- Loaded modules: `decode.js`, `trace_parser.js`, `trace_analysis.js`, `format.js`, `panel_layout.js`, `flamegraph.js` (+ `flamegraph.css`), `flamegraph_export.js`, `creds.js`
- Backend/network consumed: trace object URLs (fetched + gunzipped client-side, streamed or buffered), `demo-trace.bin` from origin, and (only via `creds.js`) `POST /api/credentials/check` + `GET /api/buckets`. Credential headers (`x-dial9-aws-*`) ride same-origin fetches only.

## How to read this document

| Column           | Meaning                                                                                                                |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Feature**      | One discrete capability.                                                                                               |
| **What it does** | Behavior, including edge cases and non-obvious rules.                                                                  |
| **Access path**  | Precise way to reach/trigger it in the running UI (click path / interaction / keyboard shortcut / URL param).          |
| **Source**       | `file:line` (+ function name). Line numbers are a snapshot; the function name is the stable anchor.                    |

Statuses used in notes: `OK` (works), `DEAD` (present in markup/CSS but not wired), `CONDITIONAL` (only appears under a server/runtime/data condition).

---

## A. Application shell & global rendering

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| A1. Viewer container | Flex-column wrapper for the whole viewer; hidden (`display:none`) until a trace loads, then `display:flex`, `flex:1`. | CSS/JS controlled; shown on load, hidden on reset. | `viewer.html:113-119` |
| A2. Viewer body | Horizontal flex row arranging `#main-area` and `#stack-sidebar`; `min-height:0`, `overflow:hidden`. | CSS layout. | `viewer.html:121-127` |
| A3. Body dark theme | Dark bg `#1a1a2e`, light text `#e0e0e0`, system font stack, full-viewport flex column, `overflow:hidden`, `100vh`. No user toggle. | Automatic on load. | `viewer.html:27-37` |
| A4. Global CSS reset | `* { margin:0; padding:0; box-sizing:border-box }` normalizing layout. | Automatic. | `viewer.html:22-26` |
| A5. Z-index layers | `--z-overlay`=10 (crosshair/selected-event markers), `--z-legend`=20 (span/CE legends float above markers via `position:relative`). | Automatic layering. | `viewer.html:13-21` |
| A6. Click-to-focus main-area | Clicking lanes/timeline focuses `#main-area` (`role=application`, `tabindex=0`), enabling arrow/`?`/`Esc` keyboard nav; Escape also refocuses it. | Click lanes/timeline. | `viewer.html:884`, `6290` |
| A7. Focus-visible outline | `#main-area`, `#queue-chart`, `#btn-info` get a 2px `#6c63ff` outline (offset -2px) on keyboard focus only. | Tab to element. | `viewer.html:816-819` |
| A8. Enter/Space activation | Enter on a `role=button`/checkbox triggers its click and `preventDefault`; panel labels toggle on Enter/Space. | Tab to control, press Enter/Space. | `viewer.html:6207-6212` |
| A9. Coalesced full render | `scheduleRenderAll()` debounces `renderAll()` into a single animation frame; used by pan/hover to avoid render backlog. | Internal; automatic. | `viewer.html:2818-2824` (`scheduleRenderAll`) |
| A10. Crosshair RAF throttle | `scheduleCrosshairRedraw()` coalesces crosshair redraws via `requestAnimationFrame` on a separate raf id from the full-render throttle. | Internal; automatic on mousemove/scroll. | `viewer.html:6351-6359` |
| A11. High-DPI rendering | All canvases scale internal resolution by `devicePixelRatio` (`ctx.scale(dpr,dpr)`), CSS size unchanged, for crisp Retina/4K output. | Automatic. | `viewer.html:2903-2909`, `4932-4939` |
| A12. Scrollbar-width compensation | `scrollbarW = lanesContainer.offsetWidth - clientWidth` subtracted from draw width so panels stay aligned when a scrollbar appears/disappears. | Automatic in zoom/pan/crosshair math. | `viewer.html:2748`, `4323`, `4948`, `6344` |
| A13. Time-panel layout invariant | `LABEL_W`=100px gutter + `drawW` + scrollbar, computed by `makeTimePanelLayout()`/`timePanelLayout()` so timeline, lanes, span, CE, CPU, queue, task-detail axes line up vertically; worker lanes use DOM flex, other panels use an internal `LABEL_W` offset. | Internal; used by every time-based render. | `viewer.html:1371-1394`, `2867-2889`; `panel_layout.js:44-66` |
| A14. Render profiler | `?prof=1` (or `window.D9PROF=1`) logs per-panel render timings; lane render also tracks poll count / fillRect calls. | URL `?prof=1` or console. | `viewer.html:1929-1930`, `2722-2731` |
| A15. Window-resize reflow | On resize, if a trace is loaded, re-runs `renderAll()`; if a flamegraph is active, calls `fgInstance.resize()` to refit both canvases. | Resize browser window. | `viewer.html:6642-6644`, `7204-7207`; `flamegraph.js:886-889` |
| A16. ARIA live region | Off-screen `aria-live=polite` div announces keyboard-selection start/complete/cancel and zoom confirmations via `announce()`. | Screen reader only. | `viewer.html:1005`, `5211-5213` |
| A17. HTML escaping | `esc()` escapes `& < > "` for all user-controlled text injected into HTML (frames, fields, sample keys). | Automatic. | `viewer.html:1019-1021` |
| A18. Stack-frame renderer + docs.rs links | `renderFrame()`/`formatFrame()` shortens Rust symbols (trait-impl collapse, generic stripping), appends `file:line`, and wraps in a docs.rs source link when the location matches a crate registry path. | Internal; used in tooltips, popups, sched panel. | `viewer.html:1024-1028`; `trace_parser.js:1673-1741` |

---

## B. File loading & drop zone

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| B1. Drop zone | Primary upload target shown when no trace is loaded: emoji, "Drop a .bin or .bin.gz trace file here or click to open", "Expects D9TF binary format", and a demo link. Dashed `#444` border turns purple `#6c63ff` on hover/dragover. `[migrated: T34]` On the new viewer the drop zone is a fixed full-cover modal layer (a sibling of `#app`, so the shell's declarative render never clobbers it); the emoji is dropped (matches the T21 F4 empty-state framing); dashed border gains the accent colour on hover/dragover; same text + demo link + hidden file input. It is the resting empty state and doubles as the New-File chooser (B15). | Initial page; reappears on reset/error. | `viewer.html:823-843`, `39-62`; new: `src/pages/viewer/load-chrome.ts` |
| B2. Drop-zone click -> file picker | Clicking anywhere in the drop zone (except children) opens the hidden `<input type=file accept=".bin,.gz">` dialog; picking a file calls `loadFile(files[0])`. | Click drop zone. | `viewer.html:842`, `1499`, `1541-1543` |
| B3. Window drag-and-drop | Document-level drag handlers route file drops to `loadFile()`; only file drags accepted (`dataTransfer.types` includes `Files`); a `dragCounter` prevents flicker across nested enter/leave; first file only is loaded. | Drag `.bin`/`.bin.gz` onto window. | `viewer.html:1501-1539` |
| B4. Drag feedback (no trace) | With no trace loaded, dragover adds `.dragover` to the drop zone (purple border/text/tint). | Drag file with drop zone showing. | `viewer.html:54-59`, `1505-1530` |
| B5. Drag feedback (replace) | With a trace already loaded, a fullscreen `#drag-overlay` (inset 20px, dark tint, dashed purple border, blur, `z-index:300`) shows "Drop trace file to load / Replaces the current trace". | Drag file over loaded trace. | `viewer.html:64-89`, `823-828`, `1508-1514` |
| B6. Demo trace link | "or load demo trace" fetches `demo-trace.bin` via `loadTraceFromUrl()` (`local` perf mode); re-created and re-wired each time the drop zone resets. | Click "or load demo trace". | `viewer.html:839`, `1959-1965`, `1835-1853` |
| B7. Loading state | Enters loading view (drop zone shows spinner + label + live elapsed timer + "Back" button + "or press Escape" hint); `showLoadingState`. Spinner (`will-change:transform`) stays smooth while the main thread parses. | Automatic after drop/pick/demo. | `viewer.html:91-106`, `1793-1822` |
| B8. Loading progress text | `updateLoadProgress()` shows `Decompressing...`, `Fetching...`, `Parsing: N% / X MB - Yk events`, `Analyzing N events...` depending on stream vs buffered mode. `[2026-07-08]` (#600) Multi-URL loads label `Loading N traces...` (streamed) / `Fetching N traces...` (buffered). | Visible during load. | `viewer.html:1590-1610`, `1824-1827`; labels `1873`, `1877` |
| B9. Loading elapsed timer | Wall-clock ` - X.Xs` updates every 250ms via `startLoadTimer`; frozen (`stopLoadTimer`) when the viewer shows, so it excludes the synchronous analysis phase. | Visible during load. | `viewer.html:1749-1791` |
| B10. Cancel load (Escape) | While `loadAbortController` is active, Escape calls `cancelLoad()`: aborts in-flight fetch, stops the timer, resets to drop zone; `AbortError` is swallowed silently. | Press Escape during load. | `viewer.html:1829-1833`, `6216-6219` |
| B11. Cancel load (Back button) | "Back" button in loading view calls `cancelLoad()` (same as Escape). | Click "Back". | `viewer.html:1814-1818` |
| B12. Stream vs buffered path | `[2026-07-08]` REWRITTEN by #600 (issue #595): STREAM whenever `TraceParser.canStreamDecode()`, for single OR multiple `trace=` URLs - single via `fetchTraceStream`, multiple via `fetchTracesStream` (all component fetches dispatched up front, chunks emitted back-to-back in `urls` order into ONE `parseTraceStream`, so parsing segment 0 overlaps the other downloads; byte-identical to the buffered concat - the decoder resets on each mid-stream `TRC\0` header; unconsumed sibling streams cancelled on early exit, per-component rejections pre-handled). Label "Loading...", `mode=stream`; captured gunzipped chunks reassembled for re-parse (B14). `fetchTraces`+`parseTrace` (label "Fetching...", `mode=buffered`) is now ONLY the no-`DecompressionStream` fallback. | Automatic by runtime support. | `viewer.html:1678-1712` (`streamAndShowTrace`), `1855-1880` (`loadTraceFromUrl`); `trace_parser.js:270-345` (`fetchTracesStream`); tests `test_fetch_traces.js` |
| B13. Load errors | Parse failure -> `alert("Error: ...")` + reset. URL `HTTP 401` with `Dial9Creds` present but no creds -> credentials-hint alert; other network errors -> "Error loading trace from URL: ...". `processTrace` finding no usable data -> alert + reset. | On failed load. | `viewer.html:1671-1675`, `1891-1903`, `1970-1973` |
| B14. In-memory re-parse | Set/Clear Range re-parse the retained `currentTraceBuffer` with a new time filter (no re-fetch), `mode=reparse`; preserves URL range unless replacing. | Set Range / Clear Range buttons. | `viewer.html:1552-1555`, `1907-1923` |
| B15. New File / reset | "New File" clears all state (selection, correlation, buffer/name), hides viewer, clears URL range, and returns to the drop zone via `resetTraceState`+`resetDropZone`. `[migrated: T34]` On the new viewer "New File" CONFIRMS (window.confirm) before opening the load chooser when a trace is resident (04 S3 amendment); the chooser is dismissible via Esc (esc-cascade `load` band) AND a visible close control, returning to the trace (#281 amendment); reopening works. Loading a replacement is non-destructive until it succeeds (T16 worker updates the trace slice only on success), so a cancelled/failed replace keeps the old trace. Legacy button behavior unchanged. | Toolbar "New File". | `viewer.html:875`, `1544-1575`; new: `src/pages/viewer/load-controller.ts`, `load-chrome.ts` |
| B16. Load-perf record | `loadPerf` tracks `startMs/fetchDoneMs/parseDoneMs/totalMs`, `mode` (`local/stream/buffered/reparse`), `events`, `bytes`; `totalMs` finalized via double-rAF after layout; logs `loaded in X.Xs`. | Internal; surfaced via Parse-perf popup (D9). | `viewer.html:1752-1811`, `1608-1661` |
| B17. Credential header injection | Before `fetchTraceStream`/`fetchTraces`, if `window.Dial9Creds` exists, spreads `Dial9Creds.headers()` into fetch options (same-origin only). | Automatic if creds module present. | `viewer.html:1691`, `1881`; `creds.js:240-250` |
| B18. Parse-yield paint throttle | `[2026-07-08]` NEW in #600. Both parse loops (whole-buffer `parseTraceBuffer` and streaming `parseTraceStream`) share one `makePaintThrottle()` policy: the yield-to-paint macrotask (which forces a repaint) fires at most every 200 ms wall-clock - the buffered path previously repainted every 100 KB decoded (~100+ paints on a 10 MB trace). The per-100KB `onProgress` counter still fires every time, so B8's text stays live. User-visible only as smoother multi-trace loading. | Automatic during parse. | `trace_parser.js:1130-1148` (`makePaintThrottle`); test `test_parse_yield_throttle.js` |

---

## C. Toolbar: file info & Points-of-Interest navigation

Toolbar is a two-row flex column (`#toolbar`, `#toolbar-row-data` + `#toolbar-row-view`), dark bg `#16213e`, `flex-shrink:0`; shared button/select styling (dark bg, 1px `#444`, 4px radius, hover brighten, `:disabled` opacity 0.4). Source: `viewer.html:214-288`, `846-882`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| C1. File info display | Shows filename (with a `v15` version suffix) or, in structured-metadata mode, service/host/time range; plus event count, worker count, duration across all sliceable timestamped records, truncation/filter notes, and an inline ` - loaded in X.Xs` suffix appended after render. Filename ellipsized past its max width. | Toolbar row 1, top-left. | `viewer.html:848-851`, `2172-2208` (`showViewer`), `5733-5737` (`updateInlineLoadTime`) |
| C1a. Trace-embedded segment metadata | `[2026-07-13]` NEW (T45; closes #68), extended 2026-08-03. Surfaces trace-embedded `service`/`host` as summary chips and every entry in `ParsedTrace.segmentMetadata` through the existing trace-details menu. The key-sorted table is scrollable; long keys and values stay on one line, truncate visually, and expose their complete text in a tooltip. Summary identity is RECONCILED with the S3-key-derived `svc`/`host` URL params (W11): embedded metadata wins, disagreements are tooltipped, and key-derived values remain a fallback when embedded identity is absent. | Toolbar analysis actions; activate `ⓘ`, then see “Segment metadata”. | `src/lib/trace/segment-metadata.ts` (`readSegmentMetadataEntries` plus identity reconciliation); `src/pages/viewer/toolbar.ts` (`infoMenu`, `[data-info-menu]`); `src/types/trace.d.ts` (`SegmentIdentity`) |
| C2. POI filter dropdown | 5 options: Kernel Scheduling Delays, Long Polls (>1ms), Polls with CPU Samples, Wake->Poll Delays (>100us), Uninstrumented Polls. Change recomputes the POI list via `filterPointsOfInterest` and auto-jumps to the first match. | Toolbar `#poi-filter`. | `viewer.html:853-859`, `5025`, `2399-2411`; `trace_analysis.js:875-970` |
| C3. Worst-first checkbox | Checked (default) sorts POIs by descending severity/value; unchecked sorts chronologically. Toggling re-filters and auto-jumps. | Toolbar `#sort-by-worst`. | `viewer.html:860-863`, `5026` |
| C4. Prev POI button | Jumps to previous POI; if none selected (index -1) and list non-empty, jumps to index 0; disabled when `currentPoiIndex <= 0` or list empty. | Toolbar `#btn-prev-poi` ("Prev"). | `viewer.html:864`, `5039-5041`, `2413-2428` |
| C5. Next POI button | Jumps to next POI; if none selected, jumps to index 0; disabled at end of list or empty. | Toolbar `#btn-next-poi` ("Next"). | `viewer.html:865`, `5043-5045`, `2413-2428` |
| C6. POI counter | Shows `N/Total` (or `0/Total` when none selected); "None found" when the filter matches zero. Read-only. | Toolbar `#poi-counter`. | `viewer.html:866`, `2413-2428` |
| C7. Jump-to-POI behavior | Centers the viewport on the POI: `viewDur = max(spanDur*5, 1ms)`, 30% left padding; for wake-delay POIs uses the full wake->poll window (`~3x`, 20% pad) and selects the task; scrolls the worker lane into view (`scrollTop = laneIdx*LANE_H`); highlights the current POI span red (`#ff4444`, white 2px stroke). | Prev/Next click or filter change. | `viewer.html:2430-2455`, `3181-3193` |
| C8. Initial POI setup | On load `updatePointsOfInterest()` runs with `autoJump:false` (computes list, no jump), so the overview view is preserved until the user interacts. | Automatic post-load. | `viewer.html:2211` |

---

## D. Toolbar: analysis buttons & popups

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| D1. Blocking Calls button | Opens the scheduling/blocking-calls sidebar (all sched samples, no range). `CONDITIONAL`: `display:none` unless the trace has scheduling samples (`hasSchedEvents`). Red/orange styling. | Toolbar `#btn-sched-panel` ("Blocking Calls"). | `viewer.html:868`, `5027`, `2349`, `6008-6051` |
| D2. CPU Flamegraph button | Opens the whole-trace CPU flamegraph (`showFlamegraph(minTs,maxTs)`); label shows sample count. `CONDITIONAL` on `hasCpuProfileSamples`. Purple styling. | Toolbar `#btn-cpu-flamegraph` ("Flamegraph (N)"). | `viewer.html:869`, `5029-5032`, `2369-2375`, `6989-7050` |
| D3. Heap Flamegraph button | Opens the heap-allocation flamegraph (`showHeapFlamegraph()`); label shows alloc-event count. `CONDITIONAL` on `hasAllocEvents`. Green styling. | Toolbar `#btn-heap-flamegraph` ("Heap (N)"). | `viewer.html:870`, `5028`, `2361-2367`, `6825-6984` |
| D4. Uninstrumented info button | Toggles a popup listing tasks lacking wake tracking (raw `tokio::spawn`). `CONDITIONAL` on `uninstrumentedCount > 0`; label updates to "Uninstrumented (N)" (initial markup reads "Blind spawns"). Blue styling. | Toolbar `#btn-uninstrumented-info`. | `viewer.html:871`, `5033-5035`, `2352-2359`, `5645-5708` |
| D5. Uninstrumented popup | Fixed popup below the button: header "N uninstrumented task(s) at M site(s)"; hint text linking to `TelemetryHandle::spawn` docs and the "Uninstrumented Polls" filter; sites grouped and sorted by count desc, each an auto-generated docs.rs link when the path matches a crate registry pattern, else plain text. Toggles off on repeat click; closes on `x`, click-outside, or Escape. | Click button; `x`/outside/Escape to close. | `viewer.html:957`, `5645-5717` |
| D6. Parse-perf button | Toggles a popup with the fetch/parse/analysis timing breakdown. `CONDITIONAL` on `loadPerf` (after successful load). Blue styling. | Toolbar `#btn-parse-perf` ("Parse perf"). | `viewer.html:872`, `5036-5037`, `2381-2382`, `5742-5854` |
| D7. Parse-perf popup content | "Load breakdown" with Mode (streamed/buffered/local/reparse + note), Total, mode-specific Fetch/Parse rows, Analysis+render, and optional throughput (events/s, MB/s). Uses a provisional `performance.now()-startMs` total if opened before finalization; stream-mode note explains the combined figure. | Opened by D6; `x`/outside/Escape to close. | `viewer.html:958`, `5742-5854`, `5750-5806` |
| D8. Popup positioning | Both popups: `position:fixed`, placed 4px below their button, right-aligned to the button's right edge, `z-index:200`, scrollable overflow. | Automatic on open. | `viewer.html:5699-5703`, `5835-5840` |
| D9. Popup toggle + global Escape order | Repeat button click closes; a global keydown closes popups on Escape in order (help -> uninstrumented -> parse-perf -> stack sidebar), then clears task selection and refocuses main-area. | Escape / repeat click. | `viewer.html:5649-5653`, `5745-5749`, `6273-6292` |

---

## E. Toolbar: time display & range filter

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| E1. Time mode toggle | Toggles `useAbsoluteTime` between relative (offset from trace start, `+` prefix) and absolute (wall-clock via clock-sync anchors, falling back to relative if none). Reveals/hides the TZ button; re-renders all timestamps. | Toolbar row 2 `#btn-time-mode` ("Time: Relative/Absolute"). | `viewer.html:879`, `5074-5081`, `1236-1262` (`fmtTs`/`fmtWallClock`) |
| E2. Timezone toggle | Toggles `useLocalTz` (UTC vs local) for absolute timestamps only; hidden unless in absolute mode; re-renders. | Toolbar row 2 `#btn-tz-mode` ("TZ: UTC/Local"). | `viewer.html:880`, `5082-5086` |
| E3. Set Range | Captures the current viewport (`viewStart`/`viewEnd`, rounded) as a time filter, updates URL `?start`/`?end`, and re-parses the retained buffer to only events in range; reveals Clear Range. | Toolbar `#btn-set-range`. | `viewer.html:873`, `5067-5068`, `1907-1923` |
| E4. Clear Range | Re-parses the full trace (`reparseWithRange(null,null)`), removing `start`/`end` URL params. `display:none` until a range is active (set, or present in URL on load). | Toolbar `#btn-clear-range`. | `viewer.html:874`, `5070-5072`, `1922` |
| E5. URL `start`/`end` params | `?start=<ns>&end=<ns>` (either optional) filter the trace at parse time (inclusive on both ends; uncapped event types kept for structural integrity). If present on load, Clear Range shows immediately. Managed via `history.replaceState` (no reload). | URL query params. | `viewer.html:1932-1952`, `5071-5072`; `trace_parser.js:552-553,642-646` |

---

## F. Timeline header (time axis)

> AMENDED 2026-07-12 by T25 (chunk-2 viewer migration). Rows marked
> `[2026-07-12]` describe the MIGRATED viewer's time-axis track
> (`src/pages/viewer/axis.ts`, filling the "timeline" track slot from T21's
> `track-layout.ts`); the legacy `#timeline-canvas` keeps its recorded
> behavior. The row-walker asserts each side per-side (T15 convention). Two
> deltas land here: (1) a layout-seam adaptation - in the unified track
> column every track's canvas is `drawW`-wide and sits after a shared
> `LABEL_W` DOM label gutter, so the axis draws at draw-area-relative x
> (the F2 `nsToX`) instead of the legacy full-width canvas that added
> `LABEL_W` inline; the shared DOM gutter is what keeps every track aligned
> (A13). (2) the date-qualification amendment (no 04 finding id; S2/#137
> time-legibility family) - see F1.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. Time axis ruler | Fixed 30px canvas drawing tick marks + `fmtTs`-formatted labels. Interval auto-picked from a nice-values list (`1e3..1e10` ns) targeting ~4-16 ticks (`max(4, floor(drawW/100))`); ticks offset by `LABEL_W` to align with lanes. Non-interactive; redraws on zoom/pan/resize. `[2026-07-12]` (T25, migrated page) The ruler is the "timeline" track: same nice-value interval + `fmtTs`-parity labels, but drawn at draw-area-relative x (canvas is `drawW`-wide, the `LABEL_W` offset comes from the DOM label gutter - F2). Clock/format mode is READ from the store (`uiPrefs.timeMode` E1, `uiPrefs.tz` E2 - the toggle buttons are T33's). DATE-QUALIFICATION AMENDMENT: when the visible span's start and end fall on DIFFERENT calendar days in the active tz (absolute mode with resolvable clock-sync anchors), every tick label gains a `MM-DD ` date prefix (`MM-DD HH:MM:SS`) so day-crossing ticks are unambiguous; same-day spans, and relative mode, stay time-only (same rule as T15's heatmap-axis amendment, features/01 F10; narrower `MM-DD` prefix). Legacy page stays full-width-canvas + time-only. | Above the lanes (`#timeline-canvas`; migrated: the `timeline` track canvas). | legacy `viewer.html:887-888`, `2900-2940` (`renderTimeline`); migrated `src/pages/viewer/axis.ts` (`renderTimeAxis`, `fmtAxisTick`, `isDateQualified`) |
| F2. Coordinate transform | `nsToX(ns, drawW)` maps timestamp to pixel (relative to draw area, no `LABEL_W`, used by lane-style canvases); `makeTimePanelLayout` variants add the `LABEL_W` offset for panels. `[2026-07-12]` (T25, migrated page) The axis uses this draw-area-relative form directly (`nsToDrawX` == `nsToX(ns, drawW)`) so its ticks are byte-identical to the lanes' canvas-local x; the shared `PanelGeometry` from `lib/canvas/layout` is the single producer of the mapping (the A13 invariant, asserted pixel-exact at three widths in `axis.test.ts`). | Internal. | legacy `viewer.html:2826-2828`; migrated `src/pages/viewer/axis.ts` (`nsToDrawX`) |

---

## G. Worker lanes

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| G1. Lane grid | One 60px `.lane` per worker in a scrollable `#lanes-container` (`overflow-y:auto`, `overflow-x:hidden`); each lane = 100px `Worker N` label (`z-index:1`) + flex canvas (`worker-<id>-canvas`), 1px bottom border. Built by `buildLanes` on trace load. `[2026-07-08]` (#596) Lanes are now built per runtime group in group order, each lane carrying `data-worker` for DOM-geometry hit-testing (G21-G24); single-runtime traces render identically to before. | Main area. | `viewer.html:2466-2506` (`buildLanes`), `669-726`, `374-379` |
| G2. Lane background states | Active periods dark `#1a1e2a`; parks reddish-brown `#2a1520` with orange accent; scheduling delay bright red; `block_in_place` handoff gaps drawn as `#3a2a00` fill with orange dashed diagonal hatching. | Visual per lane. | `viewer.html:2978-3079` (`renderLane`) |
| G3. CPU scheduling tint | When `trace.hasCpuTime`, active periods tint green (>=95% on-CPU), yellow (50-95%), red (<50%); `pixelDownsampleSpans` picks one representative per pixel column (longest wins). | Visual; hover shows on-CPU %. | `viewer.html:2992-3010`; `trace_analysis.js:337-360` |
| G4. Poll bars | Polls drawn in a center band (y 10-30) with a duration heatmap color (log scale navy->cyan->orange->red, 24 quantized bins) via `pollHeatmapColorQuantized`; `pixelDownsampleSpans` + `makeBarCoalescer` collapse millions of polls to a few fillRects. | Visual. | `viewer.html:3082-3148`; `trace_analysis.js:229-250`, `280-305` |
| G5. Open-ended poll marker | Polls still running at trace end / `block_in_place` boundary get an orange dashed right edge. | Visual. | `viewer.html:3149-3163` |
| G6. Selected-task highlight | With a task selected, its polls render solid yellow `#ffeb3b` (drawn over a dimmed pass where non-selected polls use `pollColorDim`, RGB*0.4 cached). | Click a poll. | `viewer.html:3138-3147`, `1166-1178` |
| G7. Selected-span highlight | With a span focused, polls containing a segment of that span (or its selected ancestors) get a yellow 2px outline. | Focus a span. | `viewer.html:3248-3276` |
| G8. Hovered-waker highlight | Hovering a waker label in task detail highlights that waker task's polls orange `#ff8a65` (`hoveredWakerTaskId`). | Hover waker label. | `viewer.html:3235-3246` |
| G9. CPU sample ticks | Small magenta ticks below the poll band mark CPU-sample timestamps. | Visual (when CPU profiling on). | `viewer.html:3198-3214` |
| G10. Sched event triangles | Red downward triangles below the band mark polls with blocking sched events. | Visual. | `viewer.html:3217-3233` |
| G11. Wake markers | With a task selected, green downward triangles at lane top mark where that task was woken. | Select task. | `viewer.html:3278-3296` |
| G12. Local-queue step line | Orange step line (`rgba(255,200,50,.8)`) below the band traces local queue depth, scaled to the shared visible max, with a `q:N` label. | Visual. | `viewer.html:3299-3344` |
| G13. Lane click -> select task | Click (non-drag, within lanes/draw area) finds the poll at the timestamp; if it has a `taskId`, sets `selectedTaskId` (yellow highlight across all lanes) and shows the task-detail panel; clicking the same task again toggles it off; clears any custom-event marker. Clicks in the label gutter or outside valid lanes clear selection. `[2026-07-08]` (#596) Worker resolution now via `workerAtClientY` (G23); a click on a runtime header instead toggles that runtime's collapse (G22) WITHOUT clearing the selection. | Click a poll. | `viewer.html:5348-5453` |
| G14. Lane click -> span auto-focus | The same click walks up the span tree to the outermost ancestor containing the click timestamp on that worker, focusing it (and its ancestor chain, cycle-guarded at 1024) in the span panel. | Click a poll. | `viewer.html:5401-5439` |
| G15. Stack popup on poll click | If the clicked poll has CPU or sched samples, `showStackPopup` opens the Poll Detail sidebar near the click; otherwise `hideStackPopup`. | Click poll with samples. | `viewer.html:5380-5386` |
| G16. Lane hover tooltip | Rich tooltip: worker id, timestamp, state (Active/Parked/block_in_place/Polling) with on-CPU %/park/poll durations, kernel sched delay, task id + spawn location, CPU/sched sample counts ("click to view"), span count + names, global/local queue depths, active-task count, current span detail + parent. Cursor becomes `pointer` over a clickable stack. | Hover a lane. | `viewer.html:6361-6564` |
| G17. Vertical scroll sync | Scrolling `#lanes-container` re-renders the crosshair so it stays aligned. `[2026-07-08]` (#596) The old `laneIdx = floor((mouseY + scrollTop)/LANE_H)` mapping survives only as `workerAtClientY`'s single-runtime fast path (G23). `[T22 amended -> ledger]` unified column: the whole track column scrolls as one, lanes hold no private scroll handler; all worker rows stack into the one lanes canvas (`components/canvas/lanes`). | Scroll lanes. | `viewer.html:5118-5120` |
| G18. Auto-scroll to lane | Selecting a task / POI / filtered span / keyboard-nav target scrolls the worker's lane into view. `[2026-07-08]` (#596) Now via `scrollToWorkerLane` (uses the lane's real `offsetTop` and auto-expands a collapsed runtime first, G24) instead of `scrollTop = idx*LANE_H`. `[T22 amended -> ledger]` becomes a column-level navigation concern (T23/T33 drive it); the lanes render is scroll-agnostic. | Automatic on those actions. | `viewer.html:2550-2558` (`scrollToWorkerLane`); call sites `2314`, `2452`, `6200` |
| G19. Legend | Toolbar legend explains the poll heatmap gradient, parked, kernel sched delay, CPU-sampled, sched (blocking), wake, local-queue swatches; non-interactive. `[T22 amended (04 F3) -> ledger]` relocated to a non-interactive ribbon on the lanes track (`LANES_LEGEND`, `components/canvas/lanes/legend.ts`) covering EVERY in-lane mark incl. block_in_place, open-ended poll, selected/waker highlights, and the `q:NN` label. | Toolbar row. | `viewer.html:6646-6670` |
| G20. Legacy `selectedEvent` poll indicator | `selectedEvent` is cleared on any lane click but has no lane-render code. Status: `DEAD` (leftover from older design). `[T22 retired -> ledger]` not ported; the live in-lane pinned-event mark is `selection.pinnedEvent.poll` (I5/K4, T27), distinct from this dead row. | N/A. | `viewer.html:1299`, `5362` |
| G21. Runtime group headers | `[2026-07-08]` NEW in #596. `CONDITIONAL`: only when the trace has MORE THAN ONE runtime group (named runtimes from `runtime.<name>` segment metadata via `computeRuntimeGroups`; workers no named runtime claims form an inferred `main` block). A sticky 24px header row sits above each runtime's lanes: collapse caret, `runtime: <name>` (or `<name> runtime` for the inferred block; name set via `textContent` - no HTML injection), and `N worker(s)`. Worker lane order becomes group order. Single-runtime traces render no headers (unchanged look). | Load a multi-runtime trace. | CSS `viewer.html:675-705`; state `1183-1188`; grouping `1981-1993`; header build `2470-2486`; `trace_analysis.js:1554-1599` (`computeRuntimeGroups`); test `test_runtime_groups.js` |
| G22. Runtime collapse/expand | `[2026-07-08]` NEW in #596. Clicking a runtime header toggles that runtime's lanes (caret flips; collapsed lanes are removed from the DOM and skipped by `renderAll`); handled BEFORE selection-clearing so the toggle never destroys the current task/event selection. Collapse state (`collapsedRuntimes`) survives a same-trace time-range reparse but is dropped for runtime names absent from the next trace. | Click a runtime header. | `viewer.html:5358-5359`, `2508-2513` (`toggleRuntimeCollapsed`), `1988-1993`, `2769-2773` |
| G23. Geometry-based lane hit-testing | `[2026-07-08]` NEW in #596. `workerAtClientY` resolves the worker under a vertical mouse position: O(1) `index*LANE_H` arithmetic in the single-runtime case (keeps the high-frequency hover path cheap), DOM `getBoundingClientRect` scan over `.lane[data-worker]` when grouped (headers + collapsed runtimes break the uniform-row assumption). Returns null over headers/empty space. Used by lane click (G13) and lane hover (G16). | Internal (click/hover). | `viewer.html:2515-2544` (`workerAtClientY`); callers `5368`, `6409` |
| G24. Auto-expand on navigation | `[2026-07-08]` NEW in #596. `scrollToWorkerLane` expands the target worker's runtime FIRST if collapsed (rebuilding lanes), then scrolls to the lane's `offsetTop` - so POI jumps (C7), span navigation (J11), and sched-panel jump-to-poll (R8) always reveal their target instead of silently scrolling to nothing. | POI/span/sched navigation into a collapsed runtime. | `viewer.html:2546-2558` (`scrollToWorkerLane`); call sites `2314`, `2452`, `6200` |

---

## H. Viewport navigation & region selection

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| H1. Zoom In button | `zoom(0.5)` centered at mid-view; min duration clamped to 100ns; clears toasts. | Viewport controls `#btn-zoom-in`. | `viewer.html:894-896`, `5050-5051`, `5089-5097` |
| H2. Zoom Out button | `zoom(2)` centered at mid-view; clamped to `[minTs,maxTs]`. | Viewport controls `#btn-zoom-out`. | `viewer.html:897-899`, `5053-5054` |
| H3. Fit All button | Resets `viewStart=minTs`, `viewEnd=maxTs`. | Viewport controls `#btn-fit`. | `viewer.html:901-903`, `5055-5059` |
| H4. Viewport controls panel | Floating panel bottom-right of main-area (`right:16px bottom:16px`, `z-index:50`, glass blur bg `rgba(22,33,62,.92)`), 3 buttons + separator; button clicks `stopPropagation` so they don't trigger poll clicks. | Bottom-right of timeline. | `viewer.html:319-356`, `893-904`, `5062-5064` |
| H5. Ctrl/Cmd+wheel zoom | Wheel with Ctrl (or Cmd on Mac) zooms centered on the cursor (`factor 1.3`, up=in, down=out); `preventDefault`. Plain wheel scrolls lanes vertically. | Ctrl/Cmd+scroll over lanes. | `viewer.html:5100-5115` |
| H6. Drag pan | Left-drag (no modifier), >3px, pans the view; cursor `grabbing`; clamped to `[minTs,maxTs]`; RAF-throttled render during drag. | Click-drag lanes. | `viewer.html:5144-5181`, `5270-5301` |
| H7. Shift+drag region select | Shift-drag draws a blue overlay (`rgba(66,133,244,.15)`), and on release (>3px, >=100ns) opens a panel by data present in the range: sched-only -> blocking calls, heap-only -> heap flamegraph, else CPU flamegraph. Shows an error toast if the trace has no CPU samples. | Hold Shift, drag lanes. | `viewer.html:5148-5173`, `5302-5345` |
| H8. Alt/Option+drag zoom | Alt-drag draws a teal overlay (`rgba(0,188,180,.15)`) and on release zooms the view to the selection. | Hold Alt/Option, drag lanes. | `viewer.html:5152-5173`, `5310-5316` |
| H9. Keyboard Shift/Alt selection | Pressing Shift (region) or Alt (zoom) starts a keyboard selection: cursor at mouse position (if in view) else view center; arrow keys extend by 5% of view; Shift/Alt again or Enter confirms; Escape cancels; announced via ARIA. Blocked if the sidebar already holds a retained range. | Press Shift/Alt (no drag). | `viewer.html:6227-6268`, `5224-5268` |
| H10. Selection overlay | `#selection-overlay` div spans full main-area height during shift/alt drag or keyboard selection; blue for shift, teal for alt (`setSelOverlayColor`); `pointer-events:none`; cleared on mouseup/Escape. Shift selections are owned by the sidebar and persist until it closes. | Visible during selection. | `viewer.html:728-735`, `5133-5142`, `5183-5209` |
| H11. Arrow zoom | Arrow Up = `zoom(0.5)` (in), Arrow Down = `zoom(2)` (out); requires a loaded trace; `preventDefault`. | Up/Down arrows. | `viewer.html:6293-6300` |
| H12. Arrow pan | Arrow Left/Right pan by 20% of view duration, clamped to `[minTs,maxTs]`, preserving duration. | Left/Right arrows. | `viewer.html:6301-6324` |
| H13. Set/Clear Range (nav) | See E3/E4 - viewport-derived time filter reparse. | Toolbar buttons. | `viewer.html:5067-5070` |

---

## I. Crosshair & selection overlays

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| I1. Crosshair-overlay canvas | Fullscreen `position:absolute`, `pointer-events:none`, `z-index:10` canvas rendering all crosshairs and the selected-event marker; redraws on mousemove/scroll/zoom. | `#crosshair-overlay`. | `viewer.html:886`, `4929-5022` (`renderCrosshair`) |
| I2. Mouse crosshair | Dashed white vertical line (`rgba(255,255,255,.3)`, 4px dash) at the mouse timestamp; hidden when `mouseNs` is outside `[viewStart,viewEnd]` or during drag. | Hover lanes/panels. | `viewer.html:4951-4962` |
| I3. Keyboard-selection cursor | Solid bright line (`rgba(255,255,255,.8)`, 1.5px) at `kbCursorNs` during keyboard Shift/Alt selection. | During keyboard selection. | `viewer.html:4964-4973` |
| I4. Custom-event hover guide | Orange dashed line (`rgba(255,140,0,.4)`, 3px dash) across all lanes at a hovered custom event's timestamp (`hoverEventTs`); cleared on mouseleave. | Hover a custom-event tick. | `viewer.html:4978-4989` |
| I5. Selected-event marker | Persistent orange dashed line (`rgba(255,140,0,.9)`) + label chip (`name @ HH:MM:SS.mmm`) at the pinned event's timestamp; clamped inside the viewport; cleared on deselect/Escape. | Click a custom-event tick. | `viewer.html:4991-5021` |
| I6. Queue info panel | Top-right `#info-panel` shows `Global Q / Local max / Active tasks` at the mouse position (via `updateInfoPanel`), cleared outside the view range. | Hover over lanes. | `viewer.html:656-667`, `4894-4927` |

---

## J. Span panel & span filtering

Foldable panel (`#span-panel`, `data-panel-key=spans`, 120px expanded / 24px collapsed, initially collapsed). Sources include `viewer.html:390-398`, `922-925`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| J1. Span canvas | Renders visible spans as clustered bars (6px x-grid, log-scale duration y-position, cluster height scaled by `log2(size)`), with darker active-time segments; hidden when collapsed. | Expand Spans panel. | `viewer.html:3378-3538` (`renderSpanPanel`) |
| J2. Span focus (click) | Click a span/cluster to focus it: pinned to top (y=4, ~6x height), descendants below (y=34, ~3x), non-focused dimmed to 0.08 alpha; selects the task polling at span start; info shows percentile stats; click same span again or empty area clears (`clearSpanTaskSelection`). | Click a span bar. | `viewer.html:3843-3895`, `4193-4198` |
| J3. Span label / metadata | Left label area (184px, scrollable) shows the focused span name + key/value field rows (unit-aware via `formatFieldValue`), each with a copy button; shows "Spans" when none focused. | Left of panel when expanded. | `viewer.html:923`, `3361-3376`, `440-474` |
| J4. Metadata copy button | Clipboard button per k/v row copies the value, flashing a checkmark for 800ms; clicks on it do not toggle the panel fold. | Click copy button. | `viewer.html:461-474`, `3964-3971` (`copyFromKvButton`) |
| J5. Span info text | Top-right shows `N spans - M clusters` and, when focused, `name: duration (P%% of N) P50=.. P99=..`. Non-interactive. | Top-right of panel. | `viewer.html:924`, `3451-3452`, `3877-3888` |
| J6. Span hover tooltip | Single span: name, duration + percentile rank, active/idle, poll count, worker ids, fields. Cluster: size, top names, min-max duration. Positioned near cursor, flips above if overflowing. | Hover span canvas. | `viewer.html:3785-3834` |
| J7. Span filter (text) | `#span-filter` filters spans by name or field key/value (case-insensitive substring); clear button appears when non-empty; rebuilds `filteredSpans`. | Toolbar `#span-filter`. | `viewer.html:907-908`, `2263-2274`, `1112-1130` (`spanMatchesFilter`) |
| J8. Percentile filter | `#span-pct-filter`: All / >=P50 / >=P90 / >=P95 / >=P99; shows only spans at/above that percentile of their name's duration distribution (cached via `getSpanDurations`). | `#span-pct-filter` dropdown. | `viewer.html:912-918`, `2276-2281`, `1134-1150` |
| J9. Span-name chips | One color chip per unique span name; click toggles inclusion in `selectedSpanNames` (active=filled, inactive=bordered); AND-combines with text + percentile filters. | Click chips in `#span-legend-items`. | `viewer.html:920`, `2220-2242` |
| J10. Clear-names button | Deselects all span-name chips; visible only when >=1 selected. | `#btn-span-clear-names`. | `viewer.html:919`, `2255-2260` |
| J11. Filtered span nav | `#btn-span-prev`/`#btn-span-next` jump through `filteredSpans` (sorted by time, wraps), centering the view (~5-10x span, min 1ms), scrolling the lane, and highlighting the span + ancestors; disabled when no filter/matches. | Prev/Next span buttons. | `viewer.html:909-910`, `2306-2337` |
| J12. Filter count | `#span-filter-count` shows `N matches` or `M/N` during navigation; empty when no filter active. | Read-only. | `viewer.html:911`, `2301`, `2327` |
| J13. Unmatched-spans warning | "N unmatched" (red) when spans have an enter but no exit (trace ended mid-span / segment rotated); hover tooltip explains. | Below legend chips. | `viewer.html:2244-2254` |
| J14. Span legend bar visibility | The whole `#span-legend` bar (filter input, buttons, dropdown, chips) shows only when the Spans panel is expanded, not CSS-hidden, and `allSpans.length > 0`. | Automatic. | `viewer.html:906`, `1443-1445`, `2261` |

---

## K. Custom events panel

Foldable panel (`#custom-events-panel`, `data-panel-key=events`, 40px expanded / 24px collapsed, hidden unless `genericCustomEvents.length > 0`). Sources: `viewer.html:586-607`, `931-934`, `2155-2159`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| K1. Event tick canvas | Renders visible events as colored ticks clustered per pixel column; tick width `max(3, min(3+log2(size)*2, 10))`; alpha `min(0.4+size*0.15, 1)`, unrelated ticks fade to 20% when a task is selected; mixed-name clusters show a secondary-color bottom stripe. Hit regions padded to >=12px. | Expand Events panel. | `viewer.html:3541-3637` (`renderCustomEventsPanel`) |
| K2. Panel info | Top-right `#ce-panel-info` shows `N events - M markers`. | Top-right of panel. | `viewer.html:933`, `3585` |
| K3. Hover tooltip + guide | Hover shows event fields, timestamp, and task line ("click to select/inspect"); cursor becomes `pointer`; draws the orange guide crosshair (I4). | Hover a tick. | `viewer.html:4221-4266` |
| K4. Click to select event | Click a tick pins the selected-event marker (I5), populates the sidebar Event/Related tabs, and (if it resolves to a task) selects that task's poll; repeat click on the same tick toggles off. | Click a tick. | `viewer.html:4272-4316` |
| K5. Name filter chips | `#ce-legend-items` chips per unique event name; click toggles `selectedCENames` (active=filled); filters the canvas. | Click chips. | `viewer.html:2128-2148` |
| K6. Clear-names button | Clears all event-name filters; visible only when >=1 active. | `#btn-ce-clear-names`. | `viewer.html:928`, `2150-2154` |
| K7. Task resolution | `taskForEvent`/`pollForEvent` resolve an event to a task/poll via `task_id`, then `worker_id`+timestamp, then unambiguous scan; results cached (WeakMap). | Internal (highlight/click). | `viewer.html:2637-2697` |
| K8. Inline tick label | No per-tick text label is rendered (names only appear in tooltip/sidebar/chips). Status: `DEAD` (not implemented). | N/A. | `viewer.html:3541-3637` |

---

## L. CPU usage panel

Foldable panel (`#cpu-panel`, `data-panel-key=cpu`, 92px expanded / 24px collapsed, hidden unless `processCpuUsage.intervals.length > 0`). Sources: `viewer.html:608-629`, `936-938`, `2058-2059`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| L1. CPU stacked-area chart | Draws avg-cores-over-time bars, bg `#111b2e`, y-axis `0..max(1, max cores, available parallelism)`, grid at 25/50/75%; bars colored by load (blue-ish low -> pink/red high); a dashed orange capacity line (`rgba(255,207,153,.65)` + "N core capacity") when parallelism is known. | Expand CPU Usage panel. | `viewer.html:3669-3747` |
| L2. Info label | Top-right shows `avg X cores [ - avg Y%] - max Z cores` for the visible window (percent only if parallelism known; non-finite shows `-`). | Top-right of panel. | `viewer.html:937`, `3652-3679` |
| L3. Hover tooltip | Over an interval: Window/CPU-time durations, Cores, optional Total CPU %; cursor `crosshair`; binary-search interval lookup (`findProcessCpuIntervalAt`). | Hover CPU chart. | `viewer.html:4318-4346`, `3749-3771` |
| L4. Crosshair sync | Hovering updates global `mouseNs` and redraws the crosshair; the panel owns its own tooltip and suppresses the lanes tooltip. | Hover CPU chart. | `viewer.html:6386-6390` |
| L5. Data source | Built by `buildProcessCpuUsageSeries` from `ProcessResourceUsageEvent` custom events (user/system CPU deltas); `available_parallelism` from `segmentMetadata('process.available_parallelism')`. | Internal on load. | `viewer.html:2056-2057`; `trace_analysis.js:85-148` |
| L6. Click handler | None wired on the CPU canvas (mousemove/mouseleave only). Status: `DEAD` (no click affordance). | N/A. | `viewer.html:4318-4346` |

---

## M. Queue depth panel

Foldable panel (`#queue-chart`, `data-panel-key=queue`, `tabindex=0`, `aria-label="Queue depth chart"`, 120px expanded / 24px collapsed, initially collapsed). Sources: `viewer.html:639-667`, `946-951`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| M1. Global queue area | Blue filled step-area (`rgba(79,195,247,.3)` fill, `#4fc3f7` stroke) of max global queue depth per pixel bucket. | Expand panel. | `viewer.html:4815-4835` |
| M2. Max-local queue line | Orange step line (`#ff8a65`, 1px) of the max per-worker local queue per bucket. | Expand panel. | `viewer.html:4837-4856` |
| M3. Active-task line | Green step line (`#81c784`, 1.5px) of active task count on a separate right-side y-axis (`tasks:N`). `CONDITIONAL` on `activeTaskSamples.length > 0`. | Expand panel (when data). | `viewer.html:4858-4891` |
| M4. Y-axis labels | Left: max queue (top) and `0` (bottom), gray 10px monospace, right-aligned at `LABEL_W-6`; the max excludes the active-task scale. | Left of chart. | `viewer.html:4808-4813` |
| M5. Legend | Header legend swatches: Global (blue box), Max local (orange line), Active tasks (green line); hidden when collapsed. | Header when expanded. | `viewer.html:947-949` |
| M6. Hover info | See I6 (`#info-panel`) - Global Q / Local max / Active tasks at cursor. | Hover chart. | `viewer.html:4894-4927` |
| M7. Drag-select spawned tasks | Click-drag (>=3px, `crosshair` cursor, green overlay `rgba(129,199,132,.2)`) selects a time range; on release, finds tasks whose spawn time falls in range, groups by spawn location (sorted by count desc, 5 shown per group), and lists them in the sidebar with clickable hex task-id links and range duration. `CONDITIONAL` on `activeTaskSamples` + `taskFirstPoll` present and panel expanded. | Drag on queue canvas. | `viewer.html:7079-7205` |
| M8. Spawned-task link click | Clicking a listed task id sets `selectedTaskId`, closes the stack popup, and re-renders (task highlighted in lanes). | Click a spawned-task link. | `viewer.html:7193-7199` |
| M9. Expanded-panel click | Clicking non-label areas of the expanded panel does nothing (to avoid interfering with drag); only the label (or any click while collapsed) toggles. Status: intentional (`DEAD` for non-label expanded click). | N/A. | `viewer.html:1474-1479` |

---

## N. Task detail panel

Optional 160px panel (`#task-detail`), not foldable, shown only when a task is selected. Source: `viewer.html:381-389`, `941-945`, `4358-4713`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| N1. Panel visibility | Appears below the lanes/queue chart when a task is selected (has polls); hides on deselect. | Click a poll to select a task. | `viewer.html:4361-4374` |
| N2. Label header | Shows hex task id, spawn location, poll count, wake count (if instrumented), lifetime, completion mark, and status badges. | Top-left of panel. | `viewer.html:4397-4412` |
| N3. "no wake data" badge | Red badge (links to `TelemetryHandle::spawn` docs) for tasks spawned via raw `tokio::spawn` (uninstrumented). | Click badge. | `viewer.html:4404-4406`, `775-789` |
| N4. Idle flamegraph link | "idle flamegraph (N)" opens a time-weighted flamegraph of idle periods (weight = idle us). `CONDITIONAL` on the task having task dumps. | Click link. | `viewer.html:4407-4424`, `6765-6823` (`showIdleTimeFlamegraph`) |
| N5. Status tooltip | Hover updates top-right status text with an icon + description (polling / scheduled / idle); clears on mouseleave. | Hover canvas. | `viewer.html:943`, `6594-6609` |
| N6. Wake->poll delay bands | Colored bands (green <=100us, orange <=1ms, red >1ms) between wake and next poll, with a duration label (if >25px) and a green wake triangle. Wake matching uses binary search (`computePollWakes`, O(P logP)). | Visual. | `viewer.html:4478-4556`; `trace_analysis.js:822-863` |
| N7. Waker label | "<label>" under each delay band: "io" for runtime/worker wakes, spawn filename, or hex id; clickable when band >40px. | Below delay bands. | `viewer.html:4530-4555` |
| N8. Waker hover/click | Hovering a waker label highlights that waker's polls (G8) and re-renders; clicking selects the waker task. | Hover/click waker label. | `viewer.html:6575-6591`, `6615-6626` |
| N9. Task lifespan bar | Faint green bar from spawn to terminate with spawn/done edge lines + labels, when both timestamps exist. | Visual. | `viewer.html:4558-4588` |
| N10. Polling sections | Cyan bars for active execution; when polls > drawW, renders a per-pixel coverage histogram (opacity = fraction polling) instead of per-poll bars; per-poll duration labels when zoomed. | Visual; hover status. | `viewer.html:4590-4644` |
| N11. Idle gaps + stacks | Dark bands between polls; gaps with task dumps get a purple cross-hatch and dashed purple border; hover shows duration + (click for async stack); clicking one opens the captured async stack(s) as a sidebar flamegraph ("Waiting on - N captures"). | Click cross-hatched idle gap. | `viewer.html:4646-4704`, `6628-6639`, `6755-6763` |
| N12. Legend | Bottom-left canvas legend: "Task" label and "▲ = wake" marker note. | Bottom-left of canvas. | `viewer.html:4706-4712` |

---

## O. Foldable panel mechanics

Applies to spans / events / cpu / queue panels (Task Detail is NOT foldable). Sources: `viewer.html:399-427`, `1406-1481`.

> AMENDED 2026-07-13 by T36 (chunk-2 viewer migration). Under the unified
> time-aligned track column (concept-1), the legacy one-line-fold PRESENTATION
> is retired by S1 (analysis surfaces are visible by default), but the section-O
> BEHAVIOR contract - per-surface show/hide with persistence - survives as
> per-track COLLAPSE, and the column gains drag-REORDER (O8, added). Both persist
> to `uiPrefs` (localStorage) and survive reload. Rows below carry the migrated
> semantics inline (`[T36 ...]`); the legacy viewer keeps the folds. Migrated
> source: `src/pages/viewer/track-management.ts`, `tracks.ts`, `styles/viewer.css`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| O1. Collapse toggle (click) | Clicking `.chart-label` (or anywhere on a collapsed panel) toggles `is-collapsed`: 24px height, canvas hidden (`display:none !important`), label max-height 18px. Clicking `.kv-copy`, or a non-label area of an expanded panel, does not toggle. `[T36 amended -> ledger]` A caret button in the track's label gutter toggles that track's collapse to a label-only height (`COLLAPSED_TRACK_H` = 24px, matching legacy); CSS (`.d9-track-manage.is-collapsed`) hides the drawing body. Only the manageable analysis tracks (cpu/queue/spans/events - section-O scope) are collapsible; timeline/lanes/task-detail are pinned (task-detail was never foldable). The whole-collapsed-body click affordance is dropped - the caret is the single, explicit control (and does not intercept the spans copy buttons: the overlay strip is pointer-events:none except the caret+grip). | Click the track-label caret. | `viewer.html:1474-1479`; new: `src/pages/viewer/tracks.ts` (`manageWrapper`), `track-management.ts` |
| O2. Collapse toggle (keyboard) | Label has `role=button`, `tabindex=0`, `aria-expanded`; Enter/Space toggles when focused. `[T36 amended]` The caret is a real `<button aria-expanded>` with a collapse/expand `aria-label`, so Tab + Enter/Space toggles it (keyboard collapse preserved). | Tab to the caret, Enter/Space. | `viewer.html:1466-1472`; new: `src/pages/viewer/tracks.ts` |
| O3. Chevron indicator | CSS `::before` caret shows expanded vs collapsed state (`#6c63ff`, 0.85em, 7px right margin). `[T36 amended]` Preserved as a CSS-driven chevron on `.d9-track-caret::before`: down (▾) expanded, right (▸) collapsed (glyph kept out of the TS source). | Visual caret in the label gutter. | `viewer.html:410-420`; new: `src/styles/viewer.css` (`.d9-track-caret`) |
| O4. localStorage persistence | State stored under `dial9.viewer.panelCollapsed.<key>` (`collapsed`/`expanded`), with an in-memory `viewerStorageFallback` map when localStorage is unavailable; all four panels start `is-collapsed` and are synced on load. `[T36 amended -> ledger]` Replaced by ONE JSON blob under `dial9.viewer.trackPrefs` = `{ trackOrder, collapsed }` (both O-collapse and O8-reorder ride it); `hydrateTrackPrefs` seeds `uiPrefs` on boot BEFORE first paint, `mountTrackPrefsPersistence` writes on change; the try/catch in-memory fallback is preserved. Default is EXPANDED, not all-collapsed (the S1 amendment - analysis surfaces visible by default). This is the headline DoD: survives reload. | Automatic. | `viewer.html:1410-1460`; new: `src/pages/viewer/track-management.ts` (`loadTrackPrefs`/`saveTrackPrefs`/`hydrateTrackPrefs`/`mountTrackPrefsPersistence`) |
| O5. Legend sync | Collapsing Spans hides `#span-legend`; Events hides `#ce-legend`; each shows only if expanded, not display:none, and its data is non-empty. `[T36 amended]` Subsumed by the unified body-hide: the span/event legend strips now live INSIDE each track's body (T26/T27), so `.d9-track-manage.is-collapsed .d9-track-body { display:none }` hides them with the rest of the drawing body when the track collapses - same net effect. | Automatic. | `viewer.html:1443-1449`; new: `src/styles/viewer.css` |
| O6. Render on toggle | `setPanelCollapsed()` calls `renderAll()` (unless `{redraw:false}`) so the layout is responsive. `[T36 amended]` Toggling dispatches a `uiPrefs` update; the store's RAF scheduler coalesces the shell re-render to <= 1/frame (F2) - no synchronous `renderAll`. `sizeTracks` skips painting a collapsed track and re-paints it from CURRENT windowed state on re-expand (carried T17-audit notes 6-7: windowing respected). | Automatic. | `viewer.html:1456-1460`; new: `src/pages/viewer/tracks.ts` (`sizeTracks`) |
| O7. Expanded-only labels | `.panel-expanded-label` elements (queue legend items) are hidden when collapsed. `[T36 amended]` Subsumed by O5's whole-body hide: the queue legend strip is part of the collapsed track body, hidden with it. | Automatic. | `viewer.html:425-427`; new: `src/styles/viewer.css` |
| O8. Track reorder (drag) | `[T36 added -> ledger]` NEW: a drag grip in each manageable track's label gutter drag-reorders whole tracks; drop onto another manageable track SWAPS their positions (no animation). Structural tracks (timeline/lanes) and task-detail stay pinned; the resolved order is persisted in `uiPrefs.trackOrder` (same `dial9.viewer.trackPrefs` blob) and survives reload. The order resolver is robust to stale/partial stored orders (unknown/duplicate ids dropped, a newly-added track appears in its catalogue slot). No legacy equivalent. | Drag the track-label grip onto another track. | new: `src/pages/viewer/track-management.ts` (`orderedTracks`/`computeReorder`), `tracks.ts` |

---

## P. Stack sidebar & tabs

Right panel (`#stack-sidebar`, 640px / 50vw default, min 200px, max 92vw, hidden by default). Sources: `viewer.html:129-189`, `960-974`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| P1. Show/hide | Opens on poll click, event click, or region selection; the `x` close (`#stack-sidebar-close`), Escape, or `hideStackPopup()` closes it, clearing flamegraph/sched state, resetting range, and re-rendering; the flamegraph container returns to `#flamegraph-panel`. | Click x / Escape. | `viewer.html:5541-5561` |
| P2. Resize handle | 4px purple bar on the left edge; drag left widens / right narrows (`[200px, 92vw]`); cursor `col-resize`, body `user-select:none` during drag; RAF re-renders while dragging; on mouseup resizes the active flamegraph. | Drag left edge. | `viewer.html:142-156`, `5959-6002` |
| P3. Sidebar title | Context text: `Nms selected` (flamegraph/blocking/heap), event/cluster name (event), `Blocking Calls - N events`, `Waiting on - N captures`, etc.; ellipsized. | Header. | `viewer.html:166-172`, `5622-5624`, `7007` |
| P4. Tab families | Two mutually exclusive groups: Poll Detail (alone), and Event/Related (event) vs Flamegraph/Blocking/Heap (range). `showSidebarTabs(active)` sets `.active` and toggles `display` per data availability. | Click tab headers. | `viewer.html:967-973`, `5857-5911` |
| P5. Auto-narrow on event | On a fresh event open the sidebar narrows to `EVENT_DEFAULT_WIDTH`=350px; manual resizes persist. | Automatic. | `viewer.html:6715-6718` |
| P6. Auto-widen on flamegraph | On a fresh flamegraph/heap/blocking open the sidebar widens to `FLAMEGRAPH_DEFAULT_VW`=78vw; manual resizes persist. | Automatic. | `viewer.html:6707-6711` |
| P7. Body scrolling | `#stack-sidebar .sidebar-body` (`flex:1`, `overflow-y:auto`) scrolls; scroll position is preserved across Related re-renders (collapse/load-more). | Scroll body. | `viewer.html:183-189`, `4169-4176` |
| P8. Width persistence | The region intent to "persist width in storage" is not implemented; no localStorage write for sidebar width; each panel type re-applies its default on fresh open. Status: `DEAD`. | N/A. | `viewer.html:6708-6718` |

---

## Q. Event & Related detail (sidebar content)

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| Q1. Event tab | Single event: k/v field rows (unit-formatted timestamps/fields, `formatFieldValue`) with copy + correlation buttons, timestamp, task id. Cluster: count, top event types, timestamp range (no correlation). | Click event -> Event tab. | `viewer.html:968`, `3976-4005` (`eventDetailHtml`) |
| Q2. Copy button | Copies a field value, flashing a checkmark for 800ms (delegated on the sidebar body). | Click copy button. | `viewer.html:5565-5566`, `3964-3971` |
| Q3. Correlation button | Shown only on field values shared by multiple events; clicking sets `correlateField`, switches to Related, and shows a "Same field=value" section. | Click correlation button. | `viewer.html:3955-3960`, `5567-5572` |
| Q4. Related tab | `CONDITIONAL` on a single-event selection. Sections: field correlation (if set), enclosing spans (by depth), same span, same task, same type; each collapsible, with a windowed view (`RELATED_INITIAL`=5) and "load more" (`RELATED_STEP`=25). | Click Related tab. | `viewer.html:969`, `4044-4167` (`relatedHtml`) |
| Q5. Related section toggle | Clickable headers with a caret; empty/self-only sections default collapsed; user toggles persist in `relatedCollapsed` across selections (cleared on reset); hidden rows stay indexed. | Click a section header. | `viewer.html:4058-4070`, `5575-5582` |
| Q6. Load-more affordance | Per-section, per-direction "load N more earlier/later (M hidden)" reveals more rows by `RELATED_STEP`; disappears when exhausted; scroll preserved. | Click load-more. | `viewer.html:4088-4095`, `5585-5592` |
| Q7. Related row navigation | Each non-self row is clickable: span rows focus + center on the span; event rows pin + center + mark the event. The self row (`r-self`) is a non-navigable highlighted anchor. | Click a row. | `viewer.html:4074-4085`, `5595-5601` |
| Q8. Related empty message | Placeholder ("none", "task unresolved", "No event selected") in empty sections. | Automatic. | `viewer.html:574-579`, `4046`, `4155` |

---

## R. Poll detail & blocking-calls / scheduling panel

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| R1. Poll Detail tab | For a clicked poll: deduplicated blocking sched events (red) and CPU profile samples (orange), each grouped by leaf frame with count/percentage bars and expandable frames (>3 frames). Title `Poll <duration> - N CPU samples - N sched events`. | Click a poll with samples. | `viewer.html:967`, `5455-5538` (`showStackPopup`) |
| R2. Frame expand/collapse | Toggle in each group reveals/hides frames beyond the first 3 (text flips between collapse and "N more frames"). | Click the toggle. | `viewer.html:5492-5498`, `5521-5527` |
| R3. Blocking Calls tab/panel | Scheduling events (kernel deschedules during polls) for a range or the whole trace; opened via toolbar D1, region selection, or the tab. Title `Nms selected` or `Blocking Calls - N events`. | D1 / Shift-drag / tab. | `viewer.html:971`, `6008-6051` (`showSchedPanel`) |
| R4. Group-by dropdown | `#sched-group-by-sb`: "blocking call" (leaf frame, default) vs "full stack"; changing re-renders the panel. | Select in panel. | `viewer.html:6098-6102`, `6179-6186` |
| R5. Summary bar chart | Horizontal bars per blocking-call type (count, bar, name, %), color-coded (red lock/mutex, cyan epoll/poll, orange I/O, gray syscall, purple other). | Top of panel. | `viewer.html:6108-6126` |
| R6. Summary rows (click) | Rows have `cursor:pointer` + `data-leaf` but no click handler wired. Status: `DEAD`. | N/A. | `viewer.html:6119` |
| R7. Expandable group stacks | Each group header (count, name, %, toggle) expands unique full stacks with counts/percentages and color-coded frames (group headers use a simplified red/orange scheme; the summary uses the full 5-color scheme). | Click group header. | `viewer.html:6129-6160` |
| R8. Jump-to-poll links | Up to 5 example polls per group (`W<id> @<ts> (<dur>)`); clicking centers the view (5x pad) and scrolls to the worker lane. | Click an example poll. | `viewer.html:6162-6171`, `6188-6203` |
| R9. No-data toasts | Empty sched range -> "No scheduling events found..."; empty CPU range -> "No CPU samples in selected region"; empty heap range -> "No heap samples in selected region" (all 4s error toasts). | On empty selection/tab. | `viewer.html:6011-6013`, `6991-6992`, `6875-6877` |

---

## S. Flamegraph (CPU / heap / idle / task-dump)

Rendered via `FlamegraphRenderer.createFlamegraph` (`flamegraph.js`) inside the sidebar; `#flamegraph-panel` stays `display:none` and only holds `#fg-container` when idle. Sources: `viewer.html:737-739`, `954-955`, `6989-7050`; `flamegraph.js:128-1007`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. Two-section layout | Always renders "Worker threads" (top) and "Off-worker (sampler thread)" (bottom, `workerId=255`) canvases, each with its own zoom stack; section labels show sample counts; empty sections hide. | Open any flamegraph. | `flamegraph.js:301-317`, `401-412` |
| F2. Frame rendering | Frames = deterministic name-colored rects, 18px rows, name shown if width >30px (ellipsized); ancestors at 0.6 alpha, search non-matches at 0.25; frames <0.1% of total culled; DPR-scaled. | Visual. | `flamegraph.js:323-383`, `45-103` |
| F3. Frame click -> zoom | Left-click zooms into a frame (full-width, children below, ancestor context bars at top); nesting; independent per section; no-op if childless. | Left-click a frame. | `flamegraph.js:676-700`, `520-533` |
| F4. Right-click zoom out | Pops one zoom level; falls back to the other section if the clicked one isn't zoomed. | Right-click. | `flamegraph.js:702-713` |
| F5. Breadcrumb nav | Bar above the canvases shows the zoom path per section (separated by `|`), `(all)` resets to root; each level clickable; hidden when unzoomed. | Click breadcrumb. | `flamegraph.js:291-293`, `420-476` |
| F6. Hover tooltip | Frame name, full name (if different), location (collapsible when pinned), sample count/self with percentages (format overridable for heap/idle). Positioned at container top; `pointer-events:none` unless pinned. | Hover a frame. | `flamegraph.js:564-635`, `638-656` |
| F7. Alt+click pin | Alt/Option+click pins the tooltip (selectable, `pointer-events:auto`, clickable location toggle + docs link); Escape or outside click unpins. | Alt/Option+click. | `flamegraph.js:683-685`, `630-636` |
| F8. Ctrl+click docs.rs | Ctrl/Cmd+click opens the frame's docs.rs page in a new tab (if `docsUrl`). | Ctrl/Cmd+click. | `flamegraph.js:680-682` |
| F9. Search | `.fg-search-input` filters/highlights frames (case-insensitive substring on name/fullName); matches full alpha, others dimmed; stats show `count frames - X% of samples` / "no matches"; clear `x` appears when non-empty. | Type / Ctrl-F / `/`. | `flamegraph.js:148-167`, `478-518`, `758-762` |
| F10. Spawn-location + runtime filters | Dropdown of task spawn locations (sorted by frequency, with counts) filters samples and rebuilds both trees; "All tasks" default. `[2026-07-08]` (#596) A second runtime dropdown appears for multi-runtime traces and AND-combines with the spawn filter (full semantics: features/03 F167). In the viewer, `runtimeWorkers` is passed to `setData` ONLY by the CPU-flamegraph path - heap/idle/task-dump flamegraphs pass no `runtimeWorkers`, so they never show the runtime dropdown. | Change dropdown. | `flamegraph.js:156-157,173`, `779-820` (`applyFilters`), `829-884`; `viewer.html:7040-7044` |
| F11. Export menu | "Export" opens a menu: Interactive SVG (`treeToInteractiveSvg`) and Folded stacks (`treeToFolded`, per-section headers); reflects the current spawn filter (full trees, not zoom); disabled with no data. | Click Export -> format. | `flamegraph.js:158-164`, `236-265` |
| F12. Help overlay | Info button toggles a shortcuts overlay (Click, Alt+click, Ctrl/Cmd+click, Right-click, Ctrl/Cmd+F or /, Esc); Esc / outside click closes. | Click info button. | `flamegraph.js:166-204` |
| F13. Escape cascade | Esc: unpin tooltip -> close export menu -> close help -> clear search -> reset zoom; returns true if it consumed the key, else the viewer closes the sidebar. | Press Esc. | `flamegraph.js:752-777`; `viewer.html:6282-6287` |
| F14. Resize / destroy | `resize()` refits both canvases to the container; `destroy()` removes listeners and orphans the tooltip; `getZoomPath`/`zoomToPath` save/restore zoom state. | Internal (resize/pop-out). | `flamegraph.js:886-921`, `946-967` |
| F15. CPU flamegraph (region/whole) | `showFlamegraph(start,end)` filters CPU samples to the range (or whole trace via D2); whole trace spans all sliceable records, including profiling data outside the Tokio-event extent. No CPU samples in range -> error toast; shows sample count + Pop Out; widens the sidebar. | D2 / Shift-drag / Flamegraph tab. | `viewer.html:970`, `6989-7050` |
| F16. Heap flamegraph | `showHeapFlamegraph(start,end)` strips allocator hook frames, estimates bytes/allocs via Horvitz-Thompson (`invP=1/(1-exp(-size/R))`, R=524288), with a Bytes/Count toggle (Bytes default). `CONDITIONAL` on alloc events with callchains in range. | D3 / Heap tab. | `viewer.html:972`, `6825-6987` |
| F17. Idle-time flamegraph | Time-weighted flamegraph of a task's idle-period async stacks (weight = idle us, min 1, scaled to max 10000; includes post-last-poll dumps to trace end). `CONDITIONAL` on task dumps. | Task-detail idle link (N4). | `viewer.html:6765-6823` |
| F18. Task-dump stack | Renders the async stack captures from a single idle gap ("Waiting on - N captures"), merging same-period dumps; error toast on failure. | Click a cross-hatched idle gap (N11). | `viewer.html:6755-6763` |
| F19. Pop Out | "Pop Out" opens `flamegraph.html` in a new tab preserving trace URL(s), `start`/`end`, and per-section zoom paths; blob URL for in-memory traces (info toast "keep this tab open"); error toast if no trace URL. | Click Pop Out above a flamegraph. | `viewer.html:7026`, `7052-7074` |
| F20. Frame -> timeline link `[2026-07-13]` (T32; 04 S7 "flamegraph severed from time") | Zooming a CPU flamegraph frame offers "Show in timeline", which navigates the viewport to the `[min,max]` timestamp extent of that frame's samples within the analyzed region (padded, clamped). NEW behavior on the migrated viewer only (no legacy analogue). Also: the CPU panel's sample count is labeled "N samples (on-CPU, with stacks) of M CPU records" to reconcile the S7 count contradiction (toolbar total records vs foldable on-CPU samples - different units, not a bug). | Zoom a frame in the region CPU flamegraph -> click "Show in timeline". | `src/pages/viewer/region-analysis-model.ts` (`frameSampleTimeExtent`, `cpuCountLabel`), `src/pages/viewer/region-analysis.ts` |

---

## T. Help overlay

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| T1. Help button | Dynamically created toolbar info button (question-mark SVG, `aria-label=Help`) toggling the overlay; hover brightens color/border. | Click info button. | `viewer.html:6674-6692` |
| T2. Help overlay modal | Fullscreen `rgba(0,0,0,.6)` backdrop, `z-index:200`, `role=dialog`, centered dark dialog (max-width 520px) with Mouse and Keyboard shortcut tables and a "Press Esc or click outside to close" hint. | Shown via T1 or `?`. | `viewer.html:790-819`, `981-1005` |
| T3. Toggle / close | `?` toggles it (canceling any active keyboard selection first); Escape closes it with priority over other Escape actions; clicking the backdrop (target === overlay) closes it. | `?` / Esc / backdrop click. | `viewer.html:6325-6329`, `6273-6275`, `6693-6695` |
| T4. Shortcut content | Mouse: scroll, Ctrl/Cmd+scroll, drag, click poll, Shift+drag, Option+drag. Keyboard: Tab, Up/Down, Left/Right, Shift, Option/Alt, Esc, `?`. Static reference. | Inside the overlay. | `viewer.html:983-1001` |

---

## U. Toasts & notifications

`#toast-container` (`position:absolute`, top-left of the timeline header, `z-index:60`, gap 8px, `pointer-events:none`). Sources: `viewer.html:740-774`, `889`, `1033-1065`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| U1. showToast / hideToast / clearToasts | `showToast(id,msg,type,autoHideMs,persistent)` creates/updates a toast; duplicate ids re-trigger a wiggle instead of duplicating; auto-hide via `setTimeout`; `clearToasts()` removes all non-persistent (`_persistentToasts`) toasts. | Programmatic. | `viewer.html:1033-1065` |
| U2. Types & animation | `toast-info` (blue), `toast-warn` (orange), `toast-error` (red); `toast-in` slide/fade 0.2s on create; `toast-wiggle` 0.4s on duplicate. | Automatic. | `viewer.html:740-774` |
| U3. Persistent hints | On load, two persistent info toasts: "Shift+drag to select a region" and "Option+drag to zoom"; each hidden when its selection type starts/completes. | Auto on load. | `viewer.html:2384-2385`, `5157-5158`, `6238-6239` |
| U4. Error toasts | "No CPU samples in trace..." (Shift+drag without CPU), plus the region no-data toasts (R9), pop-out errors (F19), task-dump / idle-flamegraph exceptions. | Contextual. | `viewer.html:5148-5150`, `6633-6635`, `4419-4421` |
| U5. Auto-clear triggers | `clearToasts()` fires on region-drag mouseup, lane click, zoom, Escape, sched-panel open, and queue-chart drag. | Those interactions. | `viewer.html:5306-5307`, `5349-5350`, `5090`, `6277`, `6047`, `7138` |

---

## V. Tooltips (general)

Single shared `#tooltip` element (`display:none`, `position:fixed`, `#222244` bg, `#555` border, 6px radius, max-width 320px, `z-index:100`, `pointer-events:none`). Source: `viewer.html:290-309`, `979`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| V1. placeTooltip | Positions the tooltip near the cursor (`x+12` clamped to `innerWidth-w-8`, `y+dy` default 16); flips above the cursor when it would overflow the bottom; min 8px margins; uses actual offset size. | Automatic after setting innerHTML. | `viewer.html:1486-1493` |
| V2. Panel-owned tooltips | Span, custom-events, and CPU panels each set the shared tooltip's contents and suppress the lanes tooltip while hovered (see J6/K3/L3); crosshair keeps tracking for correlation. | Hover the respective panel. | `viewer.html:3785-3837`, `4221-4266`, `4318-4346` |
| V3. Hide on drag / leave | Tooltip hides (and `mouseNs=null`) during drag; hidden on mouseleave of lanes/panels and when the cursor leaves the draw area (label gutter, past `drawW`, out-of-range lane). | Automatic. | `viewer.html:6364-6367`, `6395-6414`, `6567-6571` |

---

## W. Cross-cutting behaviors (replication-critical, not single buttons)

| Behavior | Detail | Source |
| --- | --- | --- |
| W1. Time formatting | `useAbsoluteTime`/`useLocalTz` drive `fmtTs`, `fmtDuration`, `fmtWallClock`, `fmtDelta`; toggling re-renders all views and rewrites labels. | `viewer.html:1192-1267` |
| W2. Coloring | 20-color `SPAN_COLORS` palette; `spanColor`/`ceColor` memoize per-name color (round-robin, cleared on reload); poll heatmap `pollColor`/`pollColorDim` (log-scale, 24-bin quantized, dim cache); frame color via `flamegraphColor` (shared with SVG export). | `viewer.html:1087-1178`; `trace_analysis.js:184-250`, `1010-1024` |
| W3. Render pipeline | `renderAll()` orchestrates timeline/lanes/span/CE/CPU/queue/task-detail/crosshair each frame, computing `window.sharedVisibleMaxQ` first for consistent y-scaling; `scheduleRenderAll` (RAF) coalesces; profiling under `D9PROF`. | `viewer.html:2740-2824` |
| W4. Performance LOD | `pixelDownsampleSpans` (one representative per pixel column), `makeBarCoalescer` (merge adjacent same-color bars), `pixelCoverage` (poll sampling-coverage), binary-search hit tests, and precomputed color palettes keep millions of spans/polls smooth. | `viewer.html:3113-3136`; `trace_analysis.js:280-360` |
| W5. Trace parsing | `TraceDecoder` (D9TF binary: magic, self-describing schemas, ULEB128, pooled strings/frames, delta timestamps, streaming snapshot/restore); `TraceParser.parseTrace`/`parseTraceStream` produce the full `ParsedTrace`; `canStreamDecode`, `fetchTraces`, `fetchTraceStream`, `[2026-07-08]` `fetchTracesStream` + shared `makePaintThrottle` (one 200 ms wall-clock yield policy for both parse loops, #600), `deriveBlockInPlaceGaps`, `symbolizeChain`, `deduplicateSamples`, `formatFrame`. `[2026-07-08]` All `trace_parser.js` anchors re-derived after the #582 whole-file reindent (no behavior change from #582 itself). | `decode.js:121-406`; `trace_parser.js:32-1806` |
| W6. Analysis | `TraceAnalysis`: `buildWorkerSpans`, `attachCpuSamples`, `computeSchedulingDelays`, `computePollWakes`, `buildProcessCpuUsageSeries`, `buildActiveTaskTimeline`, `getTraceTimeRange`, `hasCpuProfileSamples`, `buildFlamegraphTree`/`flatten`, `filterPointsOfInterest`, `analyzeAllocations`, span layout helpers; `[2026-07-08]` (#596) `computeRuntimeGroups` (worker->runtime grouping from `runtime.<name>` segment metadata + inferred `main` block) and `buildRuntimeFilterData` (runtime filter options for the flamegraph). | `trace_analysis.js` (throughout); `1554-1629` (runtime helpers) |
| W7. Formatting utils | `formatHumanDuration` (ns->d/h/m/s/us/ns), `formatHumanBytes` (binary units), `formatFieldValue` (unit-aware: ns/us/ms/s, bytes). | `format.js:9-69` |
| W8. Flamegraph export | `FlamegraphExport`: `treeToFolded`, `treeToInteractiveSvg` (self-contained, hover/zoom/regex-search/URL-state), `buildExportRoot`, `layoutTree`, `filenameStem`, `escapeXml`, re-exported `flamegraphColor`. | `flamegraph_export.js:60-585` |
| W9. Credentials module | `Dial9Creds` (sessionStorage): `get/has/set/clear/parse/check/listBuckets/headers`; never persists beyond the tab, headers only ride same-origin fetches; fires `dial9:credentials-changed`. | `creds.js:51-257` |
| W10. State reset on load | `processTrace` clears all selection/filter state (`selectedTaskId`, span/event selections, `selectedCENames`, `selectedSpanNames`, span filters, `processCpuUsage`) before analyzing a new trace, preventing stale carry-over. | `viewer.html:2070-2119` |
| W11. URL parameters | `?trace=` (load), `?start`/`?end` (time-range filter), `?prof=1` (profiler), `?svc`/`?host`/`?from`/`?to`/`?segs` (structured metadata for the info block). Range params managed without reload. | `viewer.html:1869`, `1927-1952`, `2173-2177` |
| W12. Layout constants | `LABEL_W`=100px (gutter), `LANE_H`=60px (worker lane height); used across positioning, hit-testing, and lane auto-scroll. | `viewer.html:1394-1395` |

---

## X. Overview minimap & status bar (T35 additions - new surfaces)

New persistent surfaces on the migrated viewer with NO legacy `viewer.html`
equivalent (added by T35; ledger "additions"). Sources are the new `src/`
modules. The minimap addresses 04 S8 (no position context); the status bar
carries 04 F7 (selection visibility), S3's status-line + copy-link SURFACE
clause (S3's URL-state clause is T19's), and the 2.8 segment feedback edge.
Density is TIER-1 only (architecture 2.8): an unfetched (tier-1-only) region
renders as such, never as empty/complete (T17-audit notes 6-7).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| X1. Overview minimap surface | Persistent band under the toolbar showing the WHOLE trace time range—all sliceable timestamped records, not only Tokio events—with a viewport box; the viewer's position context (S8). | Always visible once a trace loads. | `src/pages/viewer/minimap.ts` (`mountMinimap`); shell host `.d9-minimap` (`shell.ts`) |
| X2. Tier-1 density | Per-bin density bars from tier-1 sources: aggregate density when coverage is `full`, else listing-metadata (segment extents + gzip sizes), else the whole-trace event histogram, else a flat band. Falls back off aggregate on `partial`/`none` (never presents a partial density as complete). | Automatic; reflects the T17 segments slice / T18 coverage. | `src/pages/viewer/minimap-model.ts` (`computeDensityBins`, `binTimestamps`) |
| X3. Fetched vs tier-1-only coverage | Bins map segment residency: `parsed` -> solid "complete"; `listed`/`fetching`/`evicted` -> dimmed "truncated"; `oversized` -> distinct color; a `partial` badge appears when any in-range region is tier-1-only. Same residency->coverage mapping as the at-cursor readout (`overlay/readout.coverageAt`). | Automatic; badge visible on partial coverage. | `minimap-model.ts` (`segmentBinCoverage`, `overallCoverage`); `minimap.ts` (`updateBadge`) |
| X4. POI ticks | Amber ticks at points-of-interest times, unioning the applicable frozen-core detectors (long-poll always; sched/wake-delay when sched-wait data; uninstrumented when instrumented; `[2026-09-03]` (#872) spawn-delay when the trace carries spawn timestamps, at the DEFAULT threshold - the ticks are cached on trace identity, so the rail's live threshold would make the overview flicker; `[2026-09-03]` (#861) off-cpu-active when the worker CPU-time readings are real) - the SAME detector source as T33's issues rail, no code dependency. `cpu-sampled` and `off-cpu-poll` are excluded: both read the samples attachCpuSamples writes onto the shared poll objects. | Automatic. | `src/pages/viewer/minimap-poi.ts` (`deriveMinimapPois` over `filterPointsOfInterest`) |
| X5. Draggable/clickable viewport box | The box marks `[viewStart, viewEnd]`. A press jumps (centers the current-width view on the cursor); a drag scrubs, keeping the grabbed point under the pointer; both clamp to the overview range and DISPATCH a store viewport update (never a direct render). The unfetched tail stays navigable. | Click / drag on the minimap. | `minimap.ts` (`onMouseDown`/`onMouseMove`, `minimapNavigate`); `minimap-model.ts` (`minimapClickWindow`, `minimapDragWindow`, `grabOffsetFor`) |
| X6. Keyboard pan | ArrowLeft / ArrowRight pan the view by 15% of its width on the focused minimap region (so the focusable region is not a keyboard trap). | Focus the minimap; Left/Right. | `minimap.ts` (`onKeyDown`) |
| X7. Accessible overview label | The canvas `aria-label` announces the viewed range vs total duration and a partial-data note when regions are unfetched. | Screen reader on the minimap canvas. | `minimap.ts` (`draw`) |
| X8. Status bar surface | Persistent footer with selection, view range, segment progress, copy-link, and key hints. | Always visible. | `src/pages/viewer/status-bar.ts` (`createStatusBar`); shell host `.d9-status` (`shell.ts`) |
| X9. Selection line (F7) | Shows the selected task (`Task 0x<hex> selected`), else the focused span, else the pinned event, else "No selection". | Automatic; updates on the selection slice. `[data-status-selection]`. | `status-bar.ts` (`selectionState`) |
| X10. Clear-selection affordance (F7) | An `x` button clears the selection highlight state (task / span focus / focused span / pinned event). | Click the `x` when a selection exists. | `status-bar.ts` (`.d9-status-clear`); `main.ts` `clearSelection` |
| X11. View range readout | `view +X.XXs - +Y.YYs (duration)` from the viewport slice; "no trace loaded" at rest. | Automatic; updates on viewport. `[data-status-view]`. | `status-bar.ts` (`statusViewModel`) |
| X12. Segment fetch/parse progress (2.8) | `Segments P/N loaded · fetching F · O oversized` from the segments slice, with a spinner while any segment fetches; hidden when segment windowing is inactive (whole-trace path). | Automatic; visible during windowed loading. `[data-status-progress]`. | `status-bar.ts` (`segmentProgress`) |
| X13. Copy-link button (S3 surface) | The T19 copy-link control copies the current view URL (a `beforeCopyLink` seam flushes any pending view-state write; the viewer's view-state URL sync itself is T19's clause). | Click "Copy link". | `src/lib/url/copy-link.ts` (`mountCopyLink`) via `status-bar.ts` |
| X14. Key hints | Persistent hint text: `/ search · n/p POI · f fit · z undo zoom · g goto · ? help`. | Always visible. | `status-bar.ts` (`KEY_HINTS`) |

---

## 2026-07-08 refresh (drift commits #596/#600; anchor re-derivation)

Method: code read + Node-level checks + local unit tests; dev-server on :3001
for backend facts. NO browser driver this pass, so DOM verdicts are CODE-READ
(re-derivable by the T12 row-walker). Local `node` runs (all green):
`test_fetch_traces.js`, `test_parse_yield_throttle.js`,
`test_runtime_groups.js`, `test_heatmap.js`.

Anchor re-derivation: the file's original anchors were found to match commit
`1f257f1` (#564), not the pre-drift tip `544afd2` (#581) - only `viewer.html`
differs between those trees, so this affected viewer.html anchors only.
Anchors were remapped `1f257f1 -> HEAD` from the git diff; anchors whose code
changed, plus ALL `trace_parser.js` anchors (#582 reindented the file), were
re-derived by hand. Spot-checks against HEAD: `esc()` 1019-1021, `LABEL_W`/
`LANE_H` 1394-1395, `addLegend` 6646, sidebar Escape branch 6282-6287, POI
`<select>` 853-859, `eventDetailHtml` 3976, `relatedHtml` 4044,
`showStackPopup` 5455, `showSchedPanel` 6008, `showIdleTimeFlamegraph` 6765,
`showHeapFlamegraph` 6825, `showFlamegraph` 6989 - all land on their cited
functions.

| Row | Verdict | Evidence / note |
|---|---|---|
| B8 (amended) multi-trace labels | CODE-READ | labels at `viewer.html:1873,1877`; browser load not driven. |
| B12 (rewritten) stream-always | CODE-READ + unit-tested | `streamAndShowTrace`/`loadTraceFromUrl` read at `1678-1712,1855-1880`; `fetchTracesStream` behavior covered by `test_fetch_traces.js` (concat parity, order, concurrent dispatch, late-failure no-unhandled-rejection). |
| B18 paint throttle | VERIFIED (unit) | `test_parse_yield_throttle.js` green; `makePaintThrottle` at `trace_parser.js:1130-1148`. |
| G1/G13/G17/G18 (amended) | CODE-READ | grouped `buildLanes`, `workerAtClientY`, `scrollToWorkerLane` read at `2466-2558`; single-runtime rendering is the unchanged path. |
| G21 runtime headers | NOT-TRIGGERABLE (dev data) | demo-trace.bin has ZERO named runtimes (`trace.runtimeWorkers` empty Map; `computeRuntimeGroups` -> one inferred `main` group, 2 workers - checked via Node against the parser), so headers never render locally. Grouping logic unit-tested (`test_runtime_groups.js`). |
| G22 collapse/expand | NOT-TRIGGERABLE (dev data) | requires headers (G21); logic read at `2508-2513,5358-5359`. |
| G23 geometry hit-testing | CODE-READ | fast path == old arithmetic (exercised by any lane hover); grouped path NOT-TRIGGERABLE locally. |
| G24 auto-expand on nav | NOT-TRIGGERABLE (dev data) | requires a collapsed runtime; read at `2546-2558` + call sites. |
| F10 (amended) runtime dropdown | NOT-TRIGGERABLE (dev data) | same root cause; viewer passes `runtimeWorkers` only in the CPU-flamegraph path (`7040-7044`). |
| W5/W6 (amended) | CODE-READ | module exports confirmed by grep + unit tests above. |
