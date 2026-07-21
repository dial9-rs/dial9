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

View state (what the reader is LOOKING at - not what data is loaded)
travels in the URL **hash**, as a form-encoded payload with a leading
version field:

```
#v=1&fg.w=<tab-joined frame path>&tm=abs
```

- The payload is `URLSearchParams` syntax: `&`-separated `key=value`
  pairs, percent-encoded, space as `+`. Both sides of the codec use
  `URLSearchParams`, so any value that survives it round-trips.
- Empty state = **no hash at all** (a pristine page keeps a clean URL,
  the same omit-defaults rule url_state.js uses on the browser page).
- The hash was chosen over new query params so view state never reaches
  the server, never collides with backend-read params, and is dropped -
  by design - by the raw dual-UI switch (T38 maintainer decision: no
  state porting across UI generations).

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
| `tm` | defined, unwritten | `rel` \| `abs` | clock display mode (viewer E1); no migrated page has the control yet - the chunk-2 viewer writes it |
| `tz` | defined, unwritten | `utc` \| `local` | timezone for absolute timestamps (viewer E2); meaningful with `tm=abs` |
| `vp` | reserved (chunk 2) | suggested `<startNs>-<endNs>` | viewport time window (ViewportSlice) |
| `sel.*` | reserved (chunk 2) | per field | selection slice (e.g. `sel.task`, `sel.span`, `sel.event`) |
| `poi` | reserved (chunk 2) | TBD | POI position for n/p stepping |

Reserved rows claim the NAME only; the implementing ticket defines the
exact value grammar here when it lands. Grammar rules for new keys:
values must round-trip `URLSearchParams` (they do, for any string); path-
like values reuse the TAB-join convention, which means TAB cannot appear
inside a component (the legacy zoom params' own limitation). `timeMode`/
`timeZone` are in the state set by design decision (T19 ticket): restoring
a shared view with the wrong clock mode changes what the reader sees.

## Precedence vs the legacy zoom params

The flamegraph page's `worker-zoom` / `offworker-zoom` **query** params
(features/03 F148/F149) keep their exact legacy semantics - N10 says old
links must keep working, on both page generations:

- **Read:** legacy query params are the base; v=1 hash fields override
  **per field**; legacy fills the gaps. (They only diverge on hand-edited
  URLs: the writer always emits them together.)
- **Write:** the migrated page mirrors zoom state into the legacy params
  AND the hash in the same replaceState, so an address-bar copy still
  opens correctly on the legacy page. The mirror touches ONLY the two
  zoom params; every other query param is preserved verbatim (F153).
- The legacy page itself ignores the hash entirely (verified: it loads
  and simply isn't zoomed), consistent with the raw-switch policy.

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
| browser page (index.html) | `bucket`, `aws_region`, `prefix`, `tab`, `tz`, `last`, `from`, `to`, `q` (query) | `url_state.js` (#585), untouched by the codec. Its `tz` is a QUERY param on a different page; the codec's `tz` is a HASH key - no interference, vocabulary deliberately identical. If the migrated browser page ever needs view state beyond url_state.js's set, it adopts the hash codec additively; url_state.js params are never migrated into the hash. |
| flamegraph exact mode | `trace`, `start`, `end`, `svc`, `host`, `segs`, `from`, `to` (query) | page-owned LOAD SCOPE, read-only, preserved verbatim by every codec write |
| flamegraph exact mode | `worker-zoom`, `offworker-zoom` (query) | legacy view state; codec-mirrored (see precedence above) |
| flamegraph api mode | `api`, `data_dir`, `bucket`, `prefix`, `service`, `host`*, `start_ns`, `end_ns`, `source`, `thread_class`, `spawn_location`, `max_files` (query) | page-owned scope/facets, `pushState` on Apply/facet change (F180); canvas zoom NOT URL-synced in this mode by legacy design - the codec stays out |
| all pages | `ui` (query) | ui-switch.js (T38); the switch preserves the query minus `ui` and DROPS the hash (raw switch, recorded maintainer policy) |
| all migrated pages | the hash | this codec, exclusively - EXCEPT foreign hashes, which are left alone |

The machine-usable form of the flamegraph rows (with inventory anchors)
is `src/lib/url/legacy-params.fixture.ts`.

## Extending in chunk 2 (checklist)

1. Add the key to the registry table above with its value grammar.
2. Add the field to `ViewState` + `KNOWN_KEYS` + encode/decode in
   `view-state.ts` (drop invalid values on decode; omit empty on encode).
3. Wire the owning page: project the store slice in its
   `bindViewStateToUrl` call; restore on load BEFORE binding effects (and
   outside the store update path, so restoring writes nothing).
4. No version bump. Old links: the field is simply absent - the page
   falls back to its defaults; the JS reader must tolerate absence.
5. Round-trip cases in the codec property test; a restore case in the
   owning page's test; extend J9 (or add a page journey) if the state is
   observable in a readout.
