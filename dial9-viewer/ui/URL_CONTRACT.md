# dial9 viewer URL contract

The viewer pages take their state from the URL, so a link can open a trace at
an exact window with a task, span, poll or analysis already selected. This is
the authoritative list of what they honor, checked against the viewer's code
by its contract tests. Anything not listed here is not part of the contract:
the pages silently ignore unknown params, so do not invent any (there is no
`?worker=` or `?source=`).

**Stability:** a documented param never changes meaning and is never removed.
New capability only adds params, so a link keeps working.

`dial9 serve` serves every page, by default at `http://localhost:3000/`: the
trace browser (`index.html`), `viewer.html` and `flamegraph.html`. A page
fetches its trace over HTTP from its own origin, so `file://` paths do not
work; with `dial9 serve --local-dir <dir>`, a trace is at
`/api/object?key=<path relative to dir>`. The `dial9` binary comes from
`cargo install dial9 --features cli` or `cargo binstall dial9`.

The query string holds what to load plus state each page owns; the hash
(`#v=1&...`) holds versioned view state and never reaches the server.

## Query params - viewer.html and flamegraph.html (exact mode)

Both trace-rendering pages accept these params:

| Param | Value | Meaning |
|-------|-------|---------|
| `trace` | URL, **repeatable** | Trace component to fetch, raw or gzipped; N values parse as one trace. Relative or absolute; must be same-origin-fetchable. |
| `start` | absolute monotonic ns (integer) | Viewer: visible viewport start. Flamegraph: inclusive parse-time filter start. |
| `end` | absolute monotonic ns (integer) | Viewer: visible viewport end. Flamegraph: inclusive parse-time filter end. |
| `svc` | string | Service name, display label only. |
| `host` | string | Host name, display label only. |
| `segs` | integer as string | Segment COUNT for the header/stats display; NOT a list of segment keys. |
| `from` | string | Human-readable wall-clock range start, display only. |
| `to` | string | Human-readable wall-clock range end, display only. |
| `worker-zoom` | TAB-joined frame names, root -> target | Flamegraph only: stable worker-tree zoom path, mirrored with the hash key `fg.w` when zoom changes. |
| `offworker-zoom` | same | Flamegraph only: off-worker-tree zoom path. |
| `inspect` | frame display name | Flamegraph only: inspected/butterfly focus. |
| `inspect_full` | full frame symbol | Flamegraph only: inspection identity when it differs from `inspect`; omitted otherwise. |
| `prof` | `1` | Viewer only, debug: enables the render profiler. Honored but not part of any UI's emitted links. |

`start`/`end` are ABSOLUTE monotonic nanoseconds on the trace record clock, NOT
offsets from trace start. `viewer.html` uses them as viewport bounds, clamped to
the extent of all sliceable timestamped records, and uses the additive
`data-start`/`data-end` pair for its parse filter. `flamegraph.html` retains
the original parse-filter meaning.

## Query params - viewer.html durable view state

The trace viewer owns these additive parameters and rewrites them into the
URL as the view settles; defaults are omitted. Values are semantic anchors
where possible, so agents can construct them directly. `start`/`end` are the
visible viewport here;
`data-start`/`data-end` are the distinct parse-time Set Range filter. For
`v1:` lists, percent-encode the complete query value once in the normal URL
way (`TAB` becomes `%09`, newline `%0A`); commas inside names need no special
list escaping. Previously emitted comma/pre-encoded list values remain readable.

| Param | Value | Meaning |
|-------|-------|---------|
| `start` | monotonic ns | Visible viewport start (does not discard data). |
| `end` | monotonic ns | Visible viewport end. Valid only with `start < end`. |
| `task` | integer or `0x` hex | Selected task. |
| `span-filter` | string | Span text filter. |
| `track-order` | comma-separated track ids | Analysis-track order. |
| `collapsed` | comma-separated track ids | Collapsed analysis tracks. |
| `field-chart` | **repeatable** `<id>,<event>,<field>,<kind>` | Numeric custom-event chart definitions; event and field names containing commas are unsupported. `kind` is `gauge`, `counter`, or `updown-counter`. Dynamic ids may also appear in `track-order`/`collapsed`. |
| `span` | span id | Lane-highlighted span (ancestor chain is re-derived). |
| `span-focus` | span id | Span-panel subtree root and inspected span. Opens Span by default when no poll, event, or analysis takes precedence. |
| `poll` | `<startNs>:<taskId>` | Poll-detail anchor. |
| `task-dump` | `<taskId>:<timestamp>[,<timestamp>...]` | Selected task-dump captures. |
| `event` | monotonic ns | Pinned custom-event cluster timestamp. |
| `region` | `<startNs>-<endNs>` | Retained analysis region. |
| `highlight` | `<startNs>-<endNs>[@worker]` | Marked region; `@worker` bounds the box to one lane. |
| `spawned` | `<startNs>-<endNs>` | Queue-track spawned-task range. |
| `issue` | POI detector id | Issues filter. |
| `issue-sort` | `<worker\|kind\|time\|duration>,<asc\|desc>` | Issues ordering. |
| `issue-threshold` | non-negative integer (microseconds) | Severity floor for the spawn-to-first-poll delay detector. Omitted at its default. |
| `issue-worst` | `10` \| `50` \| `200` | How many of the worst points the issues rail lists. The detectors rank by severity, so this resizes the list rather than filtering it. Omitted at its default. |
| `issue-index` | non-negative integer | Current issues cursor. |
| `issue-anchor` | `<worker>:<timeNs>:<spanStartNs>:<taskId\|->` | Stable identity for the current issue; takes precedence over its sorted index. |
| `span-pct` | `50` \| `90` \| `95` \| `99` | Span percentile floor. |
| `span-names` | `v1:` + TAB-joined names | Enabled span legend chips. |
| `event-names` | same | Enabled custom-event legend chips. |
| `rail` | `issues` \| `tasks` | Visible rail tab. |
| `task-sort` | `<id\|loc\|polls\|total\|longest\|lifetime>,<asc\|desc>` | Tasks ordering. |
| `task-index` | non-negative integer | Current Tasks cursor. |
| `runtime-collapsed` | `v1:` + TAB-joined names | Folded runtime groups. |
| `runtime-metrics-collapsed` | `v1:` + TAB-joined names | Runtimes whose summary lane is folded to its one-line strip. |
| `inspector-width` | positive CSS pixels | Inspector width. |
| `rail-width` | positive CSS pixels | Issues/Tasks rail width. |
| `label-width` | positive CSS pixels | Shared time-track label gutter width. |
| `task-cols` | `v1:` + TAB-joined `<id\|loc\|polls\|total\|longest\|lifetime>,<px>` entries | Tasks-table column widths. |
| `issue-cols` | `v1:` + TAB-joined `<dot\|worker\|kind\|time\|duration>,<px>` entries | Issues-table column widths. |
| `lanes-height` | positive CSS pixels | Worker-lanes viewport height. |
| `lanes-scroll` | non-negative CSS pixels | Worker-lanes vertical position. |
| `stack-view` | `list` \| `flame` | Poll/blocking stack presentation. |
| `inspector` | `task` \| `span` \| `poll` \| `event` \| `related` \| `stack` | Visible inspector tab. |
| `poll-section` | `cpu` \| `sched` | Poll flamegraph sample family. |
| `task-scope` | spawn location | The pinned spawn location. Filters the rail's task list to tasks spawned there, tints their polls in the worker lanes, and folds their samples in the Task tab. Omitted when nothing is pinned. |
| `poll-expanded` | `v1:` + TAB-joined group ids | Expanded poll list groups. |
| `poll-worker-zoom` | TAB-joined frame path | Poll worker-tree flamegraph zoom. |
| `poll-offworker-zoom` | same | Poll off-worker-tree flamegraph zoom. |
| `related-collapsed` | `v1:` + TAB-joined titles | Collapsed Related sections. |
| `related-expand` | `v1:` + newline-joined `<title><TAB><before><TAB><after>` entries | Related load-more counts. |
| `related-key` | string | Correlation field key. |
| `related-value` | string | Correlation field value; active with `related-key`. |
| `analysis` | `cpu` \| `blocking` \| `heap` | Region-analysis mode. |
| `heap-weight` | `bytes` \| `count` | Heap flamegraph weighting. |
| `blocking-group` | `leaf` \| `full` | Blocking-list grouping. |
| `analysis-worker-zoom` | TAB-joined frame path | Region worker-tree flamegraph zoom. |
| `analysis-offworker-zoom` | same | Region off-worker-tree flamegraph zoom. |
| `analysis-inspect` | full frame key | Region flamegraph butterfly/inspect focus. |
| `span-index` | non-negative integer | Current filtered-span navigation cursor. |
| `data-start` | monotonic ns | Parse-time Set Range lower bound. |
| `data-end` | monotonic ns | Parse-time Set Range upper bound. |

Clock mode (`tm`) and timezone (`tz`) remain in the versioned hash. Unknown
query params are preserved. Invalid known values are ignored rather than
coerced. Hover, in-flight drag, temporary search/help modals, toasts, and load
progress are intentionally transient and are not deep-linked.

## Query params - flamegraph.html aggregated API mode (`?api=1`)

`api=1` switches the flamegraph page to server-aggregated mode; scope and
facet params are rebuilt and pushed to the browser history on every
Apply/facet change, so Back walks the filter history:

| Param | Value | Meaning |
|-------|-------|---------|
| `api` | `1` | Mode switch. |
| `data_dir` | path | Local-directory scope (alternative to bucket/prefix). |
| `bucket` | string | S3 scope bucket. |
| `prefix` | string | S3 scope key prefix. |
| `service` | string | Scope service name. |
| `host` | string, **repeatable** | Scope host filter. |
| `start_ns` | epoch ns | Scope window start (seeds the UTC picker). |
| `end_ns` | epoch ns | Scope window end. |
| `source` | `cpu` (default) etc. | Facet filter: sample source. |
| `thread_class` | string | Facet filter. |
| `spawn_location` | string | Facet filter. |
| `max_files` | integer | Refinement fold ceiling. |
| `inspect` | frame display name | Inspected/butterfly focus; replaced in place and preserved across scope changes. |
| `inspect_full` | full frame symbol | Inspection identity when it differs from `inspect`; omitted otherwise. |

Canvas zoom remains deliberately NOT URL-synced in api mode. Inspection
is restored as aggregate snapshots arrive.

## Query params - index.html (trace browser)

Serialized with the History API on state changes, defaults omitted.
Service-tab changes push history entries so Back/Forward restores the focused
service:

| Param | Value | Meaning |
|-------|-------|---------|
| `bucket` | string | S3 bucket name. |
| `aws_region` | string | Region the bucket lives in (cross-region buckets). |
| `prefix` | string | User-entered key prefix. |
| `service` | string | Focused Browse service tab and exact backend filter. |
| `tab` | `raw` | Active tab; `browse` is the default and omitted. |
| `tz` | `local` | Timezone toggle; `utc` is the default and omitted. |
| `last` | positive number | Relative quick range in hours ("last N hours from now"). Mutually exclusive with `from`/`to`; wins when both present. |
| `from` | epoch seconds | Precise window start. NOTE: same NAME as the viewer's display-only `from`, different page, different meaning - both stable. |
| `to` | epoch seconds | Precise window end. |
| `q` | string | Raw-search prefix query. |

## Hash - versioned view state (`#v=1`)

The hash carries a form-encoded payload with a leading integer version:
`#v=1&fg.w=<tab-joined path>&tm=abs`. A live hash key overrides the matching
query param, which still fills any field the hash omits. The v1 key registry:

| Key | Status | Meaning |
|-----|--------|---------|
| `v` | live | Schema version, currently `1`. Required; a hash without a well-formed integer `v` is foreign and left alone. |
| `fg.w` | live (flamegraph) | Worker-tree zoom path; overrides `worker-zoom` per field, which fills gaps. |
| `fg.o` | live (flamegraph) | Off-worker-tree zoom path. |
| `fg.i` | live (flamegraph) | Inspect (butterfly) focus display name; overrides legacy `inspect`. |
| `fg.if` | live (flamegraph) | Inspect focus symbol; overrides legacy `inspect_full`. Emitted only when it differs from `fg.i`. |
| `fg.s` | live (flamegraph) | Frames-search query; overrides legacy `search`. |
| `fg.sp` | live (flamegraph) | Spawn-location filter value (exact mode); overrides legacy `spawn`. |
| `fg.rt` | live (flamegraph) | Runtime filter value (exact mode); overrides legacy `runtime`. |
| `tm` | live (viewer) | Clock display mode (`rel`\|`abs`). |
| `tz` | live (viewer) | Timezone (`utc`\|`local`) for absolute timestamps. |
| `vp` | reserved hash name | Not honored in hash; the viewer uses query `start`/`end`. |
| `sel.*` | reserved hash names | Not honored in hash; the viewer uses readable selection query params. |
| `poi` | reserved hash name | Not honored in hash; rail cursors are page-owned query params. |

Reserved hash keys claim the NAME only. Emitting them does nothing; the
viewer query implementation does not activate or reinterpret them.

## Deep-link recipes

To inspect a span's fields in the right panel, use:

```
viewer.html?trace=<trace-url>&span-focus=<span-id>&inspector=span
```

The Span tab shows user fields with units and copy buttons, followed by timing
and task details. Clicking a bar in the Spans track opens this tab, including
when an analysis is already open. The retained analysis remains on Stack.
Copy the resulting URL to preserve the selected span and active tab.

Common links:

1. **Open the viewer at an exact window, optionally with Set Range:**

   ```
   viewer.html?trace=<trace-url>&start=<visible-start-ns>&end=<visible-end-ns>
   viewer.html?trace=<trace-url>&data-start=<parse-start-ns>&data-end=<parse-end-ns>&start=<visible-start-ns>&end=<visible-end-ns>
   ```

   All values are absolute monotonic nanoseconds. Omit `data-start`/`data-end`
   to keep the full trace zoomable; include them to reproduce a Set Range
   reparse exactly.

2. **Open a flamegraph, optionally pre-zoomed to a subtree:**

   ```
   flamegraph.html?trace=<trace-url>&start=<ns>&end=<ns>&worker-zoom=<f1>%09<f2>
   ```

   Emit the stable `worker-zoom`/`offworker-zoom` QUERY form for maximum
   compatibility with existing links. The hash form `#v=1&fg.w=<f1>%09<f2>`
   is equivalent and wins per field when both are present. Zoom restore is
   gated on the time-range filter reproducing the shared tree, so carry the
   same `start`/`end` the zoomed view had (or none).

3. **Select an analysis target and exact surface:**

   ```
   viewer.html?trace=<trace-url>&task=0x2a&start=<ns>&end=<ns>
   viewer.html?trace=<trace-url>&region=<a>-<b>&analysis=heap&heap-weight=count&inspector=stack
   viewer.html?trace=<trace-url>&poll=<poll-start>:<task-id>&inspector=poll&stack-view=flame&poll-section=sched&poll-worker-zoom=<f1>%09<f2>
   ```

   Selection anchors that depend on trace content are validated after load;
   an anchor absent from that trace is dropped without disturbing the rest of
   the link. The parameter tables above are the complete list.
