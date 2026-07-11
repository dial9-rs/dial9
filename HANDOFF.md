# T43 HANDOFF - URL contract as an API (#303)

(Replaces the T19 HANDOFF inherited through the branch chain; T19's record
lives at commit e6b95a6.)

## STATUS

COMPLETE - no STOP-gate hit. All DoD items done, all gates green, dev-server
killed. Work spans three sessions: the first died at a session limit leaving
two commits + uncommitted debris; the second (this one, restarted once after
an org spend limit) audited the debris, salvaged ALL of it (it was coherent:
an emdash cleanup of lines the first session itself added, the parity-tooling
doc entry the committed Enforcement section promised, and the two contract
test files whose imports/fixtures/selectors all verify), and finished the
verification work. Nothing was discarded.

Branch: `ticket/T43-url-contract-api`, based on integrated tip 009582d.

## COMPLETED (commits)

| sha | what |
| --- | --- |
| 10625b8 | contract doc: `dial9-viewer/ui/README.md` "URL contract (stable deep-link API)" (full query/hash schema, N10 additive-only promise, honest reserved-key status, #303 recipes) + `docs/agents/domain.md` "Emitting viewer links" pointer |
| cb18d5a | skills emit contract-conformant links: dial9-html-report params table reconciled (segs = COUNT, from/to display-only, flamegraph pre-zoom recipe, no-selection-params-yet honesty); dial9-zoom-window prints a viewer deep link per window (`zoom.js`) + SKILL.md conversion recipe |
| 2f6954f | style: plain hyphens in the two new SKILL.md lines (salvage) |
| 3ec00f7 | enforcement (salvage): `src/lib/url/url-contract.test.ts` (vitest codec/doc pin) + `parity/url-contract.mjs` (live T12-family check) + README parity-tooling (f) entry |

## WHAT SHIPPED

- The contract doc: every stable query param per page (viewer/flamegraph
  exact mode, flamegraph api mode, index.html via url_state.js, the `ui`
  switch), the hash key registry (live / defined-unwritten / reserved), the
  N10 stability promise (additive-only, forever), and the three #303 recipes
  (exact window; pre-zoomed flamegraph; highlight = honestly NOT
  constructible until chunk 2 flips `sel.*`/`vp`/`poi` to live).
- Skills updated to emit and document only contract-conformant links;
  zoom-window's every window report now ends with a ready deep link.
- Enforcement, two layers:
  - vitest pin (12 tests): schema version pinned at 1; README tables kept in
    lockstep with the recorded legacy-param fixture, url_state.js's
    serialization surface, and the codec's encode surface (renaming a
    documented param breaks the suite BY DESIGN); reserved keys proven
    preserved-verbatim-but-never-honored; recipe URL shapes resolve at codec
    level.
  - live parity script: constructs the recipe URLs in plain Node (no
    browser) and drives real pages on a dev-server; 4 legs, exit 0 only when
    all pass.

## DoD EVIDENCE

1. "A curl-constructed URL opens the viewer at an exact window,
   script-verified" - PASS. `node parity/url-contract.mjs --base
   http://localhost:3111` output (built dist, dev-server per the README
   recipe):

   ```
   PASS  viewer-window
         url: http://localhost:3111/viewer.html?trace=demo-trace.bin&start=145406365856&end=145506365856
         fullEvents = "294,465"
         windowEvents = "306"
         windowDuration = "89.89ms"
   PASS  fg-legacy-zoom
         url: http://localhost:3111/flamegraph.html?trace=demo-trace.bin&worker-zoom=0xffff9b8cbf1c%090xffff9b862030%09Thread%3A%3Anew%3A%3Athread_start+unix.rs%3A130
         breadcrumb = "(all) > 0xffff9b8cbf1c > 0xffff9b862030 > Thread::new::thread_start unix.rs:130"
   PASS  fg-hash-zoom
         url: http://localhost:3111/new/flamegraph.html?trace=demo-trace.bin#v=1&fg.w=0xffff9b8cbf1c%090xffff9b862030%09Thread%3A%3Anew%3A%3Athread_start+unix.rs%3A130&sel.task=3
         breadcrumb = "(all) > 0xffff9b8cbf1c > 0xffff9b862030 > Thread::new::thread_start unix.rs:130"
   PASS  fg-foreign-version
         url: http://localhost:3111/new/flamegraph.html?trace=demo-trace.bin#v=2&fg.w=...
   url-contract: all 4 legs green
   ```

   The demonstrated URL (recipe 1, the #303 "set time range / open at an
   exact window" ask):
   `viewer.html?trace=demo-trace.bin&start=145406365856&end=145506365856`
   -> 306 of 294,465 events, duration readout 89.89ms inside the requested
   100ms window, Clear Range visible on load, URL not rewritten. The legacy
   viewer honoring `?start/?end` as parse-time filters is also verified in
   code: `viewer.html:1937` (`getParseOptions` -> parseTrace start/end) and
   `viewer.html:5072` (range params reveal Clear Range) - features/02 E5.

2. "Skills emit working links" - PASS, end-to-end. Unmodified
   `zoom.js public/demo-trace.bin 100 50` printed:
   `viewer.html?trace=<TRACE_URL>&start=145406365856&end=145506365856`;
   a throwaway driver (stdout-parsed, `<TRACE_URL>` -> `demo-trace.bin`,
   Playwright against the same dev-server) loaded it:
   `toolbar: demo-trace.bin 306 events - 2 workers - 89.89ms
   (time-filtered)`, `clear-range visible: true`, `SKILL-LINK CHECK: PASS
   (windowed load, 306 events in a 100ms window)`. The emitted URL is
   byte-identical to leg 1's constructed URL.

3. Schema-version pin - PASS. `npx vitest run
   src/lib/url/url-contract.test.ts`: 12/12 (version pinned at 1 + the doc
   documents it; table lockstep per page; reserved-key behavior; recipes).

4. Doc exists + is linked - `dial9-viewer/ui/README.md` section "URL
   contract (stable deep-link API)"; linked from `docs/agents/domain.md`
   ("Emitting viewer links" - skill-convention home confirmed) and from
   both updated SKILL.md files.

5. Highlight/selection ask: honestly reserved, not faked. `sel.*`/`vp`/`poi`
   are name reservations completing with chunk 2 (T21-T23); the doc, both
   skills, and the fg-hash-zoom leg (inert `sel.task=3` rides along,
   preserved, changes nothing) all state/prove it.

## LEDGER

No entries needed: T43 introduces NO new query param or hash key (the doc
records existing behavior; `vp`/`sel.*`/`poi` are reserved-only names, which
per the ticket convention get ledger rows only when they go live). Existing
T19 rows already cover the hash mechanics.

## GATES

- `npx tsc --noEmit`: clean.
- `npm run test` (FULL suite incl. check:boundary pretest): 54 files passed
  + 1 skipped; 936 passed / 1 expected fail / 11 skipped - the pre-existing
  baseline (T19-era: 888/1/11 over 50 files) plus T42's merged suites and
  this ticket's 12. 0 unexpected failures.
- `npm run build`: clean (dist + 17 static-copied items).
- `cargo build -p dial9-viewer`: clean (rust-embed pickup check).
- Dev-server (PORT=3111) killed; port verified free.
- JS/TS/docs-only change (no .rs touched, no trace-format change): per
  AGENTS.md, cargo nextest/stress/clippy not required.

## SCOPE FENCE

- Docs + skills + contract tests only. No page-code changes; frozen core
  untouched; url_state.js untouched (read-only from the test).
- No push, no PRs, no GitHub access.

## OPEN QUESTIONS / NOTES FOR MAINTAINER

- MAINTAINER FOLLOW-UP (the ticket's closing criterion): re-read
  `gh issue view 303` at close time to confirm the recorded asks (set time
  range, highlight, open flamegraph) are the complete list - GitHub access
  was forbidden for the implementing agents, so the ticket doc's recording
  of #303 is what this work answered. Actual issue closing is T44's
  sign-off flow. The "highlight" ask closes as "reserved, lands with chunk
  2" - the maintainer may prefer to keep #303 open until T21-T23 instead.
- `parity/url-contract.mjs` reuses the J9 fixture path, so it inherits J9's
  demo-trace dependency: after a demo-trace regeneration, re-record J9
  (comment in parity/journeys.mjs) and both zoom legs follow automatically.
  Leg 1 computes its window from the parsed trace's minTs, so it survives
  regeneration as-is.
- `parity/url-contract.mjs` is not wired into any CI job (parity scripts
  are on-demand tooling per the README); the vitest pin IS in `npm run
  test`. If the maintainer wants the live check in CI, that is a separate
  decision.
