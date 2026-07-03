# UI Feature Inventory: `viewer.html` (Trace timeline viewer, panels, sidebar & flamegraph)

> Companion to `01-index-html.md`. Purpose: capture every existing functionality of the trace viewer precisely enough that (a) each one can be validated in the running UI and (b) it can be re-implemented without losing anything. Derived from the code; source refs are `file:line` snapshots whose anchor is the function/handler name.

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
| A6. Click-to-focus main-area | Clicking lanes/timeline focuses `#main-area` (`role=application`, `tabindex=0`), enabling arrow/`?`/`Esc` keyboard nav; Escape also refocuses it. | Click lanes/timeline. | `viewer.html:853`, `6132` |
| A7. Focus-visible outline | `#main-area`, `#queue-chart`, `#btn-info` get a 2px `#6c63ff` outline (offset -2px) on keyboard focus only. | Tab to element. | `viewer.html:785-788` |
| A8. Enter/Space activation | Enter on a `role=button`/checkbox triggers its click and `preventDefault`; panel labels toggle on Enter/Space. | Tab to control, press Enter/Space. | `viewer.html:6049-6054` |
| A9. Coalesced full render | `scheduleRenderAll()` debounces `renderAll()` into a single animation frame; used by pan/hover to avoid render backlog. | Internal; automatic. | `viewer.html:2654-2660` (`scheduleRenderAll`) |
| A10. Crosshair RAF throttle | `scheduleCrosshairRedraw()` coalesces crosshair redraws via `requestAnimationFrame` on a separate raf id from the full-render throttle. | Internal; automatic on mousemove/scroll. | `viewer.html:6193-6201` |
| A11. High-DPI rendering | All canvases scale internal resolution by `devicePixelRatio` (`ctx.scale(dpr,dpr)`), CSS size unchanged, for crisp Retina/4K output. | Automatic. | `viewer.html:2739-2745`, `4777-4784` |
| A12. Scrollbar-width compensation | `scrollbarW = lanesContainer.offsetWidth - clientWidth` subtracted from draw width so panels stay aligned when a scrollbar appears/disappears. | Automatic in zoom/pan/crosshair math. | `viewer.html:2587`, `4159`, `4793`, `6186` |
| A13. Time-panel layout invariant | `LABEL_W`=100px gutter + `drawW` + scrollbar, computed by `makeTimePanelLayout()`/`timePanelLayout()` so timeline, lanes, span, CE, CPU, queue, task-detail axes line up vertically; worker lanes use DOM flex, other panels use an internal `LABEL_W` offset. | Internal; used by every time-based render. | `viewer.html:1331-1354`, `2703-2725`; `panel_layout.js:44-66` |
| A14. Render profiler | `?prof=1` (or `window.D9PROF=1`) logs per-panel render timings; lane render also tracks poll count / fillRect calls. | URL `?prof=1` or console. | `viewer.html:1876-1877`, `2561-2570` |
| A15. Window-resize reflow | On resize, if a trace is loaded, re-runs `renderAll()`; if a flamegraph is active, calls `fgInstance.resize()` to refit both canvases. | Resize browser window. | `viewer.html:6488-6490`, `7049-7052`; `flamegraph.js:842-845` |
| A16. ARIA live region | Off-screen `aria-live=polite` div announces keyboard-selection start/complete/cancel and zoom confirmations via `announce()`. | Screen reader only. | `viewer.html:974`, `5056-5058` |
| A17. HTML escaping | `esc()` escapes `& < > "` for all user-controlled text injected into HTML (frames, fields, sample keys). | Automatic. | `viewer.html:988-990` |
| A18. Stack-frame renderer + docs.rs links | `renderFrame()`/`formatFrame()` shortens Rust symbols (trait-impl collapse, generic stripping), appends `file:line`, and wraps in a docs.rs source link when the location matches a crate registry path. | Internal; used in tooltips, popups, sched panel. | `viewer.html:993-997`; `trace_parser.js:1397-1461` |

---

## B. File loading & drop zone

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| B1. Drop zone | Primary upload target shown when no trace is loaded: emoji, "Drop a .bin or .bin.gz trace file here or click to open", "Expects D9TF binary format", and a demo link. Dashed `#444` border turns purple `#6c63ff` on hover/dragover. | Initial page; reappears on reset/error. | `viewer.html:792-812`, `39-62` |
| B2. Drop-zone click -> file picker | Clicking anywhere in the drop zone (except children) opens the hidden `<input type=file accept=".bin,.gz">` dialog; picking a file calls `loadFile(files[0])`. | Click drop zone. | `viewer.html:811`, `1459`, `1501-1503` |
| B3. Window drag-and-drop | Document-level drag handlers route file drops to `loadFile()`; only file drags accepted (`dataTransfer.types` includes `Files`); a `dragCounter` prevents flicker across nested enter/leave; first file only is loaded. | Drag `.bin`/`.bin.gz` onto window. | `viewer.html:1461-1499` |
| B4. Drag feedback (no trace) | With no trace loaded, dragover adds `.dragover` to the drop zone (purple border/text/tint). | Drag file with drop zone showing. | `viewer.html:54-59`, `1465-1490` |
| B5. Drag feedback (replace) | With a trace already loaded, a fullscreen `#drag-overlay` (inset 20px, dark tint, dashed purple border, blur, `z-index:300`) shows "Drop trace file to load / Replaces the current trace". | Drag file over loaded trace. | `viewer.html:64-89`, `792-797`, `1468-1474` |
| B6. Demo trace link | "or load demo trace" fetches `demo-trace.bin` via `loadTraceFromUrl()` (`local` perf mode); re-created and re-wired each time the drop zone resets. | Click "or load demo trace". | `viewer.html:808`, `1906-1912`, `1786-1804` |
| B7. Loading state | Enters loading view (drop zone shows spinner + label + live elapsed timer + "Back" button + "or press Escape" hint); `showLoadingState`. Spinner (`will-change:transform`) stays smooth while the main thread parses. | Automatic after drop/pick/demo. | `viewer.html:91-106`, `1744-1773` |
| B8. Loading progress text | `updateLoadProgress()` shows `Decompressing...`, `Fetching...`, `Parsing: N% / X MB - Yk events`, `Analyzing N events...` depending on stream vs buffered mode. | Visible during load. | `viewer.html:1550-1570`, `1775-1778` |
| B9. Loading elapsed timer | Wall-clock ` - X.Xs` updates every 250ms via `startLoadTimer`; frozen (`stopLoadTimer`) when the viewer shows, so it excludes the synchronous analysis phase. | Visible during load. | `viewer.html:1700-1742` |
| B10. Cancel load (Escape) | While `loadAbortController` is active, Escape calls `cancelLoad()`: aborts in-flight fetch, stops the timer, resets to drop zone; `AbortError` is swallowed silently. | Press Escape during load. | `viewer.html:1780-1784`, `6058-6061` |
| B11. Cancel load (Back button) | "Back" button in loading view calls `cancelLoad()` (same as Escape). | Click "Back". | `viewer.html:1765-1769` |
| B12. Stream vs buffered path | Single URL + `TraceParser.canStreamDecode()` -> `fetchTraceStream`+`parseTraceStream` (overlapped, label "Loading...", `mode=stream`, captured chunks reassembled for re-parse); else `fetchTraces`+`parseTrace` (label "Fetching...", `mode=buffered`). | Automatic by URL count/support. | `viewer.html:1806-1851`, `1645-1674` |
| B13. Load errors | Parse failure -> `alert("Error: ...")` + reset. URL `HTTP 401` with `Dial9Creds` present but no creds -> credentials-hint alert; other network errors -> "Error loading trace from URL: ...". `processTrace` finding no usable data -> alert + reset. | On failed load. | `viewer.html:1631-1635`, `1838-1850`, `1917-1920` |
| B14. In-memory re-parse | Set/Clear Range re-parse the retained `currentTraceBuffer` with a new time filter (no re-fetch), `mode=reparse`; preserves URL range unless replacing. | Set Range / Clear Range buttons. | `viewer.html:1512-1515`, `1854-1870` |
| B15. New File / reset | "New File" clears all state (selection, correlation, buffer/name), hides viewer, clears URL range, and returns to the drop zone via `resetTraceState`+`resetDropZone`. | Toolbar "New File". | `viewer.html:844`, `1504-1535` |
| B16. Load-perf record | `loadPerf` tracks `startMs/fetchDoneMs/parseDoneMs/totalMs`, `mode` (`local/stream/buffered/reparse`), `events`, `bytes`; `totalMs` finalized via double-rAF after layout; logs `loaded in X.Xs`. | Internal; surfaced via Parse-perf popup (D9). | `viewer.html:1703-1762`, `1568-1621` |
| B17. Credential header injection | Before `fetchTraceStream`/`fetchTraces`, if `window.Dial9Creds` exists, spreads `Dial9Creds.headers()` into fetch options (same-origin only). | Automatic if creds module present. | `viewer.html:1647`, `1828`; `creds.js:240-250` |

---

## C. Toolbar: file info & Points-of-Interest navigation

Toolbar is a two-row flex column (`#toolbar`, `#toolbar-row-data` + `#toolbar-row-view`), dark bg `#16213e`, `flex-shrink:0`; shared button/select styling (dark bg, 1px `#444`, 4px radius, hover brighten, `:disabled` opacity 0.4). Source: `viewer.html:214-288`, `815-851`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| C1. File info display | Shows filename (with a `v15` version suffix) or, in structured-metadata mode, service/host/time range; plus event count, worker count, duration, truncation/filter notes, and an inline ` - loaded in X.Xs` suffix appended after render. Filename ellipsized past its max width. | Toolbar row 1, top-left. | `viewer.html:817-820`, `2086-2122` (`showViewer`), `5574-5578` (`updateInlineLoadTime`) |
| C2. POI filter dropdown | 5 options: Kernel Scheduling Delays, Long Polls (>1ms), Polls with CPU Samples, Wake->Poll Delays (>100us), Uninstrumented Polls. Change recomputes the POI list via `filterPointsOfInterest` and auto-jumps to the first match. | Toolbar `#poi-filter`. | `viewer.html:822-828`, `4870`, `2314-2326`; `trace_analysis.js:875-970` |
| C3. Worst-first checkbox | Checked (default) sorts POIs by descending severity/value; unchecked sorts chronologically. Toggling re-filters and auto-jumps. | Toolbar `#sort-by-worst`. | `viewer.html:829-832`, `4871` |
| C4. Prev POI button | Jumps to previous POI; if none selected (index -1) and list non-empty, jumps to index 0; disabled when `currentPoiIndex <= 0` or list empty. | Toolbar `#btn-prev-poi` ("Prev"). | `viewer.html:833`, `4884-4886`, `2328-2343` |
| C5. Next POI button | Jumps to next POI; if none selected, jumps to index 0; disabled at end of list or empty. | Toolbar `#btn-next-poi` ("Next"). | `viewer.html:834`, `4888-4890`, `2328-2343` |
| C6. POI counter | Shows `N/Total` (or `0/Total` when none selected); "None found" when the filter matches zero. Read-only. | Toolbar `#poi-counter`. | `viewer.html:835`, `2328-2343` |
| C7. Jump-to-POI behavior | Centers the viewport on the POI: `viewDur = max(spanDur*5, 1ms)`, 30% left padding; for wake-delay POIs uses the full wake->poll window (`~3x`, 20% pad) and selects the task; scrolls the worker lane into view (`scrollTop = laneIdx*LANE_H`); highlights the current POI span red (`#ff4444`, white 2px stroke). | Prev/Next click or filter change. | `viewer.html:2345-2374`, `3017-3029` |
| C8. Initial POI setup | On load `updatePointsOfInterest()` runs with `autoJump:false` (computes list, no jump), so the overview view is preserved until the user interacts. | Automatic post-load. | `viewer.html:2125` |

---

## D. Toolbar: analysis buttons & popups

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| D1. Blocking Calls button | Opens the scheduling/blocking-calls sidebar (all sched samples, no range). `CONDITIONAL`: `display:none` unless the trace has scheduling samples (`hasSchedEvents`). Red/orange styling. | Toolbar `#btn-sched-panel` ("Blocking Calls"). | `viewer.html:837`, `4872`, `2264`, `5849-5892` |
| D2. CPU Flamegraph button | Opens the whole-trace CPU flamegraph (`showFlamegraph(minTs,maxTs)`); label shows sample count. `CONDITIONAL` on `hasCpuProfileSamples`. Purple styling. | Toolbar `#btn-cpu-flamegraph` ("Flamegraph (N)"). | `viewer.html:838`, `4874-4877`, `2284-2290`, `6835-6895` |
| D3. Heap Flamegraph button | Opens the heap-allocation flamegraph (`showHeapFlamegraph()`); label shows alloc-event count. `CONDITIONAL` on `hasAllocEvents`. Green styling. | Toolbar `#btn-heap-flamegraph` ("Heap (N)"). | `viewer.html:839`, `4873`, `2276-2282`, `6671-6830` |
| D4. Uninstrumented info button | Toggles a popup listing tasks lacking wake tracking (raw `tokio::spawn`). `CONDITIONAL` on `uninstrumentedCount > 0`; label updates to "Uninstrumented (N)" (initial markup reads "Blind spawns"). Blue styling. | Toolbar `#btn-uninstrumented-info`. | `viewer.html:840`, `4878-4880`, `2267-2274`, `5486-5549` |
| D5. Uninstrumented popup | Fixed popup below the button: header "N uninstrumented task(s) at M site(s)"; hint text linking to `TelemetryHandle::spawn` docs and the "Uninstrumented Polls" filter; sites grouped and sorted by count desc, each an auto-generated docs.rs link when the path matches a crate registry pattern, else plain text. Toggles off on repeat click; closes on `x`, click-outside, or Escape. | Click button; `x`/outside/Escape to close. | `viewer.html:926`, `5486-5558` |
| D6. Parse-perf button | Toggles a popup with the fetch/parse/analysis timing breakdown. `CONDITIONAL` on `loadPerf` (after successful load). Blue styling. | Toolbar `#btn-parse-perf` ("Parse perf"). | `viewer.html:841`, `4881-4882`, `2296-2297`, `5583-5695` |
| D7. Parse-perf popup content | "Load breakdown" with Mode (streamed/buffered/local/reparse + note), Total, mode-specific Fetch/Parse rows, Analysis+render, and optional throughput (events/s, MB/s). Uses a provisional `performance.now()-startMs` total if opened before finalization; stream-mode note explains the combined figure. | Opened by D6; `x`/outside/Escape to close. | `viewer.html:927`, `5583-5695`, `5591-5647` |
| D8. Popup positioning | Both popups: `position:fixed`, placed 4px below their button, right-aligned to the button's right edge, `z-index:200`, scrollable overflow. | Automatic on open. | `viewer.html:5540-5544`, `5676-5681` |
| D9. Popup toggle + global Escape order | Repeat button click closes; a global keydown closes popups on Escape in order (help -> uninstrumented -> parse-perf -> stack sidebar), then clears task selection and refocuses main-area. | Escape / repeat click. | `viewer.html:5490-5494`, `5586-5590`, `6115-6134` |

---

## E. Toolbar: time display & range filter

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| E1. Time mode toggle | Toggles `useAbsoluteTime` between relative (offset from trace start, `+` prefix) and absolute (wall-clock via clock-sync anchors, falling back to relative if none). Reveals/hides the TZ button; re-renders all timestamps. | Toolbar row 2 `#btn-time-mode` ("Time: Relative/Absolute"). | `viewer.html:848`, `4919-4926`, `1199-1225` (`fmtTs`/`fmtWallClock`) |
| E2. Timezone toggle | Toggles `useLocalTz` (UTC vs local) for absolute timestamps only; hidden unless in absolute mode; re-renders. | Toolbar row 2 `#btn-tz-mode` ("TZ: UTC/Local"). | `viewer.html:849`, `4927-4931` |
| E3. Set Range | Captures the current viewport (`viewStart`/`viewEnd`, rounded) as a time filter, updates URL `?start`/`?end`, and re-parses the retained buffer to only events in range; reveals Clear Range. | Toolbar `#btn-set-range`. | `viewer.html:842`, `4912-4913`, `1854-1870` |
| E4. Clear Range | Re-parses the full trace (`reparseWithRange(null,null)`), removing `start`/`end` URL params. `display:none` until a range is active (set, or present in URL on load). | Toolbar `#btn-clear-range`. | `viewer.html:843`, `4915-4917`, `1869` |
| E5. URL `start`/`end` params | `?start=<ns>&end=<ns>` (either optional) filter the trace at parse time (inclusive on both ends; uncapped event types kept for structural integrity). If present on load, Clear Range shows immediately. Managed via `history.replaceState` (no reload). | URL query params. | `viewer.html:1879-1899`, `4916-4917`; `trace_parser.js:570-579` |

---

## F. Timeline header (time axis)

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. Time axis ruler | Fixed 30px canvas drawing tick marks + `fmtTs`-formatted labels. Interval auto-picked from a nice-values list (`1e3..1e10` ns) targeting ~4-16 ticks (`max(4, floor(drawW/100))`); ticks offset by `LABEL_W` to align with lanes. Non-interactive; redraws on zoom/pan/resize. | Above the lanes (`#timeline-canvas`). | `viewer.html:856-857`, `2736-2776` (`renderTimeline`) |
| F2. Coordinate transform | `nsToX(ns, drawW)` maps timestamp to pixel (relative to draw area, no `LABEL_W`, used by lane-style canvases); `makeTimePanelLayout` variants add the `LABEL_W` offset for panels. | Internal. | `viewer.html:2662-2664` |

---

## G. Worker lanes

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| G1. Lane grid | One 60px `.lane` per worker in a scrollable `#lanes-container` (`overflow-y:auto`, `overflow-x:hidden`); each lane = 100px `Worker N` label (`z-index:1`) + flex canvas (`worker-<id>-canvas`), 1px bottom border. Built by `buildLanes` on trace load. | Main area. | `viewer.html:2378-2397` (`buildLanes`), `669-695`, `374-379` |
| G2. Lane background states | Active periods dark `#1a1e2a`; parks reddish-brown `#2a1520` with orange accent; scheduling delay bright red; `block_in_place` handoff gaps drawn as `#3a2a00` fill with orange dashed diagonal hatching. | Visual per lane. | `viewer.html:2814-2915` (`renderLane`) |
| G3. CPU scheduling tint | When `trace.hasCpuTime`, active periods tint green (>=95% on-CPU), yellow (50-95%), red (<50%); `pixelDownsampleSpans` picks one representative per pixel column (longest wins). | Visual; hover shows on-CPU %. | `viewer.html:2828-2846`; `trace_analysis.js:337-360` |
| G4. Poll bars | Polls drawn in a center band (y 10-30) with a duration heatmap color (log scale navy->cyan->orange->red, 24 quantized bins) via `pollHeatmapColorQuantized`; `pixelDownsampleSpans` + `makeBarCoalescer` collapse millions of polls to a few fillRects. | Visual. | `viewer.html:2918-2984`; `trace_analysis.js:229-250`, `280-305` |
| G5. Open-ended poll marker | Polls still running at trace end / `block_in_place` boundary get an orange dashed right edge. | Visual. | `viewer.html:2985-2999` |
| G6. Selected-task highlight | With a task selected, its polls render solid yellow `#ffeb3b` (drawn over a dimmed pass where non-selected polls use `pollColorDim`, RGB*0.4 cached). | Click a poll. | `viewer.html:2974-2983`, `1135-1147` |
| G7. Selected-span highlight | With a span focused, polls containing a segment of that span (or its selected ancestors) get a yellow 2px outline. | Focus a span. | `viewer.html:3084-3112` |
| G8. Hovered-waker highlight | Hovering a waker label in task detail highlights that waker task's polls orange `#ff8a65` (`hoveredWakerTaskId`). | Hover waker label. | `viewer.html:3071-3082` |
| G9. CPU sample ticks | Small magenta ticks below the poll band mark CPU-sample timestamps. | Visual (when CPU profiling on). | `viewer.html:3034-3050` |
| G10. Sched event triangles | Red downward triangles below the band mark polls with blocking sched events. | Visual. | `viewer.html:3053-3069` |
| G11. Wake markers | With a task selected, green downward triangles at lane top mark where that task was woken. | Select task. | `viewer.html:3114-3132` |
| G12. Local-queue step line | Orange step line (`rgba(255,200,50,.8)`) below the band traces local queue depth, scaled to the shared visible max, with a `q:N` label. | Visual. | `viewer.html:3135-3180` |
| G13. Lane click -> select task | Click (non-drag, within lanes/draw area) finds the poll at the timestamp; if it has a `taskId`, sets `selectedTaskId` (yellow highlight across all lanes) and shows the task-detail panel; clicking the same task again toggles it off; clears any custom-event marker. Clicks in the label gutter or outside valid lanes clear selection. | Click a poll. | `viewer.html:5193-5294` |
| G14. Lane click -> span auto-focus | The same click walks up the span tree to the outermost ancestor containing the click timestamp on that worker, focusing it (and its ancestor chain, cycle-guarded at 1024) in the span panel. | Click a poll. | `viewer.html:5242-5280` |
| G15. Stack popup on poll click | If the clicked poll has CPU or sched samples, `showStackPopup` opens the Poll Detail sidebar near the click; otherwise `hideStackPopup`. | Click poll with samples. | `viewer.html:5221-5227` |
| G16. Lane hover tooltip | Rich tooltip: worker id, timestamp, state (Active/Parked/block_in_place/Polling) with on-CPU %/park/poll durations, kernel sched delay, task id + spawn location, CPU/sched sample counts ("click to view"), span count + names, global/local queue depths, active-task count, current span detail + parent. Cursor becomes `pointer` over a clickable stack. | Hover a lane. | `viewer.html:6203-6410` |
| G17. Vertical scroll sync | Scrolling `#lanes-container` re-renders the crosshair so it stays aligned; `laneIdx = floor((mouseY + scrollTop)/LANE_H)`. | Scroll lanes. | `viewer.html:4963-4965` |
| G18. Auto-scroll to lane | Selecting a task / POI / filtered span / keyboard-nav target scrolls the worker's lane into view (`scrollTop = idx*LANE_H`). | Automatic on those actions. | `viewer.html:2229`, `2366-2370`, `6041-6042` |
| G19. Legend | Toolbar legend explains the poll heatmap gradient, parked, kernel sched delay, CPU-sampled, sched (blocking), wake, local-queue swatches; non-interactive. | Toolbar row. | `viewer.html:6492-6516` |
| G20. Legacy `selectedEvent` poll indicator | `selectedEvent` is cleared on any lane click but has no lane-render code. Status: `DEAD` (leftover from older design). | N/A. | `viewer.html:1259`, `5201` |

---

## H. Viewport navigation & region selection

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| H1. Zoom In button | `zoom(0.5)` centered at mid-view; min duration clamped to 100ns; clears toasts. | Viewport controls `#btn-zoom-in`. | `viewer.html:863-865`, `4895-4896`, `4934-4942` |
| H2. Zoom Out button | `zoom(2)` centered at mid-view; clamped to `[minTs,maxTs]`. | Viewport controls `#btn-zoom-out`. | `viewer.html:866-868`, `4898-4899` |
| H3. Fit All button | Resets `viewStart=minTs`, `viewEnd=maxTs`. | Viewport controls `#btn-fit`. | `viewer.html:870-872`, `4900-4904` |
| H4. Viewport controls panel | Floating panel bottom-right of main-area (`right:16px bottom:16px`, `z-index:50`, glass blur bg `rgba(22,33,62,.92)`), 3 buttons + separator; button clicks `stopPropagation` so they don't trigger poll clicks. | Bottom-right of timeline. | `viewer.html:319-356`, `862-873`, `4907-4909` |
| H5. Ctrl/Cmd+wheel zoom | Wheel with Ctrl (or Cmd on Mac) zooms centered on the cursor (`factor 1.3`, up=in, down=out); `preventDefault`. Plain wheel scrolls lanes vertically. | Ctrl/Cmd+scroll over lanes. | `viewer.html:4945-4960` |
| H6. Drag pan | Left-drag (no modifier), >3px, pans the view; cursor `grabbing`; clamped to `[minTs,maxTs]`; RAF-throttled render during drag. | Click-drag lanes. | `viewer.html:4989-5026`, `5115-5146` |
| H7. Shift+drag region select | Shift-drag draws a blue overlay (`rgba(66,133,244,.15)`), and on release (>3px, >=100ns) opens a panel by data present in the range: sched-only -> blocking calls, heap-only -> heap flamegraph, else CPU flamegraph. Shows an error toast if the trace has no CPU samples. | Hold Shift, drag lanes. | `viewer.html:4993-5018`, `5147-5190` |
| H8. Alt/Option+drag zoom | Alt-drag draws a teal overlay (`rgba(0,188,180,.15)`) and on release zooms the view to the selection. | Hold Alt/Option, drag lanes. | `viewer.html:4997-5018`, `5155-5161` |
| H9. Keyboard Shift/Alt selection | Pressing Shift (region) or Alt (zoom) starts a keyboard selection: cursor at mouse position (if in view) else view center; arrow keys extend by 5% of view; Shift/Alt again or Enter confirms; Escape cancels; announced via ARIA. Blocked if the sidebar already holds a retained range. | Press Shift/Alt (no drag). | `viewer.html:6069-6110`, `5069-5113` |
| H10. Selection overlay | `#selection-overlay` div spans full main-area height during shift/alt drag or keyboard selection; blue for shift, teal for alt (`setSelOverlayColor`); `pointer-events:none`; cleared on mouseup/Escape. Shift selections are owned by the sidebar and persist until it closes. | Visible during selection. | `viewer.html:697-704`, `4978-4987`, `5028-5054` |
| H11. Arrow zoom | Arrow Up = `zoom(0.5)` (in), Arrow Down = `zoom(2)` (out); requires a loaded trace; `preventDefault`. | Up/Down arrows. | `viewer.html:6135-6142` |
| H12. Arrow pan | Arrow Left/Right pan by 20% of view duration, clamped to `[minTs,maxTs]`, preserving duration. | Left/Right arrows. | `viewer.html:6143-6166` |
| H13. Set/Clear Range (nav) | See E3/E4 - viewport-derived time filter reparse. | Toolbar buttons. | `viewer.html:4912-4915` |

---

## I. Crosshair & selection overlays

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| I1. Crosshair-overlay canvas | Fullscreen `position:absolute`, `pointer-events:none`, `z-index:10` canvas rendering all crosshairs and the selected-event marker; redraws on mousemove/scroll/zoom. | `#crosshair-overlay`. | `viewer.html:855`, `4774-4867` (`renderCrosshair`) |
| I2. Mouse crosshair | Dashed white vertical line (`rgba(255,255,255,.3)`, 4px dash) at the mouse timestamp; hidden when `mouseNs` is outside `[viewStart,viewEnd]` or during drag. | Hover lanes/panels. | `viewer.html:4796-4807` |
| I3. Keyboard-selection cursor | Solid bright line (`rgba(255,255,255,.8)`, 1.5px) at `kbCursorNs` during keyboard Shift/Alt selection. | During keyboard selection. | `viewer.html:4809-4818` |
| I4. Custom-event hover guide | Orange dashed line (`rgba(255,140,0,.4)`, 3px dash) across all lanes at a hovered custom event's timestamp (`hoverEventTs`); cleared on mouseleave. | Hover a custom-event tick. | `viewer.html:4823-4834` |
| I5. Selected-event marker | Persistent orange dashed line (`rgba(255,140,0,.9)`) + label chip (`name @ HH:MM:SS.mmm`) at the pinned event's timestamp; clamped inside the viewport; cleared on deselect/Escape. | Click a custom-event tick. | `viewer.html:4836-4866` |
| I6. Queue info panel | Top-right `#info-panel` shows `Global Q / Local max / Active tasks` at the mouse position (via `updateInfoPanel`), cleared outside the view range. | Hover over lanes. | `viewer.html:656-667`, `4739-4772` |

---

## J. Span panel & span filtering

Foldable panel (`#span-panel`, `data-panel-key=spans`, 120px expanded / 24px collapsed, initially collapsed). Sources include `viewer.html:390-398`, `891-894`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| J1. Span canvas | Renders visible spans as clustered bars (6px x-grid, log-scale duration y-position, cluster height scaled by `log2(size)`), with darker active-time segments; hidden when collapsed. | Expand Spans panel. | `viewer.html:3214-3374` (`renderSpanPanel`) |
| J2. Span focus (click) | Click a span/cluster to focus it: pinned to top (y=4, ~6x height), descendants below (y=34, ~3x), non-focused dimmed to 0.08 alpha; selects the task polling at span start; info shows percentile stats; click same span again or empty area clears (`clearSpanTaskSelection`). | Click a span bar. | `viewer.html:3679-3731`, `4029-4034` |
| J3. Span label / metadata | Left label area (184px, scrollable) shows the focused span name + key/value field rows (unit-aware via `formatFieldValue`), each with a copy button; shows "Spans" when none focused. | Left of panel when expanded. | `viewer.html:892`, `3197-3212`, `440-474` |
| J4. Metadata copy button | Clipboard button per k/v row copies the value, flashing a checkmark for 800ms; clicks on it do not toggle the panel fold. | Click copy button. | `viewer.html:461-474`, `3800-3807` (`copyFromKvButton`) |
| J5. Span info text | Top-right shows `N spans - M clusters` and, when focused, `name: duration (P%% of N) P50=.. P99=..`. Non-interactive. | Top-right of panel. | `viewer.html:893`, `3287-3288`, `3713-3724` |
| J6. Span hover tooltip | Single span: name, duration + percentile rank, active/idle, poll count, worker ids, fields. Cluster: size, top names, min-max duration. Positioned near cursor, flips above if overflowing. | Hover span canvas. | `viewer.html:3621-3670` |
| J7. Span filter (text) | `#span-filter` filters spans by name or field key/value (case-insensitive substring); clear button appears when non-empty; rebuilds `filteredSpans`. | Toolbar `#span-filter`. | `viewer.html:876-877`, `2177-2188`, `1081-1099` (`spanMatchesFilter`) |
| J8. Percentile filter | `#span-pct-filter`: All / >=P50 / >=P90 / >=P95 / >=P99; shows only spans at/above that percentile of their name's duration distribution (cached via `getSpanDurations`). | `#span-pct-filter` dropdown. | `viewer.html:881-887`, `2190-2195`, `1103-1119` |
| J9. Span-name chips | One color chip per unique span name; click toggles inclusion in `selectedSpanNames` (active=filled, inactive=bordered); AND-combines with text + percentile filters. | Click chips in `#span-legend-items`. | `viewer.html:889`, `2134-2156` |
| J10. Clear-names button | Deselects all span-name chips; visible only when >=1 selected. | `#btn-span-clear-names`. | `viewer.html:888`, `2169-2174` |
| J11. Filtered span nav | `#btn-span-prev`/`#btn-span-next` jump through `filteredSpans` (sorted by time, wraps), centering the view (~5-10x span, min 1ms), scrolling the lane, and highlighting the span + ancestors; disabled when no filter/matches. | Prev/Next span buttons. | `viewer.html:878-879`, `2220-2252` |
| J12. Filter count | `#span-filter-count` shows `N matches` or `M/N` during navigation; empty when no filter active. | Read-only. | `viewer.html:880`, `2215`, `2242` |
| J13. Unmatched-spans warning | "N unmatched" (red) when spans have an enter but no exit (trace ended mid-span / segment rotated); hover tooltip explains. | Below legend chips. | `viewer.html:2158-2168` |
| J14. Span legend bar visibility | The whole `#span-legend` bar (filter input, buttons, dropdown, chips) shows only when the Spans panel is expanded, not CSS-hidden, and `allSpans.length > 0`. | Automatic. | `viewer.html:875`, `1403-1405`, `2175` |

---

## K. Custom events panel

Foldable panel (`#custom-events-panel`, `data-panel-key=events`, 40px expanded / 24px collapsed, hidden unless `genericCustomEvents.length > 0`). Sources: `viewer.html:586-607`, `900-903`, `2069-2073`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| K1. Event tick canvas | Renders visible events as colored ticks clustered per pixel column; tick width `max(3, min(3+log2(size)*2, 10))`; alpha `min(0.4+size*0.15, 1)`, unrelated ticks fade to 20% when a task is selected; mixed-name clusters show a secondary-color bottom stripe. Hit regions padded to >=12px. | Expand Events panel. | `viewer.html:3377-3473` (`renderCustomEventsPanel`) |
| K2. Panel info | Top-right `#ce-panel-info` shows `N events - M markers`. | Top-right of panel. | `viewer.html:902`, `3421` |
| K3. Hover tooltip + guide | Hover shows event fields, timestamp, and task line ("click to select/inspect"); cursor becomes `pointer`; draws the orange guide crosshair (I4). | Hover a tick. | `viewer.html:4057-4102` |
| K4. Click to select event | Click a tick pins the selected-event marker (I5), populates the sidebar Event/Related tabs, and (if it resolves to a task) selects that task's poll; repeat click on the same tick toggles off. | Click a tick. | `viewer.html:4108-4152` |
| K5. Name filter chips | `#ce-legend-items` chips per unique event name; click toggles `selectedCENames` (active=filled); filters the canvas. | Click chips. | `viewer.html:2042-2062` |
| K6. Clear-names button | Clears all event-name filters; visible only when >=1 active. | `#btn-ce-clear-names`. | `viewer.html:897`, `2064-2068` |
| K7. Task resolution | `taskForEvent`/`pollForEvent` resolve an event to a task/poll via `task_id`, then `worker_id`+timestamp, then unambiguous scan; results cached (WeakMap). | Internal (highlight/click). | `viewer.html:2476-2536` |
| K8. Inline tick label | No per-tick text label is rendered (names only appear in tooltip/sidebar/chips). Status: `DEAD` (not implemented). | N/A. | `viewer.html:3377-3473` |

---

## L. CPU usage panel

Foldable panel (`#cpu-panel`, `data-panel-key=cpu`, 92px expanded / 24px collapsed, hidden unless `processCpuUsage.intervals.length > 0`). Sources: `viewer.html:608-629`, `905-907`, `1972-1973`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| L1. CPU stacked-area chart | Draws avg-cores-over-time bars, bg `#111b2e`, y-axis `0..max(1, max cores, available parallelism)`, grid at 25/50/75%; bars colored by load (blue-ish low -> pink/red high); a dashed orange capacity line (`rgba(255,207,153,.65)` + "N core capacity") when parallelism is known. | Expand CPU Usage panel. | `viewer.html:3505-3583` |
| L2. Info label | Top-right shows `avg X cores [ - avg Y%] - max Z cores` for the visible window (percent only if parallelism known; non-finite shows `-`). | Top-right of panel. | `viewer.html:906`, `3488-3515` |
| L3. Hover tooltip | Over an interval: Window/CPU-time durations, Cores, optional Total CPU %; cursor `crosshair`; binary-search interval lookup (`findProcessCpuIntervalAt`). | Hover CPU chart. | `viewer.html:4154-4182`, `3585-3607` |
| L4. Crosshair sync | Hovering updates global `mouseNs` and redraws the crosshair; the panel owns its own tooltip and suppresses the lanes tooltip. | Hover CPU chart. | `viewer.html:6228-6232` |
| L5. Data source | Built by `buildProcessCpuUsageSeries` from `ProcessResourceUsageEvent` custom events (user/system CPU deltas); `available_parallelism` from `segmentMetadata('process.available_parallelism')`. | Internal on load. | `viewer.html:1970-1971`; `trace_analysis.js:85-148` |
| L6. Click handler | None wired on the CPU canvas (mousemove/mouseleave only). Status: `DEAD` (no click affordance). | N/A. | `viewer.html:4154-4182` |

---

## M. Queue depth panel

Foldable panel (`#queue-chart`, `data-panel-key=queue`, `tabindex=0`, `aria-label="Queue depth chart"`, 120px expanded / 24px collapsed, initially collapsed). Sources: `viewer.html:639-667`, `915-920`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| M1. Global queue area | Blue filled step-area (`rgba(79,195,247,.3)` fill, `#4fc3f7` stroke) of max global queue depth per pixel bucket. | Expand panel. | `viewer.html:4660-4680` |
| M2. Max-local queue line | Orange step line (`#ff8a65`, 1px) of the max per-worker local queue per bucket. | Expand panel. | `viewer.html:4682-4701` |
| M3. Active-task line | Green step line (`#81c784`, 1.5px) of active task count on a separate right-side y-axis (`tasks:N`). `CONDITIONAL` on `activeTaskSamples.length > 0`. | Expand panel (when data). | `viewer.html:4703-4736` |
| M4. Y-axis labels | Left: max queue (top) and `0` (bottom), gray 10px monospace, right-aligned at `LABEL_W-6`; the max excludes the active-task scale. | Left of chart. | `viewer.html:4653-4658` |
| M5. Legend | Header legend swatches: Global (blue box), Max local (orange line), Active tasks (green line); hidden when collapsed. | Header when expanded. | `viewer.html:916-918` |
| M6. Hover info | See I6 (`#info-panel`) - Global Q / Local max / Active tasks at cursor. | Hover chart. | `viewer.html:4739-4772` |
| M7. Drag-select spawned tasks | Click-drag (>=3px, `crosshair` cursor, green overlay `rgba(129,199,132,.2)`) selects a time range; on release, finds tasks whose spawn time falls in range, groups by spawn location (sorted by count desc, 5 shown per group), and lists them in the sidebar with clickable hex task-id links and range duration. `CONDITIONAL` on `activeTaskSamples` + `taskFirstPoll` present and panel expanded. | Drag on queue canvas. | `viewer.html:6924-7050` |
| M8. Spawned-task link click | Clicking a listed task id sets `selectedTaskId`, closes the stack popup, and re-renders (task highlighted in lanes). | Click a spawned-task link. | `viewer.html:7038-7044` |
| M9. Expanded-panel click | Clicking non-label areas of the expanded panel does nothing (to avoid interfering with drag); only the label (or any click while collapsed) toggles. Status: intentional (`DEAD` for non-label expanded click). | N/A. | `viewer.html:1434-1439` |

---

## N. Task detail panel

Optional 160px panel (`#task-detail`), not foldable, shown only when a task is selected. Source: `viewer.html:381-389`, `910-914`, `4194-4549`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| N1. Panel visibility | Appears below the lanes/queue chart when a task is selected (has polls); hides on deselect. | Click a poll to select a task. | `viewer.html:4197-4210` |
| N2. Label header | Shows hex task id, spawn location, poll count, wake count (if instrumented), lifetime, completion mark, and status badges. | Top-left of panel. | `viewer.html:4233-4248` |
| N3. "no wake data" badge | Red badge (links to `TelemetryHandle::spawn` docs) for tasks spawned via raw `tokio::spawn` (uninstrumented). | Click badge. | `viewer.html:4240-4242`, `744-758` |
| N4. Idle flamegraph link | "idle flamegraph (N)" opens a time-weighted flamegraph of idle periods (weight = idle us). `CONDITIONAL` on the task having task dumps. | Click link. | `viewer.html:4243-4260`, `6611-6669` (`showIdleTimeFlamegraph`) |
| N5. Status tooltip | Hover updates top-right status text with an icon + description (polling / scheduled / idle); clears on mouseleave. | Hover canvas. | `viewer.html:912`, `6440-6455` |
| N6. Wake->poll delay bands | Colored bands (green <=100us, orange <=1ms, red >1ms) between wake and next poll, with a duration label (if >25px) and a green wake triangle. Wake matching uses binary search (`computePollWakes`, O(P logP)). | Visual. | `viewer.html:4314-4392`; `trace_analysis.js:822-863` |
| N7. Waker label | "<label>" under each delay band: "io" for runtime/worker wakes, spawn filename, or hex id; clickable when band >40px. | Below delay bands. | `viewer.html:4366-4391` |
| N8. Waker hover/click | Hovering a waker label highlights that waker's polls (G8) and re-renders; clicking selects the waker task. | Hover/click waker label. | `viewer.html:6421-6437`, `6461-6472` |
| N9. Task lifespan bar | Faint green bar from spawn to terminate with spawn/done edge lines + labels, when both timestamps exist. | Visual. | `viewer.html:4394-4424` |
| N10. Polling sections | Cyan bars for active execution; when polls > drawW, renders a per-pixel coverage histogram (opacity = fraction polling) instead of per-poll bars; per-poll duration labels when zoomed. | Visual; hover status. | `viewer.html:4426-4480` |
| N11. Idle gaps + stacks | Dark bands between polls; gaps with task dumps get a purple cross-hatch and dashed purple border; hover shows duration + (click for async stack); clicking one opens the captured async stack(s) as a sidebar flamegraph ("Waiting on - N captures"). | Click cross-hatched idle gap. | `viewer.html:4482-4540`, `6474-6485`, `6601-6609` |
| N12. Legend | Bottom-left canvas legend: "Task" label and "▲ = wake" marker note. | Bottom-left of canvas. | `viewer.html:4542-4548` |

---

## O. Foldable panel mechanics

Applies to spans / events / cpu / queue panels (Task Detail is NOT foldable). Sources: `viewer.html:399-427`, `1366-1441`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| O1. Collapse toggle (click) | Clicking `.chart-label` (or anywhere on a collapsed panel) toggles `is-collapsed`: 24px height, canvas hidden (`display:none !important`), label max-height 18px. Clicking `.kv-copy`, or a non-label area of an expanded panel, does not toggle. | Click panel label / collapsed body. | `viewer.html:1434-1439` |
| O2. Collapse toggle (keyboard) | Label has `role=button`, `tabindex=0`, `aria-expanded`; Enter/Space toggles when focused. | Tab to label, Enter/Space. | `viewer.html:1426-1432` |
| O3. Chevron indicator | CSS `::before` caret shows expanded vs collapsed state (`#6c63ff`, 0.85em, 7px right margin). | Visual first char of label. | `viewer.html:410-420` |
| O4. localStorage persistence | State stored under `dial9.viewer.panelCollapsed.<key>` (`collapsed`/`expanded`), with an in-memory `viewerStorageFallback` map when localStorage is unavailable; all four panels start `is-collapsed` and are synced on load. | Automatic. | `viewer.html:1370-1420` |
| O5. Legend sync | Collapsing Spans hides `#span-legend`; Events hides `#ce-legend`; each shows only if expanded, not display:none, and its data is non-empty. | Automatic. | `viewer.html:1403-1409` |
| O6. Render on toggle | `setPanelCollapsed()` calls `renderAll()` (unless `{redraw:false}`) so the layout is responsive. | Automatic. | `viewer.html:1416-1420` |
| O7. Expanded-only labels | `.panel-expanded-label` elements (queue legend items) are hidden when collapsed. | Automatic. | `viewer.html:425-427` |

---

## P. Stack sidebar & tabs

Right panel (`#stack-sidebar`, 640px / 50vw default, min 200px, max 92vw, hidden by default). Sources: `viewer.html:129-189`, `929-943`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| P1. Show/hide | Opens on poll click, event click, or region selection; the `x` close (`#stack-sidebar-close`), Escape, or `hideStackPopup()` closes it, clearing flamegraph/sched state, resetting range, and re-rendering; the flamegraph container returns to `#flamegraph-panel`. | Click x / Escape. | `viewer.html:5382-5402` |
| P2. Resize handle | 4px purple bar on the left edge; drag left widens / right narrows (`[200px, 92vw]`); cursor `col-resize`, body `user-select:none` during drag; RAF re-renders while dragging; on mouseup resizes the active flamegraph. | Drag left edge. | `viewer.html:142-156`, `5800-5843` |
| P3. Sidebar title | Context text: `Nms selected` (flamegraph/blocking/heap), event/cluster name (event), `Blocking Calls - N events`, `Waiting on - N captures`, etc.; ellipsized. | Header. | `viewer.html:166-172`, `5463-5465`, `6853` |
| P4. Tab families | Two mutually exclusive groups: Poll Detail (alone), and Event/Related (event) vs Flamegraph/Blocking/Heap (range). `showSidebarTabs(active)` sets `.active` and toggles `display` per data availability. | Click tab headers. | `viewer.html:936-942`, `5698-5752` |
| P5. Auto-narrow on event | On a fresh event open the sidebar narrows to `EVENT_DEFAULT_WIDTH`=350px; manual resizes persist. | Automatic. | `viewer.html:6561-6564` |
| P6. Auto-widen on flamegraph | On a fresh flamegraph/heap/blocking open the sidebar widens to `FLAMEGRAPH_DEFAULT_VW`=78vw; manual resizes persist. | Automatic. | `viewer.html:6553-6557` |
| P7. Body scrolling | `#stack-sidebar .sidebar-body` (`flex:1`, `overflow-y:auto`) scrolls; scroll position is preserved across Related re-renders (collapse/load-more). | Scroll body. | `viewer.html:183-189`, `4005-4012` |
| P8. Width persistence | The region intent to "persist width in storage" is not implemented; no localStorage write for sidebar width; each panel type re-applies its default on fresh open. Status: `DEAD`. | N/A. | `viewer.html:6554-6564` |

---

## Q. Event & Related detail (sidebar content)

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| Q1. Event tab | Single event: k/v field rows (unit-formatted timestamps/fields, `formatFieldValue`) with copy + correlation buttons, timestamp, task id. Cluster: count, top event types, timestamp range (no correlation). | Click event -> Event tab. | `viewer.html:937`, `3812-3841` (`eventDetailHtml`) |
| Q2. Copy button | Copies a field value, flashing a checkmark for 800ms (delegated on the sidebar body). | Click copy button. | `viewer.html:5406-5407`, `3800-3807` |
| Q3. Correlation button | Shown only on field values shared by multiple events; clicking sets `correlateField`, switches to Related, and shows a "Same field=value" section. | Click correlation button. | `viewer.html:3791-3796`, `5408-5413` |
| Q4. Related tab | `CONDITIONAL` on a single-event selection. Sections: field correlation (if set), enclosing spans (by depth), same span, same task, same type; each collapsible, with a windowed view (`RELATED_INITIAL`=5) and "load more" (`RELATED_STEP`=25). | Click Related tab. | `viewer.html:938`, `3880-4003` (`relatedHtml`) |
| Q5. Related section toggle | Clickable headers with a caret; empty/self-only sections default collapsed; user toggles persist in `relatedCollapsed` across selections (cleared on reset); hidden rows stay indexed. | Click a section header. | `viewer.html:3894-3906`, `5416-5423` |
| Q6. Load-more affordance | Per-section, per-direction "load N more earlier/later (M hidden)" reveals more rows by `RELATED_STEP`; disappears when exhausted; scroll preserved. | Click load-more. | `viewer.html:3924-3931`, `5426-5433` |
| Q7. Related row navigation | Each non-self row is clickable: span rows focus + center on the span; event rows pin + center + mark the event. The self row (`r-self`) is a non-navigable highlighted anchor. | Click a row. | `viewer.html:3910-3921`, `5436-5442` |
| Q8. Related empty message | Placeholder ("none", "task unresolved", "No event selected") in empty sections. | Automatic. | `viewer.html:574-579`, `3882`, `3991` |

---

## R. Poll detail & blocking-calls / scheduling panel

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| R1. Poll Detail tab | For a clicked poll: deduplicated blocking sched events (red) and CPU profile samples (orange), each grouped by leaf frame with count/percentage bars and expandable frames (>3 frames). Title `Poll <duration> - N CPU samples - N sched events`. | Click a poll with samples. | `viewer.html:936`, `5296-5379` (`showStackPopup`) |
| R2. Frame expand/collapse | Toggle in each group reveals/hides frames beyond the first 3 (text flips between collapse and "N more frames"). | Click the toggle. | `viewer.html:5333-5339`, `5362-5368` |
| R3. Blocking Calls tab/panel | Scheduling events (kernel deschedules during polls) for a range or the whole trace; opened via toolbar D1, region selection, or the tab. Title `Nms selected` or `Blocking Calls - N events`. | D1 / Shift-drag / tab. | `viewer.html:940`, `5849-5892` (`showSchedPanel`) |
| R4. Group-by dropdown | `#sched-group-by-sb`: "blocking call" (leaf frame, default) vs "full stack"; changing re-renders the panel. | Select in panel. | `viewer.html:5939-5943`, `6020-6027` |
| R5. Summary bar chart | Horizontal bars per blocking-call type (count, bar, name, %), color-coded (red lock/mutex, cyan epoll/poll, orange I/O, gray syscall, purple other). | Top of panel. | `viewer.html:5949-5967` |
| R6. Summary rows (click) | Rows have `cursor:pointer` + `data-leaf` but no click handler wired. Status: `DEAD`. | N/A. | `viewer.html:5960` |
| R7. Expandable group stacks | Each group header (count, name, %, toggle) expands unique full stacks with counts/percentages and color-coded frames (group headers use a simplified red/orange scheme; the summary uses the full 5-color scheme). | Click group header. | `viewer.html:5970-6001` |
| R8. Jump-to-poll links | Up to 5 example polls per group (`W<id> @<ts> (<dur>)`); clicking centers the view (5x pad) and scrolls to the worker lane. | Click an example poll. | `viewer.html:6003-6012`, `6029-6045` |
| R9. No-data toasts | Empty sched range -> "No scheduling events found..."; empty CPU range -> "No CPU samples in selected region"; empty heap range -> "No heap samples in selected region" (all 4s error toasts). | On empty selection/tab. | `viewer.html:5852-5854`, `6837-6838`, `6721-6723` |

---

## S. Flamegraph (CPU / heap / idle / task-dump)

Rendered via `FlamegraphRenderer.createFlamegraph` (`flamegraph.js`) inside the sidebar; `#flamegraph-panel` stays `display:none` and only holds `#fg-container` when idle. Sources: `viewer.html:706-708`, `923-924`, `6835-6895`; `flamegraph.js:127-939`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. Two-section layout | Always renders "Worker threads" (top) and "Off-worker (sampler thread)" (bottom, `workerId=255`) canvases, each with its own zoom stack; section labels show sample counts; empty sections hide. | Open any flamegraph. | `flamegraph.js:294-310`, `401-412` |
| F2. Frame rendering | Frames = deterministic name-colored rects, 18px rows, name shown if width >30px (ellipsized); ancestors at 0.6 alpha, search non-matches at 0.25; frames <0.1% of total culled; DPR-scaled. | Visual. | `flamegraph.js:316-376`, `45-103` |
| F3. Frame click -> zoom | Left-click zooms into a frame (full-width, children below, ancestor context bars at top); nesting; independent per section; no-op if childless. | Left-click a frame. | `flamegraph.js:669-693`, `520-533` |
| F4. Right-click zoom out | Pops one zoom level; falls back to the other section if the clicked one isn't zoomed. | Right-click. | `flamegraph.js:695-706` |
| F5. Breadcrumb nav | Bar above the canvases shows the zoom path per section (separated by `|`), `(all)` resets to root; each level clickable; hidden when unzoomed. | Click breadcrumb. | `flamegraph.js:284-286`, `420-476` |
| F6. Hover tooltip | Frame name, full name (if different), location (collapsible when pinned), sample count/self with percentages (format overridable for heap/idle). Positioned at container top; `pointer-events:none` unless pinned. | Hover a frame. | `flamegraph.js:557-628`, `638-656` |
| F7. Alt+click pin | Alt/Option+click pins the tooltip (selectable, `pointer-events:auto`, clickable location toggle + docs link); Escape or outside click unpins. | Alt/Option+click. | `flamegraph.js:676-678`, `630-636` |
| F8. Ctrl+click docs.rs | Ctrl/Cmd+click opens the frame's docs.rs page in a new tab (if `docsUrl`). | Ctrl/Cmd+click. | `flamegraph.js:673-675` |
| F9. Search | `.fg-search-input` filters/highlights frames (case-insensitive substring on name/fullName); matches full alpha, others dimmed; stats show `count frames - X% of samples` / "no matches"; clear `x` appears when non-empty. | Type / Ctrl-F / `/`. | `flamegraph.js:143-161`, `478-518`, `727-731` |
| F10. Spawn-location filter | Dropdown of task spawn locations (sorted by frequency, with counts) filters samples and rebuilds both trees; "All tasks" default. | Change dropdown. | `flamegraph.js:151`, `772-839` |
| F11. Export menu | "Export" opens a menu: Interactive SVG (`treeToInteractiveSvg`) and Folded stacks (`treeToFolded`, per-section headers); reflects the current spawn filter (full trees, not zoom); disabled with no data. | Click Export -> format. | `flamegraph.js:152-158`, `236-265` |
| F12. Help overlay | Info button toggles a shortcuts overlay (Click, Alt+click, Ctrl/Cmd+click, Right-click, Ctrl/Cmd+F or /, Esc); Esc / outside click closes. | Click info button. | `flamegraph.js:160-197` |
| F13. Escape cascade | Esc: unpin tooltip -> close export menu -> close help -> clear search -> reset zoom; returns true if it consumed the key, else the viewer closes the sidebar. | Press Esc. | `flamegraph.js:745-770`; `viewer.html:6124-6129` |
| F14. Resize / destroy | `resize()` refits both canvases to the container; `destroy()` removes listeners and orphans the tooltip; `getZoomPath`/`zoomToPath` save/restore zoom state. | Internal (resize/pop-out). | `flamegraph.js:842-877`, `915-936` |
| F15. CPU flamegraph (region/whole) | `showFlamegraph(start,end)` filters CPU samples to the range (or whole trace via D2); no CPU samples in range -> error toast; shows sample count + Pop Out; widens the sidebar. | D2 / Shift-drag / Flamegraph tab. | `viewer.html:939`, `6835-6895` |
| F16. Heap flamegraph | `showHeapFlamegraph(start,end)` strips allocator hook frames, estimates bytes/allocs via Horvitz-Thompson (`invP=1/(1-exp(-size/R))`, R=524288), with a Bytes/Count toggle (Bytes default). `CONDITIONAL` on alloc events with callchains in range. | D3 / Heap tab. | `viewer.html:941`, `6671-6833` |
| F17. Idle-time flamegraph | Time-weighted flamegraph of a task's idle-period async stacks (weight = idle us, min 1, scaled to max 10000; includes post-last-poll dumps to trace end). `CONDITIONAL` on task dumps. | Task-detail idle link (N4). | `viewer.html:6611-6669` |
| F18. Task-dump stack | Renders the async stack captures from a single idle gap ("Waiting on - N captures"), merging same-period dumps; error toast on failure. | Click a cross-hatched idle gap (N11). | `viewer.html:6601-6609` |
| F19. Pop Out | "Pop Out" opens `flamegraph.html` in a new tab preserving trace URL(s), `start`/`end`, and per-section zoom paths; blob URL for in-memory traces (info toast "keep this tab open"); error toast if no trace URL. | Click Pop Out above a flamegraph. | `viewer.html:6872`, `6897-6919` |

---

## T. Help overlay

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| T1. Help button | Dynamically created toolbar info button (question-mark SVG, `aria-label=Help`) toggling the overlay; hover brightens color/border. | Click info button. | `viewer.html:6520-6538` |
| T2. Help overlay modal | Fullscreen `rgba(0,0,0,.6)` backdrop, `z-index:200`, `role=dialog`, centered dark dialog (max-width 520px) with Mouse and Keyboard shortcut tables and a "Press Esc or click outside to close" hint. | Shown via T1 or `?`. | `viewer.html:759-788`, `950-974` |
| T3. Toggle / close | `?` toggles it (canceling any active keyboard selection first); Escape closes it with priority over other Escape actions; clicking the backdrop (target === overlay) closes it. | `?` / Esc / backdrop click. | `viewer.html:6167-6171`, `6115-6117`, `6539-6541` |
| T4. Shortcut content | Mouse: scroll, Ctrl/Cmd+scroll, drag, click poll, Shift+drag, Option+drag. Keyboard: Tab, Up/Down, Left/Right, Shift, Option/Alt, Esc, `?`. Static reference. | Inside the overlay. | `viewer.html:952-970` |

---

## U. Toasts & notifications

`#toast-container` (`position:absolute`, top-left of the timeline header, `z-index:60`, gap 8px, `pointer-events:none`). Sources: `viewer.html:709-743`, `858`, `1002-1034`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| U1. showToast / hideToast / clearToasts | `showToast(id,msg,type,autoHideMs,persistent)` creates/updates a toast; duplicate ids re-trigger a wiggle instead of duplicating; auto-hide via `setTimeout`; `clearToasts()` removes all non-persistent (`_persistentToasts`) toasts. | Programmatic. | `viewer.html:1002-1034` |
| U2. Types & animation | `toast-info` (blue), `toast-warn` (orange), `toast-error` (red); `toast-in` slide/fade 0.2s on create; `toast-wiggle` 0.4s on duplicate. | Automatic. | `viewer.html:709-743` |
| U3. Persistent hints | On load, two persistent info toasts: "Shift+drag to select a region" and "Option+drag to zoom"; each hidden when its selection type starts/completes. | Auto on load. | `viewer.html:2299-2300`, `5002-5003`, `6080-6081` |
| U4. Error toasts | "No CPU samples in trace..." (Shift+drag without CPU), plus the region no-data toasts (R9), pop-out errors (F19), task-dump / idle-flamegraph exceptions. | Contextual. | `viewer.html:4993-4995`, `6479-6481`, `4255-4257` |
| U5. Auto-clear triggers | `clearToasts()` fires on region-drag mouseup, lane click, zoom, Escape, sched-panel open, and queue-chart drag. | Those interactions. | `viewer.html:5151-5152`, `5194-5195`, `4935`, `6119`, `5888`, `6983` |

---

## V. Tooltips (general)

Single shared `#tooltip` element (`display:none`, `position:fixed`, `#222244` bg, `#555` border, 6px radius, max-width 320px, `z-index:100`, `pointer-events:none`). Source: `viewer.html:290-309`, `948`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| V1. placeTooltip | Positions the tooltip near the cursor (`x+12` clamped to `innerWidth-w-8`, `y+dy` default 16); flips above the cursor when it would overflow the bottom; min 8px margins; uses actual offset size. | Automatic after setting innerHTML. | `viewer.html:1446-1453` |
| V2. Panel-owned tooltips | Span, custom-events, and CPU panels each set the shared tooltip's contents and suppress the lanes tooltip while hovered (see J6/K3/L3); crosshair keeps tracking for correlation. | Hover the respective panel. | `viewer.html:3621-3673`, `4057-4102`, `4154-4182` |
| V3. Hide on drag / leave | Tooltip hides (and `mouseNs=null`) during drag; hidden on mouseleave of lanes/panels and when the cursor leaves the draw area (label gutter, past `drawW`, out-of-range lane). | Automatic. | `viewer.html:6206-6209`, `6238-6259`, `6413-6417` |

---

## W. Cross-cutting behaviors (replication-critical, not single buttons)

| Behavior | Detail | Source |
| --- | --- | --- |
| W1. Time formatting | `useAbsoluteTime`/`useLocalTz` drive `fmtTs`, `fmtDuration`, `fmtWallClock`, `fmtDelta`; toggling re-renders all views and rewrites labels. | `viewer.html:1155-1230` |
| W2. Coloring | 20-color `SPAN_COLORS` palette; `spanColor`/`ceColor` memoize per-name color (round-robin, cleared on reload); poll heatmap `pollColor`/`pollColorDim` (log-scale, 24-bin quantized, dim cache); frame color via `flamegraphColor` (shared with SVG export). | `viewer.html:1056-1147`; `trace_analysis.js:184-250`, `979-993` |
| W3. Render pipeline | `renderAll()` orchestrates timeline/lanes/span/CE/CPU/queue/task-detail/crosshair each frame, computing `window.sharedVisibleMaxQ` first for consistent y-scaling; `scheduleRenderAll` (RAF) coalesces; profiling under `D9PROF`. | `viewer.html:2579-2660` |
| W4. Performance LOD | `pixelDownsampleSpans` (one representative per pixel column), `makeBarCoalescer` (merge adjacent same-color bars), `pixelCoverage` (poll sampling-coverage), binary-search hit tests, and precomputed color palettes keep millions of spans/polls smooth. | `viewer.html:2949-2972`; `trace_analysis.js:280-360` |
| W5. Trace parsing | `TraceDecoder` (D9TF binary: magic, self-describing schemas, ULEB128, pooled strings/frames, delta timestamps, streaming snapshot/restore); `TraceParser.parseTrace`/`parseTraceStream` produce the full `ParsedTrace`; `canStreamDecode`, `fetchTraces`, `fetchTraceStream`, `deriveBlockInPlaceGaps`, `symbolizeChain`, `deduplicateSamples`, `formatFrame`. | `decode.js:121-406`; `trace_parser.js:90-1514` |
| W6. Analysis | `TraceAnalysis`: `buildWorkerSpans`, `attachCpuSamples`, `computeSchedulingDelays`, `computePollWakes`, `buildProcessCpuUsageSeries`, `buildActiveTaskTimeline`, `getTraceTimeRange`, `hasCpuProfileSamples`, `buildFlamegraphTree`/`flatten`, `filterPointsOfInterest`, `analyzeAllocations`, span layout helpers. | `trace_analysis.js` (throughout) |
| W7. Formatting utils | `formatHumanDuration` (ns->d/h/m/s/us/ns), `formatHumanBytes` (binary units), `formatFieldValue` (unit-aware: ns/us/ms/s, bytes). | `format.js:9-69` |
| W8. Flamegraph export | `FlamegraphExport`: `treeToFolded`, `treeToInteractiveSvg` (self-contained, hover/zoom/regex-search/URL-state), `buildExportRoot`, `layoutTree`, `filenameStem`, `escapeXml`, re-exported `flamegraphColor`. | `flamegraph_export.js:60-585` |
| W9. Credentials module | `Dial9Creds` (sessionStorage): `get/has/set/clear/parse/check/listBuckets/headers`; never persists beyond the tab, headers only ride same-origin fetches; fires `dial9:credentials-changed`. | `creds.js:51-257` |
| W10. State reset on load | `processTrace` clears all selection/filter state (`selectedTaskId`, span/event selections, `selectedCENames`, `selectedSpanNames`, span filters, `processCpuUsage`) before analyzing a new trace, preventing stale carry-over. | `viewer.html:1984-2033` |
| W11. URL parameters | `?trace=` (load), `?start`/`?end` (time-range filter), `?prof=1` (profiler), `?svc`/`?host`/`?from`/`?to`/`?segs` (structured metadata for the info block). Range params managed without reload. | `viewer.html:1816`, `1874-1899`, `2087-2091` |
| W12. Layout constants | `LABEL_W`=100px (gutter), `LANE_H`=60px (worker lane height); used across positioning, hit-testing, and lane auto-scroll. | `viewer.html:1354-1355` |