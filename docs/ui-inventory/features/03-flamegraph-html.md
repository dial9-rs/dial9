# UI Feature Inventory: `flamegraph.html` (CPU-profile flamegraph viewer)

> Code-derived inventory of the standalone flamegraph surface, with per-feature verification verdicts (`CONFIRMED` = matched code as documented; `CORRECTED` = re-derived from code after an initial mis-read). Purpose: capture every existing behavior precisely enough that (a) each can be validated in the running UI and (b) the surface can be re-implemented without losing anything. Source line numbers are a snapshot; the function name is the stable anchor.
>
> REFRESHED 2026-07-08 against HEAD (drift commits #570, #596, #600; anchors
> re-derived from commit 84a21e5's tree; `trace_parser.js` anchors fully
> re-derived because #582 reindented that file). Rows added or
> behavior-updated are marked `[2026-07-08]`; post-snapshot rows continue the
> id sequence from F166 regardless of which section they sit in. The new
> aggregated server-side mode (#570) is section P. Verification statuses for
> the refresh are in the "2026-07-08 refresh" section at the end.

## What this surface is

The standalone CPU-profile flamegraph. It has TWO modes since #570: (1) the exact mode - opened from the S3 browser (or via a hand-built URL) with one or more `?trace=` components, an optional `?start=/?end=` nanosecond window, and optional title metadata; fetches and decodes the trace client-side, builds two flame trees (worker threads vs off-worker sampler thread); and (2) the aggregated mode (`?api=1`, section P) - polls the server's `/api/flamegraph` demand-driven refinement loop and renders the pre-built tree it returns (no client decode). Both render on interactive canvases with search, spawn-location/runtime filtering, zoom, tooltips, and SVG / folded-stacks export.

- Entry file: `dial9-viewer/ui/flamegraph.html` (markup + inline `<style>` + inline bootstrap `<script>`)
- Loaded modules: `decode.js`, `trace_parser.js`, `creds.js`, `trace_analysis.js`, `flamegraph_export.js`, `flamegraph.js`, stylesheet `flamegraph.css`; since #570 also `format.js` and `flamegraph_api.js` (pure helpers for the aggregated mode, unit-tested in `test_flamegraph_api.js`)
- Backend endpoints consumed: exact mode - none directly (fetches whatever `?trace=<url>` values it is handed, typically `/api/object?bucket&key`); aggregated mode - `GET /api/flamegraph` (#570). AWS credential headers (`x-dial9-aws-*`) ride along on same-origin fetches only.

## How to read this document

| Column | Meaning |
| --- | --- |
| **Feature** | One discrete capability. |
| **What it does** | Behavior, including edge cases and non-obvious rules. |
| **Access path** | Precise way to reach/trigger it in the running UI (click path / interaction / keyboard shortcut / URL param). |
| **Source** | `file:line` (+ function name). Line numbers are a snapshot; the function name is the stable anchor. |

Status tags used in notes: `OK` (works), `DEAD` (present in markup/CSS but not wired), `CONDITIONAL` (only present/active under a server or runtime condition). Plain ASCII arrows (`->`) are used throughout; where the running UI renders a Unicode glyph (em-dash, U+2192 arrow, middle-dot, etc.) the codepoint is called out in parentheses rather than typed.

---

## A. Page bootstrap and trace loading

The inline IIFE in `flamegraph.html` (`92-642`) parses URL params, fetches + decodes the trace, then hands off to `FlamegraphRenderer`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. `?trace=` param parsing | Reads the repeatable `trace` query param via `getAll`. Each value is a separate (possibly gzipped) component to fetch and concatenate. `[2026-07-08]` (#600) Multiple values no longer force the buffered path - they stream back-to-back when the runtime supports it (F4/F166). | URL param `?trace=<url>` (repeatable: `?trace=a&trace=b`). | `flamegraph.html:96`; `trace_parser.js:92` |
| F2. `start` / `end` ns params | Reads optional `start` and `end` as `Number` nanoseconds (null if absent). Drive CPU-sample time-window filtering and the duration/label logic. | URL params `?start=<ns>&end=<ns>` (numeric, optional). | `flamegraph.html:97-98` |
| F3. No-trace-URL validation | If `getAll("trace")` is empty (and not in api mode, section P), shows error `No trace URL provided. Use ?trace=<url>&start=<ns>&end=<ns> or ?api=1&data_dir=...` and returns early (no load attempt). `[2026-07-08]` message extended by #570. | Open the page with no `?trace=` param. | `flamegraph.html:503-506` |
| F4. Streaming vs buffered decision | `[2026-07-08]` REWRITTEN by #600: STREAM whenever `canStreamDecode()` is true, for single OR multiple URLs (multi via `fetchTracesStream`, F166); the buffered fetch+concatenate path is now only the fallback when streaming is unsupported. Transparent to the user but changes the progress text. `canStreamDecode()` checks `ReadableStream` + `DecompressionStream` support. | Automatic on load. | `flamegraph.html:508-527`; `trace_parser.js:129-135` |
| F5. Single-URL streaming path | `fetchTraceStream()` fetches, peeks the first chunk for the gzip magic `0x1f 0x8b`, pipes through `DecompressionStream('gzip')` if gzipped, and returns an async iterable; `parseTraceStream()` drains complete frames incrementally so download and decode overlap. `[2026-07-08]` (#600) Also adapts responses with no readable `body` stream via a one-shot reader (buffered, same interface). | Automatic when `traceUrls.length === 1 && canStreamDecode()`. | `flamegraph.html:520-527`; `trace_parser.js:171-265,1203-1215` |
| F6. Buffered fallback path | `fetchTraces()` fetches all URLs in parallel, gunzips each component independently, concatenates into one `ArrayBuffer`; `parseTrace()` decodes once. Same-origin credential rule applies per URL. `[2026-07-08]` (#600) Now reached ONLY when `!canStreamDecode()` (multi-URL loads stream, F166). | Automatic when `!canStreamDecode()`. | `flamegraph.html:528-535`; `trace_parser.js:92-127` |
| F7. Gzip auto-detection | Streaming: `fetchTraceStream()` sniffs the gzip magic and conditionally pipes through `DecompressionStream`. Buffered: `maybeGunzip()` checks the first 2 bytes and decompresses via `DecompressionStream` (browser) or `zlib.gunzipSync` (Node). Caller always receives uncompressed bytes. | Automatic; no user control. | `trace_parser.js:32-60,186-230` |
| F8. AWS credential attachment | If `window.Dial9Creds` is present, `Dial9Creds.headers()` supplies `x-dial9-aws-*` headers (accessKeyId, secretAccessKey, sessionToken, region). `isSameOrigin()` withholds them from cross-origin (or unparseable) trace URLs to prevent exfiltration via a crafted `?trace=https://attacker/` link; off-browser (Node tests) all URLs are treated as same-origin. | Automatic; applied to trace fetches when creds are stored. | `flamegraph.html:517`; `creds.js:240-249`; `trace_parser.js:65-88,100,172` |
| F9. Loading indicator + phase text | `#loading` element shows a spinner (#570) + centered progress text through phases: `Loading trace...` / `Loading N traces...` (streaming, N since #600), `Fetching trace...` / `Fetching N traces...` (buffered), `Parsing trace...`, then `Analyzing...`. | Automatic during load. | markup `flamegraph.html:80`; CSS `52-59`; phases `520-535`, `550` |
| F10. Loading visibility toggle | `.hidden` on `#loading` flips `display:flex` -> `display:none`. Added by `showError()` and once on successful completion; never removed. | Automatic (error or success). | `flamegraph.html:42-51` (CSS), `107`, `605` |
| F11. Error indicator | `#error` element (red `#ff6b6b`, initially `display:none`) is shown via `showError()`, which hides loading, sets `display:flex`, and sets `textContent`. | Automatic on error; reload/navigate to dismiss. | `flamegraph.html:61-71` (CSS), `106-110` (`showError`) |
| F12. HTTP 401 credentials error | `CONDITIONAL`. If a fetch error message matches `HTTP 401` AND `window.Dial9Creds` exists AND `!Dial9Creds.has()`, shows the specific message `This trace requires AWS credentials. Open it from the dial9 home page after applying your credentials.` instead of the generic error. | Fetch a credentialed trace with no creds applied. | `flamegraph.html:537-541`; `creds.js:61-64` |
| F13. Generic fetch/parse error | Any other error from the fetch/parse stage shows `Failed to load trace: <err.message>`. | Any load/parse failure (HTTP, malformed data, network). | `flamegraph.html:536-545` |
| F166. Multi-URL pipelined streaming | `[2026-07-08]` NEW in #600 (issue #595). `fetchTracesStream(urls)` streams MULTIPLE `trace=` components as one logical trace: every component's fetch is dispatched up front (concurrent downloads), chunks are emitted strictly in `urls` order back-to-back, and a mid-stream `TRC\0` header resets decoder state - so parsing segment 0 overlaps the in-flight downloads of segments 1..N (~max(download, parse), byte-identical to the buffered concat). Early-exit cancels not-yet-consumed component streams; per-component rejections are pre-handled to avoid unhandled-rejection noise until emission reaches them. Loading label reads `Loading N traces...`. | Automatic when `traceUrls.length > 1 && canStreamDecode()`. | `flamegraph.html:520-527`; `trace_parser.js:270-345` (`fetchTracesStream`); tests `test_fetch_traces.js`, `test_parse_yield_throttle.js` |

---

## B. Trace analysis pipeline

Runs after decode, before rendering (`flamegraph.html:548-581`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F14. Analysis progress | Sets `#loading` to a spinner + `Analyzing...` for the span-building + sample-attachment phase. | Automatic, just before render. | `flamegraph.html:550` |
| F15. Worker ID extraction | Iterates `trace.events`, collecting `workerId`s, excluding events with `eventType === QueueSample (4)` or `WakeEvent (9)`; sorts numerically ascending. Partitions downstream per-worker span building. | Internal. | `flamegraph.html:552-557`; `trace_parser.js:389-402` (`EVENT_TYPES`) |
| F16. Worker span building | `TraceAnalysis.buildWorkerSpans(events, workerIds, maxTs)` builds per-worker polls, parks, actives, and `cpuSampleTimes`. `blockInPlaceGaps` (4th arg) is not passed, so gap-based span filtering is disabled here. | Internal. | `flamegraph.html:559`; `trace_analysis.js:438` |
| F17. CPU sample attachment | `CONDITIONAL` (only if `trace.cpuSamples.length > 0`). `attachCpuSamples(cpuSamples, workerSpans)` binary-searches each sample into the poll it landed in, setting `sample.spawnLoc` (or null) and `sample.inPoll`. Enables spawn-location annotation. | Automatic when the trace has CPU samples. | `flamegraph.html:560-562`; `trace_analysis.js:636-684` |
| F18. Span-build error recovery | Wraps span-build + attach in try/catch; on exception logs `console.warn("Failed to attach spawn locations:", err)` and continues. Non-fatal: the flamegraph still renders, just without `spawnLoc`. | Automatic; observable in console. | `flamegraph.html:563-566` |
| F19. CPU sample time-range filter | `FlamegraphRenderer.filterCpuSamples(cpuSamples, startNs, endNs)` drops (1) empty-callchain samples, (2) `source===1` scheduler samples, (3) samples before `startNs` (if set), (4) samples after `endNs` (if set). With both null, no time filter is applied. Result populates `allSamples`. | Triggered by `?start=`/`?end=`; both optional. | `flamegraph.html:570`; `flamegraph.js:121-126` |
| F20. Time-range match + fallback | `timeRangeMatched = allSamples.length > 0`. If false AND (`startNs != null` OR `endNs != null`), re-runs the filter with `null,null`, logs `console.warn("Time range filter matched 0 samples - showing all N samples")`, and uses the unfiltered set (graceful degradation). `timeRangeMatched` also gates zoom restoration (F55). | Automatic when `?start`/`?end` do not intersect the trace. | `flamegraph.html:571-577,626` |
| F21. No-samples error | If both the filtered and unfiltered sets are empty, shows `No CPU samples found in the specified time range.` and returns (halts render). Means the trace has no CPU profiling data at all. | Automatic when there are zero CPU samples. | `flamegraph.html:578-580` |

---

## C. Page header: title and stats bar

Populated once during init from URL params (`flamegraph.html:583-606`). The stats bar does not refresh on zoom. `[2026-07-08]` Exact mode only: in aggregated mode (`?api=1`) the title and stats are instead driven by the backend's response metadata on every poll (F175/F176, section P).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F22. Browser tab title | Sets `document.title` to `Flamegraph - {label}` (em-dash separator, U+2014) plus ` (X.XXms)` when `start`+`end` are both present. | Browser tab/window title. | `flamegraph.html:596` |
| F23. Page header title | Sets `#fg-title` (top-left, purple/bold) to `Flamegraph - {label}` (same label, no duration suffix). | Top-left of the page header. | `flamegraph.html:597` |
| F24. Service/host label construction | Builds `label`: if `svc` param is present, uses `svc` and appends ` @ {host}` only when `host` is also present; otherwise falls back to the last path segment of the first trace URL, or `trace` if empty. | Applied to F22 and F23. | `flamegraph.html:585-586,590-592` |
| F25. Sample count display | First stats item, always present: `N samples`, where `N` is `allSamples.length` after time-range filtering (reflects the fallback in F20 when the window did not match). | Right side of header, first stat. | `flamegraph.html:570-581,598` |
| F26. Segment count display | `CONDITIONAL` (only if `?segs=` present and non-empty). Shows `N segment` / `N segments` with plural handling keyed on `segs !== "1"`. Purely informational. | URL param `?segs=<n>`. | `flamegraph.html:587,599` |
| F27. Time range display | `CONDITIONAL` (only if `?from=` present). Shows `FROM -> TO` (renders as U+2192) when `to` is present and differs from `from`; otherwise shows just `FROM`. | URL params `?from=...&to=...`. | `flamegraph.html:588-589,600` |
| F28. Duration display | `CONDITIONAL` (only if both `start` and `end` present). Shows `X.XXms selected`, computed as `((endNs - startNs) / 1e6).toFixed(2)`. Shown even when the window later fails to match samples. | URL params `?start=<ns>&end=<ns>`. | `flamegraph.html:593-595,601` |
| F29. Time-range mismatch warning | `CONDITIONAL` (only when `timeRangeMatched === false`). Appends the last stat `full trace - selected region could not be reproduced` (em-dash) to signal the F20 fallback to all samples. | Last stat item when `?start`/`?end` did not match. | `flamegraph.html:571-581,602` |
| F30. Stats bar layout / separator | `#fg-stats` joins the conditional bits in order (samples, segs, time range, duration, warning) with a middle-dot separator (U+00B7, ` . `) into a single span. | Second span in the header, right side. | `flamegraph.html:77,598-603` |

---

## D. Search bar (frame search)

The toolbar is built by `createFlamegraph()` (`flamegraph.js:148-166`). Search state lives in the module-scoped `searchQuery` (`flamegraph.js:136`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F31. Search input field | 260px text input that filters frames by case-insensitive substring on `name` or `treeNode.fullName`. Live, synchronous, no debounce; empty query clears filtering. Operates across both trees at once. | Click the field, or focus via Ctrl/Cmd+F or `/`. | `flamegraph.js:148-153,169,520-525` |
| F32. Placeholder + platform hint | Placeholder reads `Search frames... (Cmd + F or /)` on Mac/iPhone/iPad (Cmd symbol U+2318, detected via `navigator.platform` regex) or `Search frames... (Ctrl + F or /)` elsewhere. Informational only. | Visible in the empty field. | `flamegraph.js:150-153` |
| F33. Live update handler | On each `input` event, `onSearchInput()` copies value to `searchQuery`, toggles the clear button, and triggers a full repaint (canvas + stats). No debounce; large traces may lag under fast typing. | Type in the field. | `flamegraph.js:520-525` (`onSearchInput`) |
| F34. Name + fullName matching | Match logic: `name.toLowerCase().includes(q)` OR (`treeNode.fullName` exists AND its lowercase includes `q`). No regex; special chars are literal. Rust-style qualified names live in `fullName`. | Type a short name (`Vec`) or qualified path (`std::vec::Vec`). | `flamegraph.js:107-119` (`countSearchMatches`), `flamegraph.js:357` |
| F35. Search respects zoom level | `updateSearchStats()` counts matches under the current zoom root (`zoomStack[-1]` if zoomed, else tree root), aggregating both trees independently. `% of samples` is thus relative to the zoomed subtree. | Zoom into a frame, then search. | `flamegraph.js:485-518,494-507` |
| F36. Clear button (x) | An x-glyph button (U+00D7) that clears the input and `searchQuery`, hides itself, repaints, and refocuses the input. | Click the x (visible only when a query is active). | `flamegraph.js:154,170,282-289` |
| F37. Clear button visibility | Starts hidden; toggled on every input event via `display = searchQuery ? "" : "none"`; also hidden on clear/Escape. | Appears when the field has text. | `flamegraph.js:282,523` |
| F38. Search statistics display | Shows `N frame`/`N frames` (singular/plural), plus ` . X.X% of samples` whenever the in-view total is > 0. `[2026-07-11]` (T48, #593) The percentage is now the highlighted-area share: the union of INCLUSIVE sample counts of the topmost matching frames (nested matches add no new extent) over the total samples in view, summed across both panels - exactly the share of the canvas the F40 highlight lights up, and the same semantic as the export SVG's `Matched:` figure (F84) and flamegraph.pl. Previously: matched SELF samples over total, hidden entirely when `matchedSelf == 0` (mid-stack-only matches); that guard is removed. Core-side change (`flamegraph.js`), so BOTH page generations changed together. Shows `no matches` on zero, blank when the query is empty. Recomputed every repaint. | Auto-updates in the toolbar as you type. | `flamegraph.js:106-127` (`countSearchMatches`), `flamegraph.js:493-529` (`updateSearchStats`); regression: `tests/core/flamegraph_search.test.ts` |
| F39. No-match handling | Query with zero matches shows exactly `no matches`; all frames stay visible but dimmed to alpha 0.25. No modal/error. | Search for a string that matches nothing. | `flamegraph.js:508-510` |
| F40. Frame dimming on search | While a query is active, matching frames render at alpha 1.0 and non-matching frames at 0.25 (ancestor bars included). Immediate, no animation. | Type in the field; canvas re-renders instantly. | `flamegraph.js:347-363` |
| F41. Focus shortcuts (Ctrl/Cmd+F, /) | Global keydown: Ctrl/Cmd+F always `preventDefault()` + focus + select-all (even if the field is already focused, so you can replace the query). `/` does the same but only when the field is not the active element (so `/` can be typed into the query). Guarded by container visibility (F86). | Press Ctrl/Cmd+F or `/` on the page. | `flamegraph.js:732-740` (`onKeyDown`) |
| F42. Text selection on keyboard focus | Keyboard-focus paths call `searchInput.select()` so existing text is highlighted for immediate replacement. A direct mouse click does not auto-select. | Focus via Ctrl/Cmd+F or `/`. | `flamegraph.js:736-737` |
| F43. Search across both trees | Dimming and stats apply to `workerTree` and `offworkerTree` simultaneously with the same query; an empty tree is skipped. | Type; results span both sections. | `flamegraph.js:485-517` |
| F44. Search query persistence | `searchQuery` survives zoom, resize, and spawn-filter changes; only the clear button or Escape resets it. Resetting zoom does NOT clear search. | Search, then zoom/filter/resize -> search stays active. | `flamegraph.js:136,424` |
| F45. Search bar styling | `.fg-search-bar` is a flex row (`gap:8px`, bg `#16213e`, bottom border, `flex-shrink:0`). Input: dark `#2a2a4a`, 1px `#444`, 4px radius. | Visible at the top of the flamegraph container. | `flamegraph.css:3-31`; `flamegraph.js:148-166` |
| F46. Focus visual indicator | On focus, the input border changes from `#444` to `#6c63ff` (purple). | Focus the search field. | `flamegraph.css:22` |

---

## E. Spawn-location + runtime filters

Populated by `setData()`; applied by `applyFilters()` (`flamegraph.js:779-820`; RENAMED from `applySpawnFilter` and extended with the runtime filter by #596 - the two filters AND-combine).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F47. Spawn filter dropdown | `<select>` in the toolbar that filters all samples to a single task spawn location. | Click the dropdown; select an option. | `flamegraph.js:157,172,822`; `flamegraph.css:32-41` |
| F48. All-tasks default option | First option, value `""`, text `All tasks (N samples)` where `N` is the full sample count; resets the filter. | Default selection. | `flamegraph.js:844` |
| F49. Individual location options | One option per unique spawn location: `short_path (count)` (directory prefix stripped), full path in the option `title` tooltip; count reflects the current view. | Options after the default. | `flamegraph.js:845-853,850` |
| F50. Sorting + unknown grouping | Options sorted by count descending (`sort((a,b) => b[1]-a[1])`); samples with no location grouped under `(unknown)` (`s.spawnLoc || "(unknown)"`). | Dropdown order. | `flamegraph.js:841,845` |
| F51. Filtering by location | On `change`, keeps only samples matching the selected location (empty value = all). `[2026-07-08]` (#596) Runs inside `applyFilters()`, AND-combined with the runtime filter (F167). | Select an option. | `flamegraph.js:779-793,822` |
| F52. Tree rebuild + zoom reset | On filter change, rebuilds both worker/off-worker trees from the filtered samples and resets both zoom stacks to root. Search query is NOT cleared (still filters the new trees). | Change the dropdown. | `flamegraph.js:796-804` |
| F53. Section-label count update | Updates the worker / off-worker section labels to `{prefix} - {count} samples` (em-dash separator; prefixes default `Worker threads` / `Off-worker (sampler thread)`); shows `0 samples` when filtered to nothing. | Labels above each canvas. | `flamegraph.js:806-809,826-828` |
| F54. Export availability on change | Recomputes export enabled/disabled from the filtered trees and closes any open export menu to avoid a stale dataset. | Automatic on filter change. | `flamegraph.js:814-817` |
| F55. Spawn-location attachment | Each CPU sample is enriched with `spawnLoc` during analysis (F17) by binary-searching its timestamp into a task poll interval; unmatched samples get `spawnLoc=null` -> `(unknown)`. | Internal, during load. | `trace_analysis.js:636-667`; `flamegraph.html:561` |
| F56. Dropdown styling | Dark `#2a2a4a` bg, `#444` border, 4px radius, max-width 350px, `0.9em`. No explicit hover style. `[2026-07-08]` (#596) Selector now shared: `.fg-spawn-filter, .fg-runtime-filter`. | Visual. | `flamegraph.css:32-41` |
| F167. Runtime filter dropdown | `[2026-07-08]` NEW in #596. `CONDITIONAL`: hidden unless the trace has MORE THAN ONE runtime (from `runtime.<name>` segment metadata via `trace.runtimeWorkers`; `buildRuntimeFilterData` in the analysis module supplies workerId->runtime map + options with per-runtime sample counts). Options: `All runtimes` (default), then `runtime: <name> (N)` / `<name> runtime (N)` for the inferred main block. Selecting a runtime keeps only samples whose worker belongs to it - off-worker samples (workerId 255) have no runtime and are EXCLUDED by any runtime filter. AND-combines with the spawn filter; rebuilds trees + resets zoom like F52. Hidden and cleared in api mode (`setTreeDirect`, section P). Mirrors the viewer's multi-runtime lane grouping. | Dropdown next to the spawn filter (multi-runtime traces only). | `flamegraph.js:144,156,173,779-793,865-885`; `trace_analysis.js:1601-1629` (`buildRuntimeFilterData`); `trace_parser.js:665,928-945` (`runtimeWorkers`); test `test_runtime_groups.js` |

---

## F. Export menu

Toggle + menu built in `createFlamegraph()`; wiring at `flamegraph.js:225-280`. Requires `flamegraph_export.js`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F57. Export button toggle | `[down-arrow] Export` button (aria-expanded) that opens/closes the format menu; disabled (opacity 0.4, cursor default, title `No samples to export`) when there is no exportable data; `stopPropagation` prevents parent handlers. | Click `Export` in the toolbar. | `flamegraph.js:159-160,243-249`; `flamegraph.css:55-66` |
| F58. Export menu dropdown | Absolutely-positioned menu (`role=menu`, min-width 170px, dark `#16213e`, shadow) below the button, hidden by default, with two items (SVG, folded stacks). | Opens on toggle. | `flamegraph.js:161-164`; `flamegraph.css:67-78` |
| F59. Menu dismiss | Closes on: another toggle click, Escape (F84), clicking outside the search bar (`onExportOutsideClick`), opening the help overlay, or a spawn-filter change. | Click outside / Escape / open help / change filter. | `flamegraph.js:199,275,278-280,817` |
| F60. Interactive SVG export | Generates and downloads a standalone interactive SVG (zoom, Ctrl-F regex search, Ctrl-I case toggle, Reset Zoom, `?x=&y=&s=` URL state, self-contained). Filename `filenameStem(title) + ".svg"`. Menu closes after click. | `Export` -> `Interactive SVG (.svg)`. | `flamegraph.js:162,251-258`; `flamegraph_export.js:432-564` |
| F61. Folded stacks export | Generates and downloads folded stacks text (`frame1;frame2;frame3 count`), consumable by inferno / flamegraph.pl / speedscope. Exports the FULL tree (no visual/zoom pruning); self-weight per full path; `(all)` root omitted; counts rounded to integers; only frames with `self > 0` emitted; children sorted by descending count. Filename `filenameStem(title) + ".folded.txt"`. | `Export` -> `Folded stacks (.txt)`. | `flamegraph.js:163,260-272`; `flamegraph_export.js:75-96` |
| F62. Export data availability | `hasExportableData()` returns true iff `workerTree.count > 0` OR `offworkerTree.count > 0`; drives the disabled state and blocks export of empty datasets. | Observe button state after load/filter. | `flamegraph.js:232-234,245,814-817` |
| F63. Filename sanitization | `filenameStem()` strips a leading `Flamegraph -` prefix, replaces non-alphanumerics with `_`, trims leading/trailing dots/underscores (no dotfiles), and falls back to `flamegraph` if empty. Applied to both exports. | Automatic on export. | `flamegraph_export.js:569-575` |
| F64. Custom value formatter | SVG export uses `exportFormatValue` (from `setData` options) for hover-title weights; defaults to `N samples` with commas. Other views (heap, etc.) can pass bytes/allocs formatters. | Set via `exportFormatValue`; visible in SVG hover text. | `flamegraph.js:213,832-836`; `flamegraph_export.js:427-439,532` |
| F65. Menu item hover effect | SVG / folded menu items highlight to bg `#2a2a4a`, text `#fff` on hover. | Hover a menu item. | `flamegraph.css:92` |
| F66. Export module optional | `CONDITIONAL`. If `flamegraph_export.js` fails to load, the entire export wrap is hidden and a `console.warn` is logged once; the rest of the UI degrades gracefully. | Check console if export controls are missing. | `flamegraph.js:218-223` |
| F67. Multi-panel merge/wrapping | `buildExportRoot` synthesizes a single root when both panels exist, wrapping each tree in a labeled frame (e.g. `[Worker threads]`); folded output prepends `# label` comment headers per panel and skips empty panels. | Automatic when both panels have data. | `flamegraph.js:225-229,265-269`; `flamegraph_export.js:108-126` |
| F68. Empty-graph SVG fallback | With no exportable data, produces a minimal valid SVG showing `No data to export.` rather than erroring. | Automatic when exporting empty data. | `flamegraph_export.js:442-456` |

---

## G. Interactive SVG (the exported artifact)

Behaviors of the self-contained SVG produced by F60, ported from Brendan Gregg's `flamegraph.pl`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F69. Frame layout geometry | Lays nodes into rectangles in cumulative count-space, children packed left-to-right by descending count; frames narrower than `MINWIDTH_PX` (0.1px) pruned (root always kept). `DEFAULT_WIDTH=1200`, `FRAME_HEIGHT=16`, `XPAD=10`. | Automatic when generating SVG. | `flamegraph_export.js:33-42,133-150,458-464` |
| F70. Frame rectangle rendering | Each frame is a `<g>` containing, in order, `<title>`, `<rect>`, `<text>`; coordinates rounded to 0.1px; root uses `flamegraphColor('root')`. | Visible in exported SVG. | `flamegraph_export.js:509-559` |
| F71. Frame coloring | Hash-based warm palette (hue 10-50, red/orange), root gets the special root color; sourced from `TraceAnalysis.flamegraphColor` so SVG matches the on-screen canvas. | Frame `fill` attributes. | `flamegraph_export.js:48-58,548` |
| F72. Frame title tooltip | Native `<title>` shows `name (value, percentage%)` on hover (`all (value, 100%)` for root); value via `exportFormatValue`. | Hover a frame in the SVG. | `flamegraph_export.js:530-535,550` |
| F73. Frame label text | Truncated name inside the frame when width permits (`chars = floor(width / (FONT_SIZE*FONT_WIDTH))`, ellipsis `..`); no label if narrower than 3 char-widths or for root. | Visible inside frames. | `flamegraph_export.js:537-546` |
| F74. Embedded JS init | On `onload='init(evt)'`, restores zoom (x/y params) and search (`s=`) from the URL and wires click, mouseover, mouseout, keydown listeners. | Automatic on opening the SVG. | `flamegraph_export.js:164-176,469` |
| F75. Frame hover details | Mouseover updates bottom-left details text `Function: name (weight, percentage%)`; mouseout clears it; walks the parent chain to find the frame group. | Hover a frame. | `flamegraph_export.js:204-211,503` |
| F76. Frame click zoom | Click re-roots to that frame (updates coords/visibility via `zoom_child`/`zoom_parent`), shows Reset Zoom, stores x/y in the URL; ancestors render at reduced opacity (`parent` class); clicking a parent unzooms first. | Click a frame in the SVG. | `flamegraph_export.js:178-203,306-332` |
| F77. Reset Zoom button | Top-left text, visible only while zoomed; restores original visibility/positions, removes x/y from the URL, re-runs the current search. | Click `Reset Zoom` (visible when zoomed). | `flamegraph_export.js:333-350,504` |
| F78. Search prompt (Ctrl-F / F3) | Opens a browser prompt `Enter a search term (regexp allowed, eg: ^ext4_)`; entering a term calls `search()`; if already searching, resets instead. | Press Ctrl-F or F3 in the SVG. | `flamegraph_export.js:212-215,364-377` |
| F79. Search execution (regex) | Compiles the term as `RegExp` (optional ignorecase), highlights matching frames in magenta, computes matched-% of total width, stores `s=` in the URL, updates the button to `Reset Search`. | Enter a term in the prompt. | `flamegraph_export.js:378-421` |
| F80. Search reset | Restores original frame fills, clears `s=`, hides matched-% and reverts the button to `Search`; does not exit zoom. | Toggle search off / click `Reset Search`. | `flamegraph_export.js:357-363,369-375` |
| F81. Case-insensitive toggle (Ctrl-I) | Toggles the ignorecase flag, updates the `ic` button state, and re-runs any active search. | Press Ctrl-I in the SVG. | `flamegraph_export.js:212-215,351-356` |
| F82. Search button UI | `Search` text top-right at `opacity:0.1`, fully opaque on hover/active; click opens the prompt; becomes `Reset Search` when active. | Click `Search` (or F3). | `flamegraph_export.js:483-506,201` |
| F83. `ic` toggle UI | `ic` text near Search, `opacity:0.1` normally, opaque on hover/active; click toggles case-insensitive mode. | Click `ic` (or Ctrl-I). | `flamegraph_export.js:485-506,202` |
| F84. Matched-percentage display | Bottom-right `Matched: X.X%` (1 decimal unless exactly 100%), hidden unless a search is active. | Appears after a search. | `flamegraph_export.js:417-421,507` |
| F85. URL state - zoom | Zoom stored as `x`/`y` frame coordinates; `zoom()` runs on load if both present; updated via `history.replaceState`. | Share/reopen the SVG URL after zooming. | `flamegraph_export.js:174-175,194-197` |
| F86. URL state - search | Search stored as URL-encoded `s=`; `search()` runs on load if present; updated via `history.replaceState`. | Share/reopen the SVG URL after searching. | `flamegraph_export.js:176,400-402` |
| F87. Frame-group contract | Each frame `<g>` is guaranteed to contain exactly `<title>`, `<rect>`, `<text>` in order so the embedded zoom script can find children by tag; ancestors render as full-width context rows during zoom. | SVG DOM structure. | `flamegraph_export.js:318-325,549-558` |
| F88. Coordinate precision rounding | Coordinates rounded to 0.1px (x1/x2 first, width derived) to match `flamegraph.pl`'s `filledRectangle`, avoiding right-edge drift that would break the ancestor test (`fudge=0.0001`). | SVG `rect` x/width attributes. | `flamegraph_export.js:513-527` |
| F89. minWidth pruning | `layoutTree()` prunes frames/subtrees narrower than `minTime` (from `MINWIDTH_PX` and the width/time ratio); root always kept. | Automatic during SVG generation. | `flamegraph_export.js:42,133-150,460` |
| F90. XML escaping | Frame names escaped via `escapeXml()` (`&<>"'`) for titles and labels to prevent XML injection. | Frame names in the SVG. | `flamegraph_export.js:60-67,452,550,556` |
| F91. Metadata / styling / background | Includes XML/DOCTYPE, `flamegraph.pl` + Netflix/Joyent/Brendan Gregg attribution and CDDL-1.0 notice; embedded CSS (Verdana 12px, hover stroke, `hide`/`parent` classes); `#background` gradient from `#eeeeee` to `#eeeeb0`. | View SVG source / background. | `flamegraph_export.js:466-493,477-482,501` |

---

## H. Help overlay

Built in `createFlamegraph()` (`flamegraph.js:166,180-204`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F92. Help button | `[info]` icon button (`tabindex=0`, `role=button`, title `Keyboard shortcuts`) that closes any open export menu, then toggles the overlay between `flex` and `none`. Hover -> text `#fff`, border `#888`. | Click the `[info]` button in the toolbar. | `flamegraph.js:166,178,198-201`; `flamegraph.css:52` |
| F93. Help overlay modal | Dark semi-transparent overlay (`rgba(0,0,0,0.5)`, z-index 100) covering the container, with a centered box titled `[keyboard] Flamegraph Shortcuts` and a shortcuts table; hidden by default. | Opens on the help button. | `flamegraph.js:180-196`; `flamegraph.css:132-153` |
| F94. Keyboard reference table | Rows: `Click -> Zoom into frame`; `Option/Alt + click -> Pin tooltip`; `Cmd/Ctrl + click -> Open docs.rs`; `Right-click -> Zoom out one level`; `Cmd/Ctrl + F or / -> Search frames`. Non-interactive reference. Modifier symbol is platform-aware (Cmd U+2318 on Mac, else Ctrl). | Open the overlay. | `flamegraph.js:182-194,150,188,190` |
| F95. Dismiss instructions | Footer text `Press Esc or click outside to close` (`#666`, 0.78em). | In the overlay. | `flamegraph.js:193`; `flamegraph.css:153` |
| F96. Click-outside close | Clicking the dark background (target is the overlay itself, not the content box) closes the overlay. | Open the overlay, click outside the box. | `flamegraph.js:202-204` |

---

## I. Flamegraph canvases: rendering

Two stacked canvases built at `flamegraph.js:297-317`; drawing in `renderCanvas`/`flattenFromNode` (`flamegraph.js:46-101,323-383`). Colors from `TraceAnalysis.flamegraphColor`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F97. Worker canvas | Top canvas rendering samples from worker threads (`workerId != 255`); height scales with tree depth; frames are rectangles with width proportional to sample count; ancestor context bars shown when zoomed. | Below the `Worker threads` label. | `flamegraph.js:306-308,422` |
| F98. Off-worker canvas | Bottom canvas rendering the sampler thread (`workerId == 255`); a separate tree with identical rendering/interaction. | Below the `Off-worker (sampler thread)` label. | `flamegraph.js:315-317,422` |
| F99. Section labels | `Worker threads - N samples` and `Off-worker (sampler thread) - N samples` (em-dash), counts updated on spawn-filter change; auto-hide when filtered to zero. | Above each canvas. | `flamegraph.js:301-304,310-313,806-809` |
| F100. Scrollable body | Both canvases sit in a `flex` column `.fg-body` with `overflow-y:auto`; each canvas is independently sized. | Scroll the canvas area. | `flamegraph.js:297-299`; `flamegraph.css:116-121` |
| F101. Canvas background fill | Fills `#1a1a2e` before drawing, every repaint. | Visual. | `flamegraph.js:339-340` |
| F102. Deterministic frame coloring | `flamegraphColor(name)` maps each name to a stable HSL (hue 10-49, sat 60-89%, light 40-54%); same name -> same color across renders (and matches the SVG export). | Visual. | `trace_analysis.js:979-985`; `flamegraph.js:25` |
| F103. Node rectangles | One filled rect per frame; width = `count/total`, fixed 17px drawn height (`FG_ROW_H - 1`, 18px row), y from stack depth; 0.5px horizontal gaps. | Visual. | `flamegraph.js:350-366` |
| F104. Node labels + truncation | Name centered in the rect only when width > 30px; truncated with ellipsis (`floor((width-10)/7)` chars), clipped to rect bounds via `rect()`+`clip()`; monospace 11px, white. | Visible on wide frames. | `flamegraph.js:368-379` |
| F105. Dynamic canvas sizing | Width = parent `clientWidth * devicePixelRatio`; height = `(maxDepth + 2) * 18 + 8`; CSS style set in logical px, backing store in physical px (retina). Recomputed every render. | Visual; adapts to width/depth. | `flamegraph.js:330-336` |
| F106. Node filtering (< 0.1%) | `flattenFromNode` drops nodes narrower than 0.1% of the total before flattening (optimization affecting which frames exist). | Automatic. | `flamegraph.js:75` |
| F107. Sub-pixel render threshold | Nodes narrower than 0.5px skip both rendering AND hit-region generation. Independent of F106. | Automatic. | `flamegraph.js:354,365` |
| F108. Ancestor + zoom-target bars | When zoomed, ancestors render as full-width context bars at depths 0..N-1 and the zoom target as a full-width bar at depth N, separating context from the zoomed subtree. | Visible when zoomed. | `flamegraph.js:46-70,362` |
| F109. Ancestor dimming (60%) | Ancestor bars render at alpha 0.6 (when not search-dimmed) to read as context. | Visible when zoomed. | `flamegraph.js:356,362` |
| F110. Search dimming (25%) | With an active query, non-matching frames render at 0.25 alpha, matches at 1.0 (ancestors dimmed too). | Type in search. | `flamegraph.js:347-363` |
| F111. Hover highlight dimming | A hovered frame's name becomes `highlightName`; when set, non-matching frames drop to 0.25 alpha (tracked independently of search). | Hover a frame. | `flamegraph.js:358-363,645-663` |
| F112. Search + highlight combined | A frame is dimmed if `(searching && !match) || (highlighting && !highlighted)`. | Hover while searching. | `flamegraph.js:359` |
| F113. Hit-region tracking + hit test | Each rendered frame pushes `{x1,x2,y,node,totalSamples,rootTotal}`; `hitTest()` scans in reverse (top-most wins) for the frame under the cursor; sub-0.5px frames generate no region. | Internal; drives click/hover. | `flamegraph.js:342-383,548-562` |
| F114. Tree sorting by frequency | Children sorted by descending sample count before render, so hot frames sit left. | Visual layout. | `flamegraph.js:86-88,95-97` |
| F115. X/Y coordinate calc | Per node: x = cumulative count / total, width = `count/total`, y = `baseY - (depth+1)*FG_ROW_H`. | Internal positioning. | `flamegraph.js:74-80,353` |
| F116. Tree-node reference | Each flattened node keeps a `treeNode` back-reference used by click-zoom and tooltips. | Internal. | `flamegraph.js:83` |

---

## J. Canvas interactions

Handlers `canvasClick` / `canvasContextMenu` / `canvasMouseMove` / `canvasMouseLeave` (`flamegraph.js:645-713`), registered per canvas (`flamegraph.js:716-726`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F117. Left-click zoom in | Clicking a frame with `children.size > 0` pushes it onto the zoom stack, rebuilds with it as root, updates breadcrumb + URL, and clears any pinned tooltip; leaf frames are a no-op. | Left-click a frame with children. | `flamegraph.js:676-700,527-533` |
| F118. Ancestor-bar re-root | Clicking a full-width ancestor bar (`isAncestor`) replaces the whole zoom stack with that frame (non-linear navigation) rather than pushing. | Click an ancestor context bar. | `flamegraph.js:689-694` |
| F119. Right-click zoom out | Right-click pops one level from the clicked canvas's zoom stack; if empty, falls back to the other canvas's stack; `preventDefault()` suppresses the browser menu; no-op if neither is zoomed. | Right-click a canvas. | `flamegraph.js:702-713` |
| F120. Alt/Option+click pin tooltip | Alt/Option+click pins the tooltip (`pointer-events:auto`, selectable text, clickable links) on any frame regardless of children; stays until unpinned. | Alt/Option+click a frame. | `flamegraph.js:683-685,607-635` |
| F121. Ctrl/Cmd+click docs.rs | Ctrl/Cmd+click opens the frame's `docsUrl` (docs.rs) in a new window (`_blank`) when present; no-op otherwise; suppresses zoom. | Ctrl/Cmd+click a frame with docs. | `flamegraph.js:680-682` |
| F122. Click empty -> unpin | Clicking empty canvas space (no hit) unpins a pinned tooltip; no-op otherwise. | Click a gap between frames. | `flamegraph.js:678` |
| F123. Hover highlight + RAF batching | On mousemove, `hitTest` updates `highlightName` only when it changes, queueing a single repaint per frame via `requestAnimationFrame` (batches rapid moves). | Move the mouse over frames. | `flamegraph.js:645-655` |
| F124. Cursor feedback | Cursor becomes `pointer` over a frame, reverts to default off-frame. | Move over/off a frame. | `flamegraph.js:658,661` |
| F125. Mouse-leave cleanup | On mouseleave, hides the tooltip (unless pinned), clears `highlightName`, and queues a repaint (RAF) to drop the hover state. | Move the mouse off a canvas. | `flamegraph.js:665-674` |
| F126. Pinned-tooltip hover guard | `canvasMouseMove` returns early when `tooltipPinned`, so hover highlight/tooltip do not change while interacting with a pinned tooltip. | Pin a tooltip, then move the mouse. | `flamegraph.js:646` |
| F127. Listener registration | Named per-canvas `mousemove` handlers (`onWorkerMove`/`onOffworkerMove`) plus a shared `mouseleave` handler are registered so `destroy()` can remove them cleanly. | Internal. | `flamegraph.js:716-726` |

---

## K. Tooltip

Built by `buildTooltipHtml` / shown by `showTooltip` (`flamegraph.js:564-635`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F128. Tooltip on hover | Floating tooltip appears over a hovered frame and auto-hides on mouse leave unless pinned. | Hover a frame. | `flamegraph.js:564-605,607-635,645-674` |
| F129. Frame name / full name | Bold short name at top; full qualified name in gray below, shown only when it differs from the short name. | Hover. | `flamegraph.js:572-579` |
| F130. Location + expand toggle | Short `file:line` in gray; when pinned and a full path exists, a right-triangle toggle `(>)` expands to the full path (`(v)`) and collapses again; wired only for pinned tooltips. | Hover to see short path; Alt+click, then click the toggle. | `flamegraph.js:580-593,618-633` |
| F131. Sample count + % | Total samples for the frame and its percentage `(count/total*100)` to 1 decimal. For ancestor bars the percentage is relative to the root total, not the current zoom level. | Hover. | `flamegraph.js:567-571,594` |
| F132. Self count + % | Self-samples (frame at top of stack) and their percentage, using the same total basis as F131. | Hover. | `flamegraph.js:571,594` |
| F133. docs.rs link | When `docsUrl` exists: pinned tooltip shows a clickable `docs.rs` link (`target=_blank rel=noopener`); unpinned shows grayed text with hint `(Ctrl + click)`. | Alt+click to pin, then click the link (or Ctrl/Cmd+click the frame). | `flamegraph.js:595-600` |
| F134. Pin hint | Unpinned tooltips show `Alt + click to pin` (`Option + click` on Mac) in gray at the bottom. | Hover a frame. | `flamegraph.js:601-602` |
| F135. Pin / pointer-events | Pinning sets `tooltipPinned` and `pointer-events:auto` (selectable text, clickable links); unpinned is `pointer-events:none` (mouse passes through). | Alt/Option+click. | `flamegraph.js:609,683-685` |
| F136. Unpin (outside click / Esc) | A document click outside the tooltip (and not consumed by canvas) unpins it (`onDocClick`); Escape also unpins as the first cascade stage. | Click elsewhere, or press Esc. | `flamegraph.js:637-643,742-748,752-755` |
| F137. Positioning + overflow | Fixed position, cursor + 12px offset, aligned to the top of the container to avoid covering the frame, constrained to `min(600px, 100vw-24px)` and shifted off the right edge; `overflow-wrap:anywhere`; z-index 200. | Automatic on hover/pin. | `flamegraph.js:611-616`; `flamegraph.css:155-168` |

---

## L. Breadcrumb navigation

Bar built at `flamegraph.js:291-293`; rendered in `renderBreadcrumb` (`flamegraph.js:427-483`); shown only when zoomed.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F138. Breadcrumb bar | Full-width bar showing the zoom path; hidden when neither tree is zoomed; wraps on narrow viewports. | Appears above the canvases when zoomed. | `flamegraph.js:291-293,427-434`; `flamegraph.css:94-104` |
| F139. Root `(all)` link | First item; clickable link that resets zoom to the full tree for that thread type; blue/purple `#6c63ff`. | Click `(all)`. | `flamegraph.js:450-458` |
| F140. Non-leaf items | Clickable frame names (all but the deepest); truncated at 250px with ellipsis; blue + underline on hover; clicking zooms back to that level (and updates the URL). | Click any non-last crumb. | `flamegraph.js:467-481,474-478`; `flamegraph.css:105-113` |
| F141. Leaf item | Deepest frame name, gray `#aaa`, non-clickable, truncated at 250px, full name on hover. | Rightmost crumb. | `flamegraph.js:467-481`; `flamegraph.css:105-111` |
| F142. Item separators | Right-angle-quote separator (U+203A) between crumbs, dark gray `#555`. | Between crumbs. | `flamegraph.js:462-465`; `flamegraph.css:114` |
| F143. Dual-tree separator | Pipe separator between the worker and off-worker trails, shown only when both trees are zoomed. | When both are zoomed. | `flamegraph.js:438-444` |
| F144. Worker / off-worker trails | Worker trail rendered first (`(all)` + frame names), then the off-worker trail; each reflects its own zoom stack. | Visible per zoomed tree. | `flamegraph.js:437-445` |

---

## M. Zoom state and URL persistence

`createFlamegraph` returns the `fg` API used by the bootstrap script.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F145. Renderer creation | `createFlamegraph(containerEl, updateUrlZoom)` builds the toolbar, help overlay, breadcrumb, both canvases, and wires all listeners; the `updateUrlZoom` callback syncs zoom to the URL. | Automatic during load (`flamegraph.html:619`). | `flamegraph.js:128,147-196,306-317`; `flamegraph.html:608-617` |
| F146. `setData` | `[2026-07-08]` `fg.setData(allSamples, callframeSymbols, { exportTitle, runtimeWorkers })` builds worker (`workerId != 255`) and off-worker (`== 255`) trees, populates the spawn dropdown, builds the runtime dropdown from `opts.runtimeWorkers` (#596, F167), and applies the filters to render. | Called right after creation (`flamegraph.html:620-623`). | `flamegraph.js:829-857` (`setData`), `865-884` (`buildRuntimeFilter`), `779-820` (`applyFilters`) |
| F147. Automatic zoom -> URL | On every zoom change, `updateUrlZoom` encodes each tree's zoom path via `fg.getZoomPath()` and calls `history.replaceState` (no reload). | Any zoom in/out. | `flamegraph.html:608-617`; `flamegraph.js:908-921` |
| F148. `worker-zoom` param | Set to the tab-separated worker zoom path when non-empty, deleted when empty. Enables bookmarking a zoom level. | URL `?worker-zoom=a\tb\tc`. | `flamegraph.html:611-612,627-629`; `flamegraph.js:959-979` |
| F149. `offworker-zoom` param | Same as F148 for the off-worker tree. | URL `?offworker-zoom=a\tb\tc`. | `flamegraph.html:613-614,628-630`; `flamegraph.js:959-979` |
| F150. Zoom-path format | Paths are frame names joined by tab (`\t`), root -> target; if a path cannot be walked child-by-child, `zoomToPath` falls back to a DFS for the last frame name. | Visible in the URL when zoomed. | `flamegraph.html:611,613`; `flamegraph.js:964-979` |
| F151. Conditional zoom restore | On load, restores `worker-zoom`/`offworker-zoom` via `zoomToPath` only when `timeRangeMatched` (F20) is true, because a fallback trace has a different tree. | Load with a zoom param + matching time range. | `flamegraph.html:625-630`; `flamegraph.js:959-979` |
| F152. Zoom param cleanup | Escape-reset (F158) clears both zoom params from the URL, leaving a clean shareable link. | Press Esc to reset zoom. | `flamegraph.html:637-641`; `flamegraph.js:772-774` |
| F153. URL context preservation | Zoom updates use `URLSearchParams`, preserving all other params (`trace`, `start`, `end`, `svc`, `host`, `segs`, `from`, `to`) while touching only the zoom params. | Zoom on any parameterized URL. | `flamegraph.html:609,615` |
| F154. `isZoomed()` | Returns true if either zoom stack is non-empty; used by the escape cascade and other logic. | Internal. | `flamegraph.js:542-544` |
| F155. Resize handler | `window` resize re-renders both canvases to the new width (DPR-aware), reapplying search highlighting; zoom state preserved. | Resize the window. | `flamegraph.html:634`; `flamegraph.js:886-889,323` |
| F156. Destroy / cleanup | `destroy()` removes all listeners (keyboard, mouse, click, context menu), removes the tooltip from the DOM, and clears the container HTML to avoid leaks. | Internal (teardown). | `flamegraph.js:891-906` |

---

## N. Keyboard and Escape cascade

Global keydown wired in the bootstrap (`flamegraph.html:637-641`) delegating to `handleEscape` / `onKeyDown`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F157. Escape cascade | Priority order, stopping at the first action taken (returns true if consumed, false if nothing to dismiss): (1) unpin tooltip; (2) close export menu; (3) close help overlay; (4) clear search; (5) reset zoom. `[2026-07-12]` T20, MIGRATED PAGE (both modes): an OPEN unified `?` overlay (F186) closes ahead of stage 1 - overlay-local Escape while focused, page-listener check otherwise; the widget cascade is byte-identical whenever that overlay is closed. | Press Esc. | `flamegraph.js:752-777`; `flamegraph.html:637-641`; migrated stage `src/pages/flamegraph/fg-keys.ts` (`closeHelpOnEscape`) |
| F158. Cascade stage details | Stage 1 sets `tooltipPinned=false` and hides the tooltip; stage 2 hides the export menu (`aria-expanded=false`); stage 3 hides the help overlay; stage 4 clears the input/`searchQuery`, hides the clear button, `renderAll()`; stage 5 clears both zoom stacks, `renderAll()` + `onZoomChange()`. | Press Esc in the relevant state. | `flamegraph.js:753-775` |
| F159. Container visibility guard | `onKeyDown` returns early when `container.offsetHeight === 0`, so shortcuts (Ctrl/Cmd+F, `/`) do nothing while the flamegraph is hidden. | Implicit; keys inert when hidden. | `flamegraph.js:733` |
| F186. Unified `?` help overlay | `[2026-07-12]` NEW in T20, MIGRATED PAGE ONLY, both modes (`?` was verified dead on the legacy page - K3: `?`, h, F1 all no-ops). Toggles the unified help overlay: the F94 reference table as the Mouse section plus a Keyboard section for the page's live keys (`/` and Cmd/Ctrl+F search, `f`, `z`, Esc cascade, `?`). Escape and backdrop click close (F157 amendment); focus captured/restored. Suppressed while typing in a text field. The widget's own [info]-button overlay (F92-F96) is untouched and still opens independently. | Press `?` outside a text field, after the widget renders. | `src/pages/flamegraph/fg-keys.ts`; component `src/lib/interact/help-overlay.ts` |
| F187. `f` fit (reset zoom, keep search) | `[2026-07-12]` NEW in T20, MIGRATED PAGE ONLY, both modes (K4: `f` was dead). Resets BOTH zoom stacks to root WITHOUT touching the search query (F44's invariant; the Escape cascade cannot do this - it clears search first). Implemented over released widget behaviors only: synthetic contextmenu pops (the public right-click zoom-out, F94) drain the stacks; frozen core untouched. Records ONE undo step (z after f returns to the pre-fit state), settles the URL sync once in exact mode (api mode stays URL-silent, F180), and announces (aria-live + toast). No-op announce when already at root. | Press `f` outside a text field while zoomed. | `src/pages/flamegraph/fg-keys.ts` (`fit`) |
| F188. `z` zoom undo (view history) | `[2026-07-12]` NEW in T20, MIGRATED PAGE ONLY, both modes (K4). Steps back one zoom state through a bounded page-owned history (64 deep, deliberately NOT URL state - see docs/ui-inventory/05-url-view-state.md) recorded from the widget's onZoomChange; restore = pop-to-root then `zoomToPath`, with a reentrancy guard keeping intermediate pops out of the history. Name-path fidelity equals the URL zoom restore (F150's DFS fallback). Survives api-mode refinement polls (paths are name-based, like F181's zoom preservation). Announced with remaining depth; "Nothing to undo" at the baseline. | Press `z` outside a text field after zooming. | `src/pages/flamegraph/fg-keys.ts` (`undoZoom`); stack `src/lib/interact/zoom-history.ts` |

---

## O. Cross-cutting behaviors

App-wide behaviors that are not tied to a single control.

| Behavior | Detail | Source |
| --- | --- | --- |
| F160. Same-origin credential security | Credential headers are attached only to same-origin trace fetches (`isSameOrigin`), across both the streaming and buffered paths, so a cross-origin `?trace=` cannot exfiltrate stored AWS creds. | `trace_parser.js:63-70,94-98,165` |
| F161. Gzip transparency | Both load paths auto-detect the gzip magic and decompress (browser `DecompressionStream`, Node `zlib`), so the parser always sees plain bytes. | `trace_parser.js:31-49,189-208` |
| F162. Shared deterministic coloring | `TraceAnalysis.flamegraphColor` is the single color source for both the on-screen canvases and the exported SVG, so exports match the screen. | `trace_analysis.js:979-985`; `flamegraph_export.js:48-58` |
| F163. RAF-batched repaint | Hover/leave highlight changes queue at most one `requestAnimationFrame` repaint (`repaintQueued`), coalescing rapid mouse events into one redraw per frame. | `flamegraph.js:651-654,667-672` |
| F164. Persistent search + zoom state | `searchQuery` and the two zoom stacks are module-scoped and survive resize, spawn-filter changes, and each other; only explicit user actions (clear button, Escape, filter reset) mutate them. | `flamegraph.js:135,417,772-805` |
| F165. Export reflects filters, not zoom | Exports (SVG + folded) reflect the current spawn-location AND runtime filters (`[2026-07-08]` #596: trees rebuilt by `applyFilters`, the renamed `applySpawnFilter`) but always emit the full, un-zoomed trees. | `flamegraph.js:206-214,225-230,814-817` |

---

## P. Aggregated server-side mode (`?api=1`) `[2026-07-08]`

NEW surface added by #570. `CONDITIONAL`: reachable from the UI only when the
server reports `aggregation_enabled` (the S3 browser's Flamegraph button then
builds this URL - features/01 H3); also reachable by hand-built URL. Instead of
fetching + decoding trace bytes client-side, the page polls the server's
demand-driven `GET /api/flamegraph` refinement loop and renders the pre-built
tree each response carries. The whole mode lives in the bootstrap IIFE
(`flamegraph.html:112-501`); pure helpers (coverage math, UTC picker
conversion, facet options) live in `flamegraph_api.js` (unit-tested in
`test_flamegraph_api.js`). All canvas-level behaviors (sections D, F, I-N:
search, export, zoom, tooltips, breadcrumb, Escape cascade) apply unchanged to
the rendered tree; sections A-C (exact-mode loading, analysis, header) are
bypassed.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F168. API-mode switch | `?api=1` short-circuits the exact-mode loader: F3's no-trace validation never runs, no client decode happens, and the bootstrap returns after wiring the poll loop. Scope comes from URL params: `data_dir` (local-dir mode) OR `bucket`/`prefix`/`service`, repeatable `host`, `start_ns`/`end_ns`, optional facet filters (`source` - default `cpu`, `thread_class`, `spawn_location`) and a `max_files` ceiling. | Open `flamegraph.html?api=1&...` (built by features/01 H3 in agg mode). | `flamegraph.html:113-114,143-175,500` |
| F169. Filter toolbar | Injected below the page header: a data-driven facets span (F170), From/To `datetime-local` pickers (1s step), Apply, "Refine more", and Stop buttons. Present on every api-mode load. | Visible in api mode. | `flamegraph.html:122-134` |
| F170. Data-driven facet selectors | Facet `<select>`s are built from the response `metadata.facets` (name/label/values), NOT hard-coded; rebuilt only when the available set changes (guard avoids clobbering a select mid-interaction). Values are UNIONED monotonically across responses - backend facets are scoped to the current query, so without the union narrowing one dimension would collapse the others and strand the user. Facets with <= 1 value render no control. The `host` facet is special: options via `hostFacetOptions` - "All (N hosts)" re-applies the ORIGINAL scope host set (never broadens past it), a named host narrows to one; seeded from the URL's `host` params so it is correct before the first response. Any facet change resets the `max_files` ceiling, syncs the URL, and restarts polling. | Change a toolbar select (multi-value facets only). | `flamegraph.html:221-305` (`renderFacets`); `flamegraph_api.js:168-174` (`hostFacetOptions`) |
| F171. UTC time pickers | `start_ns`/`end_ns` URL params seed the pickers via `nsToPickerUtc`; queries read them back via `pickerUtcToNs`. Both sides deliberately treat the picker value as UTC wall-clock (S3 trace keys are bucketed in UTC); the appended `Z` on parse keeps it symmetric and timezone-independent. Unit-tested. | From/To fields in the toolbar. | `flamegraph.html:158-165,189-192`; `flamegraph_api.js:94-109`; test `test_flamegraph_api.js` |
| F172. Apply button | Resets the `max_files` ceiling, pushes the new query onto the browser URL (F180), and restarts the poll loop from scratch (loading overlay reappears). | Toolbar -> "Apply". | `flamegraph.html:466-471` |
| F173. Demand-driven poll loop | First poll per scope is READ-ONLY (`refine` omitted): the server instantly returns whatever is already folded. Subsequent polls send `refine=true` (literal `"true"` - the backend param is a serde bool) every 800 ms, each folding a batch of files server-side. A monotonically-bumped `pollToken` cancels superseded loops (filter change / Stop / Refine more); stale in-flight responses are dropped on arrival. | Automatic on load and after every filter change. | `flamegraph.html:353-364` (state), `177-200` (`buildApiUrl`), `384-447` (`poll`), `449-464` (`startPolling`) |
| F174. Loading overlay (api) | Spinner + `Loading aggregated flamegraph...` until the first successful response of each (re)start; the canvas container is hidden meanwhile, so filter changes show the overlay again. Refining polls after the first update in place with no overlay. | Automatic. | `flamegraph.html:408-411,453-464` |
| F175. Scope header from backend truth | `renderScopeHeader` sets the tab title to `Flamegraph - {service}` and the header title to service + host summary (single host name, or "N hosts") + resolved UTC time range - all from response `metadata` (falls back to `aggregated` when no service), NOT from the URL params the client guessed at. | Page header, api mode. | `flamegraph.html:307-326` |
| F176. Stats bar + coverage badge | `baseStats` = total samples, host count (only when > 1), UTC time range + human duration (`formatHumanDuration`). When the response has `coverage`, appends `formatCoverageBadge`: `folded / matched files (pct%) [. folded / matched hosts] . N samples [. bytes]` (host fraction omitted unless hosts_matched > 1), then a state suffix: inline spinner + `refining...`, `refined` (auto-stop, F177), or `stopped` (manual, F179). | Header stats, api mode. | `flamegraph.html:338-351,431-440`; `flamegraph_api.js:28-51`; `format.js:9` (`formatHumanDuration`) |
| F177. Auto-stop heuristics | Refining stops when coverage FREEZES (`files_folded` did not increase between consecutive refining polls - `isCoverageFrozen`) or PLATEAUS (`shouldAutoStopRefining`: 3 consecutive per-poll gains each < 0.5 percentage points, tracked in `coverageDeltas`; the read-only first poll only sets the baseline). Suffix becomes `refined`. | Automatic during refinement. | `flamegraph.html:414-439`; `flamegraph_api.js:56-88` |
| F178. Refine more button | Enabled only while idle. Raises the `max_files` ceiling to `nextMaxFiles(files_folded)` (~4x current fold count, min 16, capped 100000), clears the plateau history so it does not instantly re-stop, and resumes refining immediately (stats spinner reappears in place). | Toolbar -> "Refine more" (after refinement stops). | `flamegraph.html:473-488`; `flamegraph_api.js:116-123` |
| F179. Stop button | Enabled only while refining (`setRefiningUi` swaps the Stop/Refine-more enabled states as a pair). Cancels the loop and freezes the stats at the current coverage with suffix `stopped`. | Toolbar -> "Stop" (during refinement). | `flamegraph.html:367-382,490-495` |
| F180. Browser URL sync (api) | Apply / any facet change rebuilds the full query (`api=1` + scope + facet state + picker times) and `history.pushState`s it - NOTE pushState, so Back walks the filter history (unlike exact-mode zoom's replaceState, F147). The canonical page also `replaceState`s `inspect`/`inspect_full`, carries the focus through scope changes, and retries restoration as streamed trees arrive. Canvas zoom remains NOT URL-synced. | Automatic on Apply/facet change and inspection changes. | legacy `flamegraph.html:202-219,336`; canonical `src/pages/flamegraph/api-mode.ts`, `view-state.ts` |
| F181. Direct tree render | Each response's tree is converted (`toFgTree`) and handed to `fg.setTreeDirect`: ONE panel labeled `All threads - N samples` (em-dash; no worker/off-worker split - the off-worker canvas + label are hidden), spawn AND runtime dropdowns hidden (the toolbar facets replace them), and the current zoom target is PRESERVED across refinement polls by re-resolving its frame name in the new tree via DFS (zoom does not reset as coverage grows). | Automatic per poll. | `flamegraph.html:328-334,412`; `flamegraph.js:982-1005` (`setTreeDirect`) |
| F182. API error handling | A failed poll (HTTP error body or network failure) shows `Failed to load flamegraph: <msg>` via the standard error element; superseded polls (token mismatch) are dropped silently. There is NO api-mode analogue of the 401-credentials hint (F12). | Automatic on poll failure. | `flamegraph.html:388-398` |
| F183. No-coverage response (legacy/local-dir) | When a response carries no `coverage` (older server / `data_dir` mode), the page renders it once with plain `baseStats` and stops - single-fetch behavior, no refinement loop. | api mode against a `data_dir`/legacy server. | `flamegraph.html:414-420` |
| F184. Credential headers (api) | Every poll spreads `Dial9Creds.headers()` into the fetch, so BYO creds ride to `/api/flamegraph` (same-origin by construction; no `isSameOrigin` gate needed). | Automatic when creds stored. | `flamegraph.html:390-391` |
| F185. Escape / resize wiring (api) | Same window-level wiring as exact mode: resize re-renders via `fg.resize()`; Escape runs the renderer cascade (F157) - with the api-only stages being tooltip/export/help/search/zoom exactly as in exact mode. | Resize / press Esc. | `flamegraph.html:498-499` |

Notes:

- `flamegraph_api.js` also exports `sourceFacetOptions` / `threadFacetOptions`
  (`flamegraph_api.js:139-162`), but at HEAD NO page consumes them - the
  generic facet renderer builds `All` + raw values inline and only the host
  facet uses a helper. They are exercised solely by `test_flamegraph_api.js`.
  Status: `CODE-ONLY` (dead-helper candidates for the redesign).
- The refinement loop's server side is `dial9-viewer/src/server/flamegraph.rs`
  (#570, assume-role support #597); scope/facet semantics are backend-owned
  and only the response contract matters to this page.

---

## 2026-07-08 refresh (drift commits #570/#596/#600)

Method: dev-server on :3001 (`PORT=3001 cargo run -p dial9-viewer --bin
dev-server --features dev-server`), driven with `curl` for the API contract +
Node for parser-level facts + code read for DOM behavior. NO browser driver
this pass: DOM-interaction verdicts are CODE-READ (weaker than driven
verification; re-derivable by the T12 row-walker). Local `node` runs (all
green): `test_flamegraph_api.js` (48 passed), `test_fetch_traces.js`,
`test_parse_yield_throttle.js`, `test_runtime_groups.js`.

Dev-server facts observed: `/api/config` -> `aggregation_enabled:true`.
`GET /api/flamegraph?bucket=demo-traces&prefix=traces` (read-only first poll)
returned instantly with an empty tree and `coverage.files_folded:0` plus the
generic `metadata.facets` array (source/thread_class/host/spawn_location, all
values empty pre-fold); the same URL with `refine=true` folded the demo trace
(`total_samples:147`, coverage `1 / 1 files`, `1 / 1 hosts`,
`total_bytes:4336378`, facets populated: source `[cpu, sched]`, thread_class
`[worker]`, host `[local]`, spawn_location x4); a subsequent read-only poll
returned the folded tree. The demo trace has ZERO named runtimes
(`trace.runtimeWorkers` is an empty Map; `computeRuntimeGroups` yields one
inferred `main` group of 2 workers), so all multi-runtime UI is not
exercisable on this seed data.

| Row | Verdict | Evidence / note |
|---|---|---|
| F1/F4-F6/F9 (amended, #600) | CODE-READ + unit-tested | streaming decision read at `flamegraph.html:508-535`; `fetchTracesStream` behavior covered by `test_fetch_traces.js` (concat parity, order, concurrent dispatch, late-failure). |
| F166 multi-URL pipelined streaming | CODE-READ + unit-tested | same tests; browser load with repeated `trace=` not driven this pass. |
| F167 runtime filter | NOT-TRIGGERABLE (dev data) | demo trace has no named runtimes -> dropdown stays hidden; `buildRuntimeFilterData` unit-tested (`test_runtime_groups.js` green); code read at `flamegraph.js:865-884,779-820`. |
| F146/F165 (amended, #596) | CODE-READ | `setData` opts + `applyFilters` rename read at `flamegraph.js:829-857,779-820`. |
| F168 api-mode switch | VERIFIED (API) + CODE-READ | endpoint contract walked (above); DOM short-circuit read at `flamegraph.html:113-114,500`. |
| F169 toolbar | CODE-READ | markup built at `122-134`; not driven. |
| F170 data-driven facets | VERIFIED (API) + CODE-READ | generic `metadata.facets` array observed empty -> populated across polls (exactly the shape `renderFacets` consumes); union/rebuild logic read at `238-305`; selects not driven. |
| F171 UTC pickers | VERIFIED (unit) | `nsToPickerUtc`/`pickerUtcToNs` covered by `test_flamegraph_api.js`. |
| F172 Apply | CODE-READ | `466-471`; not driven. |
| F173 poll loop | VERIFIED (API) | read-only poll instant + empty (`files_folded:0`); `refine=true` folded (147 samples); post-refine read-only returned the folded tree. Token cancellation CODE-READ (`353-364,395,399`). |
| F174 loading overlay (api) | CODE-READ | `408-411,453-464`. |
| F175 scope header | CODE-READ | metadata fields (`service`, `host_names`... as `hosts`, `min/max_timestamp_ns`) observed in the response; render not driven. |
| F176 stats + coverage badge | VERIFIED (unit + API) | badge inputs observed on the wire (incl. `total_bytes`, `hosts_matched/folded`); `formatCoverageBadge` unit-tested. |
| F177 auto-stop | VERIFIED (unit) + API-consistent | `isCoverageFrozen`/`shouldAutoStopRefining` unit-tested; the dev seed freezes after one refine (1/1 files - a second refine cannot increase `files_folded`). |
| F178 Refine more | CODE-READ + unit | `nextMaxFiles` unit-tested; button flow `473-488` not driven. |
| F179 Stop | CODE-READ | `367-382,490-495`. |
| F180 URL sync (api) | CODE-READ | `202-219`; pushState semantics read, not driven. |
| F181 setTreeDirect | CODE-READ | `flamegraph.js:982-1005`; zoom-preservation DFS read. |
| F182 api errors | CODE-READ | `388-398`. |
| F183 no-coverage response | NOT-TRIGGERABLE (dev data) | dev-server always returns `coverage`; `data_dir`/legacy path read at `414-420`. |
| F184 cred headers (api) | CODE-ONLY | header spread at `390-391`; not asserted on the wire. |
| F185 escape/resize (api) | CODE-READ | `498-499`. |

Anchor spot-checks against HEAD (beyond the rows above): `createFlamegraph`
128, search-bar build 148-166, `onSearchInput` 520-525, clear button 282-289,
`zoomTo` 527-533, breadcrumb 291-293/427-434, `handleEscape` 752, `resize`
886-889, `destroy` 891-906, `getZoomPath` ~908, `zoomToPath` 959-981,
`applyFilters` 779-820, `setTreeDirect` 982-1005 - all match the cited rows.
