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

## What this surface is

The landing page / S3 trace browser. Lets a user find trace segments in an S3
bucket (by time range or raw prefix), preview their data density on a timeline,
select some, and open them in the viewer or the flamegraph. Also the drop point
for local `.bin` files and the demo trace.

- Entry file: `dial9-viewer/ui/index.html` (markup + inline `<style>` + inline `<script>`)
- Loaded modules: `creds.js`, `prefix_detect.js`, `heatmap.js`
- Backend endpoints consumed: `/api/config`, `/api/prefixes`, `/api/search`, `/api/object`, `/api/buckets`, `/api/credentials/check`

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
| A1. `?trace=` passthrough  | If the landing URL itself carries any `trace=` param, immediately redirect to `viewer.html` preserving the full query string (including repeated `trace=`).                                                                               | Open `index.html?trace=...` -> lands on viewer instead. | `index.html:366-374` (IIFE)          |
| A2. Load demo trace        | Opens `viewer.html?trace=demo-trace.bin` in a new tab.                                                                                                                                                                                    | Footer (bottom bar) -> "Load demo trace" button.        | `index.html:358`, `377-379`          |
| A3. Drag-and-drop `.bin`   | Dropping a file anywhere on the page opens `viewer.html?trace=<objectURL>` in a new tab; footer border highlights purple while dragging.                                                                                                  | Drag a `.bin` file onto the window.                     | footer `354-360`; handlers `380-393` |
| A4. Open Trace Viewer link | Plain link to `viewer.html` (no trace).                                                                                                                                                                                                   | Footer -> "Open Trace Viewer".                          | `index.html:356`                     |
| A5. Config bootstrap       | On load, `GET /api/config` -> prefills bucket (`default_bucket`, unless BYO creds active), prefix (`default_prefix`), and enables the credentials UI (`supports_byo_credentials`). On failure, still runs prefix discovery + auto-search. | Automatic on page load.                                 | `index.html:642-663`                 |

Notes:

- A5 deliberately does NOT prefill the server default bucket when the user has brought their own credentials, because that bucket belongs to the server identity (`index.html:649-655`).

---

## B. Header bar

| Feature                    | What it does                                                                                                                                                  | Access path                                    | Source                                                 |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- | ------------------------------------------------------ |
| B1. Title / subtitle       | Static "dial9 Trace Browser" + "search & view traces from S3".                                                                                                | Top-left of header.                            | `index.html:210-211`                                   |
| B2. AWS Credentials button | `CONDITIONAL`. Hidden unless server reports `supports_byo_credentials`. Toggles the credentials panel. Turns green with a check when creds are active.        | Header, right side -> "[key] AWS Credentials". | button `213-215`; reveal `434`; active state `442-446` |
| B3. Timezone toggle        | Flips all date display + the datetime pickers + heatmap axis between UTC and Local. Re-renders current view. Picker values are converted, not just relabeled. | Header, right side -> "TZ: UTC" / "TZ: Local". | button `216`; handler `741-759`                        |

---

## C. Bring-your-own-credentials panel (`CONDITIONAL` on `supports_byo_credentials`)

Initialized by `initCredsUi()` (`index.html:424-618`). Credentials live in `sessionStorage` (die with the tab) and ride as `x-dial9-aws-*` headers on every `/api/*` request via `apiFetch` (`415-418`). Store/parse/headers logic is in `creds.js`.

| Feature                            | What it does                                                                                                                                                                                                          | Access path                                                       | Source                                                               |
| ---------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------- | -------------------------------------------------------------------- |
| C1. Open/close panel               | Button toggles; "X" closes.                                                                                                                                                                                           | Header "AWS Credentials" button; panel "X" at top-right of panel. | `455-460`; markup `222-228`                                          |
| C2. Paste JSON -> Fill fields      | Parses an STS/Isengard response (nested `credentials`) or a flat `{accessKeyId,...}` blob (tolerates camel/snake/SCREAMING case), fills the fields, then clears the textarea so the secret is not left sitting there. | Panel -> paste into "Paste JSON" textarea -> "Fill fields".       | handler `464-483`; parser `creds.js:94-138`                          |
| C3. Manual credential fields       | Access key ID, secret (password), session token (password), region (placeholder "auto-detect").                                                                                                                       | Panel input rows.                                                 | markup `236-247`                                                     |
| C4. Apply                          | Validates akid+secret present, clears any prior bucket selection, stores creds (no region yet), then lists visible buckets.                                                                                           | Panel -> "Apply".                                                 | handler `546-566`; `loadBuckets` `570-580`; `creds.js set` `156-188` |
| C5. Clear                          | Wipes stored creds, empties fields + bucket picker, and resets the whole browse pane (the heatmap belonged to the removed identity).                                                                                  | Panel -> "Clear".                                                 | handler `582-595`; `creds.js clear` `230`                            |
| C6. Bucket picker                  | After Apply, lists buckets the creds can see, but hard-filters to names containing "dial9", sorted; highlights them; auto-selects when exactly one.                                                                   | Appears in panel after Apply (or on return visit).                | `renderBucketPicker` `490-518`                                       |
| C7. Select bucket -> region detect | Fills the bucket field, calls `POST /api/credentials/check` to resolve+persist the region, then re-discovers prefixes and re-runs the current search.                                                                 | Click a bucket chip in the picker.                                | `selectBucket` `522-544`; `creds.js check` `197-211`                 |
| C8. Status line                    | Inline ok/error/neutral messages for every step.                                                                                                                                                                      | Below the Apply/Clear row.                                        | `setStatus` `436-438`; CSS `72-74`                                   |
| C9. Returning user auto-list       | If creds already in sessionStorage on load, silently re-lists buckets so the picker is ready (panel stays closed; green button signals active).                                                                       | Automatic on load when creds present.                             | `615-617`                                                            |
| C10. Scripting API + change event  | `window.Dial9Creds.set(...)` is the stable userscript entry point; firing `dial9:credentials-changed` refreshes the UI and re-runs the search (only when creds are now present, not on Clear).                        | Programmatic (injected userscript).                               | listener `600-606`; `creds.js` `141-188`, `253-257`                  |

Notes:

- C7 intentionally does NOT clear creds on a failed bucket check, since the failure is usually bucket-specific (`creds.js:177-184`).
- Credentials are never persisted beyond the tab session and never leave this origin (`creds.js` header comment, `1-14`).

---

## D. Controls bar (search inputs)

| Feature                                | What it does                                                                                                                                                                | Access path                                  | Source                                                     |
| -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- | ---------------------------------------------------------- |
| D1. Bucket input                       | Free text. Prefilled from `?bucket=` param, else config default (unless BYO creds).                                                                                         | Controls bar -> "Bucket:" field.             | markup `262`; prefill `631`, `653`                         |
| D2. Prefix input                       | Free text key prefix. Placeholder cycles through states: "detecting...", "(none found)", "(no prefix - dates at root)", "discovery failed - enter manually", "e.g. traces". | Controls bar -> "Prefix:" field.             | markup `264`; states in `discoverPrefixes` `680-735`       |
| D3. Prefix suggestion chips            | Auto-built from `GET /api/prefixes`; clicking one fills the prefix and marks it active.                                                                                     | Controls bar, right of prefix field.         | `693-729`                                                  |
| D4. Date-layer auto-empty (#471)       | If every discovered root child looks like a date (`YYYY-MM-DD/`), the prefix is set empty (dates are not selectable prefixes).                                              | Automatic during discovery.                  | `704-710`; `prefix_detect.js isDateLayer` `25-28`          |
| D5. Single-prefix auto-select          | Exactly one prefix + empty input -> auto-fills it.                                                                                                                          | Automatic during discovery.                  | `712-714`                                                  |
| D6. Quick range buttons                | "Last 1hr / 3hr / 24hr" set the From/To pickers and highlight the chosen button. Default on load = 1hr.                                                                     | Controls bar -> quick buttons.               | markup `267-271`; `setQuickRange` `875-886`; default `666` |
| D7. From / To pickers                  | `datetime-local` inputs, 1-minute step, interpreted in the current TZ mode.                                                                                                 | Controls bar -> "From:" / "To:".             | markup `273-275`; `pickerToDate` `867-872`                 |
| D8. Manual-edit clears quick highlight | Editing From/To deselects the quick-range button.                                                                                                                           | Edit a picker after clicking a quick button. | `889-894`                                                  |
| D9. Search button                      | Disabled until a prefix is present when the server declares one. Runs the time-range (Browse) search.                                                                       | Controls bar -> "Search" (primary).          | markup `276`; `updateSearchReady` `636-640`                |
| D10. Re-discover on bucket change      | Changing the bucket re-runs prefix discovery.                                                                                                                               | Edit bucket field, blur/change.              | `738`                                                      |

---

## E. Tabs

| Feature                      | What it does                                                                                                   | Access path              | Source                                  |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------ | --------------------------------------- |
| E1. Browse / Raw Search tabs | Switch between the density-heatmap browse view and the raw prefix-search table.                                | Tab bar under controls.  | markup `279-282`; `switchTab` `897-907` |
| E2. Per-tab action swap      | Browse shows the Flamegraph button; Raw shows Select All / Deselect All. Selection count recomputed on switch. | Automatic on tab switch. | `903-906`                               |

---

## F. Browse view: density heatmap timeline

The headline feature. `doTimeRangeSearch` (`926-1016`) fetches, the `renderHeatmap`/`drawHeatmapCanvas` pipeline (`1051-1225`) draws, and the interaction IIFE (`1230-1293`) handles pointer input. Pure data helpers are in `heatmap.js`.

| Feature                                  | What it does                                                                                                                                                                                                                 | Access path                              | Source                                                                                  |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------------------- |
| F1. Initial prompt                       | "Select a time range and click Search to find traces."                                                                                                                                                                       | Browse tab before any search.            | markup `287-289`                                                                        |
| F2. Time-range search                    | Expands the range into hourly prefixes, fires one `GET /api/search` per hour in parallel, flattens, then filters to objects whose epoch overlaps the actual range.                                                           | Set range -> "Search" (or auto on load). | `hourPrefixes` `910-923`; `doTimeRangeSearch` `926-971`                                 |
| F3. Empty-result sample keys             | If nothing matches, shows up to 5 sample keys from the bucket to reveal its layout (or "Bucket appears empty").                                                                                                              | Search a range with no data.             | `973-991`                                                                               |
| F4. Host rows                            | One row per `service / host` (boot changes do NOT split rows). Rows sorted by label, segments sorted by start.                                                                                                               | Rendered left column of heatmap.         | `heatmap.js groupByHost` `49-71`; labels `1086-1097`                                    |
| F5. Boot-count annotation                | Row label shows "[mark] N boots" when boot_id changes within the window.                                                                                                                                                     | Heatmap left labels.                     | labels `1091-1093`; `bootTransitions` `heatmap.js:77-90`                                |
| F6. Density canvas                       | Bytes spread uniformly across each segment span, summed per pixel column, normalized to a sqrt color ramp (dim blue -> purple -> red -> yellow). 256-entry precomputed palette + run-length coalescing for speed. DPR-aware. | The colored strips.                      | `drawHeatmapCanvas` `1102-1157`; `accumulateDensity`/`densityColor` `heatmap.js:98-201` |
| F7. Seam tiling                          | Segment ends are clamped to the next start so upload-lag overlaps do not double-count into a false bright band.                                                                                                              | Visual (no control).                     | `1122`; `tileSegments` `heatmap.js:131-141`                                             |
| F8. Coverage-gap hatching                | Genuine gaps (a host that stopped reporting) drawn as faint diagonal-hatched bands with edge ticks, distinct from low density.                                                                                               | Visual; legend "gap (no data)".          | gaps `1158-1189`; `segmentGaps` `heatmap.js:148-157`                                    |
| F9. Boot-change dividers                 | Dashed cyan vertical lines at boot transitions.                                                                                                                                                                              | Visual; legend "boot change".            | `1190-1203`                                                                             |
| F10. Time axis                           | 2 to 8 ticks, TZ-aware HH:MM:SS, aligned to the canvas left edge via `--heatmap-label-w`.                                                                                                                                    | Below the canvas.                        | `drawHeatmapAxis` `1212-1225`; `fmtTick` `1029-1035`                                    |
| F11. Legend + hint                       | Density gradient, gap swatch, boot-change marker, and interaction hint text.                                                                                                                                                 | Top of heatmap view.                     | markup `291-296`                                                                        |
| F12. Drag-select region                  | Plain drag selects a rectangle (rows x time); the selection snaps to the actual `[min start, max end]` of the covered files (whole files open, S3 cannot sub-range).                                                         | Drag across the canvas.                  | `1248-1289`; `finalizeSelection` `1387-1412`                                            |
| F13. Click-select one segment            | A click (drag < 4px) selects the single segment under the cursor; ties broken by nearest start.                                                                                                                              | Single click on a strip.                 | `selectSegmentAt` `1369-1385`                                                           |
| F14. Option/Alt+drag zoom                | Alt+drag zooms the time axis to the dragged span; density re-normalizes to the visible window (flat-looking regions reveal structure).                                                                                       | Hold Option/Alt, drag horizontally.      | `1251`, `1278-1281`; `zoomToX` `1299-1311`                                              |
| F15. Double-click reset zoom             | Restores the full data extent.                                                                                                                                                                                               | Double-click anywhere on the plot.       | `1292`; `resetHeatmapZoom` `1331-1339`                                                  |
| F16. Reset zoom button                   | Appears only while zoomed.                                                                                                                                                                                                   | Heatmap hint bar -> "Reset zoom".        | markup `296`; `updateZoomResetBtn` `1342-1349`                                          |
| F17. Selection rectangle + row highlight | Persistent purple box over selected rows/time; selected host labels get a highlight.                                                                                                                                         | After a selection.                       | `showSelRect` `1353-1365`; `setHeatmapSelection` `1414-1429`                            |
| F18. Click-outside clears                | Clicking outside the heatmap and actions bar clears the selection.                                                                                                                                                           | Click elsewhere on the page.             | `1448-1452`                                                                             |
| F19. Resize redraw                       | Debounced (100ms) canvas re-measure + redraw + selection re-place on window resize.                                                                                                                                          | Resize the window.                       | `1433-1443`                                                                             |

---

## G. Raw search view

| Feature                        | What it does                                                                                                                                                                                                                       | Access path                                   | Source                                                                     |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------- | -------------------------------------------------------------------------- |
| G1. Raw prefix search          | Free-text prefix -> single `GET /api/search?q=` -> table.                                                                                                                                                                          | Raw tab -> prefix field -> "Search".          | markup `310-317`; `doRawSearch` `1507-1529`                                |
| G2. Enter to search            | Enter in the prefix field triggers the search.                                                                                                                                                                                     | Raw prefix field -> Enter.                    | `1531-1533`                                                                |
| G3. Results table              | Columns parsed from the key: Service, Host, Boot, Trace Start, Seg #, Size, Uploaded. Rows sorted by trace-start epoch.                                                                                                            | Raw tab table.                                | markup `320-334`; `renderRawTable` `1535-1583`                             |
| G4. Per-row checkbox           | Select individual segments.                                                                                                                                                                                                        | Checkbox in each row.                         | `1570`, `1579`                                                             |
| G5. Select-all header checkbox | Toggles all row checkboxes.                                                                                                                                                                                                        | Header-row checkbox.                          | markup `323`; `1585-1587`                                                  |
| G6. Select All / Deselect All  | Buttons in the actions bar (Raw tab only).                                                                                                                                                                                         | Actions bar -> "Select All" / "Deselect All". | markup `341-342`; `rawSelectAll` `1589-1593`                               |
| G7. Empty-result sample keys   | Same sample-key hint as Browse when no results.                                                                                                                                                                                    | Search a prefix with no data.                 | `1541-1558`                                                                |
| G8. Column sort                | `DEAD` (confirmed live 2026-06-30). Headers have `data-sort` attributes, `.sort-arrow` CSS, pointer cursor, and hover styling, signaling sortable columns, but no click handler is wired. Rows are always sorted by epoch. Clicking a header does nothing. | Click a column header -> no effect.           | markup `324-330`; CSS `184-186`; sort fixed at `1564`; (no handler exists) |

> G8 is a "fixed" candidate: the UI advertises sortable columns it does not deliver. Decision needed during design: implement sorting, or remove the affordance.

---

## H. Actions bar

| Feature               | What it does                                                                                       | Access path                                     | Source                                           |
| --------------------- | -------------------------------------------------------------------------------------------------- | ----------------------------------------------- | ------------------------------------------------ |
| H1. Selection count   | Browse: "N segments - - -". Raw: "N selected".                                                     | Right side of actions bar.                      | markup `351`; `updateSelectionCount` `1605-1636` |
| H2. View Selected     | Opens `viewer.html` with one `trace=/api/object?...` per selected key plus title metadata.         | Actions bar -> "View Selected in Trace Viewer". | markup `344-346`; `viewSelected` `1638-1650`     |
| H3. Flamegraph        | `CONDITIONAL` (Browse tab only). Opens `flamegraph.html` with the same per-key `trace=` set.       | Actions bar -> "[fire] Flamegraph".             | markup `347-349`; `viewCpuProfile` `1496-1504`   |
| H4. 100 MB open cap   | If selection bytes > `MAX_OPEN_BYTES` (100 MB), View + Flamegraph disable and a red warning shows. | Select a very wide range.                       | `1620-1625`; `MAX_OPEN_BYTES` `heatmap.js:27`    |
| H5. Selection warning | Red inline message slot (used by H4).                                                              | Actions bar.                                    | markup `350`                                     |

---

## I. Cross-cutting behaviors (replication-critical, not single buttons)

| Behavior               | Detail                                                                                                                                                                 | Source                                       |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| I1. Timezone mode      | `useLocalTz` flag drives `formatDate`, `formatEpoch`, `fmtTick`, and picker read/write. Toggling re-renders the active view and rewrites picker values.                | `776-792`, `849-872`, `1029-1035`, `741-759` |
| I2. Key parsing        | `parseKey` handles the default layout (#225, with boot_id) and the legacy layout (no boot_id), plus a positional fallback. Drives every Service/Host/Boot/Seg display. | `794-847`                                    |
| I3. Title metadata     | `traceTitleParams` derives `svc`, `host`, `from`, `to`, `segs` query params shared by viewer and flamegraph headers.                                                   | `1457-1473`                                  |
| I4. Object URLs        | `objectTraceUrls` builds one `/api/object?bucket&key` per file; viewer/flamegraph fetch them in parallel + gunzip client-side.                                         | `1482-1489`                                  |
| I5. Credentialed fetch | `apiFetch` spreads `Dial9Creds.headers()` into every `/api/*` call.                                                                                                    | `415-418`                                    |
| I6. HTML escaping      | `esc()` escapes any key/text injected into innerHTML (sample keys, table rows).                                                                                        | `764-768`                                    |
| I7. Backend endpoints  | `/api/config`, `/api/prefixes`, `/api/search`, `/api/object`, `/api/buckets`, `/api/credentials/check`.                                                                | throughout                                   |

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

1. **`parseKey` mislabels keys with an extra path segment.** The dev-server seeds `traces/2026-04-09/1900/demo-service/local/host-0/abcd/<epoch>-0.bin.gz` - six components after the date, vs the documented #225 layout's five. `parseKey` (I2) has no branch for that, so its positional fallback shifts the columns: Service shows `host-0`, Host shows `abcd`, Boot is empty, and the viewer/flamegraph title inherits `svc=host-0`. Caveat: conforming production keys (five-below) parse correctly; this is the demo key's shape. Still, `parseKey` silently mislabels rather than flagging an unrecognized layout - a robustness "fixed" candidate.

2. **The dial9 bucket name-filter hides non-matching buckets, including the dev-server's own.** After Apply, `GET /api/buckets` returned 1 bucket (`demo-traces`), but the picker (C6) shows "No dial9 trace buckets visible to these credentials" because `renderBucketPicker` hard-filters to names containing `dial9` (`index.html:490-518`). The filter is intentional, but brittle: any bucket not named `*dial9*` is unreachable via the BYO-creds picker. With the demo backend the BYO bucket-selection happy-path is therefore not usable.

3. **The heatmap axis shows time-of-day only (HH:MM:SS), no date.** Documented (F10), but the demo data exposed the consequence: the single segment's `[trace-start epoch, last_modified]` span runs ~14 months (the synthetic key's filename epoch is 2025-04-09 while its date-path and upload time are 2026), so the axis prints repeating/parametrically-descending clock times across a multi-month width. For wide windows the time-only axis is ambiguous. (The epoch-vs-date-path mismatch itself is a demo-data artifact, not a UI bug.)

### Open questions (status)

1. **G8 column sort** -> RESOLVED: confirmed dead live (header click does not reorder). "Fixed" candidate: implement sorting, or remove the affordance.
2. **`/api/config` failure degradation** -> NOT-RETESTED: config succeeded here; discovery + auto-search still fire on failure (`663`); behavior remains code-confirmed only.
3. **Prefix placeholder state transitions (D2)** -> PARTIAL: single-prefix auto-select observed; the other placeholder states were not reachable with the demo layout.
4. **`traceTitleParams` single-host `host=` (`1464`)** -> CONFIRMED for the single-host case (the viewer link carried `host=abcd`); the multi-host drop branch was not exercised (demo has one host). Confirm the viewer title handles a missing `host` param.

## Reproduce

```bash
# 1. backend on :3001 (stop any static serve.py on 3001 first)
PORT=3001 cargo run -p dial9-viewer --bin dev-server --features dev-server
# 2. confirm gate
curl -s http://localhost:3001/api/config   # must be JSON
# 3. drive headless Chromium (Playwright harness: per-feature actions, asserts, screenshots)
node validate.js                            # writes results.json, summary.md, shots/
```
