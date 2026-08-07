# T38 audit - Dual-UI raw switch (all pages)

Audited tree: `integration/chunk-1` @ 851b02e. Implementation:
`dial9-viewer/ui/ui-switch.js` (commits 10aa900, 1042add, tests e890916,
merged at e4af116; byte-identical between the ticket branch tip 2d4c91d and
the integration tip - later merges did not touch it).

Authorities: `docs/tickets/chunk-2-viewer.md` T38 (lines 536-575) +
chunk-1 SHARED DECISIONS (`?ui=new` on the canonical URL);
`docs/adr/0004-viewer-ui-migration.md` section 8 raw-switch note (query
preserved, hash view state not, always-visible control on both, legacy
default until the flip); maintainer decision 2026-07-08 (raw switch, no
state porting, control visible all the time).

## VERDICT: FINDINGS

The mechanism is sound at the logic level: precedence order is correct and
param-first (no stale localStorage can override an explicit `?ui=legacy`),
repeated `trace=` params survive every rebuild, the hash is dropped in both
directions, the legacy edit is exactly one line per page, the default is
legacy in exactly one place, and the 33 tests are real and pass on the
integration tip. But the control's target URL is computed once at page boot
while every legacy page rewrites its query string live; the pages T13/T14
ship through this switch will lose user state on switch. Fix belongs in
ui-switch.js and should land before or with T13.

## Findings

1. MEDIUM - Switch-control href is frozen at boot; legacy pages rewrite
   their query string live, so switching after any in-page state change
   navigates with the stale boot-time query.
   `dial9-viewer/ui/ui-switch.js:268-272` (decide() consumes
   `window.location.search` once) + `:231-243` (mountControl renders a
   static `<a href>` from that one decision).
   Live rewriters on the legacy side: `flamegraph.html:219` (browser-mode
   scope: bucket/prefix/service/host/periods, pushState),
   `flamegraph.html:617` (worker-zoom), `tokio_stats.html:155` (hosts +
   periods), `index.html:749` (browse state via url_state),
   `viewer.html:1952` (start/end).
   Failure: on legacy flamegraph browser mode, narrow to one host and a
   custom period (URL rewritten), click "Switch to new UI" -> the new UI
   opens with the boot-time scope; the data set being viewed is gone.
   Violates T38 "Switching preserves the page's QUERY STRING (trace source
   - else the loaded trace is lost)" and the round-trip DoD. The same
   defect applies on the new side once its SPA updates the query after
   mount. Fix: resolve the target from live `location.search` in the click
   handler (refresh `href` on click/mousedown so middle-click and
   copy-link stay coherent).

2. LOW - The `ui=legacy` pin is stripped by three of the four legacy
   pages' own URL syncs, defeating its stated purpose.
   `dial9-viewer/ui/ui-switch.js:88-91` pins `ui=legacy` on new->legacy
   targets precisely so "the canonical page's dispatch cannot bounce back
   to new even when localStorage is unavailable or stale" - but
   `index.html:749`, `flamegraph.html:219`, and `tokio_stats.html:155`
   rebuild the query from scratch and drop the unknown `ui` param
   (`viewer.html:1946-1952` preserves it).
   Failure: from the new UI, open "Switch to legacy UI" in a new tab
   (middle-click skips the pref-writing click handler at
   `ui-switch.js:243`, so the stored pref stays "new"); the legacy page
   loads pinned, its first URL sync drops the pin, and a reload - or the
   copied address-bar URL - dispatches on `storedPref=new` straight back
   to the new UI, against the explicit choice. Legacy pages are fenced to
   the single script line, so the fix must live in ui-switch.js (e.g. on
   booting a canonical page with an explicit `?ui=legacy` while storage
   says "new", write "legacy" - aligning storage with the pinned intent).

3. LOW - The redirect-loop guard is partial: only exact self-registration
   is caught (`dial9-viewer/ui/ui-switch.js:140`). A registry cycle
   between two root-level canonical pages (`{"a.html": "b.html",
   "b.html": "a.html"}`) location.replace()-loops forever with no escape,
   because `buildQuery(search, "new")` strips the `ui` param on every hop
   so not even `?ui=legacy` survives into the loop. Requires a
   repo-authored misconfiguration, but T13/T14/T41 each add registry lines
   independently during the wave. Cheap hardening: reject entries that are
   themselves registry keys, or a sessionStorage one-shot loop breaker.

## Audit trail (per the brief's eight probes)

1. QUERY-STRING PRESERVATION - correct at the builder level.
   `buildQuery` (`ui-switch.js:88-95`) round-trips through URLSearchParams:
   repeated `trace=` params are preserved in order (tested:
   `tests/ui_switch.test.ts` "keeps repeated trace= params"), all `ui`
   occurrences are deleted, `ui=legacy` is appended once for legacy
   targets. Percent-encoding normalizes (`%20` -> `+`, etc.) but is
   value-preserving and symmetric: every reader on both sides decodes via
   URLSearchParams (legacy: `viewer.html:1928-1929` getAll('trace'),
   `flamegraph.html:94-97`, `tokio_stats.html:66`, `index.html:382`,
   `url_state.js` parse/serialize; new UI: `src/lib/trace/*.ts`). No raw
   `location.search` string-splitting reader exists. The residual risk is
   finding 1 (WHICH search string gets rebuilt), not HOW it is rebuilt.

2. HASH DROPPING - clean. `decide()` accepts `input.hash` and never reads
   it (`ui-switch.js:104-107` documents this as deliberate); redirect URLs
   and control hrefs are built from pathname registry entries + buildQuery
   only, so no fragment can appear, and navigating to a fragment-free URL
   drops the current hash in both directions. Unit-tested both ways
   ("legacy -> new redirect carries no hash", "switch-control hrefs carry
   no hash on either side").

3. PRECEDENCE - correct and param-first. `resolveUi`
   (`ui-switch.js:75-79`): explicit valid `?ui=` > stored pref > default;
   unknown values fall through a level; storage reads are try/catch
   wrapped and garbage reads as no-preference (`prefFromStorage`,
   `ui-switch.js:180-188`). A stale `storedPref="new"` cannot override
   `?ui=legacy` (tested), and the new->legacy href pins `ui=legacy` so the
   return trip is storage-independent. The localStorage pref is written
   ONLY on an actual click of the control (`ui-switch.js:239-244`), never
   on dispatch or on landing with an explicit param - so a shared
   `?ui=new` link does not sticky-switch the recipient. (The write-on-
   click-only choice is also the root of finding 2's narrow bounce case.)

4. LEGACY INJECTION - exactly as specified. Diff of the four legacy pages
   from the pre-T38 parent (1042add~1) to the integration tip 851b02e is
   exactly one `<script src="ui-switch.js"></script>` line in `<head>` per
   page (index.html:7, viewer.html:7, flamegraph.html:7,
   tokio_stats.html:6), nothing else, and no later commit touched them.
   404 tolerance: a failed plain script include is inert, and no legacy
   inline code references `window.D9UiSwitch` or `#d9-ui-switch` (grep:
   only README.md and the switch's own files). In a cargo-only build dist
   is entirely empty (rust-embed of `ui/dist/`, `server/mod.rs:54`), so
   the legacy pages are not served at all - no partial state where the
   page exists but the script cannot.

5. ROUTING MECHANISM - entry-script dispatch, client-side. Round trip for
   viewer.html: GET `/viewer.html?trace=a&trace=b&ui=new` -> static HTML
   served from the embedded dist (static-copy list, `vite.config.ts:50`
   ships ui-switch.js verbatim) -> the head include runs at parse time
   (`ui-switch.js:311`), before any body script -> `decide()`; with
   `NEW_UI_ENTRIES` empty today this is a no-op (stays legacy, no
   control); once T21 registers `viewer.html`, it is
   `location.replace("/new/viewer.html?trace=a&trace=b")` (replace, so no
   history pollution) -> the new entry calls
   `D9UiSwitch.mount({side:"new"})`, resolves its canonical page by
   reverse registry lookup, and renders the way-back control with
   `ui=legacy` pinned. Loops: pref=new + `?ui=legacy` stays legacy (param
   precedence, no bounce); the new side never dispatches
   (`ui-switch.js:120-133`); auto-boot on off-root new entries resolves no
   canonical page and no-ops; exact self-registration is guarded (:140).
   Only the cross-registration cycle of finding 3 can loop. Targets are
   root-absolute; root serving holds for `dial9 serve` and the dev-server
   (verified in the T38 HANDOFF dev-server smoke).

6. VISIBILITY - compliant. One shared renderer for both sides
   (`mountControl`, `ui-switch.js:231-260`): `position:fixed`, bottom
   right, `z-index 2147483647`, rendered at DOMContentLoaded, id
   `d9-ui-switch` (stable for the T12 census), textContent-only (no URL
   content interpolated into HTML). Fixed positioning means visible
   without scrolling on both UIs. Renders only when a migrated
   counterpart is registered - per the ticket, a switch to nowhere must
   not exist, so nothing renders anywhere today (registry empty).

7. DEFAULT FLIP - verified unmade and single-sourced.
   `DEFAULT_UI = "legacy"` (`ui-switch.js:59`) is the only default in the
   codebase (no other occurrence outside its own file and tests);
   `resolveUi`/`decide` take it as a parameter and `run()` passes the
   constant, so the flip is exactly that one line. The flipped behavior
   (default new, `?ui=legacy` still pins) is pre-tested.

8. TESTS - real and green. `dial9-viewer/ui/tests/ui_switch.test.ts`
   (33 cases) loads the SHIPPED ui-switch.js via createRequire and asserts
   concrete output URLs - not tautological: repeated-trace preservation,
   both precedence directions, hash dropping both ways, empty/garbage
   storage fallback, empty-registry pinning (the test that page tickets
   must consciously update), self-registration guard, the flip, and a
   three-leg logic-level round trip. Ran on the integration tip
   (INTEG worktree, `npx vitest run tests/ui_switch.test.ts`, vitest
   4.1.10): 33/33 passed. Gap (documented in the T38 HANDOFF, structurally
   pending T13/T14): no browser-level test exercises `run()`/
   `mountControl`/`writeStoredPref`, which is exactly the layer where
   findings 1 and 2 live - the pure decide() tests cannot see a stale
   `search` argument because the caller chooses it.

## Disposition

Not blocking today: the registry is empty, so no user-visible routing
exists on the integration tip and nothing strands. Finding 1 becomes
user-facing the moment T13 registers the first page and must be fixed in
ui-switch.js before or with that registration; findings 2 and 3 are cheap
hardening in the same file. All three are fixable without touching the
legacy pages (the one-line fence holds).
