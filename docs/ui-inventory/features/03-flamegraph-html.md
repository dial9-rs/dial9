# UI Feature Inventory: `flamegraph.html` (CPU-profile flamegraph viewer)

> Code-derived inventory of the standalone flamegraph surface, with per-feature verification verdicts (`CONFIRMED` = matched code as documented; `CORRECTED` = re-derived from code after an initial mis-read). Purpose: capture every existing behavior precisely enough that (a) each can be validated in the running UI and (b) the surface can be re-implemented without losing anything. Source line numbers are a snapshot; the function name is the stable anchor.

## What this surface is

The standalone CPU-profile flamegraph. It is opened from the S3 browser (or via a hand-built URL) with one or more `?trace=` components, an optional `?start=/?end=` nanosecond window, and optional title metadata. It fetches and decodes the trace client-side, builds two flame trees (worker threads vs off-worker sampler thread), and renders them on interactive canvases with search, spawn-location filtering, zoom, tooltips, and SVG / folded-stacks export.

- Entry file: `dial9-viewer/ui/flamegraph.html` (markup + inline `<style>` + inline bootstrap `<script>`)
- Loaded modules: `decode.js`, `trace_parser.js`, `creds.js`, `trace_analysis.js`, `flamegraph_export.js`, `flamegraph.js`, stylesheet `flamegraph.css`
- Backend endpoints consumed: none directly. It fetches whatever `?trace=<url>` values it is handed (typically `/api/object?bucket&key` URLs produced by the S3 browser). AWS credential headers (`x-dial9-aws-*`) ride along on same-origin fetches only.

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

The inline IIFE in `flamegraph.html` (`81-229`) parses URL params, fetches + decodes the trace, then hands off to `FlamegraphRenderer`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. `?trace=` param parsing | Reads the repeatable `trace` query param via `getAll`. Each value is a separate (possibly gzipped) component to fetch and concatenate. A single value goes down the streaming path; multiple values force the buffered path. | URL param `?trace=<url>` (repeatable: `?trace=a&trace=b`). | `flamegraph.html:85`; `trace_parser.js:90` |
| F2. `start` / `end` ns params | Reads optional `start` and `end` as `Number` nanoseconds (null if absent). Drive CPU-sample time-window filtering and the duration/label logic. | URL params `?start=<ns>&end=<ns>` (numeric, optional). | `flamegraph.html:86-87` |
| F3. No-trace-URL validation | If `getAll("trace")` is empty, shows error `No trace URL provided. Use ?trace=<url>&start=<ns>&end=<ns>` and returns early (no load attempt). | Open the page with no `?trace=` param. | `flamegraph.html:101-104` |
| F4. Streaming vs buffered decision | Single URL + `canStreamDecode()` -> streaming path (download/decode overlap). Multiple URLs or no streaming support -> buffered path (fetch all, concatenate, parse once). Transparent to the user but changes the progress text. `canStreamDecode()` checks `ReadableStream` + `DecompressionStream` support. | Automatic on load. | `flamegraph.html:114,118`; `trace_parser.js:126-129` |
| F5. Single-URL streaming path | `fetchTraceStream()` fetches, peeks the first chunk for the gzip magic `0x1f 0x8b`, pipes through `DecompressionStream('gzip')` if gzipped, and returns an async iterable; `parseTraceStream()` drains complete frames incrementally so download and decode overlap. | Automatic when `traceUrls.length === 1 && canStreamDecode()`. | `flamegraph.html:114-117`; `trace_parser.js:164-224,996-1002` |
| F6. Multi-URL buffered path | `fetchTraces()` fetches all URLs in parallel, gunzips each component independently, concatenates into one `ArrayBuffer`; `parseTrace()` decodes once. Same-origin credential rule applies per URL. | Automatic when `traceUrls.length > 1 || !canStreamDecode()`. | `flamegraph.html:118-125`; `trace_parser.js:90-118` |
| F7. Gzip auto-detection | Streaming: `fetchTraceStream()` sniffs the gzip magic and conditionally pipes through `DecompressionStream`. Buffered: `maybeGunzip()` checks the first 2 bytes and decompresses via `DecompressionStream` (browser) or `zlib.gunzipSync` (Node). Caller always receives uncompressed bytes. | Automatic; no user control. | `trace_parser.js:31-49,189-208` |
| F8. AWS credential attachment | If `window.Dial9Creds` is present, `Dial9Creds.headers()` supplies `x-dial9-aws-*` headers (accessKeyId, secretAccessKey, sessionToken, region). `isSameOrigin()` withholds them from cross-origin (or unparseable) trace URLs to prevent exfiltration via a crafted `?trace=https://attacker/` link; off-browser (Node tests) all URLs are treated as same-origin. | Automatic; applied to trace fetches when creds are stored. | `flamegraph.html:111`; `creds.js:240-249`; `trace_parser.js:63-70,94-98,165` |
| F9. Loading indicator + phase text | `#loading` element shows centered progress text through phases: `Loading trace...` (streaming), `Fetching trace...` / `Fetching N traces...` (buffered), `Parsing trace...`, then `Analyzing...`. | Automatic during load. | `flamegraph.html:71,115,119-123,140` |
| F10. Loading visibility toggle | `.hidden` on `#loading` flips `display:flex` -> `display:none`. Added by `showError()` and once on successful completion; never removed. | Automatic (error or success). | `flamegraph.html:42-50` (CSS), `96`, `195` |
| F11. Error indicator | `#error` element (red `#ff6b6b`, initially `display:none`) is shown via `showError()`, which hides loading, sets `display:flex`, and sets `textContent`. | Automatic on error; reload/navigate to dismiss. | `flamegraph.html:52-62` (CSS), `95-99` (`showError`) |
| F12. HTTP 401 credentials error | `CONDITIONAL`. If a fetch error message matches `HTTP 401` AND `window.Dial9Creds` exists AND `!Dial9Creds.has()`, shows the specific message `This trace requires AWS credentials. Open it from the dial9 home page after applying your credentials.` instead of the generic error. | Fetch a credentialed trace with no creds applied. | `flamegraph.html:127-131`; `creds.js:61-64` |
| F13. Generic fetch/parse error | Any other error from the fetch/parse stage shows `Failed to load trace: <err.message>`. | Any load/parse failure (HTTP, malformed data, network). | `flamegraph.html:126-135` |

---

## B. Trace analysis pipeline

Runs after decode, before rendering (`flamegraph.html:138-171`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F14. Analysis progress | Sets `#loading` text to `Analyzing...` for the span-building + sample-attachment phase. | Automatic, just before render. | `flamegraph.html:140` |
| F15. Worker ID extraction | Iterates `trace.events`, collecting `workerId`s, excluding events with `eventType === QueueSample (4)` or `WakeEvent (9)`; sorts numerically ascending. Partitions downstream per-worker span building. | Internal. | `flamegraph.html:142-147`; `trace_parser.js:290-291` |
| F16. Worker span building | `TraceAnalysis.buildWorkerSpans(events, workerIds, maxTs)` builds per-worker polls, parks, actives, and `cpuSampleTimes`. `blockInPlaceGaps` (4th arg) is not passed, so gap-based span filtering is disabled here. | Internal. | `flamegraph.html:149`; `trace_analysis.js:438` |
| F17. CPU sample attachment | `CONDITIONAL` (only if `trace.cpuSamples.length > 0`). `attachCpuSamples(cpuSamples, workerSpans)` binary-searches each sample into the poll it landed in, setting `sample.spawnLoc` (or null) and `sample.inPoll`. Enables spawn-location annotation. | Automatic when the trace has CPU samples. | `flamegraph.html:150-152`; `trace_analysis.js:636-684` |
| F18. Span-build error recovery | Wraps span-build + attach in try/catch; on exception logs `console.warn("Failed to attach spawn locations:", err)` and continues. Non-fatal: the flamegraph still renders, just without `spawnLoc`. | Automatic; observable in console. | `flamegraph.html:153-156` |
| F19. CPU sample time-range filter | `FlamegraphRenderer.filterCpuSamples(cpuSamples, startNs, endNs)` drops (1) empty-callchain samples, (2) `source===1` scheduler samples, (3) samples before `startNs` (if set), (4) samples after `endNs` (if set). With both null, no time filter is applied. Result populates `allSamples`. | Triggered by `?start=`/`?end=`; both optional. | `flamegraph.html:160`; `flamegraph.js:120-125` |
| F20. Time-range match + fallback | `timeRangeMatched = allSamples.length > 0`. If false AND (`startNs != null` OR `endNs != null`), re-runs the filter with `null,null`, logs `console.warn("Time range filter matched 0 samples - showing all N samples")`, and uses the unfiltered set (graceful degradation). `timeRangeMatched` also gates zoom restoration (F55). | Automatic when `?start`/`?end` do not intersect the trace. | `flamegraph.html:161-167,213` |
| F21. No-samples error | If both the filtered and unfiltered sets are empty, shows `No CPU samples found in the specified time range.` and returns (halts render). Means the trace has no CPU profiling data at all. | Automatic when there are zero CPU samples. | `flamegraph.html:168-170` |

---

## C. Page header: title and stats bar

Populated once during init from URL params (`flamegraph.html:173-196`). The stats bar does not refresh on zoom.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F22. Browser tab title | Sets `document.title` to `Flamegraph - {label}` (em-dash separator, U+2014) plus ` (X.XXms)` when `start`+`end` are both present. | Browser tab/window title. | `flamegraph.html:186` |
| F23. Page header title | Sets `#fg-title` (top-left, purple/bold) to `Flamegraph - {label}` (same label, no duration suffix). | Top-left of the page header. | `flamegraph.html:187` |
| F24. Service/host label construction | Builds `label`: if `svc` param is present, uses `svc` and appends ` @ {host}` only when `host` is also present; otherwise falls back to the last path segment of the first trace URL, or `trace` if empty. | Applied to F22 and F23. | `flamegraph.html:175-176,180-182` |
| F25. Sample count display | First stats item, always present: `N samples`, where `N` is `allSamples.length` after time-range filtering (reflects the fallback in F20 when the window did not match). | Right side of header, first stat. | `flamegraph.html:160-171,188` |
| F26. Segment count display | `CONDITIONAL` (only if `?segs=` present and non-empty). Shows `N segment` / `N segments` with plural handling keyed on `segs !== "1"`. Purely informational. | URL param `?segs=<n>`. | `flamegraph.html:177,189` |
| F27. Time range display | `CONDITIONAL` (only if `?from=` present). Shows `FROM -> TO` (renders as U+2192) when `to` is present and differs from `from`; otherwise shows just `FROM`. | URL params `?from=...&to=...`. | `flamegraph.html:178-179,190` |
| F28. Duration display | `CONDITIONAL` (only if both `start` and `end` present). Shows `X.XXms selected`, computed as `((endNs - startNs) / 1e6).toFixed(2)`. Shown even when the window later fails to match samples. | URL params `?start=<ns>&end=<ns>`. | `flamegraph.html:183-185,191` |
| F29. Time-range mismatch warning | `CONDITIONAL` (only when `timeRangeMatched === false`). Appends the last stat `full trace - selected region could not be reproduced` (em-dash) to signal the F20 fallback to all samples. | Last stat item when `?start`/`?end` did not match. | `flamegraph.html:161-171,192` |
| F30. Stats bar layout / separator | `#fg-stats` joins the conditional bits in order (samples, segs, time range, duration, warning) with a middle-dot separator (U+00B7, ` . `) into a single span. | Second span in the header, right side. | `flamegraph.html:68,188-193` |

---

## D. Search bar (frame search)

The toolbar is built by `createFlamegraph()` (`flamegraph.js:143-160`). Search state lives in the module-scoped `searchQuery` (`flamegraph.js:135`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F31. Search input field | 260px text input that filters frames by case-insensitive substring on `name` or `treeNode.fullName`. Live, synchronous, no debounce; empty query clears filtering. Operates across both trees at once. | Click the field, or focus via Ctrl/Cmd+F or `/`. | `flamegraph.js:143-148,163,513-518` |
| F32. Placeholder + platform hint | Placeholder reads `Search frames... (Cmd + F or /)` on Mac/iPhone/iPad (Cmd symbol U+2318, detected via `navigator.platform` regex) or `Search frames... (Ctrl + F or /)` elsewhere. Informational only. | Visible in the empty field. | `flamegraph.js:145-148` |
| F33. Live update handler | On each `input` event, `onSearchInput()` copies value to `searchQuery`, toggles the clear button, and triggers a full repaint (canvas + stats). No debounce; large traces may lag under fast typing. | Type in the field. | `flamegraph.js:513-518` (`onSearchInput`) |
| F34. Name + fullName matching | Match logic: `name.toLowerCase().includes(q)` OR (`treeNode.fullName` exists AND its lowercase includes `q`). No regex; special chars are literal. Rust-style qualified names live in `fullName`. | Type a short name (`Vec`) or qualified path (`std::vec::Vec`). | `flamegraph.js:106-118` (`countSearchMatches`), `350` |
| F35. Search respects zoom level | `updateSearchStats()` counts matches under the current zoom root (`zoomStack[-1]` if zoomed, else tree root), aggregating both trees independently. `% of samples` is thus relative to the zoomed subtree. | Zoom into a frame, then search. | `flamegraph.js:478-511,487-500` |
| F36. Clear button (x) | An x-glyph button (U+00D7) that clears the input and `searchQuery`, hides itself, repaints, and refocuses the input. | Click the x (visible only when a query is active). | `flamegraph.js:149,164,275-282` |
| F37. Clear button visibility | Starts hidden; toggled on every input event via `display = searchQuery ? "" : "none"`; also hidden on clear/Escape. | Appears when the field has text. | `flamegraph.js:275,516` |
| F38. Search statistics display | Shows `N frame`/`N frames` (singular/plural), optionally ` . X.X% of samples` when `matchedSelf > 0` and `totalSelf > 0` (matched self-samples over total self-samples across both panels). Shows `no matches` on zero, blank when the query is empty. Recomputed every repaint. | Auto-updates in the toolbar as you type. | `flamegraph.js:150,165,478-511` |
| F39. No-match handling | Query with zero matches shows exactly `no matches`; all frames stay visible but dimmed to alpha 0.25. No modal/error. | Search for a string that matches nothing. | `flamegraph.js:501-503` |
| F40. Frame dimming on search | While a query is active, matching frames render at alpha 1.0 and non-matching frames at 0.25 (ancestor bars included). Immediate, no animation. | Type in the field; canvas re-renders instantly. | `flamegraph.js:340-356` |
| F41. Focus shortcuts (Ctrl/Cmd+F, /) | Global keydown: Ctrl/Cmd+F always `preventDefault()` + focus + select-all (even if the field is already focused, so you can replace the query). `/` does the same but only when the field is not the active element (so `/` can be typed into the query). Guarded by container visibility (F86). | Press Ctrl/Cmd+F or `/` on the page. | `flamegraph.js:725-733` (`onKeyDown`) |
| F42. Text selection on keyboard focus | Keyboard-focus paths call `searchInput.select()` so existing text is highlighted for immediate replacement. A direct mouse click does not auto-select. | Focus via Ctrl/Cmd+F or `/`. | `flamegraph.js:729-730` |
| F43. Search across both trees | Dimming and stats apply to `workerTree` and `offworkerTree` simultaneously with the same query; an empty tree is skipped. | Type; results span both sections. | `flamegraph.js:478-510` |
| F44. Search query persistence | `searchQuery` survives zoom, resize, and spawn-filter changes; only the clear button or Escape resets it. Resetting zoom does NOT clear search. | Search, then zoom/filter/resize -> search stays active. | `flamegraph.js:135,417` |
| F45. Search bar styling | `.fg-search-bar` is a flex row (`gap:8px`, bg `#16213e`, bottom border, `flex-shrink:0`). Input: dark `#2a2a4a`, 1px `#444`, 4px radius. | Visible at the top of the flamegraph container. | `flamegraph.css:3-31`; `flamegraph.js:143-160` |
| F46. Focus visual indicator | On focus, the input border changes from `#444` to `#6c63ff` (purple). | Focus the search field. | `flamegraph.css:22` |

---

## E. Spawn-location filter

Populated by `setData()`; applied by `applySpawnFilter()` (`flamegraph.js:772-805`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F47. Spawn filter dropdown | `<select>` in the toolbar that filters all samples to a single task spawn location. | Click the dropdown; select an option. | `flamegraph.js:151,166,807`; `flamegraph.css:32-40` |
| F48. All-tasks default option | First option, value `""`, text `All tasks (N samples)` where `N` is the full sample count; resets the filter. | Default selection. | `flamegraph.js:828` |
| F49. Individual location options | One option per unique spawn location: `short_path (count)` (directory prefix stripped), full path in the option `title` tooltip; count reflects the current view. | Options after the default. | `flamegraph.js:829-837,834` |
| F50. Sorting + unknown grouping | Options sorted by count descending (`sort((a,b) => b[1]-a[1])`); samples with no location grouped under `(unknown)` (`s.spawnLoc || "(unknown)"`). | Dropdown order. | `flamegraph.js:825,829` |
| F51. Filtering by location | On `change`, keeps only samples matching the selected location (empty value = all). | Select an option. | `flamegraph.js:773-776,807` |
| F52. Tree rebuild + zoom reset | On filter change, rebuilds both worker/off-worker trees from the filtered samples and resets both zoom stacks to root. Search query is NOT cleared (still filters the new trees). | Change the dropdown. | `flamegraph.js:781-789` |
| F53. Section-label count update | Updates the worker / off-worker section labels to `{prefix} - {count} samples` (em-dash separator; prefixes default `Worker threads` / `Off-worker (sampler thread)`); shows `0 samples` when filtered to nothing. | Labels above each canvas. | `flamegraph.js:791-794,809-818` |
| F54. Export availability on change | Recomputes export enabled/disabled from the filtered trees and closes any open export menu to avoid a stale dataset. | Automatic on filter change. | `flamegraph.js:799-802` |
| F55. Spawn-location attachment | Each CPU sample is enriched with `spawnLoc` during analysis (F17) by binary-searching its timestamp into a task poll interval; unmatched samples get `spawnLoc=null` -> `(unknown)`. | Internal, during load. | `trace_analysis.js:636-667`; `flamegraph.html:151` |
| F56. Dropdown styling | Dark `#2a2a4a` bg, `#444` border, 4px radius, max-width 350px, `0.9em`. No explicit hover style. | Visual. | `flamegraph.css:32-40` |

---

## F. Export menu

Toggle + menu built in `createFlamegraph()`; wiring at `flamegraph.js:218-273`. Requires `flamegraph_export.js`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F57. Export button toggle | `[down-arrow] Export` button (aria-expanded) that opens/closes the format menu; disabled (opacity 0.4, cursor default, title `No samples to export`) when there is no exportable data; `stopPropagation` prevents parent handlers. | Click `Export` in the toolbar. | `flamegraph.js:153-154,236-242`; `flamegraph.css:54-65` |
| F58. Export menu dropdown | Absolutely-positioned menu (`role=menu`, min-width 170px, dark `#16213e`, shadow) below the button, hidden by default, with two items (SVG, folded stacks). | Opens on toggle. | `flamegraph.js:155-158`; `flamegraph.css:66-77` |
| F59. Menu dismiss | Closes on: another toggle click, Escape (F84), clicking outside the search bar (`onExportOutsideClick`), opening the help overlay, or a spawn-filter change. | Click outside / Escape / open help / change filter. | `flamegraph.js:192,268,271-273,802` |
| F60. Interactive SVG export | Generates and downloads a standalone interactive SVG (zoom, Ctrl-F regex search, Ctrl-I case toggle, Reset Zoom, `?x=&y=&s=` URL state, self-contained). Filename `filenameStem(title) + ".svg"`. Menu closes after click. | `Export` -> `Interactive SVG (.svg)`. | `flamegraph.js:156,244-251`; `flamegraph_export.js:432-564` |
| F61. Folded stacks export | Generates and downloads folded stacks text (`frame1;frame2;frame3 count`), consumable by inferno / flamegraph.pl / speedscope. Exports the FULL tree (no visual/zoom pruning); self-weight per full path; `(all)` root omitted; counts rounded to integers; only frames with `self > 0` emitted; children sorted by descending count. Filename `filenameStem(title) + ".folded.txt"`. | `Export` -> `Folded stacks (.txt)`. | `flamegraph.js:157,253-265`; `flamegraph_export.js:75-96` |
| F62. Export data availability | `hasExportableData()` returns true iff `workerTree.count > 0` OR `offworkerTree.count > 0`; drives the disabled state and blocks export of empty datasets. | Observe button state after load/filter. | `flamegraph.js:225-227,238,799-802` |
| F63. Filename sanitization | `filenameStem()` strips a leading `Flamegraph -` prefix, replaces non-alphanumerics with `_`, trims leading/trailing dots/underscores (no dotfiles), and falls back to `flamegraph` if empty. Applied to both exports. | Automatic on export. | `flamegraph_export.js:569-575` |
| F64. Custom value formatter | SVG export uses `exportFormatValue` (from `setData` options) for hover-title weights; defaults to `N samples` with commas. Other views (heap, etc.) can pass bytes/allocs formatters. | Set via `exportFormatValue`; visible in SVG hover text. | `flamegraph.js:206,816-820`; `flamegraph_export.js:427-439,532` |
| F65. Menu item hover effect | SVG / folded menu items highlight to bg `#2a2a4a`, text `#fff` on hover. | Hover a menu item. | `flamegraph.css:91` |
| F66. Export module optional | `CONDITIONAL`. If `flamegraph_export.js` fails to load, the entire export wrap is hidden and a `console.warn` is logged once; the rest of the UI degrades gracefully. | Check console if export controls are missing. | `flamegraph.js:211-216` |
| F67. Multi-panel merge/wrapping | `buildExportRoot` synthesizes a single root when both panels exist, wrapping each tree in a labeled frame (e.g. `[Worker threads]`); folded output prepends `# label` comment headers per panel and skips empty panels. | Automatic when both panels have data. | `flamegraph.js:218-222,258-262`; `flamegraph_export.js:108-126` |
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

Built in `createFlamegraph()` (`flamegraph.js:160,173-197`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F92. Help button | `[info]` icon button (`tabindex=0`, `role=button`, title `Keyboard shortcuts`) that closes any open export menu, then toggles the overlay between `flex` and `none`. Hover -> text `#fff`, border `#888`. | Click the `[info]` button in the toolbar. | `flamegraph.js:160,171,191-194`; `flamegraph.css:51` |
| F93. Help overlay modal | Dark semi-transparent overlay (`rgba(0,0,0,0.5)`, z-index 100) covering the container, with a centered box titled `[keyboard] Flamegraph Shortcuts` and a shortcuts table; hidden by default. | Opens on the help button. | `flamegraph.js:173-189`; `flamegraph.css:131-152` |
| F94. Keyboard reference table | Rows: `Click -> Zoom into frame`; `Option/Alt + click -> Pin tooltip`; `Cmd/Ctrl + click -> Open docs.rs`; `Right-click -> Zoom out one level`; `Cmd/Ctrl + F or / -> Search frames`. Non-interactive reference. Modifier symbol is platform-aware (Cmd U+2318 on Mac, else Ctrl). | Open the overlay. | `flamegraph.js:175-187,145,181,183` |
| F95. Dismiss instructions | Footer text `Press Esc or click outside to close` (`#666`, 0.78em). | In the overlay. | `flamegraph.js:186`; `flamegraph.css:152` |
| F96. Click-outside close | Clicking the dark background (target is the overlay itself, not the content box) closes the overlay. | Open the overlay, click outside the box. | `flamegraph.js:195-197` |

---

## I. Flamegraph canvases: rendering

Two stacked canvases built at `flamegraph.js:290-310`; drawing in `renderCanvas`/`flattenFromNode` (`flamegraph.js:45-100,316-376`). Colors from `TraceAnalysis.flamegraphColor`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F97. Worker canvas | Top canvas rendering samples from worker threads (`workerId != 255`); height scales with tree depth; frames are rectangles with width proportional to sample count; ancestor context bars shown when zoomed. | Below the `Worker threads` label. | `flamegraph.js:299-301,415` |
| F98. Off-worker canvas | Bottom canvas rendering the sampler thread (`workerId == 255`); a separate tree with identical rendering/interaction. | Below the `Off-worker (sampler thread)` label. | `flamegraph.js:308-310,415` |
| F99. Section labels | `Worker threads - N samples` and `Off-worker (sampler thread) - N samples` (em-dash), counts updated on spawn-filter change; auto-hide when filtered to zero. | Above each canvas. | `flamegraph.js:294-297,303-306,791-794` |
| F100. Scrollable body | Both canvases sit in a `flex` column `.fg-body` with `overflow-y:auto`; each canvas is independently sized. | Scroll the canvas area. | `flamegraph.js:290-292`; `flamegraph.css:115-120` |
| F101. Canvas background fill | Fills `#1a1a2e` before drawing, every repaint. | Visual. | `flamegraph.js:332-333` |
| F102. Deterministic frame coloring | `flamegraphColor(name)` maps each name to a stable HSL (hue 10-49, sat 60-89%, light 40-54%); same name -> same color across renders (and matches the SVG export). | Visual. | `trace_analysis.js:979-985`; `flamegraph.js:24` |
| F103. Node rectangles | One filled rect per frame; width = `count/total`, fixed 17px drawn height (`FG_ROW_H - 1`, 18px row), y from stack depth; 0.5px horizontal gaps. | Visual. | `flamegraph.js:343-359` |
| F104. Node labels + truncation | Name centered in the rect only when width > 30px; truncated with ellipsis (`floor((width-10)/7)` chars), clipped to rect bounds via `rect()`+`clip()`; monospace 11px, white. | Visible on wide frames. | `flamegraph.js:361-372` |
| F105. Dynamic canvas sizing | Width = parent `clientWidth * devicePixelRatio`; height = `(maxDepth + 2) * 18 + 8`; CSS style set in logical px, backing store in physical px (retina). Recomputed every render. | Visual; adapts to width/depth. | `flamegraph.js:323-329` |
| F106. Node filtering (< 0.1%) | `flattenFromNode` drops nodes narrower than 0.1% of the total before flattening (optimization affecting which frames exist). | Automatic. | `flamegraph.js:74` |
| F107. Sub-pixel render threshold | Nodes narrower than 0.5px skip both rendering AND hit-region generation. Independent of F106. | Automatic. | `flamegraph.js:347,358` |
| F108. Ancestor + zoom-target bars | When zoomed, ancestors render as full-width context bars at depths 0..N-1 and the zoom target as a full-width bar at depth N, separating context from the zoomed subtree. | Visible when zoomed. | `flamegraph.js:45-69,355` |
| F109. Ancestor dimming (60%) | Ancestor bars render at alpha 0.6 (when not search-dimmed) to read as context. | Visible when zoomed. | `flamegraph.js:349,355` |
| F110. Search dimming (25%) | With an active query, non-matching frames render at 0.25 alpha, matches at 1.0 (ancestors dimmed too). | Type in search. | `flamegraph.js:340-356` |
| F111. Hover highlight dimming | A hovered frame's name becomes `highlightName`; when set, non-matching frames drop to 0.25 alpha (tracked independently of search). | Hover a frame. | `flamegraph.js:351-356,638-656` |
| F112. Search + highlight combined | A frame is dimmed if `(searching && !match) || (highlighting && !highlighted)`. | Hover while searching. | `flamegraph.js:352` |
| F113. Hit-region tracking + hit test | Each rendered frame pushes `{x1,x2,y,node,totalSamples,rootTotal}`; `hitTest()` scans in reverse (top-most wins) for the frame under the cursor; sub-0.5px frames generate no region. | Internal; drives click/hover. | `flamegraph.js:335-376,541-555` |
| F114. Tree sorting by frequency | Children sorted by descending sample count before render, so hot frames sit left. | Visual layout. | `flamegraph.js:85-87,94-96` |
| F115. X/Y coordinate calc | Per node: x = cumulative count / total, width = `count/total`, y = `baseY - (depth+1)*FG_ROW_H`. | Internal positioning. | `flamegraph.js:73-79,346` |
| F116. Tree-node reference | Each flattened node keeps a `treeNode` back-reference used by click-zoom and tooltips. | Internal. | `flamegraph.js:82` |

---

## J. Canvas interactions

Handlers `canvasClick` / `canvasContextMenu` / `canvasMouseMove` / `canvasMouseLeave` (`flamegraph.js:638-706`), registered per canvas (`709-719`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F117. Left-click zoom in | Clicking a frame with `children.size > 0` pushes it onto the zoom stack, rebuilds with it as root, updates breadcrumb + URL, and clears any pinned tooltip; leaf frames are a no-op. | Left-click a frame with children. | `flamegraph.js:669-693,520-526` |
| F118. Ancestor-bar re-root | Clicking a full-width ancestor bar (`isAncestor`) replaces the whole zoom stack with that frame (non-linear navigation) rather than pushing. | Click an ancestor context bar. | `flamegraph.js:682-687` |
| F119. Right-click zoom out | Right-click pops one level from the clicked canvas's zoom stack; if empty, falls back to the other canvas's stack; `preventDefault()` suppresses the browser menu; no-op if neither is zoomed. | Right-click a canvas. | `flamegraph.js:695-706` |
| F120. Alt/Option+click pin tooltip | Alt/Option+click pins the tooltip (`pointer-events:auto`, selectable text, clickable links) on any frame regardless of children; stays until unpinned. | Alt/Option+click a frame. | `flamegraph.js:676-678,600-628` |
| F121. Ctrl/Cmd+click docs.rs | Ctrl/Cmd+click opens the frame's `docsUrl` (docs.rs) in a new window (`_blank`) when present; no-op otherwise; suppresses zoom. | Ctrl/Cmd+click a frame with docs. | `flamegraph.js:673-675` |
| F122. Click empty -> unpin | Clicking empty canvas space (no hit) unpins a pinned tooltip; no-op otherwise. | Click a gap between frames. | `flamegraph.js:671` |
| F123. Hover highlight + RAF batching | On mousemove, `hitTest` updates `highlightName` only when it changes, queueing a single repaint per frame via `requestAnimationFrame` (batches rapid moves). | Move the mouse over frames. | `flamegraph.js:638-648` |
| F124. Cursor feedback | Cursor becomes `pointer` over a frame, reverts to default off-frame. | Move over/off a frame. | `flamegraph.js:651,654` |
| F125. Mouse-leave cleanup | On mouseleave, hides the tooltip (unless pinned), clears `highlightName`, and queues a repaint (RAF) to drop the hover state. | Move the mouse off a canvas. | `flamegraph.js:658-667` |
| F126. Pinned-tooltip hover guard | `canvasMouseMove` returns early when `tooltipPinned`, so hover highlight/tooltip do not change while interacting with a pinned tooltip. | Pin a tooltip, then move the mouse. | `flamegraph.js:639` |
| F127. Listener registration | Named per-canvas `mousemove` handlers (`onWorkerMove`/`onOffworkerMove`) plus a shared `mouseleave` handler are registered so `destroy()` can remove them cleanly. | Internal. | `flamegraph.js:709-719` |

---

## K. Tooltip

Built by `buildTooltipHtml` / shown by `showTooltip` (`flamegraph.js:557-628`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F128. Tooltip on hover | Floating tooltip appears over a hovered frame and auto-hides on mouse leave unless pinned. | Hover a frame. | `flamegraph.js:557-598,600-628,638-667` |
| F129. Frame name / full name | Bold short name at top; full qualified name in gray below, shown only when it differs from the short name. | Hover. | `flamegraph.js:565-572` |
| F130. Location + expand toggle | Short `file:line` in gray; when pinned and a full path exists, a right-triangle toggle `(>)` expands to the full path (`(v)`) and collapses again; wired only for pinned tooltips. | Hover to see short path; Alt+click, then click the toggle. | `flamegraph.js:573-586,611-626` |
| F131. Sample count + % | Total samples for the frame and its percentage `(count/total*100)` to 1 decimal. For ancestor bars the percentage is relative to the root total, not the current zoom level. | Hover. | `flamegraph.js:560-564,587` |
| F132. Self count + % | Self-samples (frame at top of stack) and their percentage, using the same total basis as F131. | Hover. | `flamegraph.js:564,587` |
| F133. docs.rs link | When `docsUrl` exists: pinned tooltip shows a clickable `docs.rs` link (`target=_blank rel=noopener`); unpinned shows grayed text with hint `(Ctrl + click)`. | Alt+click to pin, then click the link (or Ctrl/Cmd+click the frame). | `flamegraph.js:588-593` |
| F134. Pin hint | Unpinned tooltips show `Alt + click to pin` (`Option + click` on Mac) in gray at the bottom. | Hover a frame. | `flamegraph.js:594-595` |
| F135. Pin / pointer-events | Pinning sets `tooltipPinned` and `pointer-events:auto` (selectable text, clickable links); unpinned is `pointer-events:none` (mouse passes through). | Alt/Option+click. | `flamegraph.js:602,676-678` |
| F136. Unpin (outside click / Esc) | A document click outside the tooltip (and not consumed by canvas) unpins it (`onDocClick`); Escape also unpins as the first cascade stage. | Click elsewhere, or press Esc. | `flamegraph.js:630-636,735-741,745-748` |
| F137. Positioning + overflow | Fixed position, cursor + 12px offset, aligned to the top of the container to avoid covering the frame, constrained to `min(600px, 100vw-24px)` and shifted off the right edge; `overflow-wrap:anywhere`; z-index 200. | Automatic on hover/pin. | `flamegraph.js:604-609`; `flamegraph.css:154-167` |

---

## L. Breadcrumb navigation

Bar built at `flamegraph.js:284-286`; rendered in `renderBreadcrumb` (`420-476`); shown only when zoomed.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F138. Breadcrumb bar | Full-width bar showing the zoom path; hidden when neither tree is zoomed; wraps on narrow viewports. | Appears above the canvases when zoomed. | `flamegraph.js:284-286,420-427`; `flamegraph.css:93-103` |
| F139. Root `(all)` link | First item; clickable link that resets zoom to the full tree for that thread type; blue/purple `#6c63ff`. | Click `(all)`. | `flamegraph.js:443-451` |
| F140. Non-leaf items | Clickable frame names (all but the deepest); truncated at 250px with ellipsis; blue + underline on hover; clicking zooms back to that level (and updates the URL). | Click any non-last crumb. | `flamegraph.js:460-474,467-471`; `flamegraph.css:104-112` |
| F141. Leaf item | Deepest frame name, gray `#aaa`, non-clickable, truncated at 250px, full name on hover. | Rightmost crumb. | `flamegraph.js:460-474`; `flamegraph.css:104-110` |
| F142. Item separators | Right-angle-quote separator (U+203A) between crumbs, dark gray `#555`. | Between crumbs. | `flamegraph.js:455-458`; `flamegraph.css:113` |
| F143. Dual-tree separator | Pipe separator between the worker and off-worker trails, shown only when both trees are zoomed. | When both are zoomed. | `flamegraph.js:431-437` |
| F144. Worker / off-worker trails | Worker trail rendered first (`(all)` + frame names), then the off-worker trail; each reflects its own zoom stack. | Visible per zoomed tree. | `flamegraph.js:430-438` |

---

## M. Zoom state and URL persistence

`createFlamegraph` returns the `fg` API used by the bootstrap script.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F145. Renderer creation | `createFlamegraph(containerEl, updateUrlZoom)` builds the toolbar, help overlay, breadcrumb, both canvases, and wires all listeners; the `updateUrlZoom` callback syncs zoom to the URL. | Automatic during load (`flamegraph.html:209`). | `flamegraph.js:127,142-189,299-310`; `flamegraph.html:198-207` |
| F146. `setData` | `fg.setData(allSamples, callframeSymbols, { exportTitle })` builds worker (`workerId != 255`) and off-worker (`== 255`) trees, populates the spawn dropdown, and applies the filter to render. | Called right after creation (`flamegraph.html:210`). | `flamegraph.js:813-840,772-805` |
| F147. Automatic zoom -> URL | On every zoom change, `updateUrlZoom` encodes each tree's zoom path via `fg.getZoomPath()` and calls `history.replaceState` (no reload). | Any zoom in/out. | `flamegraph.html:198-207`; `flamegraph.js:864-877` |
| F148. `worker-zoom` param | Set to the tab-separated worker zoom path when non-empty, deleted when empty. Enables bookmarking a zoom level. | URL `?worker-zoom=a\tb\tc`. | `flamegraph.html:201-202,214-216`; `flamegraph.js:915-935` |
| F149. `offworker-zoom` param | Same as F148 for the off-worker tree. | URL `?offworker-zoom=a\tb\tc`. | `flamegraph.html:203-204,215-217`; `flamegraph.js:915-935` |
| F150. Zoom-path format | Paths are frame names joined by tab (`\t`), root -> target; if a path cannot be walked child-by-child, `zoomToPath` falls back to a DFS for the last frame name. | Visible in the URL when zoomed. | `flamegraph.html:201,203`; `flamegraph.js:920-935` |
| F151. Conditional zoom restore | On load, restores `worker-zoom`/`offworker-zoom` via `zoomToPath` only when `timeRangeMatched` (F20) is true, because a fallback trace has a different tree. | Load with a zoom param + matching time range. | `flamegraph.html:212-217`; `flamegraph.js:915-935` |
| F152. Zoom param cleanup | Escape-reset (F158) clears both zoom params from the URL, leaving a clean shareable link. | Press Esc to reset zoom. | `flamegraph.html:224-228`; `flamegraph.js:765-767` |
| F153. URL context preservation | Zoom updates use `URLSearchParams`, preserving all other params (`trace`, `start`, `end`, `svc`, `host`, `segs`, `from`, `to`) while touching only the zoom params. | Zoom on any parameterized URL. | `flamegraph.html:199,205` |
| F154. `isZoomed()` | Returns true if either zoom stack is non-empty; used by the escape cascade and other logic. | Internal. | `flamegraph.js:535-537` |
| F155. Resize handler | `window` resize re-renders both canvases to the new width (DPR-aware), reapplying search highlighting; zoom state preserved. | Resize the window. | `flamegraph.html:221`; `flamegraph.js:842-845,316` |
| F156. Destroy / cleanup | `destroy()` removes all listeners (keyboard, mouse, click, context menu), removes the tooltip from the DOM, and clears the container HTML to avoid leaks. | Internal (teardown). | `flamegraph.js:847-862` |

---

## N. Keyboard and Escape cascade

Global keydown wired in the bootstrap (`flamegraph.html:224-228`) delegating to `handleEscape` / `onKeyDown`.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F157. Escape cascade | Priority order, stopping at the first action taken (returns true if consumed, false if nothing to dismiss): (1) unpin tooltip; (2) close export menu; (3) close help overlay; (4) clear search; (5) reset zoom. | Press Esc. | `flamegraph.js:745-770`; `flamegraph.html:224-228` |
| F158. Cascade stage details | Stage 1 sets `tooltipPinned=false` and hides the tooltip; stage 2 hides the export menu (`aria-expanded=false`); stage 3 hides the help overlay; stage 4 clears the input/`searchQuery`, hides the clear button, `renderAll()`; stage 5 clears both zoom stacks, `renderAll()` + `onZoomChange()`. | Press Esc in the relevant state. | `flamegraph.js:746-768` |
| F159. Container visibility guard | `onKeyDown` returns early when `container.offsetHeight === 0`, so shortcuts (Ctrl/Cmd+F, `/`) do nothing while the flamegraph is hidden. | Implicit; keys inert when hidden. | `flamegraph.js:726` |

---

## O. Cross-cutting behaviors

App-wide behaviors that are not tied to a single control.

| Behavior | Detail | Source |
| --- | --- | --- |
| F160. Same-origin credential security | Credential headers are attached only to same-origin trace fetches (`isSameOrigin`), across both the streaming and buffered paths, so a cross-origin `?trace=` cannot exfiltrate stored AWS creds. | `trace_parser.js:63-70,94-98,165` |
| F161. Gzip transparency | Both load paths auto-detect the gzip magic and decompress (browser `DecompressionStream`, Node `zlib`), so the parser always sees plain bytes. | `trace_parser.js:31-49,189-208` |
| F162. Shared deterministic coloring | `TraceAnalysis.flamegraphColor` is the single color source for both the on-screen canvases and the exported SVG, so exports match the screen. | `trace_analysis.js:979-985`; `flamegraph_export.js:48-58` |
| F163. RAF-batched repaint | Hover/leave highlight changes queue at most one `requestAnimationFrame` repaint (`repaintQueued`), coalescing rapid mouse events into one redraw per frame. | `flamegraph.js:644-647,660-665` |
| F164. Persistent search + zoom state | `searchQuery` and the two zoom stacks are module-scoped and survive resize, spawn-filter changes, and each other; only explicit user actions (clear button, Escape, filter reset) mutate them. | `flamegraph.js:135,417,772-805` |
| F165. Export reflects filter, not zoom | Exports (SVG + folded) reflect the current spawn-location filter (trees rebuilt by `applySpawnFilter`) but always emit the full, un-zoomed trees. | `flamegraph.js:199-207,218-223,799-802` |