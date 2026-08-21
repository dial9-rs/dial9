# UI Feature Inventory: `index.html` (Trace Browser landing page)

> PILOT section. Purpose: capture every existing functionality of the landing
> page precisely enough that (a) you can validate each one in the running UI and
> (b) it can be re-implemented without losing anything. Validate this format,
> then the same shape is applied to `viewer.html` and `flamegraph.html`.
>
> Single source of truth: code-derived inventory PLUS live validation against the running UI on
> 2026-06-30 (full dev-server on :3001, headless Chromium). Per-feature verdicts, validation
> method, findings, and resolved open questions are in the "Live validation results" section at
> the end of this file. The inventory held up: no documented feature was missing or misdescribed.
>
> REFRESHED 2026-07-08 against HEAD (drift commits #582, #570, #585, #596, #600,
> #597, #586, #587, #607; anchors re-derived from commit 84a21e5's tree). Rows
> added or behavior-updated in that refresh are marked `[2026-07-08]`; their
> verification method (curl against the dev-server + code read, no browser
> driver) is recorded in the "2026-07-08 refresh" subsection at the end. The
> original 2026-06-30 verdict table describes the pre-drift surface.
>
> AMENDED 2026-07-10 by T15 (ADR-0004 section 1: the four structurally-fixed
> defects, landed AFTER T14's faithful port). Rows marked `[2026-07-10]` (G8,
> C6, I2 + its F4/I3 display consequences, F10-axis) are the canonical
> behavior under `src/pages/browser/`. The 2026-08-10 completion of #769
> removed the superseded page; earlier validation tables remain historical.

## What this surface is

The landing page / S3 trace browser. Lets a user find trace segments in an S3
bucket (by time range or raw prefix), preview their data density on a timeline,
select some, and open them in the viewer or the flamegraph. Also the drop point
for local `.bin` files and the demo trace.

- Entry file: `dial9-viewer/ui/index.html` (thin Vite entry)
- Entry module: `src/pages/browser/main.ts`; browser-global contracts:
  `creds.js`, `url_state.js` (since #585)
- Backend endpoints consumed: `/api/config`, `/api/prefixes`, `/api/browse`, `/api/object`, `/api/buckets`, `/api/credentials/check`. (`/api/search` was REMOVED by #582; `/api/browse` replaced it. The Flamegraph/Tokio Stats buttons open pages that consume `/api/flamegraph` / `/api/tokio-stats` when aggregation is enabled - #570.)

## How to read this document

| Column           | Meaning                                                                                                                |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------- |
| **Feature**      | One discrete capability.                                                                                               |
| **What it does** | Behavior, including edge cases and non-obvious rules.                                                                  |
| **Access path**  | Precise way to reach/trigger it in the running UI.                                                                     |
| **Source**       | `file:line` (+ function name). Line numbers are a snapshot as of this writing; the function name is the stable anchor. |

Statuses used in notes: `OK` (works), `DEAD` (present in markup/CSS but not wired), `CONDITIONAL` (only appears under a server/runtime condition). Anything tagged `[VERIFY]` is behavior inferred from code that you should confirm live.

To run the UI locally with a working backend (so the search/heatmap/creds paths are exercisable), use the full dev-server, NOT the static `serve.py`: `PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server` (`dial9-viewer/src/bin/dev_server.rs`). It seeds a fake S3 `demo-traces` bucket (prefix `traces`, BYO creds `test`/`test`) with `demo-trace.bin`. `serve.py` is static-only (no `/api/*`); under it only the "Load demo trace", drag-drop, and `?trace=` passthrough paths work. See the "Reproduce" section at the end for the full validation recipe.

---

## A. Page entry and global behaviors

| Feature                    | What it does                                                                                                                                                                                                                              | Access path                                             | Source                               |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | ------------------------------------ |
| A1. `?trace=` passthrough  | If the landing URL itself carries any `trace=` param, immediately redirect to `viewer.html` preserving the full query string (including repeated `trace=`).                                                                               | Open `index.html?trace=...` -> lands on viewer instead. | `index.html:377-385` (IIFE)          |
| A2. Load demo trace        | Opens `viewer.html?trace=demo-trace.bin` in a new tab.                                                                                                                                                                                    | Footer (bottom bar) -> "Load demo trace" button.        | `index.html:368`, `388-390`          |
| A3. Drag-and-drop `.bin`   | Dropping a file anywhere on the page opens `viewer.html?trace=<objectURL>` in a new tab; footer border highlights purple while dragging.                                                                                                  | Drag a `.bin` file onto the window.                     | footer `364-370`; handlers `391-404` |
| A4. Open Trace Viewer link | Plain link to `viewer.html` (no trace).                                                                                                                                                                                                   | Footer -> "Open Trace Viewer".                          | `index.html:366`                     |
| A5. Config bootstrap       | `[2026-07-08]` On load, `GET /api/config` -> prefills bucket (`default_bucket`, unless BYO creds active) and prefix (`default_prefix`) only when the fields are still empty (URL-restored values win, #585), enables the credentials UI (`supports_byo_credentials`), and records `aggregation_enabled` (drives H3 mode + H6, #570). Then syncs the URL and runs region-detect -> prefix discovery -> auto-search (#607). On failure, still runs prefix discovery + auto-search. | Automatic on page load.                                 | `index.html:798-831`; `aggregationEnabled` `814-817`                 |
| A6. URL state sync         | `[2026-07-08]` (#585, #607) Every state-changing action mirrors page state into the query string via `syncUrl()` + `Dial9UrlState.serialize` (`history.replaceState`, no Back-stack spam): `bucket`, `aws_region` (from stored creds; secrets never serialized), `prefix`, `tab` (only `raw`), `tz` (only `local`), `q`, and either `last=N` (active quick range, kept relative) or precise epoch-second `from`/`to`. Defaults are omitted so a pristine page has a clean URL. Suppressed while restoring (A7). | Automatic on bucket/prefix/raw-query input, tab switch, TZ toggle, range edits, creds apply/clear/bucket-select. | `syncUrl` `index.html:719-749`; `url_state.js:30-100`                 |
| A7. URL state restore      | `[2026-07-08]` (#585, #607) On load, `Dial9UrlState.parse` restores TZ first (pickers format in the active TZ), then bucket/prefix/raw query/tab; pins `aws_region` into the stored credentials (+ panel field) when creds are present; then the time range: `last=N` re-anchors to now, `from`/`to` restore verbatim, else default last-1hr. One final `syncUrl()` normalizes the incoming link. | Open a link with state params.                          | `index.html:752-796`                 |
| A8. Unified `?` help overlay | `[2026-07-12]` `?` toggles the unified help overlay (dialog, h2 title + kbd rows) listing the page's live keyboard + heatmap mouse bindings; Escape (overlay-local or page-level) and backdrop click close it; focus is captured on open and restored on close. | Press `?` outside a text field (`index.html`). | `src/pages/browser/page-keys.ts`; component `src/lib/interact/help-overlay.ts` |
| A9. `/` focuses search     | `[2026-07-12]` `/` focuses + selects the active tab's search input: prefix field on Browse, key-substring field on Raw. Focus-tolerant (fires from buttons/selects, suppressed while typing in a text field); never fires when another handler consumed the key. | Press `/` outside a text field. | `src/pages/browser/page-keys.ts`; router `src/lib/interact/keyboard.ts` |

Notes:

- A5 deliberately does NOT prefill the server default bucket when the user has brought their own credentials, because that bucket belongs to the server identity (`index.html:802-808`).

---

## B. Header bar

| Feature                    | What it does                                                                                                                                                  | Access path                                    | Source                                                 |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- | ------------------------------------------------------ |
| B1. Title / subtitle       | Static "dial9 Trace Browser" + "search & view traces from S3".                                                                                                | Top-left of header.                            | `index.html:216-217`                                   |
| B2. AWS Credentials button | `CONDITIONAL`. Hidden unless server reports `supports_byo_credentials`. Toggles the credentials panel. Turns green with a check when creds are active.        | Header, right side -> "[key] AWS Credentials". | button `219-221`; reveal `461`; active state `469-473` |
| B3. Timezone toggle        | Flips all date display + the datetime pickers + heatmap axis between UTC and Local. Re-renders current view. Picker values are converted, not just relabeled. `[2026-07-08]` Mode now rides the URL as `tz=local` (A6). | Header, right side -> "TZ: UTC" / "TZ: Local". | button `222`; handler `952-971`                        |

---

## C. Bring-your-own-credentials panel (`CONDITIONAL` on `supports_byo_credentials`)

Initialized by `initCredsUi()` (`index.html:451-686`). Credentials live in `sessionStorage` (die with the tab) and ride as `x-dial9-aws-*` headers on every `/api/*` request via `apiFetch` (`442-445`). Store/parse/headers logic is in `creds.js`.

| Feature                            | What it does                                                                                                                                                                                                          | Access path                                                       | Source                                                               |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------- |
| C1. Open/close panel               | Button toggles; "X" closes.                                                                                                                                                                                           | Header "AWS Credentials" button; panel "X" at top-right of panel. | `482-487`; markup `228-234`                                          |
| C2. Paste JSON -> Fill fields      | Parses an STS/Isengard response (nested `credentials`) or a flat `{accessKeyId,...}` blob (tolerates camel/snake/SCREAMING case), fills the fields, then clears the textarea so the secret is not left sitting there. | Panel -> paste into "Paste JSON" textarea -> "Fill fields".       | handler `491-510`; parser `creds.js:94-138`                          |
| C3. Manual credential fields       | Access key ID, secret (password), session token (password), region (placeholder "auto-detect").                                                                                                                       | Panel input rows.                                                 | markup `242-253`                                                     |
| C4. Apply                          | Validates akid+secret present, clears any prior bucket selection, stores creds (no region yet), then lists visible buckets.                                                                                           | Panel -> "Apply".                                                 | handler `612-633`; `loadBuckets` `637-647`; `creds.js:156-188` (`set`) |
| C5. Clear                          | Wipes stored creds, empties fields + bucket picker, and resets the whole browse pane (the heatmap belonged to the removed identity).                                                                                  | Panel -> "Clear".                                                 | handler `649-663`; `creds.js:230` (`clear`)                            |
| C6. Bucket picker                  | `[2026-07-08]` (#607) After Apply, lists buckets the creds can see. Defaults to the filter-matched view (sorted, highlighted); a "Show all (N)" / "Show <filter> only (N)" toggle button (shown only when the two lists differ) switches to the full list, where trace buckets stay highlighted. Auto-selects only in the filtered view when exactly one matching bucket. `[2026-07-10]` The filter predicate is CONFIG-DRIVEN: page-URL `?bucket_filter=` override (empty = no filtering; carried through every URL sync) > `/api/config` `bucket_filter` (server default "dial9", settable via `AppState::with_bucket_filter`) > client fallback "dial9". Match stays case-insensitive substring. | Appears in panel after Apply (or on return visit); toggle at list end. Optionally load with `?bucket_filter=<substring>`. | `src/pages/browser/bucket-filter.ts`; `creds-panel.ts` (`renderPicker`); `src/server/config.rs` |
| C7. Select bucket -> region detect | Fills the bucket field, calls `POST /api/credentials/check` to resolve+persist the region, then re-discovers prefixes and re-runs the current search. `[2026-07-08]` (#607) The resolved region is also mirrored into the URL as `aws_region` (A6). | Click a bucket chip in the picker.                                | `selectBucket` `584-610`; `creds.js:197-211` (`check`)                 |
| C8. Status line                    | Inline ok/error/neutral messages for every step.                                                                                                                                                                      | Below the Apply/Clear row.                                        | `setStatus` `463-465`; CSS `78-80`                                   |
| C9. Returning user auto-list       | If creds already in sessionStorage on load, silently re-lists buckets so the picker is ready (panel stays closed; green button signals active).                                                                       | Automatic on load when creds present.                             | `683-685`                                                            |
| C10. Scripting API + change event  | `window.Dial9Creds.set(...)` is the stable userscript entry point; firing `dial9:credentials-changed` refreshes the UI and re-runs the search (only when creds are now present, not on Clear).                        | Programmatic (injected userscript).                               | listener `668-674`; `creds.js:141-188`, `creds.js:253-257`                  |
| C11. Region auto-detection         | `[2026-07-08]` (#607) `CONDITIONAL` on stored credentials. Before hitting data endpoints for a new bucket (on load, and on bucket-field change), `detectRegionForBucket` resolves the bucket's real region via `/api/credentials/check` (region-agnostic HeadBucket), persists it into the stored creds (`autoDetectRegion:false`), prefills the panel region field, and syncs the URL. Best-effort: on failure the prior region stays and the data request surfaces the real error (the backend maps a cross-region hit to HTTP 421 `WrongRegion`, `src/server/error.rs:15-19`). No-op without creds (ambient path uses the server's region). | Automatic: page load with creds + bucket, or editing the bucket field.                               | `detectRegionForBucket` `841-874`; load chain `825-830`; change listener `936-948`                  |

Notes:

- C7 intentionally does NOT clear creds on a failed bucket check, since the failure is usually bucket-specific (`creds.js:177-184`).
- Credentials are never persisted beyond the tab session and never leave this origin (`creds.js` header comment, `1-14`).

---

## D. Controls bar (search inputs)

| Feature                                | What it does                                                                                                                                                                | Access path                                  | Source                                                     |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- | ---------------------------------------------------------- |
| D1. Bucket input                       | Free text. Prefilled from the URL state (`?bucket=`, A7), else config default (unless BYO creds). `[2026-07-08]` Typing syncs the URL; committing (change) triggers region detect (C11) + prefix re-discovery (D10). | Controls bar -> "Bucket:" field.             | markup `268`; prefill `760`, `806-808`                         |
| D2. Prefix input                       | Free text key prefix. Placeholder cycles through states: "detecting...", "(none found)", "(no prefix - dates at root)", "discovery failed - enter manually", "e.g. traces". | Controls bar -> "Prefix:" field.             | markup `270`; states in `discoverPrefixes` `876-934`       |
| D3. Prefix suggestion chips            | Auto-built from `GET /api/prefixes`; clicking one fills the prefix and marks it active.                                                                                     | Controls bar, right of prefix field.         | `889-928`                                                  |
| D4. Date-layer auto-empty (#471)       | If every discovered root child looks like a date (`YYYY-MM-DD/`), the prefix is set empty (dates are not selectable prefixes).                                              | Automatic during discovery.                  | `900-907`; `prefix_detect.js isDateLayer` `25-28`          |
| D5. Single-prefix auto-select          | Exactly one prefix + empty input -> auto-fills it.                                                                                                                          | Automatic during discovery.                  | `909-912`                                                  |
| D6. Quick range buttons                | "Last 1hr / 3hr / 24hr" set the From/To pickers and highlight the chosen button. Default on load = 1hr (unless the URL restored a range, A7). `[2026-07-08]` (#585) The active quick range is tracked (`currentQuickRange`) and serialized as relative `last=N`, so a shared link means "the last N hours from now". | Controls bar -> quick buttons.               | markup `273-277`; `setQuickRange` `1098-1114`; default via restore `781-791` |
| D7. From / To pickers                  | `datetime-local` inputs, 1-minute step, interpreted in the current TZ mode.                                                                                                 | Controls bar -> "From:" / "To:".             | markup `279-281`; `pickerToDate` `1090-1095`                 |
| D8. Manual-edit clears quick highlight | Editing From/To deselects the quick-range button. `[2026-07-08]` (#585) Also clears `currentQuickRange`, so the URL flips from `last=N` to precise `from`/`to`. | Edit a picker after clicking a quick button. | `clearQuickRange` `1115-1124`                                                  |
| D9. Search button                      | Disabled until a prefix is present when the server declares one. Runs the time-range (Browse) search. `[2026-07-12]` Enter in any controls-bar field (bucket, prefix, From/To pickers) also runs it, gated on the same enabled state. | Controls bar -> "Search" (primary); or Enter in a controls-bar field. | `src/pages/browser/actions.ts`; Enter path `src/pages/browser/page-keys.ts` |
| D10. Re-discover on bucket change      | Changing the bucket re-runs prefix discovery. `[2026-07-08]` (#607) Region detection (C11) now runs FIRST (discovery is itself a signed S3 call), then discovery, then URL sync. | Edit bucket field, blur/change.              | `936-949`                                                      |

---

## E. Tabs

| Feature                      | What it does                                                                                                   | Access path              | Source                                  |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------ | --------------------------------------- |
| E1. Browse / Raw Search tabs | Switch between the density-heatmap browse view and the raw prefix-search table. `[2026-07-08]` (#585) The active tab rides the URL (`tab=raw`; browse is the unserialized default). | Tab bar under controls.  | markup `285-288`; `switchTab` `1127-1141` |
| E2. Per-tab action swap      | Browse shows the Flamegraph and Tokio Stats buttons (#570); Raw shows Select All / Deselect All. Selection count recomputed on switch. | Automatic on tab switch. | `1133-1139`                               |

---

## F. Browse view: density heatmap timeline

The headline feature. `doTimeRangeSearch` (`1150-1245`) fetches, the `renderHeatmap`/`drawHeatmapCanvas` pipeline (`1280-1454`) draws, and the interaction IIFE (`1459-1522`) handles pointer input. Pure data helpers are in `heatmap.js`.

| Feature                                  | What it does                                                                                                                                                                                                                 | Access path                              | Source                                                                                  |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------------------- |
| F1. Initial prompt                       | "Select a time range and click Search to find traces."                                                                                                                                                                       | Browse tab before any search.            | markup `294-296`                                                                        |
| F2. Time-range search                    | `[2026-07-08]` REWRITTEN by #582: one `GET /api/browse?bucket&from&to[&prefix]` (epoch seconds); the SERVER owns the prefix fan-out (10-minute time buckets, 1-minute for windows under 10 min, each list paginated up to 10k, merged, plus a `truncated` flag). The old client-side `hourPrefixes` + per-hour `/api/search` fan-out is gone. The client still trims returned objects to the exact range (whole time buckets can over-include) and passes the user prefix only when the server has none. | Set range -> "Search" (or auto on load). | `doTimeRangeSearch` `1150-1245` (comment `1141-1149`); server `src/server/browse.rs`                                 |
| F3. Empty-result sample keys             | If nothing matches, shows up to 5 sample keys from the bucket to reveal its layout (or "Bucket appears empty"). `[2026-07-08]` (#582) Samples now come from `/api/browse` over a fixed last-24h window, so keys older than a day are NOT found (the dev-server's April demo key now yields "Bucket appears empty", unlike the 2026-06-30 run).                                                                                                              | Search a range with no data.             | `1202-1220`                                                                               |
| F4. Host rows                            | One row per `service / host` (boot changes do NOT split rows). Rows sorted by label, segments sorted by start. `[2026-07-10]` Unknown-layout keys group by their raw DIRECTORY PATH and the row label renders that path verbatim - no guessed "service / host" split. | Rendered left column of heatmap. | `src/pages/browser/actions.ts` (`unknownGroupPath`); `browse-view.ts` (`rebuildLabels`) |
| F5. Boot-count annotation                | Row label shows "[mark] N boots" when boot_id changes within the window.                                                                                                                                                     | Heatmap left labels.                     | labels `1320-1322`; `bootTransitions` `heatmap.js:77-90`                                |
| F6. Density canvas                       | Bytes spread uniformly across each segment span, summed per pixel column, normalized to a sqrt color ramp (dim blue -> purple -> red -> yellow). 256-entry precomputed palette + run-length coalescing for speed. DPR-aware. | The colored strips.                      | `drawHeatmapCanvas` `1331-1386`; `accumulateDensity`/`densityColor` `heatmap.js:98-201` |
| F7. Seam tiling                          | Segment ends are clamped to the next start so upload-lag overlaps do not double-count into a false bright band.                                                                                                              | Visual (no control).                     | `1351`; `tileSegments` `heatmap.js:131-141`                                             |
| F8. Coverage-gap hatching                | Genuine gaps (a host that stopped reporting) drawn as faint diagonal-hatched bands with edge ticks, distinct from low density.                                                                                               | Visual; legend "gap (no data)".          | gaps `1387-1418`; `segmentGaps` `heatmap.js:148-157`                                    |
| F9. Boot-change dividers                 | Dashed cyan vertical lines at boot transitions.                                                                                                                                                                              | Visual; legend "boot change".            | `1419-1432`                                                                             |
| F10. Time axis                           | 2 to 8 TZ-aware ticks, aligned to the canvas left edge via `--heatmap-label-w`. `[2026-07-10]` When the visible span (zoomed or full) crosses a calendar-day boundary in the active TZ mode, ticks carry the date (`YYYY-MM-DD HH:MM:SS`); single-day spans keep compact `HH:MM:SS`. Tick count is unchanged. Selection-count times (H1) stay time-only. | Below the canvas. | `src/pages/browser/browse-view.ts` (`drawAxis`); `format.ts` (`crossesDayBoundary`/`fmtTick`) |
| F11. Legend + hint                       | Density gradient, gap swatch, boot-change marker, and interaction hint text.                                                                                                                                                 | Top of heatmap view.                     | markup `298-303`                                                                        |
| F12. Drag-select region                  | Plain drag selects a rectangle (rows x time); the selection snaps to the actual `[min start, max end]` of the covered files (whole files open, S3 cannot sub-range). A keyboard path exists too (F21) and lands on the same `finalizeSelection`. | Drag across the canvas. | `src/pages/browser/browse-view.ts` (`finalizeSelection`) |
| F13. Click-select one segment            | A click (drag < 4px) selects the single segment under the cursor; ties broken by nearest start.                                                                                                                              | Single click on a strip.                 | `selectSegmentAt` `1598-1614`                                                           |
| F14. Option/Alt+drag zoom                | Alt+drag zooms the time axis to the dragged span; density re-normalizes to the visible window (flat-looking regions reveal structure).                                                                                       | Hold Option/Alt, drag horizontally.      | `1480`, `1507-1510`; `zoomToX` `1528-1540`                                              |
| F15. Double-click reset zoom             | Restores the full data extent.                                                                                                                                                                                               | Double-click anywhere on the plot.       | `1521`; `resetHeatmapZoom` `1560-1568`                                                  |
| F16. Reset zoom button                   | Appears only while zoomed.                                                                                                                                                                                                   | Heatmap hint bar -> "Reset zoom".        | markup `303`; `updateZoomResetBtn` `1571-1578`                                          |
| F17. Selection rectangle + row highlight | Persistent purple box over selected rows/time; selected host labels get a highlight.                                                                                                                                         | After a selection.                       | `showSelRect` `1582-1594`; `setHeatmapSelection` `1643-1658`                            |
| F18. Click-outside clears                | Clicking outside the heatmap and actions bar clears the explicit box selection; profile actions fall back to the full current service.                                                                                       | Click elsewhere on the page.             | `src/pages/browser/heatmap-interact.ts`; `effectiveProfileSelection`                    |
| F19. Resize redraw                       | Debounced (100ms) canvas re-measure + redraw + selection re-place on window resize.                                                                                                                                          | Resize the window.                       | `1662-1672`                                                                             |
| F20. Truncation warning banner           | `[2026-07-08]` NEW in #582. `CONDITIONAL` on the `/api/browse` response's `truncated` flag: a yellow banner above the browse status ("Some traces were omitted: this time range exceeded the listing limit...") so a capped result is never mistaken for missing data. Hidden/cleared on every new search. Overflow attribution to the right prefix was fixed server-side by #586 (`buffered` vs `buffer_unordered`). | Search a range wide enough to overflow the server listing cap. | CSS `54-59`; markup `293`; `setBrowseWarning` `428-439`; set at `1197-1200`                                                                             |
| F21. Keyboard window selection           | `[2026-07-12]` The plot is keyboard-focusable (`tabindex=0`, `role=application`, labeled). With focus: Shift starts a window selection centered in the visible range (all host rows - row narrowing stays a pointer affordance), ArrowLeft/ArrowRight move the leading edge in 5% steps, Enter or Shift confirms through the SAME `finalizeSelection` as F12, Escape cancels, blur abandons. The in-progress band renders through the same transient drag channel as the mouse rubber band; every transition is announced (aria-live + toast). | Tab (or click) focus onto the heatmap plot -> Shift -> arrows -> Enter. | `src/pages/browser/heatmap-keys.ts`; announce `src/lib/interact/announce.ts` |

---

## G. Raw search view

| Feature                        | What it does                                                                                                                                                                                                                       | Access path                                   | Source                                                                     |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------- | -------------------------------------------------------------------------- |
| G1. Raw prefix search          | Free-text prefix -> single request -> table. `[2026-07-08]` (#582) Now `GET /api/browse?bucket&prefix=<q>&from&to` with an implicit LAST-30-DAYS window (reads `.objects` from the response); older keys are no longer reachable from raw search.                                                                                                                                                                          | Raw tab -> prefix field -> "Search".          | markup `317-324`; `doRawSearch` `1789-1812`                                |
| G2. Enter to search            | Enter in the prefix field triggers the search.                                                                                                                                                                                     | Raw prefix field -> Enter.                    | `1813-1815`                                                                |
| G3. Results table              | Columns parsed from the key: Service, Host, Boot, Trace Start, Seg #, Size, Uploaded. Rows sorted by trace-start epoch.                                                                                                            | Raw tab table.                                | markup `327-341`; `renderRawTable` `1817-1865`                             |
| G4. Per-row checkbox           | Select individual segments.                                                                                                                                                                                                        | Checkbox in each row.                         | `1852`, `1861`                                                             |
| G5. Select-all header checkbox | Toggles all row checkboxes.                                                                                                                                                                                                        | Header-row checkbox.                          | markup `330`; `1867-1869`                                                  |
| G6. Select All / Deselect All  | Buttons in the actions bar (Raw tab only).                                                                                                                                                                                         | Actions bar -> "Select All" / "Deselect All". | markup `348-349`; `rawSelectAll` `1871-1875`                               |
| G7. Empty-result sample keys   | Same sample-key hint as Browse when no results (same #582 caveat: samples drawn from a last-24h `/api/browse` window).                                                                                                                                                                                    | Search a prefix with no data.                 | `1823-1840`                                                                |
| G8. Column sort                | `[2026-07-10]` SORTABLE: clicking a header sorts that column ascending; clicking it again flips to descending. All columns are sortable: numeric for Trace Start (epoch), Seg # and Size, lexical otherwise. The active header carries `.sort-arrow` and `aria-sort`; a sort rebuild preserves checkbox selection; default order remains trace-start epoch ascending; `getSelectedKeys` follows table order. | Click a column header -> sorts / flips. | `src/pages/browser/raw-rows.ts` (`nextSort`/`sortValue`/`sortRawRows`); `raw-view.ts` |

> G8 was consciously amended in T15: the advertised sort affordance is now functional.

---

## H. Actions bar

| Feature               | What it does                                                                                       | Access path                                     | Source                                           |
| --------------------- | -------------------------------------------------------------------------------------------------- | ----------------------------------------------- | ------------------------------------------------ |
| H1. Selection count   | Browse shows the explicit box or the full current-service fallback; Raw shows "N selected". It also drives the enable/disable state of View / Flamegraph / Tokio Stats per H4/H6 rules.                    | Right side of actions bar.                      | `src/pages/browser/actions-bar.ts`                |
| H2. View Selected     | Opens `viewer.html` with title metadata plus a compact, stateless scope for time-partitioned sources; flat local sources use one `trace=/api/object?...` per selected key. | Actions bar -> "View Selected in Trace Viewer". | `src/pages/browser/open-links.ts`                  |
| H3. Flamegraph        | `CONDITIONAL` (Browse tab only). `[2026-07-08]` Two modes since #570: with `aggregation_enabled` (A5) it opens `flamegraph.html?api=1` with the selection's SCOPE (bucket, prefix via `extractPrefix` I8, first service, repeatable `host` set, `start_ns`/`end_ns` from the selection box) to drive the server-side sampled refinement loop; otherwise (exact mode) it opens `flamegraph.html` with the per-key `trace=` set as before. A multi-service box passes only the first service.       | Actions bar -> "[fire] Flamegraph".             | markup `354-356`; `viewCpuProfile` `1727-1767` (agg branch `1732-1759`)   |
| H4. Selection size cap | `[2026-07-08]` If selection bytes > `MAX_OPEN_BYTES` - RAISED from 100 MB to 200 MB by #600 - View disables and a red warning shows (message renders the cap via `formatSize`). Since #570, Flamegraph is EXEMPT from the cap in aggregation mode (the server samples; no client decode), and the warning is suppressed there too; Tokio Stats is never size-capped (H6). | Select a very wide range.                       | `1904-1910`; `MAX_OPEN_BYTES` `heatmap.js:27`    |
| H5. Selection warning | Red inline message slot (used by H4).                                                              | Actions bar.                                    | markup `360`                                     |
| H6. Tokio Stats button | `[2026-07-08]` NEW in #570. `CONDITIONAL` (Browse tab only; disabled unless `aggregation_enabled`). Opens `tokio_stats.html` with the same scope params as H3's aggregation mode (bucket, prefix, service, `host` set, `start_ns`/`end_ns`). The tokio_stats.html page itself is a separate surface (inventoried separately; owned by the Tokio-Stats-page ticket). | Actions bar -> "[zap] Tokio Stats".                                    | markup `357-359`; `viewTokioStats` `1769-1786`; gating `1899`, `1907`                                     |

---

## I. Cross-cutting behaviors (replication-critical, not single buttons)

| Behavior               | Detail                                                                                                                                                                 | Source                                       |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| I1. Timezone mode      | `useLocalTz` flag drives `formatDate`, `formatEpoch`, `fmtTick`, and picker read/write. Toggling re-renders the active view and rewrites picker values.                | `988-1004`, `1072-1095`, `1258-1264`, `952-971` |
| I2. Key parsing        | `parseKey` recognizes the canonical `version=1/date/service/time/instance/boot` layout and decodes one escaping layer from each partition value. It also handles both historical layouts and a dateless positional fallback. Keys whose directory layout matches no documented shape return `{ layout: "unknown" }` and render RAW: the full key spans Service/Host/Boot in the table, the heatmap groups by raw directory path (F4), and title/scope params get no guessed svc/host (I3, H3, H6). The filename `{epoch}-{index}` is layout-independent and still supplies time and segment index. | `src/lib/trace/keys.ts`; `src/pages/browser/raw-rows.ts` |
| I3. Title metadata     | `traceTitleParams` derives `svc`, `host`, `from`, `to`, `segs` query params shared by viewer and flamegraph headers. Unknown-layout keys contribute no `svc`/`host`, while their layout-independent filename epochs still feed `from`/`to`; `segs` is unchanged. | `src/lib/trace/title.ts` |
| I4. Viewer source link | Time-partitioned selections carry a compact bucket/prefix/window scope that the viewer resolves to `/api/object?bucket&key` URLs; flat local selections carry direct `/api/object?key` traces.             | `src/pages/browser/open-links.ts`; `trace_scope.js` |
| I5. Credentialed fetch | `apiFetch` spreads `Dial9Creds.headers()` into every `/api/*` call.                                                                                                    | `442-445`                                    |
| I6. HTML escaping      | `esc()` escapes any key/text injected into innerHTML (sample keys, table rows).                                                                                        | `976-980`                                    |
| I7. Backend endpoints  | `[2026-07-08]` `/api/config`, `/api/prefixes`, `/api/browse` (replaced `/api/search`, removed in #582; the old endpoint now 404s), `/api/object`, `/api/buckets`, `/api/credentials/check`.                                                                | throughout                                   |
| I8. Scope-prefix extraction | `[2026-07-08]` NEW in #570. `extractPrefix(key)` infers the prefix from the canonical versioned layout or a historical suffix. Empty string when neither is found. | Internal (used by H3/H6 URL building).                                   | `src/lib/trace/keys.ts`                                   |

---

## Live validation results

**Method.** Full `dev-server` on :3001 (`PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server`); gate: `GET /api/config` returns JSON. Driven with headless Chromium (Playwright): each feature's access path performed, DOM state asserted, every state screenshotted, and the screenshots inspected visually (not assertions alone). Backend quirk observed: the server prepends the configured `traces/` prefix to the `q` parameter, so Browse queries `2026-04-09/19` and raw search uses `2026-04-09`.

**Outcome.** Soundness: every exercisable feature behaves as documented; none missing or misdescribed. Completeness: 33 live DOM affordances captured, all map to documented features, none undocumented. Coverage caveat: the dev-server seeds exactly one segment / one host / one boot, so density variation, coverage gaps, boot markers, seam-tiling, and the 100 MB cap cannot be exercised live; those are marked `NOT-TRIGGERABLE` (logic confirmed in code, demo data insufficient), not failures.

Verdict legend: `VERIFIED` (driven + observed) / `DEAD-CONFIRMED` / `PARTIAL` (some of it observed) / `NOT-TRIGGERABLE` (demo data cannot exercise it; code path confirmed) / `NOT-TESTED` (skipped this pass) / `CODE-ONLY` (not observable from the UI surface).

| Feature | Verdict | Evidence / note |
|---|---|---|
| A1 `?trace=` passthrough | VERIFIED | `index.html?trace=demo-trace.bin` redirected to `viewer.html?trace=demo-trace.bin`. |
| A2 Load demo | VERIFIED | popup -> `viewer.html?trace=demo-trace.bin`. |
| A3 drag-drop `.bin` | NOT-TESTED | synthesizing a file drop was skipped this pass. |
| A4 Open Trace Viewer link | VERIFIED | footer `a href="viewer.html"`. |
| A5 config bootstrap | VERIFIED | bucket/prefix prefilled, creds button enabled, auto-search ran. |
| B1 title | VERIFIED | "dial9 Trace Browser". |
| B2 creds button | VERIFIED | visible (server reports BYO support). |
| B3 timezone toggle | VERIFIED | "TZ: UTC" -> "TZ: Local". |
| C1 open/close panel | VERIFIED | panel toggles. |
| C2 paste -> fill | VERIFIED | pasted blob filled `akid=AKIAEXAMPLE`. |
| C3 manual fields | VERIFIED | filled akid/secret/region. |
| C4 Apply | VERIFIED | status "1 bucket(s) - pick one below"; region auto-detected `us-east-1`. |
| C5 Clear | VERIFIED | status "Credentials cleared". |
| C6 bucket picker | PARTIAL | see Finding 2 - dial9 name-filter hides the only bucket. |
| C7 select bucket -> region | NOT-TRIGGERABLE | no selectable bucket (blocked by C6 filter). |
| C8 status line | VERIFIED | ok/neutral messages observed throughout. |
| C9 active-state button | VERIFIED | header shows "AWS Credentials [check]" after Apply. |
| C10 scripting API + event | CODE-ONLY | `Dial9Creds.set`/`dial9:credentials-changed` not driven this pass. |
| D1 bucket input | VERIFIED | prefilled "demo-traces". |
| D2 prefix input + states | PARTIAL | default "traces" shown; placeholder-state cycling not exercised. |
| D3 prefix suggestion chips | VERIFIED | "traces" chip rendered and marked active. |
| D4 date-layer auto-empty (#471) | NOT-TRIGGERABLE | demo bucket has a real `traces` prefix. |
| D5 single-prefix auto-select | VERIFIED | sole prefix `traces/` auto-filled. |
| D6 quick range | VERIFIED | "Last 1hr" highlighted on load. |
| D7 From/To pickers | VERIFIED | set + displayed. |
| D8 manual-edit clears highlight | NOT-TESTED | not driven. |
| D9 Search button | VERIFIED | enabled (prefix present). |
| D10 re-discover on bucket change | NOT-TESTED | not driven. |
| E1 tabs | VERIFIED | Browse <-> Raw toggles views. |
| E2 per-tab action swap | VERIFIED | Raw shows Select All/Deselect; Browse shows Flamegraph. |
| F1 initial prompt | NOT-OBSERVED | auto-search ran on load and replaced the prompt with the F3 sample-key hint. |
| F2 time-range search | VERIFIED | windowed search -> heatmap visible, 1 host row. |
| F3 empty-result sample keys | VERIFIED | "No traces found... Sample keys" + a key shown. |
| F4 host rows | VERIFIED | 1 row "host-0 / abcd" (see Finding 1 re labeling). |
| F5 boot-count annotation | NOT-TRIGGERABLE | single boot, no transitions. |
| F6 density canvas | VERIFIED | canvas drawn; single wide segment renders solid. |
| F7 seam tiling | NOT-TRIGGERABLE | single segment, no seams. |
| F8 coverage-gap hatching | NOT-TRIGGERABLE | single segment, no gaps. |
| F9 boot-change dividers | NOT-TRIGGERABLE | no boot changes. |
| F10 time axis | VERIFIED | 9 ticks; note: time-of-day only (Finding 3). |
| F11 legend + hint | VERIFIED | gradient + gap swatch + boot marker + hint text. |
| F12 drag-select region | VERIFIED | selection rect + count "1 segment - 4.1 MB - 18:40:00-19:05:47". |
| F13 click-select segment | VERIFIED | single click selected the segment. |
| F14 Alt+drag zoom | VERIFIED | reset-zoom button appeared. |
| F15 double-click reset | VERIFIED | reset-zoom button hidden after dblclick. |
| F16 Reset zoom button | VERIFIED | shown only while zoomed. |
| F17 selection rect + row highlight | VERIFIED | host label row highlighted. |
| F18 click-outside clears | VERIFIED | selection count cleared after outside click. |
| F19 resize redraw | NOT-TESTED | window not resized this pass. |
| G1 raw search | VERIFIED | `2026-04-09` -> 1 row. |
| G2 Enter to search | VERIFIED | Enter triggered it. |
| G3 results table | VERIFIED | 7 columns rendered. |
| G4 per-row checkbox | VERIFIED | toggled via select-all. |
| G5 select-all header | VERIFIED | checked 1/1. |
| G6 Select All / Deselect All | VERIFIED | Deselect All -> 0 checked. |
| G7 empty-result sample keys | NOT-TESTED | not driven (browse F3 covered the same code path). |
| G8 column sort | DEAD-CONFIRMED | clicking the Service header did not reorder rows. |
| H1 selection count | VERIFIED | "1 segment - 4.1 MB - 18:40:00-19:05:47". |
| H2 View Selected | VERIFIED | popup `viewer.html?...&segs=1&trace=%2Fapi%2Fobject...`. |
| H3 Flamegraph | VERIFIED | popup `flamegraph.html?...`. |
| H4 100 MB cap | NOT-TRIGGERABLE | demo segment is 4.1 MB. |
| H5 selection warning | NOT-TRIGGERABLE | tied to H4. |
| I1 timezone mode | PARTIAL | toggle flips the button; full axis/picker re-render not deeply asserted. |
| I2 key parsing | VERIFIED | parse ran; mislabels the demo key (Finding 1). |
| I3 title metadata | PARTIAL | single-host case includes `host=`; multi-host drop branch not exercised. |
| I4 object URLs | VERIFIED | viewer link carries `trace=/api/object?bucket&key`. |
| I5 credentialed fetch | CODE-ONLY | header injection not asserted on the wire. |
| I6 HTML escaping | NOT-TESTED | no hostile key injected this pass. |
| I7 backend endpoints | VERIFIED | config/search/prefixes/buckets/credentials-check all responded. |

### Findings (record for the redesign)

1. **`parseKey` mislabels keys with an extra path segment.** At the time, the dev-server seeded `traces/2026-04-09/1900/demo-service/local/host-0/abcd/<epoch>-0.bin.gz` - six components after the date, vs the documented #225 layout's five. `parseKey` (I2) had no branch for that, so its positional fallback shifted the columns: Service showed `host-0`, Host showed `abcd`, Boot was empty, and the viewer/flamegraph title inherited `svc=host-0`. Caveat: conforming production keys (five-below) parsed correctly; this was the demo key's shape. Still, `parseKey` silently mislabelled rather than flagging an unrecognized layout - a robustness "fixed" candidate. `[2026-07-10]` FIXED by T15 on the migrated page: the typed parser flags these keys (`layout: "unknown"`) and every display renders them raw (see the amended I2/F4/I3 rows); the legacy page kept the shift.

2. **The dial9 bucket name-filter hides non-matching buckets, including the dev-server's own.** After Apply, `GET /api/buckets` returned 1 bucket (`demo-traces`), but the picker (C6) shows "No dial9 trace buckets visible to these credentials" because `renderBucketPicker` hard-filtered to names containing `dial9` (as of the 2026-06-30 run). The filter was intentional, but brittle: any bucket not named `*dial9*` was unreachable via the BYO-creds picker. `[2026-07-08]` RESOLVED by #607: the picker now has a "Show all (N)" toggle exposing non-dial9 buckets (see the amended C6 row), so the demo bucket is reachable again. `[2026-07-10]` STRUCTURALLY FIXED by T15 on the migrated page: the predicate itself is config-driven (`/api/config` `bucket_filter`, page-URL `?bucket_filter=` override) - see the amended C6 row.

3. **The heatmap axis shows time-of-day only (HH:MM:SS), no date.** Documented (F10), but the demo data exposed the consequence at the time: the single segment's `[trace-start epoch, last_modified]` span ran ~14 months (the synthetic key's filename epoch was 2025-04-09 while its date-path and upload time were 2026), so the axis printed repeating/parametrically-descending clock times across a multi-month width. For wide windows the time-only axis was ambiguous. (The epoch-vs-date-path mismatch itself was a demo-data artifact, not a UI bug.) `[2026-07-10]` FIXED by T15 on the migrated page: day-crossing spans render date-carrying ticks (see the amended F10 row); the legacy page stayed time-only.

### Open questions (status)

1. **G8 column sort** -> RESOLVED: confirmed dead live (header click does not reorder). "Fixed" candidate: implement sorting, or remove the affordance. `[2026-07-10]` IMPLEMENTED by T15 on the migrated page (see the amended G8 row); the legacy page keeps the dead affordance.
2. **`/api/config` failure degradation** -> NOT-RETESTED: config succeeded here; discovery + auto-search still fire on failure (`828`); behavior remains code-confirmed only.
3. **Prefix placeholder state transitions (D2)** -> PARTIAL: single-prefix auto-select observed; the other placeholder states were not reachable with the demo layout.
4. **`traceTitleParams` single-host `host=` (`1693`)** -> CONFIRMED for the single-host case (the viewer link carried `host=abcd`); the multi-host drop branch was not exercised (demo has one host). Confirm the viewer title handles a missing `host` param.

### 2026-07-08 refresh (drift commits #582/#570/#585/#607/#600/#586)

Method: dev-server on :3001 (same launch command), driven with `curl` for API-backed
states + code read for pure-UI states. NO browser driver this pass, so DOM-interaction
verdicts are static-analysis based and marked CODE-READ (weaker than the 2026-06-30
VERIFIED; re-derivable by the inventory row-walker once it exists). Dev-server facts
observed this pass: `/api/config` returns `aggregation_enabled:true`,
`supports_byo_credentials:true`, `supports_assume_role:false`; `/api/search` -> 404
(removed); `/api/browse` works (returned the seeded object for a 2026-04-09 window,
`truncated:false`); `/api/flamegraph` + `/api/tokio-stats` are FUNCTIONAL, not 404:
the first read-only poll returns an empty tree with coverage `files_folded:0`, a
`refine=true` poll folds the demo trace (147 samples), after which read-only polls
return the folded tree + facets. The shared-decisions "may 404" caveat did not
reproduce on this seed data.

| Row | Verdict | Evidence / note |
|---|---|---|
| A5 (amended) | VERIFIED (API) | `/api/config` fields above; `aggregation_enabled` read confirmed in code (`814-817`). |
| A6 URL sync | CODE-READ + unit-tested | serialization unit-tested (`test_url_state.js` green, incl. `aws_region` round-trip); DOM triggers not driven. |
| A7 URL restore | CODE-READ | restore order (tz -> fields -> region -> range) read at `752-796`; not driven. |
| C6 (amended) show-all toggle | CODE-READ | toggle render logic at `543-557`; picker not driven this pass. |
| C11 region auto-detect | NOT-TRIGGERABLE (dev data) | fake S3 is single-region; the 421 wrong-region path is not reproducible locally. Code read at `841-874`. |
| F2 (rewritten) `/api/browse` | VERIFIED (API) | curl returned the seeded object + `truncated:false` for the April window. |
| F3/G7 (amended) sample-key window | VERIFIED (API) | curl `/api/browse` last-24h window returned `objects: []` -> "Bucket appears empty" branch (demo key is April). |
| F20 truncation banner | NOT-TRIGGERABLE (dev data) | one seeded object cannot overflow the cap; `truncated:false` observed; banner logic read at `428-439`, `1197-1200`. |
| G1 (rewritten) raw search | VERIFIED (API) | endpoint + `.objects` shape confirmed by curl; the last-30-days window makes the April demo key unreachable from raw search. |
| H3 (amended) agg mode | VERIFIED (API) + CODE-READ | the URL it builds was replayed via curl against `/api/flamegraph` (works, see above); button click not driven. |
| H4 (amended) 200 MB cap | NOT-TRIGGERABLE (dev data) | demo segment is 4.1 MB; `MAX_OPEN_BYTES` = 200 MB confirmed at `heatmap.js:27`. |
| H6 Tokio Stats button | PARTIAL | markup + tab/enable gating confirmed in the served page (`357-359`, `1136`, `1899`/`1907`); `/api/tokio-stats` responds (read-only poll shape observed); click not driven. |
| I7 (amended) endpoints | VERIFIED (API) | `/api/search` 404; `/api/browse` 200. |
| I8 extractPrefix | CODE-READ | pure helper at `1061-1071`; consumed by H3/H6 URL builders. |
| G8 column sort | DEAD (re-confirmed by code) | `data-sort` markup still present at `331-337`, still no click handler in the file. |

### 2026-07-10 T15 amendments (the four structurally-fixed defects)

The contract for these rows is CONSCIOUSLY AMENDED (ADR-0004 section 1;
`docs/tickets/ledger.md`): the entries below supersede the rows' earlier
verdicts for the row-walker gate. They are the canonical behavior after #769.
The original dual-page verification below was recorded live on 2026-07-10;
the evidence column now states only the surviving contract.

| Row | Verdict | Evidence / note |
|---|---|---|
| G8 (amended) sortable columns | VERIFIED | Header click sorts asc, second click flips desc (`.sort-arrow` + `aria-sort`); ordering semantics are pinned by `raw-rows.test.ts` (single-row dev seed). |
| C6 (amended) config-driven bucket filter | VERIFIED | Default "dial9" filter -> empty filtered view + "Show all (1)" reveals `demo-traces`; `?bucket_filter=demo` surfaces and auto-selects it. |
| I2 (amended) unknown-layout raw rendering | VERIFIED | Seeded unknown-layout key renders raw across Service/Host/Boot (`td.rawkey[colspan=3]`); Trace Start still comes from the filename epoch. |
| F4 (amended) unknown-layout row label | VERIFIED | Heatmap row label is the raw directory path `traces/2026-04-09/1900/demo-service/local/host-0/abcd`. |
| F10 (amended) day-crossing axis | VERIFIED | Day-crossing seed span renders `YYYY-MM-DD HH:MM:SS` ticks with the same tick count. |

## Reproduce

```bash
# 1. backend on :3001 (stop any static serve.py on 3001 first)
PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server
# 2. confirm gate
curl -s http://localhost:3001/api/config   # must be JSON
# 3. drive headless Chromium (Playwright harness: per-feature actions, asserts, screenshots)
node validate.js                            # writes results.json, summary.md, shots/
```
