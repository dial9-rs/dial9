# T38 HANDOFF - Dual-UI raw switch (all pages)

(Replaces the T08 HANDOFF inherited through the branch chain; T08's own
record lives at commit 0acc10a.)

## STATUS

DONE at the mechanism level - all gates pass (evidence below). No
STOP-gate hit. The DoD items that require a real migrated page are
structurally pending T13/T14 and are covered at the logic level only
(listed under REMAINING).

Scope fence honored: exactly ONE line added to each of the four legacy
pages (verified: `git diff 0acc10a..HEAD --numstat -- '*.html'` shows
1 insertion, 0 deletions per page); no page migration; the default flip
is a single commented line in ui-switch.js, left unmade.

## COMPLETED (commits on `ticket/T38-dual-ui-switch`, on top of 0acc10a)

- `10aa900` ui-switch.js (the whole mechanism) + vite.config.ts:
  static-copy list gains ui-switch.js; vitest include widened to
  `tests/**/*.test.ts` (non-core ui/-root scripts test under tests/).
- `1042add` the sanctioned single edit: one
  `<script src="ui-switch.js"></script>` line in each legacy page's
  <head> (index.html, viewer.html, flamegraph.html, tokio_stats.html),
  placed in <head> so a `?ui=new` dispatch replaces the page before any
  body script runs.
- `e890916` tests/ui_switch.test.ts: 33 vitest cases over the pure
  decision logic (createRequire pattern, as tests/core/).
- `044ae7e` ui/README.md: "Dual-UI switch" section - convention,
  precedence, control id, T13/T14/T41 registration recipe.
- (final commit): this HANDOFF.

## MECHANISM SUMMARY (for consumers T13/T14/T41/T21, and T12)

- Routing: `?ui=new` on the canonical URL (shared decision). Precedence:
  explicit `?ui=` param > localStorage `dial9-ui-preference` ("new" |
  "legacy"; reads wrapped in try/catch - failure = no preference) >
  `DEFAULT_UI` in ui-switch.js ("legacy"; THE FLIP is that one commented
  line).
- Registry: `NEW_UI_ENTRIES` in ui-switch.js maps canonical page ->
  new-entry dist path. EMPTY today. Registering a migrated page is one
  line there (plus updating the registry expectation in
  tests/ui_switch.test.ts, which pins today's empty state). Unregistered
  page = no redirect AND no control (a switch to nowhere never renders).
- Raw switch: query string preserved everywhere (URLSearchParams
  re-serialization, repeated trace= params intact - N10) except the
  script-owned `ui` param: removed when targeting new (the entry path
  selects it), pinned `ui=legacy` when targeting legacy (so the
  canonical page cannot bounce back even with storage unavailable or a
  stale "new" preference). Hash always dropped, both directions.
- Control: fixed bottom-right pill, `id="d9-ui-switch"` (stable, for the
  T12 census), textContent-only rendering, hrefs built via
  URLSearchParams (no URL content interpolated into HTML). Clicking it
  writes the localStorage preference (best-effort).
- New-UI entries: load ui-switch.js and call
  `window.D9UiSwitch.mount({ side: "new" })`; canonical page resolved by
  reverse registry lookup on location.pathname, `page:` option overrides.
  The new side never auto-dispatches; only canonical URLs dispatch.
- Guard: a self-registration (page -> same path) is treated as
  unregistered to prevent a location.replace reload loop.
- Switch targets are built root-absolute ("/" + path): assumes the UI is
  served at the server root, true for `dial9 serve` and the dev-server.

## REMAINING (structurally pending later tickets - NOT blockers)

- DoD "every page with a migrated version shows the switch on BOTH
  versions": no migrated version exists yet; the legacy side hides the
  control by design until T13/T14/T41 register entries. Verified now at
  the logic level (tests).
- DoD "round-trip switch keeps the same trace loaded on all pages":
  covered as a logic-level round-trip test; browser round-trip needs a
  real new entry (T13 first).
- DoD "no view state leaks across (zoom in new viewer, switch, legacy
  opens default view)": needs the migrated viewer (chunk 2 / T21); hash
  dropping is unit-tested in both directions now.
- T12 census: assert presence of `<script src="ui-switch.js">` in the
  four legacy pages and/or `#d9-ui-switch` when a registration exists.
- The default flip (DEFAULT_UI -> "new"): deliberately left unmade,
  maintainer decision; it is one commented line in ui-switch.js.

## PARITY BASELINE NOTE (per the ticket's own note)

This ticket intentionally breaks the T02 "byte-identical to
pre-migration" baseline for the four legacy pages by exactly one line
each - the sanctioned single edit they ever receive. The parity baseline
is re-recorded once from this state. Source-vs-dist byte identity is
UNAFFECTED and re-verified (evidence below): static-copy still ships the
edited sources verbatim.

## BLOCKERS

None.

## EVIDENCE (gates, all run in this worktree)

- `npx tsc --noEmit`: clean.
- `npm run test`: 16 files, 234 tests passed (201 inherited + 33 new in
  tests/ui_switch.test.ts).
- `npm run build`: dist/ contains ui-switch.js + the four edited pages
  ("Copied 17 items", was 16).
- Byte identity source-vs-dist (`cmp`): IDENTICAL for index.html,
  viewer.html, flamegraph.html, tokio_stats.html, ui-switch.js.
- `CARGO_TARGET_DIR=<main repo>/target cargo build -p dial9-viewer`:
  Finished (rust-embed picks up the new file).
- Dev-server smoke (PORT=3031, dev-server feature): served
  flamegraph.html line 7 = `<script src="ui-switch.js"></script>`;
  GET /ui-switch.js -> HTTP 200, 12731 bytes; all four served pages +
  ui-switch.js byte-identical (`cmp`) to their sources. Server killed
  after (curl confirms down).
- No Rust files touched (JS/HTML-only per AGENTS.md: nextest/stress/
  clippy/fmt not required; cargo build embed check done).

## OPEN QUESTIONS

None blocking. One recorded choice: on legacy pages with no registry
entry, ui-switch.js is a deliberate no-op (~13 KB script parse); that is
the cost of the uniform one-line include on all four pages, including
tokio_stats.html, which has no migration ticket in chunk 1.
