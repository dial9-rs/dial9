# T46 - End-user documentation refresh - HANDOFF

## STATUS: DONE (DoD met). Not blocked.

## SUMMARY

Most of T46's target end-state was ALREADY in place on this branch.
`ticket/T46-docs-refresh` is cut from `integration/chunk-1` (267 commits ahead
of `main`), which already merged the dependency tickets (T04 dev loops,
T10/T11 Vitest migration + dual-runner retirement, T12 parity tooling, T20
keyboard/help, T38 switch, T43 URL contract). Those tickets had already:

- Rewritten `dial9-viewer/ui/README.md` to cover dev loops, the dual-UI
  switch, the URL contract, the parity tooling, and Vitest-only test
  conventions.
- Updated the `AGENTS.md` "## Testing" section to Vitest reality (commit
  `f754bbe`, "retire the dual-runner setup"): `npm run test` = `vitest run`,
  Vitest auto-discovery, the `TRACE_SUITES` list, the `test_parser.js` Node
  exception. The stale "CI does NOT auto-discover JS tests / You MUST
  register... in scripts/e2e-trace-tests.sh" instruction was ALREADY GONE.

So the headline stale-instruction removal and the AGENTS.md / ui-README
rewrites were done by dependency work (docs-vs-ticket-assumption conflict,
resolved cleanly: reality was already AHEAD of the ticket's stated stale
state). T46's remaining, un-done delta was the END-USER-facing documentation,
which I added, plus verifying every DoD `check:` against reality.

## COMPLETED (commit shas)

- `f01acd8` docs(viewer): document new-UI opt-in switch, keyboard help,
  windowed loading for users.
  - Adds a "Using the viewer" subsection to `dial9-tokio-telemetry/README.md`
    ("Analyzing trace files" section): the staged-rollout switch (`?ui=new`
    opt-in, `?ui=legacy` to force legacy, legacy default until the flip,
    choice remembered per browser, bottom-right pill); the in-app `?` help
    overlay as the AUTHORITATIVE keyboard map (linked, not duplicated);
    windowed loading with the "partial window" / "oversized segment"
    at-cursor badges so a truncated window is never shown as whole.
  - Documents the CURRENT reality only (opt-in, legacy default). Does NOT
    document the default as flipped (that is T39's one-line `DEFAULT_UI`
    change in `ui-switch.js`, not yet made).
- (this commit) docs: T46 HANDOFF (replaces the inherited T32 HANDOFF at the
  worktree root; T32 content stays in git history).

## DECISION: README_TELEMETRY.md de-dup (update-both vs de-duplicate)

Already de-duplicated at the filesystem level - no restructure needed:

- `dial9-viewer/README_TELEMETRY.md` is a SYMLINK -> `../dial9-tokio-telemetry/README.md`
- top-level `README.md` is a SYMLINK -> `dial9-tokio-telemetry/README.md`
- `dial9-tokio-telemetry/README.md` is the single canonical file (the
  "# dial9" library README).

There is nothing to de-duplicate: both READMEs already resolve to one source
of truth via symlinks. Editing the canonical file updates all three surfaces
atomically (verified: `grep -c "Using the viewer"` returns 1 through each of
`dial9-viewer/README_TELEMETRY.md`, top-level `README.md`, and
`dial9-tokio-telemetry/README.md`). So the user-facing switch doc went into
the one canonical file. No irreversible restructure was performed or needed.

(`dial9-viewer/README.md` is a SEPARATE small crates.io stub for the
`dial9-viewer` crate that just points at the `dial9` crate README - out of
scope, untouched.)

## DoD - every check verified

- check: AGENTS.md testing rules match reality (Vitest, no hand registration)
  -> PASS (already correct via `f754bbe`; verified against reality:
  `package.json` `test` = `vitest run`; `vite.config.ts`
  `test.include: ["tests/**/*.test.ts","src/**/*.test.ts"]` = auto-discovery;
  `scripts/e2e-trace-tests.sh` `TRACE_SUITES` for regenerated-trace suites;
  `test_parser.js` still at ui root, driven by
  `dial9-tokio-telemetry/tests/js_parser.rs`). No edit needed.
- check: ui README covers dev/test/parity/URL-contract -> PASS (sections
  present: "UI development requires Node", "Dev loops", "Dual-UI switch",
  "URL contract (stable deep-link API)", "Parity gate tooling", "Tests -
  IMPORTANT for agents"). No edit needed.
- check: switch documented for users -> PASS (commit `f01acd8`).
- check: stale instructions removed (registration warnings) -> PASS. Dead-ref
  sweep over owned docs for
  `CI does NOT auto-discover|You MUST register|must register|hand-registration|serve.py`
  returns ZERO matches. Remaining `e2e-trace-tests.sh` / `node test_`
  mentions are all correct-context: the `TRACE_SUITES` reference, the
  "`node test_*.js` runner was retired (T11)" historical note, and the
  legitimate `test_parser.js` Node exception.

Extra ticket requirements:
- AGENTS.md still parses as valid markdown: unchanged by this ticket (no
  edit), so validity is identical to the committed baseline.
- Docs-only change; no path rust-embed serves was touched (the canonical
  README is not under `ui/dist/`), so no `cargo build -p dial9-viewer`
  required. Evidence is the grep sweeps + read-throughs above.

## REMAINING: none.

## BLOCKERS / QUESTIONS: none.

## OBSERVATIONS (outside owned scope - noted, not fixed per scope fence)

- `AGENTS.md` line 80 (the testing section, authored by dependency commit
  `f754bbe`) contains em-dash characters. Pre-existing dependency content,
  correct in substance; not touched. My own additions use no em-dashes.

## EVIDENCE (commands)

```
# de-dup decision / symlinks
ls -la README.md dial9-viewer/README_TELEMETRY.md   # both -> dial9-tokio-telemetry/README.md
grep -c "Using the viewer" dial9-viewer/README_TELEMETRY.md README.md dial9-tokio-telemetry/README.md  # 1,1,1

# DoD4 dead-ref sweep (clean)
git grep -nE "CI does NOT auto-discover|You MUST register|must register|hand-registration|serve.py" \
  -- AGENTS.md dial9-viewer/ui/README.md dial9-tokio-telemetry/README.md   # no matches

# reality anchors for DoD1
# package.json: "test": "vitest run"
# vite.config.ts: test.include: ["tests/**/*.test.ts","src/**/*.test.ts"]
# in-app help: src/lib/interact/help-overlay.ts, src/pages/viewer/help.ts ("?" toggles)
# partial-data badge: src/components/overlay/readout.ts ("partial window"/"oversized segment")
```
