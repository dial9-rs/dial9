# FIX-T38 HANDOFF - dual-UI switch audit fixes

(Replaces the FIX-T17 HANDOFF inherited through the branch chain;
FIX-T17's record lives at commit 502a2c5.)

Branch `fix/T38-click-time-switch-url`, based on the integrated chunk-1
tip 851b02e. Authoritative spec: the adversarial audit at
`docs/tickets/reviews/T38-audit.md` (finding 1 MEDIUM, findings 2-3 LOW;
none blocking today with the registry empty, but finding 1 must land
before the first page registers in NEW_UI_ENTRIES).

## STATUS

DONE - all three findings fixed in `dial9-viewer/ui/ui-switch.js` only,
each with failing-first regression tests in
`dial9-viewer/ui/tests/ui_switch.test.ts` (33 -> 48 cases). All gates
pass (evidence below). No STOP-gate hit: no materially forked reading of
the audit was encountered. Scope fence respected: no page HTML, no
frozen core, no .rs, no inventories, no pushes/PRs. ui-switch.js remains
a plain browser script (no build step, textContent-only rendering,
URL/URLSearchParams platform APIs only). Finding 2 carries a documented
residual (below) that is not fixable from ui-switch.js alone.

## COMPLETED (commits on top of 851b02e)

- `6677d16` finding 1 (MEDIUM): the switch-control target URL is now
  resolved from the LIVE location.pathname + location.search at
  interaction time (new pure helper `liveControlHref`, exported), never
  from the boot-time snapshot. `mountControl` refreshes the anchor href
  on `mousedown` (runs ahead of left-/middle-click navigation and of the
  context menu's copy-link) and on `click` (keyboard activation fires no
  mousedown), so navigation after any in-page history.replaceState/
  pushState carries the current query - the loaded trace/scope survives
  the switch. Label/direction stay boot-time per the spec; a null
  resolution keeps the previous href (no dead link). decide()'s registry
  lookup + loop guard were factored into `registeredEntry`
  (behavior-preserving in this commit) so both paths share them.
- `aef8350` finding 2 (LOW): when the legacy side boots with an explicit
  `?ui=legacy` pin that is LOAD-BEARING - the same URL without the pin
  would resolve "new" (new pure helper `pinWouldBounce`, exported) - the
  "legacy" choice is persisted to the localStorage preference at boot.
  Three of the four legacy pages strip the pin on their first URL sync;
  with storage aligned, a stripped pin plus a reload (or a copied
  address-bar URL in the same browser) still resolves legacy. This is
  the audit's own suggested fix ("on booting a canonical page with an
  explicit ?ui=legacy while storage says new, write legacy"),
  generalized to stay correct after the DEFAULT_UI flip (a no-preference
  visitor then resolves "new" too, so the pin is load-bearing for them
  as well). Gated on a registered counterpart (result.control): on
  unregistered pages no dispatch is possible, so a stray pin never
  touches the global preference.
- `68eb9cb` finding 3 (LOW): `registeredEntry` rejects any entry that is
  itself a registry key - subsumes the old exact self-registration
  check and kills cross-registration cycles ({"a.html": "b.html",
  "b.html": "a.html"}), which would otherwise location.replace()-loop
  with no escape (buildQuery strips `ui` on every new-bound hop). A
  legitimate entry is never a key (new-UI entries live off-root). The
  guard sits in the shared lookup, covering both decide() dispatch and
  liveControlHref. Chose this over the audit's alternative
  (sessionStorage one-shot breaker): pure, testable, no storage
  dependency. A permanent acyclicity walk over the shipped
  NEW_UI_ENTRIES pins the data side.
- (final commit) this HANDOFF.

## EVIDENCE (per finding: failing-first -> green)

All runs in `dial9-viewer/ui` (npm ci done) on this branch.

- Finding 1: the 6 cases of "click-time target resolution (T38-audit
  finding 1)" were written first and failed on the unfixed tree
  (liveControlHref is not a function); after `6677d16`: 39/39. The
  audit's stale-boot-query scenario is the first case: boot search
  `?bucket=b&host=all` yields the boot href; a simulated replaceState to
  `?bucket=b&host=h1&period=custom` resolves
  `/new/flamegraph.html?bucket=b&host=h1&period=custom`, asserted
  different from the boot href.
- Finding 2: the 5 cases of "legacy-pin storage alignment (T38-audit
  finding 2)" failed on the unfixed tree (pinWouldBounce is not a
  function); after `aef8350`: 44/44. The audit's middle-click bounce is
  a single sequential case: boot pinned with storedPref=new stays legacy
  (param precedence); the pre-fix reload after the strip resolves "new"
  (the bounce, asserted); with the pin detected load-bearing and storage
  aligned to "legacy", the same pin-less reload resolves legacy.
- Finding 3: 3 behavior cases of "registry cycle guard (T38-audit
  finding 3)" failed on the unfixed tree (the synthetic cycle produced
  redirect "/b.html" / "/a.html" from both nodes, param- and
  pref-driven); after `68eb9cb`: 48/48. The acyclicity walk over the
  shipped registry is trivially green today (registry empty) and becomes
  load-bearing as T13/T14/T41 register lines.

## REGRESSION TESTS (named so a reviewer can find them)

In `dial9-viewer/ui/tests/ui_switch.test.ts`:
- `click-time target resolution (T38-audit finding 1)`: "legacy side:
  resolves from the live query after a simulated replaceState, not the
  boot query", "new side: the live query is carried back with ui=legacy
  pinned", "re-resolves the page from the live pathname (an SPA
  pushState may move it)", "falls back to the boot page when the live
  pathname resolves nothing", "returns null when no target resolves
  (caller keeps the previous href)", "owns the live ui param: stripped
  toward new, replaced toward legacy".
- `legacy-pin storage alignment (T38-audit finding 2)`: "detects the
  audit's bounce precondition: pin present, stored pref says new",
  "middle-click scenario: once storage is aligned, a stripped pin no
  longer bounces", "no alignment when the visitor already resolves
  legacy without the pin", "no alignment without an explicit ui=legacy
  pin", "post-flip: the pin is load-bearing for a no-preference visitor
  too".
- `registry cycle guard (T38-audit finding 3)`: "a two-page
  cross-registration cycle never dispatches from either node", "an entry
  that is itself a registry key is rejected; innocent keys still
  dispatch", "the live href resolver applies the same guard", "the
  shipped registry is acyclic: no entry is a registry key".

## FINDING-2 RESIDUAL (and proposed follow-up)

The script-side fix provably kills the audit's bounce whenever
localStorage is writable, which covers every storage state that can
produce the bounce pre-flip (the bounce needs a READABLE storedPref of
"new"; a readable-but-unwritable localStorage is an exotic edge). Two
residuals remain that CANNOT be fixed from ui-switch.js, because the pin
is simply gone from the URL:

1. Storage-unwritable visitors after the DEFAULT_UI flip: with the
   default "new", a pinned visitor with NO stored preference (or whose
   storage rejects writes) bounces on reload once a page strips the pin.
   writeStoredPref is best-effort by design.
2. Post-strip copied URLs across browsers: a URL copied from the address
   bar AFTER the page's first URL sync carries no ui=legacy at all, so a
   recipient (or the same user in another browser/profile) resolving
   "new" lands on the new UI. Pre-strip copies keep the pin (and the
   finding-1 mousedown refresh keeps the control's own copy-link URL
   pinned and live).

Proposed ONE-LINE follow-up for the page tickets (recorded here, NOT
applied - page edits beyond T38's sanctioned script include are fenced):
in each query-rebuild helper that replaces the URL from scratch
(index.html:749 via url_state, flamegraph.html:219, tokio_stats.html:155),
carry the `ui` param through the rebuild before
history.replaceState/pushState, e.g.
`if (new URLSearchParams(location.search).has("ui")) params.set("ui", new URLSearchParams(location.search).get("ui"));`
(viewer.html:1946-1952 already preserves unknown params and needs
nothing). That closes both residuals; the script-side alignment stays
correct alongside it.

## DECISIONS A REVIEWER SHOULD SEE

- pinWouldBounce generalizes the audit's literal trigger ("storage says
  new") to "the pin is the only thing keeping this visitor on legacy"
  (resolution without the pin === "new"). Pre-flip the two conditions
  are identical; post-flip the general form also covers no-preference
  visitors. It stays false when the visitor already resolves legacy, so
  a shared ?ui=legacy link does NOT sticky-switch such recipients
  (mirroring the write-on-click-only rule). It DOES sticky-switch a
  recipient whose stored pref says "new" - the audit explicitly weighs
  honoring the pinned intent above the no-sticky rule for exactly that
  conflict.
- The control's href refresh is null-safe: if the live location stops
  resolving to a registered target (cannot happen with today's static
  pathnames), the previous href is kept rather than rendering a dead
  link.
- The browser wiring itself (mountControl listeners, run()'s alignment
  call) remains untestable at the DOM level in this suite - the same
  structural gap the T38 HANDOFF recorded (pure decide()-level tests;
  browser-level coverage lands with T13/T14's first registered page).
  All new decision logic is pure and exported precisely so the audit
  scenarios are testable now.
- Finding 3 hardening intentionally rejects CHAINS too (an entry that is
  a registered key, even without a full cycle): a new-UI entry is never
  a root-level canonical page, so such a registration is always a
  misconfiguration; the innocent key keeps dispatching (tested).

## GATES

1. `npx tsc --noEmit`: clean (exit 0), re-verified after each commit and
   at the end.
2. `npm run test` (full suite): 46 files passed, 1 skipped; 839 tests
   passed, 1 expected fail, 11 skipped; exit 0. Targeted suite
   `npx vitest run tests/ui_switch.test.ts`: 48/48.
3. `npm run build`: clean; `cmp ui-switch.js dist/ui-switch.js`:
   byte-identical (static-copy ships it verbatim).
4. `cargo build -p dial9-viewer`: exit 0 (AGENTS.md JS-only rule; no new
   embedded files, no .rs touched - no nextest/stress/clippy/fmt run).
   No new ui-root plain-script tests, so no e2e-trace-tests.sh
   registration (the extended suite is the existing vitest-discovered
   tests/ui_switch.test.ts).

## REMAINING

- Page tickets (T13/T14/T41): the one-line `ui`-param carry-through
  above, closing the finding-2 residuals.
- T13 (first registration) inherits the audit's browser-level test
  obligation for run()/mountControl (real round trip in a browser).

## BLOCKERS

None.
