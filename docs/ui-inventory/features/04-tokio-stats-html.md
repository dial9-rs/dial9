# UI Feature Inventory: `tokio_stats.html` (Tokio runtime stats page)

> Code-derived inventory of the fourth page, with per-feature verification
> verdicts. Purpose: capture every existing behavior precisely enough that
> (a) each can be validated in the running UI and (b) the surface can be
> re-implemented without losing anything (T41). Source line numbers are a
> snapshot; the function name is the stable anchor.
>
> SNAPSHOT 2026-07-10 against the chunk-1 integration tip (commit c1d923e,
> which includes #570 page creation, #587 XSS hardening, and the T38
> `ui-switch.js` include). Written by T40; unlike features/01..03 this file
> has no pre-drift baseline - the page postdates the original inventory
> snapshot. Verification method and per-row verdicts are in the
> "2026-07-10 validation" section at the end. No `features04` row-walker
> registry exists yet (that is T41's implementation-time deliverable);
> DOM-interaction verdicts here are CODE-READ, API-contract verdicts were
> walked live with curl against the dev-server.
>
> UPDATE 2026-08-10: #769 made the Vite implementation canonical and removed
> the prior UI and rollout switch. The snapshot remains historical; A8 records
> the switch's conscious retirement and is gated by the row-walker.

## What this surface is

The Tokio runtime health page. It is ENTIRELY an aggregated server-driven
surface (the analogue of flamegraph's section P - there is no exact/client-
decode mode): the page never downloads or decodes trace bytes. It drives the
server's demand-driven `GET /api/tokio-stats` refinement loop over the polls
Parquet aggregates, then classifies and renders "long polls" (polls above a
user-tunable duration threshold) grouped by task spawn location, split into
off-CPU (blocked), on-CPU (compute), mixed, and unknown. It supports up to
multiple comparison periods with a diff view (regressions / new offenders /
improvements between the first and last period), and per-class exemplar deep
links into the viewer.

- Entry file: `dial9-viewer/ui/tokio_stats.html` (markup + inline `<style>` +
  inline `<script>` IIFE; added by #570, XSS-hardened by #587)
- Loaded modules: `ui-switch.js` (in `<head>`, T38 dual-UI switch),
  `creds.js` (BYO AWS credential headers). NO frozen-core modules - this page
  does not parse traces.
- Backend endpoints consumed: `GET /api/tokio-stats` (the only fetch the page
  makes). Exemplar deep links open `viewer.html?trace=/api/trace?...` - and
  `/api/trace` was REMOVED by #582, so those links are dead at HEAD (row H4,
  finding 1).
- Entry point in the UI: the S3 browser's "Tokio Stats" button - features/01
  row H6 (Browse tab only, disabled unless `aggregation_enabled`). Not
  re-documented here; H6 owns the button, this file owns the page.

## How to read this document

| Column | Meaning |
| --- | --- |
| **Feature** | One discrete capability. |
| **What it does** | Behavior, including edge cases and non-obvious rules. |
| **Access path** | Precise way to reach/trigger it in the running UI (click path / interaction / URL param). |
| **Source** | `file:line` (+ function name). Line numbers are a snapshot; the function name is the stable anchor. |

Status tags used in notes: `OK` (works), `DEAD` (present but not functional),
`CONDITIONAL` (only present/active under a server or runtime condition).
Plain ASCII arrows (`->`) are used throughout; where the running UI renders a
Unicode glyph (the zap emoji in the heading, up/down arrows, the multiply
sign on the remove button, etc.) the codepoint is called out in parentheses
rather than typed.

To run the page locally with a working backend, use the full dev-server
(post-T04 it serves `ui/dist`, so build first): in `dial9-viewer/ui` run
`npm ci && npm run build`, then
`PORT=3071 cargo run -p dial9-viewer --bin dev-server --features dev-server`.
Its aggregate endpoints ARE functional against the seeded `demo-traces`
bucket (first poll cold/empty, `refine=true` folds the demo trace - see the
validation section; recipe established by T18). Time-window, multi-host and
multi-service states are NOT reachable on the seed data (reasons per row).

---

## A. Page bootstrap and URL contract

The whole page is one IIFE (`tokio_stats.html:65-429`). URL params are read
once into `params` (`:66`) and never re-read.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| A1. Page serving | Served at `/tokio_stats.html` from the embedded `ui/dist` (rust-embed; the Vite build static-copies the legacy page byte-identical, `vite.config.ts:22-28`). Also served by the dev-server the same way. | Open `/tokio_stats.html`. | `dial9-viewer/src/server/mod.rs:53-54` (Embed), `388-389` (fallback `serve_embedded`); `dial9-viewer/ui/vite.config.ts:28` |
| A2. Entry point | `CONDITIONAL`. Opened in a new tab by the S3 browser's "Tokio Stats" button with scope params `bucket`, `prefix`, `service`, repeated `host`, `start_ns`/`end_ns` from the heatmap selection. Button gating (Browse tab, `aggregation_enabled`) is features/01 H6's row, not this file's. (features/01's anchors predate T38's `ui-switch.js` include, which shifted `index.html` by +1 line; at this tree the markup is `358-360`, `viewTokioStats` `1770-1787`.) | features/01 H6 (the "[zap] Tokio Stats" button in the actions bar). | cross-link: `docs/ui-inventory/features/01-index-html.md` row H6; `index.html:358-360,1770-1787` |
| A3. Scope params | Reads `bucket`, `prefix`, `service` (single) and `host` (repeatable, `getAll`) from the URL. They scope every `/api/tokio-stats` request and are preserved verbatim by URL sync (A7). Never edited in-page: the page has no scope controls, so a different scope means a different URL. | URL `?bucket=&prefix=&service=&host=&host=...`. | `tokio_stats.html:66,376-379` (request), `149-150` (sync) |
| A4. Multi-period params | Restores comparison periods from `p{i}_start_ns` / `p{i}_end_ns` for i = 1..10 (either bound present creates the period). Values are epoch-nanosecond strings (kept as strings end-to-end for >2^53 precision). Cap: only 10 periods restore from the URL, though A7 writes every period out - an 11th+ added period round-trips into the URL but silently drops on reload. | URL `?p1_start_ns=&p1_end_ns=&p2_start_ns=...`. | `tokio_stats.html:139-144` |
| A5. Single-period fallback | When no `p{i}_` param exists, creates one period from `start_ns`/`end_ns` (the shape the features/01 H6 button builds; also the backward-compat shape). Both absent -> one blank period. | URL `?start_ns=&end_ns=` (or nothing). | `tokio_stats.html:145` |
| A6. Auto-load on open | Fires `loadAll()` immediately when the ORIGINAL URL had `start_ns` or `bucket`. Caveat: A7 rewrites `start_ns`/`end_ns` to `p1_*` during init, so a bucket-less URL (`?start_ns=...` only) auto-loads once but its rewritten URL no longer auto-loads on reload (`p1_start_ns` does not satisfy the check). With `bucket` present (the H6 shape) reloads keep auto-loading. | Open the page with `start_ns` or `bucket` in the URL. | `tokio_stats.html:428` |
| A7. URL sync | `syncUrl()` rebuilds the query from scratch on every period add/remove/edit: keeps `bucket`/`prefix`/`service`/`host*` from the original params, writes `p{i+1}_start_ns`/`_end_ns` per period (omitting unset bounds), `history.replaceState` (no history entries). DROPS everything else - the legacy `start_ns`/`end_ns` (renamed to `p1_*`) and unknown params including `ui=` (which is why `ui-switch.js` has `pinWouldBounce`, A8). | Automatic on any period change (also during init). | `tokio_stats.html:147-156` (`syncUrl`) |
| A8. Retired dual-UI switch | `DEAD` by decision (#769). The canonical page has no rollout control or alternate UI route. | Open the page; `#d9-ui-switch` must be absent. | `tokio_stats.html`; `live-checks/walkers/features04.mjs` (`A8`) |
| A9. Credential attachment | `creds.js` provides `Dial9Creds.headers()`: `x-dial9-aws-*` headers (key id, secret, optional token/region) from sessionStorage, spread into every `/api/tokio-stats` fetch; empty object when no creds stored. Same-origin by construction (URL built from `window.location.origin`), so no cross-origin withholding logic is needed here. | Automatic when creds were applied on the home page. | `tokio_stats.html:46,386-387`; `creds.js:240-252` (`headers`) |
| A10. Title and heading | Browser tab title `Tokio Stats`; page heading `[zap] Tokio Stats` (U+26A1 emoji). Static - never updated from data (unlike flamegraph's api mode F175). | Visible on load. | `tokio_stats.html:5,47` |

---

## B. Period management

Periods are `{id, startNs, endNs, data}` in the module-scoped `periods` array;
re-rendered wholesale by `renderPeriods()` (`tokio_stats.html:116-126`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| B1. Period rows | One row per period: a colored `P{i}` tag (4-color palette cycling: blue/purple/green/orange at 20% background), From/To inputs, optional remove button. Rebuilt with `innerHTML` on every add/remove/UTC-toggle; in-progress input edits are lost on rebuild. | The rows above the toolbar. | `tokio_stats.html:67` (COLORS), `116-126` (`renderPeriods`) |
| B2. From/To datetime inputs | `datetime-local` inputs with 1-second step, values formatted from the stored ns strings via `nsToDatetime`. `onchange` (inline attribute -> global `window.updatePeriod`) parses back via `datetimeToNs` (ms-precision floor, stored as ns string; empty -> null) and syncs the URL. No reload/refetch - a changed bound takes effect on the next Load. | Edit a From/To field. | `tokio_stats.html:121-122`, `127-134` (`updatePeriod`), `75-85` (converters) |
| B3. UTC toggle | Checkbox (default CHECKED = UTC). Governs BOTH directions of conversion: display (`nsToDatetime`: UTC slice of ISO string vs timezone-offset-shifted) and parse (`datetimeToNs`: appends `Z` when UTC, else local-time `Date` parse). Toggling re-renders the period inputs to the other convention (stored ns values unchanged); does NOT re-render results or sync the URL. | Toolbar checkbox "UTC". | `tokio_stats.html:57,73,75-85,414` |
| B4. Add period | `+ Add period` appends a period. If the last period has BOTH bounds, the new one is the immediately-preceding window of the same span (`[start - span, start]`) - the "compare against the previous window" gesture; otherwise blank. Renders + syncs URL. No client-side cap (but see A4's 10-period restore cap). | Toolbar -> "+ Add period". | `tokio_stats.html:416-424`, `102-107` (`addPeriod`) |
| B5. Remove period | An x button (U+2715) per row, shown only when there are 2+ periods (the last period cannot be removed). Filters the period out, re-renders, syncs URL, and re-renders results from cache (a loaded period's contribution disappears without refetch). | Click the x on a period row. | `tokio_stats.html:123`, `109-114` (`removePeriod`) |
| B6. Global inline handlers | Period-row and tab interactions use inline `onclick`/`onchange` attributes wired to `window.updatePeriod` / `window.removePeriod` / `window.setTab` / `window.openExemplar` globals (interpolated arguments are numeric ids / fixed strings only). A re-implementation must not lose these bindings when moving to declarative templates (N17). | Internal (wiring). | `tokio_stats.html:121-123,127-135,221,224,268,426` |

---

## C. Toolbar: Load and threshold

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| C1. Load button | Loads ALL periods in parallel (`Promise.all` over `loadPeriod`, section D). Primary (purple) button. Can be re-clicked to reload; each period's cached data is overwritten as responses land. | Toolbar -> "Load". | `tokio_stats.html:51,400-412,415` (`loadAll`) |
| C2. Status line | Text states: `Loading...` (U+2026 ellipsis) while loading, `Complete` on success, `Error: <message>` in red (`.error`) when any period's load throws (the message is the HTTP response body text, e.g. the backend's 404 strings, J3/J7). | Below the toolbar, during/after Load. | `tokio_stats.html:59,401-411`; CSS `:34-35` |
| C3. Threshold slider | Log-scale range input: value v in [-1, 3] step 0.1 -> threshold `10^v` ms (i.e. 100us to 1s, default v=0 -> 1ms). The label renders the live value via `formatDuration` (s/ms/us/ns units; U+00B5 micro sign). The slider floor equals the server's 100us duration floor (J10), so the client can never ask below what the wire carries. | Toolbar slider "Threshold:". | `tokio_stats.html:52-56,159-164` (`thresholdNs`), `86-91` (`formatDuration`) |
| C4. Instant re-threshold | Slider `input` re-runs the whole render pipeline from cached responses (`renderFromCache`) - no refetch. This is the point of the wire format: the server ships ALL above-floor durations (J10) so thresholding is a pure client operation. | Drag the slider after a load. | `tokio_stats.html:163` |

---

## D. Data loading and refinement loop

`loadPeriod` (`tokio_stats.html:371-398`) is the page's whole network layer -
the same demand-driven poll pattern as flamegraph's api mode (features/03
F173), minus toolbar facets, Stop/Refine-more, and the plateau heuristic.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| D1. Request construction | Builds `/api/tokio-stats` on `window.location.origin` with `bucket`/`prefix`/`service` (set only when present), every `host` repeated, and the PERIOD's `start_ns`/`end_ns` (set only when non-null). `refine=true` uses the literal string `"true"` - the backend param is a strict serde bool (`refine=1` is a 400, verified). | Automatic per poll. | `tokio_stats.html:375-384`; `src/server/tokio_stats.rs:22-33` |
| D2. Read-only first poll + refine polls | First request per Load omits `refine` (read-only: returns whatever is already folded, instantly). Each subsequent iteration sends `refine=true` (server folds a bounded batch, J5) with a 300 ms pause between polls. | Automatic during Load. | `tokio_stats.html:372-374,384,395-397` |
| D3. Loop termination | Stops when the response has no `coverage`, when `files_folded >= files_matched` (fully folded), or when `files_folded` did not change between consecutive polls (frozen - which is how capped scopes terminate, since the page NEVER sends `max_files`: there is no "Refine more"/"Stop" and no plateau heuristic here, unlike flamegraph F177-F179; wide scopes silently plateau at the default sampling cap, J5). | Automatic. | `tokio_stats.html:392-396`; cap `src/ingest/refine.rs:44-49,353-362` |
| D4. Progressive render | Every poll response overwrites `period.data` and re-renders immediately (`renderFromCache`), so numbers climb during refinement. No loading overlay or coverage badge exists on this page - the only refinement feedback is the status line (C2) and the growing numbers (a parity-relevant GAP vs flamegraph F174/F176: `coverage` is received and drives the loop but is never displayed). | Watch the cards/table during Load. | `tokio_stats.html:390-391` |
| D5. Error handling | A non-OK response throws with the response body text; `loadAll`'s catch paints the status line red (C2). One failing period rejects the whole `Promise.all`, but other periods' in-flight polls still land and their partial data stays cached and rendered - there is no per-period status. Concurrent-edit hazard: `loadPeriod` reads `period.startNs` fresh each iteration, so editing a period mid-refinement changes the window of subsequent polls. | Load a scope that 404s (e.g. wrong prefix). | `tokio_stats.html:388,404-411` |
| D6. Parallel period loads | All periods load concurrently (each with its own refine loop against the same scope but its own time window). Server-side folding is shared and idempotent, so overlapping loops are safe (the loop is stateless per-poll). | Load with 2+ periods. | `tokio_stats.html:406`; `src/ingest/refine.rs:10-14` |

---

## E. Client-side stats computation

`computeStats(data, threshNs)` (`tokio_stats.html:177-208`) turns one cached
response into the render model. Pure; re-run on every render.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| E1. Threshold filter | A poll is "long" iff `durations_ns[i] >= threshNs`. Everything below the slider threshold is ignored (but stays cached for instant re-threshold, C4). | Automatic per render. | `tokio_stats.html:184-193` |
| E2. Class buckets | Per long poll, `classes[i]`: 0 = off-CPU, 1 = on-CPU, 2 = mixed, 3 = unknown (too short to classify, J11). Unknown and mixed are counted in `long` but NOT shown as their own columns/cards - only off-CPU and on-CPU totals surface in the UI (mixed is computed then unused; see finding 3). | Automatic. | `tokio_stats.html:186-192` (comment at `:191`) |
| E3. Rates per minute | `rate = long / (time_span_ns / 60e9)` per location and in total, using the RESPONSE's `time_span_ns` (observed row span, J16) - not the requested window. Zero-span guard renders rate 0. | "Rate" card, "/min" columns. | `tokio_stats.html:179,199,206-207` |
| E4. Percentiles and max | Over the DESC-sorted above-threshold durations: `p50 = above[floor(n*0.5)]`, `p99 = above[floor(n*0.01)]`, `max = above[0]`. Correct ONLY because the server contract sorts `durations_ns` descending (J13) - a re-implementation that re-sorts ascending silently swaps p50/p99 semantics. | P50/P99/Max columns. | `tokio_stats.html:195-203` |
| E5. Exemplar passthrough | Carries the location's `exemplars` array (`[off_cpu, on_cpu, mixed, unknown]`, J12) into the render model for section H links. | Internal. | `tokio_stats.html:203` |

---

## F. Single-period view

Rendered when only one period has data, or when a `P{i} Detail` tab is active
(`renderSinglePeriod`, `tokio_stats.html:272-300`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| F1. Summary cards | Five cards: `Total Polls` (all polls in scope, not just long), `Long (>{threshold})`, `Rate` (`{x.x}/min`), `Off-CPU (blocked)`, `On-CPU (compute)`. Coloring: Long + Rate cards `warn` (amber) when any long poll exists else `good` (green); Off-CPU `bad` (red) when > 0 else `good`; On-CPU always amber (`.on-cpu`). Counts locale-formatted. | Cards after Load. | `tokio_stats.html:273-279`; CSS `:21-27,32-33` |
| F2. Per-location table | One row per spawn location with `long > 0`, sorted by rate descending. Columns: Spawn Location (in `<code>`, HTML-escaped - I1), Long, /min, Off-CPU (red column, U+1F534 circle in header), On-CPU (amber, U+1F7E1), P50, P99, Max. Off-/On-CPU counts render as exemplar links when available (H2). | Table after Load. | `tokio_stats.html:280-299` |
| F3. Empty-table state | When no location exceeds the threshold: `No polls exceed the threshold.` replaces the table (cards still render, showing zeros). | Raise the slider above the max duration. | `tokio_stats.html:283` |
| F4. Table chrome | Sticky header row (`th { position: sticky; top: 0 }`) inside a `max-height:70vh; overflow-y:auto` container; row hover highlight. | Scroll a long table. | `tokio_stats.html:28-31,62` |

---

## G. Multi-period tabs and diff view

Active when 2+ periods have loaded data (`renderTabs` / `renderDiffView`,
`tokio_stats.html:212-224,302-369`).

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| G1. View tabs | Hidden until 2+ periods have data; then a tab strip renders: `[zap] Diff` plus one `P{i} Detail` per LOADED period (unloaded periods get no tab). Active tab styled darker; clicking calls global `setTab` and re-renders. Default tab is `diff`. | Load 2+ periods. | `tokio_stats.html:60,210,212-224` |
| G2. Detail tabs | A `P{i} Detail` tab renders that period through the single-period view (section F). Guard: a stale tab pointing at a no-longer-loaded period renders `Not loaded yet.` | Click `P2 Detail`. | `tokio_stats.html:239-246` |
| G3. First-vs-last diff | The diff compares `stats[0]` (P1) against `stats[last]` ONLY - middle periods contribute locations to the union (G5) but their rates are ignored. Edge (code-read): if P1 itself failed to load while 2+ later periods loaded, `first` is null and `renderDiffView` throws (`first.rate`) - the diff tab renders nothing and the console shows a TypeError (finding 4). | Diff tab with 3+ periods. | `tokio_stats.html:303-304,321-330` |
| G4. Diff summary cards | Four cards: long-poll rate `P1 -> Plast` with a direction arrow (U+2191/U+2193/U+2192) and percent delta (`+inf` symbol, U+221E, when P1's rate was 0), card `bad` when delta > +0.5/min, `good` when < -0.5/min; off-CPU rate/min delta (thresholds +-0.1/min); P1 poll count; P{n} poll count. | Diff tab, cards. | `tokio_stats.html:302-319` |
| G5. Regressions table | Locations whose rate delta (last - first) > +0.1/min, sorted worst-first, under a red `Regressions (N)` heading (U+2B06 arrow). | Diff tab. | `tokio_stats.html:332,337-340` |
| G6. New offenders table | Locations with `firstRate === 0 && lastRate > 0`, sorted by last rate, red heading (U+1F195 "NEW" emoji). OVERLAP: a new offender with rate > 0.1/min also appears in Regressions - same location listed twice. | Diff tab. | `tokio_stats.html:334,341-344` |
| G7. Improvements table | Locations with delta < -0.1/min, sorted most-improved-first, green heading (U+2B07 arrow). | Diff tab. | `tokio_stats.html:333,345-348` |
| G8. No-change fallback | When all three lists are empty: `No significant changes between periods.` | Diff tab on identical periods. | `tokio_stats.html:349-351` |
| G9. Diff table columns | `Spawn Location` (escaped `<code>`), `P1 /min`, `P{n} /min`, `Delta /min` (signed, colored by list), `Delta%` (renders `new` when P1's rate was 0). Column headers use the Greek capital delta (U+0394). | Any diff table. | `tokio_stats.html:355-369` (`diffTable`) |

---

## H. Exemplar deep links

The server ships, per location, the worst poll per class (J12); the client
turns off-CPU/on-CPU counts into links that should open the poll's window in
the viewer.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| H1. Link construction | `exemplarLink` builds `viewer.html?trace=<traceUrl>&start_ns=<ex.start_ns>&end_ns=<ex.end_ns>` where `traceUrl` is `/api/trace?bucket=<bucket>&keys=<ex.source_key>` (bucket from the response's `bucket` field, falling back to the URL param). All values URL-encoded (URLSearchParams + encodeURIComponent). | Internal (used by H2). | `tokio_stats.html:166-175` (`exemplarLink`) |
| H2. Linked class counts | In the single-period table, the Off-CPU and On-CPU counts render as `<a>` links when that class has an exemplar (`linkedStat`; zero counts render empty, no-exemplar counts render as plain spans). The `data-url` attribute is quote-escaped (I2); click delegates to `openExemplar` -> `window.open(url, '_blank')`. Mixed and unknown exemplars (indices 2, 3) arrive on the wire but are NEVER linked or shown (finding 3). | Click a red/amber count in the table. | `tokio_stats.html:260-270` (`linkedStat`), `293-294`, `426` (`openExemplar`) |
| H3. Missing-exemplar guard | `exemplarLink` returns `""` for a null exemplar or one with no `start_ns`; `linkedStat` then falls back to an unlinked span. | Automatic. | `tokio_stats.html:166-167,265-269` |
| H4. `/api/trace` endpoint is gone | `DEAD` (regression). The deep-link target `/api/trace` was REMOVED by #582 (superseded by per-object `/api/object?bucket&key`; the route deletion is in #582's diff) - #570 then added this page still building `/api/trace` URLs. Verified live: `GET /api/trace?...` -> 404. Every exemplar click therefore opens a viewer tab whose trace fetch 404s (viewer shows its load-error state). The UI side (link rendering, new tab) works; the feature is end-to-end broken at HEAD. See finding 1; T41 must decide fix-vs-preserve via the ledger. | Click any exemplar link. | `tokio_stats.html:169`; removal: `git show 97cc9fa` (`/api/trace` route deleted from `src/server/mod.rs`); current routes `src/server/mod.rs:433-452` (no `/trace`) |

---

## I. XSS hardening and injection surface (#587)

The page renders with `innerHTML` template strings throughout; #587 added
explicit escaping for the one server-controlled string that reaches HTML.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| I1. `escapeHtml` on spawn locations | `escapeHtml` (added by #587) escapes `& < > " '` and is applied to `spawn_loc` at BOTH sinks: the single-period table (F2) and the diff tables (G9). Spawn locations are attacker-influenceable (they come from traced code's `file:line` strings via the Parquet aggregates), so this is the XSS fix under test obligation in T41 ("hostile service/host strings render inert"). | Automatic wherever a location renders. | `tokio_stats.html:93-100` (`escapeHtml`), `291`, `360`; fix commit `ea899f4` (#587) |
| I2. Attribute escaping on `data-url` | `linkedStat` additionally escapes `"` -> `&quot;` when interpolating the exemplar URL into the `data-url` attribute (the URL is already URLSearchParams-encoded, this is defense in depth for the attribute context). | Internal. | `tokio_stats.html:268` |
| I3. Remaining interpolation surface | All other `innerHTML` interpolations are numbers (`toFixed`/`toLocaleString`), `nsToDatetime` output (ISO slices), fixed strings, or the palette constants - no other server/user string reaches HTML unescaped at this snapshot. The exemplar `host` field arrives on the wire but is never rendered. The structural guard for the re-implementation is N17 (declarative templates), not this audit. | Code audit. | `tokio_stats.html:118-125,273-279,284-298,314-319,356-366` |

---

## J. Backend contract: `GET /api/tokio-stats`

`dial9-viewer/src/server/tokio_stats.rs` (#570). Same demand-driven
refinement loop as `/api/flamegraph` (shared `ingest::refine`), differing
only in the part-files read back (polls) and the response shape. Documented
here because the page is its only consumer; scope/refine semantics are
backend-owned and shared.

| Feature | What it does | Access path | Source |
| --- | --- | --- | --- |
| J1. Route | `GET /api/tokio-stats` (the `/tokio-stats` route in `api_router`, nested under `/api`). Permissive CORS like all api routes. | `curl /api/tokio-stats?...`. | `src/server/mod.rs:447-450` (route), `384-389` (nest), `455` (CORS) |
| J2. Query params | `bucket`, `prefix`, `service` (optional strings), `host` (repeatable via `axum_extra` Query), `start_ns`/`end_ns` (i64 ns), `refine` (STRICT serde bool: `true`/`false` only - `refine=1` is a 400 `Failed to deserialize query string`, verified live). | Request query string. | `src/server/tokio_stats.rs:22-33` (`TokioStatsParams`), `71-75` (`QueryExtra`) |
| J3. Aggregation gate | `agg_context_for(bucket, prefix, creds)`: with a `bucket` param, builds a per-request BYO-credentials agg context (any S3 bucket; output routed to `--agg-output-bucket` or back into the source); without one, falls back to the server's `--agg` context. Neither -> 404 `tokio-stats requires aggregation (start with --agg or supply a bucket)` (verified live on the bucket-less dev-server request). This is the same condition the client-side `aggregation_enabled` config flag advertises (`state.agg.is_some()` OR `state.allow_byo_creds`). | Request without `bucket` on a non-`--agg` server. | `src/server/tokio_stats.rs:76-84`; `src/server/mod.rs:212-256` (`agg_context_for`); gate `src/server/config.rs:14,30` |
| J4. Credential resolution | `MaybeCreds` extractor parses `x-dial9-aws-*` headers into the request's credential source (BYO keys or assume-role via `x-dial9-aws-role-arn`, #597); `resolve()` picks the storage backend. Errors surface as HTTP errors on the response. | Automatic per request. | `src/server/tokio_stats.rs:73`; `src/server/credentials.rs:283` (`MaybeCreds`) |
| J5. Shared refinement loop | `refine::refine(agg, scope, opts, fold_limits)`: lists + scope-filters source files, caps the matched set (default sampling cap `min(ceil(5% * files_matched), 100)`, floored at baseline 4, clamped to matched), and - only when `refine=true` - folds a bounded batch (first folding poll 4 files, then 12/poll). Read-only polls fold nothing and return instantly. `max_files: None` ALWAYS for this endpoint - the page has no "Fetch more", so the default cap is the ceiling (D3). | `refine=true` polls. | `src/server/tokio_stats.rs:104-116` (`RefineOpts`); `src/ingest/refine.rs:26-56` (constants), `353-362` (`sampling_cap`) |
| J6. Scope matching | A per-day resolver selects the historical layout, version 1, or both at the migration boundary. Historical keys use minute-level `{date}/{time}/{service}` prefixes; version 1 uses minute-level prefixes (<= 2h span, 2-min pad) or hour-level prefixes (> 2h, 1-hour pad, 72h cap) under `version=1/date/service/time`. A per-file interval-overlap check then uses the filename epoch (`{ts}-{i}.bin.gz`, padded by the segment duration; keys with no parseable epoch are kept). `service` = exact decoded service match; `host` = exact decoded instance-set membership (repeatable param, OR semantics). | `start_ns`/`end_ns`/`service`/`host` params. | `src/ingest/refine.rs` (`time_scoped_prefixes`); `src/ingest/aggregate.rs` (`scope_matches`) |
| J7. No-match 404 | Empty matched set -> 404 `no source files match this scope` (plain text; verified live with a bogus prefix). The page surfaces it via C2/D5. | Scope matching nothing. | `src/server/tokio_stats.rs:111-116` |
| J8. Polls part-file read | Reads ONLY the `polls/` Parquet part-files (never the samples parts) for the capped-and-folded set, concurrently. A trace whose polls part is missing is silently skipped (fetch `.ok()`). | Internal. | `src/server/tokio_stats.rs:118-127`; `src/ingest/aggregate.rs:739-765` (`read_polls_parts`) |
| J9. Row-level window filter | Within each part, rows are kept iff `start_ns <= row.start < end_ns` (each bound only when supplied) - keyed on the poll's START only, so a poll starting before the window but overlapping it is EXCLUDED (unlike the file-level overlap matching in J6). | `start_ns`/`end_ns` params. | `src/server/tokio_stats.rs:280-292` |
| J10. 100us duration floor | Every in-scope row counts toward `total_polls` and the time span, but only durations >= `DURATION_FLOOR_NS` (100_000 ns) enter `durations_ns`/`classes`/exemplars - the bandwidth floor the client threshold slider bottoms out at (C3). | Automatic. | `src/server/tokio_stats.rs:19-20,316-318` |
| J11. Poll classification | From the row's sample counts: cpu>0 && sched>0 -> mixed (2); cpu>0 -> on-CPU (1); cpu==0 -> off-CPU (0) ONLY when duration >= 10ms (`OFF_CPU_CONFIDENCE_NS` - below that, zero samples is statistically expected at 99Hz), else unknown (3). | Automatic. | `src/server/tokio_stats.rs:210-238` (`classify_poll`) |
| J12. Exemplars | Per location, the single worst (longest) poll PER CLASS: `{start_ns, end_ns, duration_ns, host, source_key}` where `source_key` is the RAW source trace key (for viewer deep links) - indices `[off_cpu, on_cpu, mixed, unknown]`, null when the class never occurred. | `exemplars` in the response. | `src/server/tokio_stats.rs:56-68,330-342` |
| J13. Response shape and sort contracts | `{time_span_ns, total_polls, bucket, by_spawn_loc[], coverage}`. `bucket` echoes the SOURCE bucket (for H1's links). Each `by_spawn_loc` entry: `spawn_loc`, `total_polls` (all polls at that location), `durations_ns` sorted DESCENDING with `classes` index-aligned (E4 depends on the ordering), `exemplars`. Locations sorted by notable-poll count descending; locations whose polls are all below floor still appear with empty arrays (client filters them out via `long > 0`). Null spawn locations become `(unknown)`. | Response JSON (all verified on the wire). | `src/server/tokio_stats.rs:35-68` (types), `155-174` (sorting), `303-305` (`(unknown)`) |
| J14. Coverage | Always present from this endpoint (`Some(...)`): `{files_matched, files_folded, samples_folded, total_bytes, hosts_matched, hosts_folded}`. QUIRK: `samples_folded` carries the number of part-FILES read this response, not samples (commented as deliberate). The client uses only `files_folded`/`files_matched` (D3) and never displays any of it (D4). | `coverage` in the response. | `src/server/tokio_stats.rs:181-189`; `src/ingest/aggregate.rs:485-497` (`Coverage`) |
| J15. Time span | `time_span_ns` = max - min of the IN-SCOPE rows' start timestamps, clamped to >= 1 (so E3's rate division is safe); 1 when no rows matched. It reflects observed data, not the requested window - rates over sparse data are computed against the data's own span. | `time_span_ns` in the response. | `src/server/tokio_stats.rs:149-153,297-301` |

---

## K. Cross-cutting characteristics

| Behavior | Detail | Source |
| --- | --- | --- |
| K1. No keyboard model | The page binds NO keyboard shortcuts and no Escape handling (nothing to dismiss); no focus management beyond native controls. T41's unified keyboard model (T20) integration starts from zero here. | absence verified by code read (`tokio_stats.html:64-429`: only `input`/`change`/`click` listeners) |
| K2. No resize/persistence handling | No canvases, no resize listener needed (pure flow layout); no localStorage/sessionStorage use of its own (creds.js's sessionStorage and ui-switch.js's localStorage are the includes' concerns). View state (periods) lives in the URL only (A7). | code read |
| K3. Server round-trip contract | The page's ONLY data dependency is J13's response shape. All analysis (thresholding, classification display, rates, percentiles, diffing) is client-side and re-derivable from cached responses - the behavioral-differ target for T41 is "same numbers from the same responses". | `tokio_stats.html:177-208,226-258` |

---

## Notable findings (2026-07-10)

1. **Exemplar deep links are broken at HEAD (H4).** `/api/trace` was removed
   by #582 (2026-06-27) in favor of per-object `/api/object`; #570 (same day)
   added this page still deep-linking through `/api/trace`. Verified: the
   endpoint 404s, so every exemplar click opens a viewer tab that fails to
   load. Fix candidate: build `viewer.html?trace=/api/object?bucket=...&key=<source_key>`
   instead (one line in `exemplarLink`). T41 must ledger this row
   (fix-vs-preserve); preserving broken behavior byte-for-byte seems
   pointless, but that is a maintainer call.
2. **Coverage is consumed but never displayed (D4).** The refinement loop has
   no user-visible progress/coverage UI (no badge, spinner, or Stop), unlike
   flamegraph's api mode (F174/F176-F179). On large scopes the page quietly
   plateaus at the default sampling cap with no indication that the numbers
   are a sample. A parity-preserving migration keeps this; a UX-improving one
   adds the flamegraph-style badge (ledger decision).
3. **Mixed/unknown classes are computed but invisible (E2/H2).** The wire
   carries per-poll `mixed` (2) and `unknown` (3) classes and their
   exemplars; the UI shows only off-CPU and on-CPU columns/cards. `mixed`
   counts are computed in `computeStats` then never read. Long counts
   therefore exceed offCpu+onCpu sums when mixed/unknown polls exist (the
   demo data shows exactly this: classes 1/2/3 present, no 0).
4. **Diff view crashes when P1 is unloaded (G3).** `renderDiffView`
   dereferences `stats[0]` without a null check; reachable when the first
   period's load failed (e.g. its window 404s) while two later periods
   loaded. Code-read finding; not walked (needs multi-period data).
5. **URL restore caps at 10 periods; sync does not (A4/A7).** Periods 11+
   survive in the URL but drop on reload.

---

## 2026-07-10 validation

Method: dev-server on :3071 (`ui` built first - post-T04 it serves
`ui/dist`: `npm ci && npm run build`, then `CARGO_TARGET_DIR=<repo>/target
PORT=3071 cargo run -p dial9-viewer --bin dev-server --features dev-server`),
driven with `curl` + node for the API contract; code read for DOM behavior.
NO browser driver this pass and no `features04` walker registry exists yet
(T41 deliverable), so all DOM-interaction verdicts are CODE-READ
(re-derivable by the T12 row-walker once T41 registers the page's walkers).
Verdict vocabulary per the chunk-1 SHARED DECISIONS.

Dev-server facts observed (fresh server, seeded `demo-traces`/`traces`):

- `/api/config` -> `aggregation_enabled:true`, `supports_byo_credentials:true`.
- `GET /tokio_stats.html` -> 200 `text/html`, byte-identical to
  `ui/tokio_stats.html` (dist static-copy verified); `ui-switch.js` and
  `creds.js` also served byte-identical.
- Cold poll `/api/tokio-stats?bucket=demo-traces&prefix=traces` -> instant
  `{time_span_ns:1, total_polls:0, bucket:"demo-traces", by_spawn_loc:[],
  coverage:{files_matched:1, files_folded:0, samples_folded:0,
  total_bytes:4336378, hosts_matched:1, hosts_folded:0}}`.
- `&refine=true` -> folded 1/1 files, 1/1 hosts, `total_polls:94212`,
  `time_span_ns:4143811668` (~4.14s), 5 spawn locations; top location
  `examples/metrics-service/src/axum_traced.rs:243:33` with 3319 notable
  polls. Verified on the wire: `durations_ns` desc-sorted, all >= 100000 ns,
  `classes.length === durations_ns.length`, locations sorted by notable
  count desc, one location with zero notable polls still present, exemplars
  populated per class with `host:"local"` and the raw `source_key`.
- Class values present: {1, 2, 3} - NO off-CPU (0) polls in the demo trace
  (its longest poll is ~1.01ms, under the 10ms off-CPU confidence bound), so
  off-CPU cards/columns/exemplars are only code-read.
- Warm read-only poll -> identical folded counts (frozen; D3's terminator).
- `prefix=no-such-prefix` -> 404 `no source files match this scope`.
- No `bucket` param -> 404 `tokio-stats requires aggregation (start with
  --agg or supply a bucket)`.
- `refine=1` -> 400 `Failed to deserialize query string: refine: provided
  string was not `true` or `false``.
- `service=demo-service` -> matches; `host=local` -> matches;
  `host=local&host=nonexistent` -> matches (OR semantics); `host=host-0` ->
  404 (the demo key's host component is `local`).
- TIME WINDOWS ARE UNWALKABLE ON THE SEED DATA: any `start_ns`/`end_ns`
  query 404s, in both directions - a window at the data's row timestamps
  (June 2026) lists date prefixes that don't exist under the key's
  2026-04-09 path, and a window at the key's date-path lists the file but
  then `scope_matches` rejects it on the filename epoch (1744224000 =
  2025-04-09). This is features/01 finding 3 (demo key epoch vs date-path
  mismatch) biting the aggregate listing; T42's synthetic fixtures unblock
  it.
- `/api/trace?...` -> 404 (finding 1).

| Row | Verdict | Evidence / note |
|---|---|---|
| A1 page serving | VERIFIED (API) | 200 + byte-identity check above. |
| A2 entry point | CODE-READ (cross-link) | features/01 H6's verdict (PARTIAL there) owns the button; URL shape read at `index.html:1770-1787`. |
| A3 scope params | VERIFIED (API) + CODE-READ | params walked through to the endpoint by curl (service/host cases above); page-side plumbing read at `:66,376-379`. |
| A4 multi-period params | CODE-READ | init loop `:139-144`; 10-cap asymmetry read, not driven. |
| A5 single-period fallback | CODE-READ | `:145`. |
| A6 auto-load | CODE-READ | `:428`; rewrite caveat derived from A7 ordering. |
| A7 URL sync | CODE-READ | `syncUrl` `:147-156`; param-drop set enumerated from code (matches ui-switch.js's own comment at `ui-switch.js:229-238`). |
| A8 retired dual-UI switch | DEAD (retired by #769) | `#d9-ui-switch` is absent from the canonical page; row-walker gates the retirement. |
| A9 cred headers | CODE-ONLY | header spread at `:386-387`; not asserted on the wire (dev-server accepts anonymous). |
| A10 title/heading | VERIFIED (served markup) | title/h1 in the served bytes. |
| B1-B2 period rows + inputs | CODE-READ | `renderPeriods` `:116-126`, `updatePeriod` `:127-134`. |
| B3 UTC toggle | CODE-READ | converters `:75-85`, listener `:414`; symmetric-conversion logic matches flamegraph's F171 pattern (that one is unit-tested; this page's copy is not). |
| B4 add period | CODE-READ | `:416-424`; previous-window derivation read. |
| B5 remove period | CODE-READ | `:109-114,123`. |
| B6 inline handlers | CODE-READ | globals at `:127,135,224,426`. |
| C1 Load | VERIFIED (API) + CODE-READ | the request sequence it drives was replayed by curl (cold -> refine -> frozen, above); button wiring read at `:415`. |
| C2 status line | CODE-READ | `:400-411`; error bodies it would render were observed on the wire (J3/J7). |
| C3 threshold slider | CODE-READ | log mapping `:161`; bounds arithmetic checked by hand (10^-1..10^3 ms). |
| C4 instant re-threshold | CODE-READ | `:163`. |
| D1 request construction | VERIFIED (API) | URL shape replayed by curl incl. repeated `host` and `refine=true` literal; `refine=1` 400 confirms the strict-bool contract. |
| D2 poll sequencing | VERIFIED (API) | cold poll instant/empty, refine folds, warm returns folded (above); 300ms pacing CODE-READ (`:396`). |
| D3 termination | VERIFIED (API, frozen case) | 1/1-file seed freezes after one refine (folded == matched AND folded == prevFolded); cap-plateau case NOT-TRIGGERABLE (needs > 4 matched files - T42). |
| D4 progressive render / no coverage UI | CODE-READ | `:390-391`; coverage-never-displayed = absence, checked by grep over the page. |
| D5 error handling | VERIFIED (API errors) + CODE-READ | both 404 bodies observed; page-side catch read at `:404-411`; partial-failure interleaving not driven. |
| D6 parallel loads | CODE-READ | `:406`. |
| E1-E5 computeStats | CODE-READ (against verified wire data) | pure function read at `:177-208`; its input contracts (desc sort, floor, class values, alignment) VERIFIED on the wire (above). |
| F1 summary cards | CODE-READ | `:273-279`. |
| F2 per-location table | CODE-READ | `:280-299`; row-filter/sort logic read. |
| F3 empty state | CODE-READ | `:283`. |
| F4 table chrome | CODE-READ | CSS `:28-31`, container `:62`. |
| G1-G2 tabs | CODE-READ | `:212-224,239-246`; NOT-TRIGGERABLE live without 2 loaded periods (time-windowed periods 404 on seed data - see facts). |
| G3 first-vs-last diff | CODE-READ | `:303-304`; null-first crash traced by hand (finding 4). |
| G4-G9 diff rendering | CODE-READ | `:302-369`; overlap G6/G5 derived from the two filters. |
| H1 link construction | CODE-READ | `:166-175`; inputs (`bucket`, `source_key`, exemplar bounds) VERIFIED on the wire. |
| H2 linked counts | CODE-READ | `:260-270,293-294`; off-CPU link NOT-TRIGGERABLE on seed data (no class-0 polls). |
| H3 guards | CODE-READ | `:166-167,265-269`. |
| H4 dead endpoint | VERIFIED (API) | `/api/trace` -> 404 on the running server; removal confirmed in #582's diff. DEAD status is the row's expected recorded behavior. |
| I1 escapeHtml | VERIFIED (code + fix diff) | #587 diff shows both sinks patched (`ea899f4`); hostile-string rendering not driven (no browser) - T41's XSS regression test owns that. |
| I2 data-url escaping | CODE-READ | `:268`. |
| I3 interpolation audit | CODE-READ | full-page audit of `innerHTML` sinks (rows' line refs). |
| J1 route | VERIFIED (API) | responded at `/api/tokio-stats`. |
| J2 params | VERIFIED (API) | repeatable host + strict bool walked (above). |
| J3 aggregation gate | VERIFIED (API, no-bucket case) | 404 body observed; `--agg` server variant NOT-TRIGGERABLE locally (dev-server always allows BYOC). |
| J4 cred resolution | CODE-READ | extractor at `tokio_stats.rs:73`; not asserted on the wire. |
| J5 refinement loop | VERIFIED (API, small scope) | fold progression observed; cap behavior (5%/100/baseline-4) NOT-TRIGGERABLE with 1 matched file (T42). |
| J6 scope matching | VERIFIED (API: service/host) / NOT-TRIGGERABLE (time windows) | service + host OR-semantics walked; both time-window failure modes reproduced (see facts) - the epoch/date-path catch-22 makes the POSITIVE window case unreachable on seed data. |
| J7 no-match 404 | VERIFIED (API) | body observed. |
| J8 polls parts read | CODE-READ | `read_polls_parts` `aggregate.rs:739-765`; also covered by the Rust unit test `tokio_stats.rs:348-393` (`test_read_polls_from_demo_trace`). |
| J9 row window filter | CODE-READ | `tokio_stats.rs:280-292`; start-keyed semantics read. NOT-TRIGGERABLE live (J6's window catch-22). |
| J10 100us floor | VERIFIED (API) | all wire durations >= 100000 ns; `total_polls` (94212) far exceeds notable rows (3379 total) - floor visibly applied. |
| J11 classification | VERIFIED (API, classes 1/2/3) | class values on the wire; off-CPU (0) NOT-TRIGGERABLE (demo max poll ~1ms < 10ms bound); logic read at `:210-238` + covered by the demo-trace unit test. |
| J12 exemplars | VERIFIED (API) | per-class exemplars with host + raw source_key observed; worst-per-class selection CODE-READ (`:330-342`). |
| J13 response shape/sorts | VERIFIED (API) | desc sort, alignment, loc ordering, zero-notable loc, `(unknown)` fallback all asserted on the wire (the `(unknown)` case via code read - demo locs are all named). |
| J14 coverage | VERIFIED (API) | fields observed cold + folded; `samples_folded == files read` quirk visible (1 after fold). |
| J15 time span | VERIFIED (API) | 1 when cold/empty, 4143811668 when folded. |
| K1-K3 | CODE-READ | absence checks (grep for listeners/storage). |

Anchor spot-checks against the tree (beyond the rows above): `escapeHtml` at
`tokio_stats.html:93`, `syncUrl` 147, `exemplarLink` 166, `computeStats` 177,
`renderTabs` 212, `renderFromCache` 226, `linkedStat` 265,
`renderSinglePeriod` 272, `renderDiffView` 302, `diffTable` 355, `loadPeriod`
371, `loadAll` 400, auto-load 428; `get_tokio_stats` at `tokio_stats.rs:71`,
`classify_poll` 224, `read_polls_part` 240; `/tokio-stats` route at
`mod.rs:448`; `aggregation_enabled` at `config.rs:30` - all match the cited
rows.

## Reproduce

```bash
# 1. build the served dist (post-T04 the dev-server serves ui/dist)
cd dial9-viewer/ui && npm ci && npm run build
# 2. backend
PORT=3071 cargo run -p dial9-viewer --bin dev-server --features dev-server
# 3. contract walk
curl -s http://localhost:3071/api/config
curl -s 'http://localhost:3071/api/tokio-stats?bucket=demo-traces&prefix=traces'                # cold
curl -s 'http://localhost:3071/api/tokio-stats?bucket=demo-traces&prefix=traces&refine=true'    # fold
curl -s 'http://localhost:3071/api/tokio-stats?bucket=demo-traces&prefix=traces'                # frozen
curl -s 'http://localhost:3071/api/trace?bucket=demo-traces&keys=x'                             # 404 (finding 1)
# 4. page walk (browser): open http://localhost:3071/tokio_stats.html?bucket=demo-traces&prefix=traces
#    -> auto-loads (A6); drag the threshold slider to 100us to see all 5 locations
```
