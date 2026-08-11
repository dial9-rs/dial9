# URL view-state schema (versioned hash codec)

T19 (UX finding S3; ADR-0004 section 9 "view state becomes shareable";
NFR N10 "URL contract stability"). This document is the extension contract
for chunk-2: any ticket that makes more view state shareable extends the
schema here and in the codec (`dial9-viewer/ui/src/lib/url/view-state.ts`)
in the same PR. Keep the two in lockstep.

Implementation: `src/lib/url/view-state.ts` (pure codec), `src/lib/url/sync.ts`
(store->URL binding), `src/lib/url/copy-link.ts` (share button),
`src/pages/flamegraph/view-state.ts` (first consumer). Tests colocated;
fixture: `src/lib/url/legacy-params.fixture.ts`; browser twin: parity
journey J9.

## Carrier and format

Shared cross-page view state travels in the URL **hash**. The trace
viewer additionally owns a readable **query-param projection** for its durable
analytical state, because those semantic anchors must be constructible and
inspectable without rendering the UI. The hash remains a form-encoded payload
with a leading version field:

```
#v=1&fg.w=<tab-joined frame path>&tm=abs
```

- The payload is `URLSearchParams` syntax: `&`-separated `key=value`
  pairs, percent-encoded, space as `+`. Both sides of the codec use
  `URLSearchParams`, so any value that survives it round-trips.
- Empty state = **no hash at all** (a pristine page keeps a clean URL,
  the same omit-defaults rule url_state.js uses on the browser page).
- The hash was chosen for shared cross-page state so it never reaches the
  server or collides with backend-read params. `viewer.html` additionally uses
  page-owned, backend-opaque query params so humans and agents can read and
  construct exact analytical views.

## Version rules

- `v` is required. A non-empty hash without a well-formed integer `v` is
  **foreign** (e.g. `#section-anchor`): never restored from, never
  rewritten. The sync layer skips hash carriage entirely when the current
  hash is foreign (query-param mirroring still applies).
- **Additive evolution within v=1.** New keys are added without a bump;
  readers ignore keys they don't recognize for state, but **preserve them
  verbatim on rewrite** (tolerant reader) - an older deployed viewer must
  not strip state a newer link carries.
- Invalid **values** of known keys are dropped on decode, never
  propagated or "fixed".
- `v` bumps ONLY on incompatible reinterpretation of an existing key
  (expected: never). A reader seeing a different version restores nothing
  and rewrites nothing - the hash stays byte-identical.

## Key registry (v1)

| Key | Status | Value grammar | Meaning |
| --- | --- | --- | --- |
| `v` | live | integer | schema version (`1`) |
| `fg.w` | live (flamegraph page) | frame names joined by TAB (`%09`), root -> target | worker-tree zoom path (legacy `worker-zoom` format, features/03 F150) |
| `fg.o` | live (flamegraph page) | same | off-worker-tree zoom path |
| `tm` | live (viewer) | `rel` \| `abs` | clock display mode (viewer E1) |
| `tz` | live (viewer) | `utc` \| `local` | timezone for absolute timestamps (viewer E2); meaningful with `tm=abs` |
| `vp` | reserved hash name | suggested `<startNs>-<endNs>` | not emitted or restored from hash; the viewer owns readable query `start`/`end` instead |
| `sel.*` | reserved hash names | per field | not emitted or restored from hash; the viewer owns readable query selection anchors instead |
| `poi` | reserved hash name | TBD | not emitted or restored from hash; issues/task cursors are page-owned query params |

Reserved hash rows claim the NAME only. The viewer's query implementation
does not activate or reinterpret them. Grammar rules for future hash keys:
values must round-trip `URLSearchParams` (they do, for any string); path-
like values reuse the TAB-join convention, which means TAB cannot appear
inside a component (the legacy zoom params' own limitation). `timeMode`/
`timeZone` are in the state set by design decision (T19 ticket): restoring
a shared view with the wrong clock mode changes what the reader sees.

## Precedence vs the stable zoom query params

The flamegraph page's `worker-zoom` / `offworker-zoom` **query** params
(features/03 F148/F149) keep their established semantics so old links remain
valid:

- **Read:** query params are the base; v=1 hash fields override
  **per field**; the query fills the gaps. (They only diverge on hand-edited
  URLs: the writer always emits them together.)
- **Write:** the page mirrors zoom state into the query params AND the hash
  in the same replaceState. The mirror touches ONLY the two
  zoom params; every other query param is preserved verbatim (F153).

## Write mechanics (sync.ts)

- One **debounced** `history.replaceState` per change burst (150 ms
  trailing; store slice change -> scheduled write). Never `pushState`:
  view tweaks must not pollute Back. The api-mode filter history (F180)
  deliberately differs and stays page-owned.
- **No-op writes are skipped** (the URL already says this), and restore
  paths bypass the store entirely, so restore-on-load produces ZERO
  writes - a shared link stays byte-stable on open (J9's url.query
  readout is the regression gate).
- **Copy-link** (`mountCopyLink`) flushes the pending debounced write
  before reading `location.href`. Chunk-2's status bar replaces the
  minimal button; it should reuse the same flush-then-read contract
  (`ViewStateBinding.flush()`).

## Ownership boundary (who owns which URL params)

| Surface | Params | Owner / mechanism |
| --- | --- | --- |
| browser page (index.html) | `bucket`, `aws_region`, `prefix`, `tab`, `tz`, `last`, `from`, `to`, `q` (query) | `url_state.js` (#585), untouched by the codec. Its `tz` is a QUERY param on a different page; the codec's `tz` is a HASH key - no interference, vocabulary deliberately identical. |
| flamegraph exact mode | `trace`, `start`, `end`, `svc`, `host`, `segs`, `from`, `to` (query) | page-owned LOAD SCOPE, read-only, preserved verbatim by every codec write |
| flamegraph exact mode | `worker-zoom`, `offworker-zoom` (query) | stable view state; codec-mirrored (see precedence above) |
| flamegraph api mode | `api`, `data_dir`, `bucket`, `prefix`, `service`, `host`*, `start_ns`, `end_ns`, `source`, `thread_class`, `spawn_location`, `max_files` (query) | page-owned scope/facets, `pushState` on Apply/facet change (F180); canvas zoom is not URL-synced in this mode |
| trace viewer (`viewer.html`) | viewport, selection, filters, rail/cursors, runtime folds, layout/lanes, URL-defined numeric field charts, inspector/disclosures/correlation, region modes, embedded zoom paths, and `data-start`/`data-end` (query) | page-owned readable durable state in `src/pages/viewer/url-state.ts`; the exact registry is `VIEWER_VIEW_QUERY_PARAMS` and is contract-pinned to the README table. Defaults omitted, unordered values sorted, invalid known values dropped, unknown params preserved, and settled updates use debounced `replaceState`. `start`/`end` are viewport bounds; `data-start`/`data-end` are parse bounds. |
| viewer and flamegraph | the hash | this codec, exclusively - EXCEPT foreign hashes, which are left alone |

The machine-usable form of the flamegraph rows (with inventory anchors)
is `src/lib/url/legacy-params.fixture.ts`.

## Durable versus transient viewer state

The viewer serializes settled state that changes the durable
analytical view: semantic selections, visible windows and parse bounds, active
analytical surfaces, filters/sorts/cursors, disclosures, layout, lane position,
and embedded flamegraph zoom/inspect focus. It deliberately excludes pointer hover and
tooltips, in-flight drag/keyboard gestures, temporary search/help modals, load
progress/timers, toasts/check flashes, and zoom undo history. These values are
interaction process, not the resulting analytical view.

`VIEWER_STATE_OWNERSHIP` in `src/pages/viewer/url-state.ts` is the exhaustive
store-field gate. Adding a field to any viewer store slice fails TypeScript
until it is classified as URL-owned, derived, source, transient, or retired.
URL-owned fields name their query/hash keys there; the query registry and URL
binding slice list are derived from that matrix. Boot-time application goes
through `hydrateViewerStore`; trace-dependent anchors go through
`resolveUrlSelection` after load.

Trace-dependent anchors are restored only after the first trace load. An anchor
that does not exist in the loaded trace is ignored without invalidating other
URL state. Local-file bytes are not carried by the URL, so only URL/scope-backed
trace sources are shareable across machines. Copy Link refuses local-file and
demo sources rather than copying a URL that cannot reproduce the loaded data.

## Extending the schema

1. Add the store field and classify it in `VIEWER_STATE_OWNERSHIP`. For a
   durable field, name its query/hash keys there.
2. Add the key to the registry table above with its value grammar.
3. Add the field to `ViewState` + `KNOWN_KEYS` + encode/decode in
   `view-state.ts` (drop invalid values on decode; omit empty on encode).
4. Wire projection/parsing in `url-state.ts`; apply trace-independent state in
   `hydrateViewerStore` or semantic trace anchors in `resolveUrlSelection`.
5. No version bump. Old links: the field is simply absent - the page
   falls back to its defaults; the JS reader must tolerate absence.
6. Round-trip cases in the codec property test; a restore case in the
   owning page's test; extend J9 (or add a page journey) if the state is
   observable in a readout.
