# T02 HANDOFF - Vite MPA scaffolding + dist-only embed

## STATUS

DONE - all DoD checks pass (evidence below). Asset-location gate RESOLVED by
orchestrator ruling 2026-07-08: Option B - `demo-trace.bin` +
`flamegraph.css` STAY at `ui/` root and ride the static-copy list; the
`public/` move belongs to T04 (chunk-1 T02/T04 Work sections amended by the
orchestrator).

## COMPLETED (commits on `ticket/T02-vite-mpa-scaffolding`, based on main @ 84a21e5)

- `15d8a16`: package.json + package-lock (vite 7.3.6, typescript 5.9,
  vite-plugin-static-copy; npm audit: 0 vulnerabilities), strict tsconfig
  (`noUncheckedIndexedAccess`, `erasableSyntaxOnly`), vite.config.ts with the
  static-copy migration list (4 legacy pages incl. tokio_stats.html + the 12
  root-relative scripts they reference), src/ skeleton dirs (+
  `src/pages/dev-probe.ts` as the placeholder build input and T04's HMR
  probe), `dist/.gitkeep` + `public/.gitkeep` (the public copy regenerates
  dist/.gitkeep on every build so `git status` stays clean),
  `ui/.gitignore`, rust-embed `#[folder]` -> `ui/dist/`.
- `4ca1593`: HANDOFF (gate question, since resolved).
- `33324a9`: vite.config.ts `legacyPageAssets` = flamegraph.css +
  demo-trace.bin added to the copy list per the ruling.
- (final commit): this HANDOFF update with complete evidence.
- Deliberate: NO `"type": "module"` in package.json - `node test_*.js` must
  keep loading the root scripts as CJS (constraint H2).

## DoD EVIDENCE

1. check `npm ci && npm run build` -> dist serves all FOUR pages + assets
   byte-identical: `npx tsc --noEmit` clean; `npm run build` copies 18 items.
   Byte-diff via two `python3 -m http.server` instances (ui/ on :3011,
   ui/dist/ on :3012), `curl` + `cmp` on all 18 served paths
   (index/viewer/flamegraph/tokio_stats.html, the 12 scripts,
   flamegraph.css, demo-trace.bin): ALL IDENTICAL, FAIL=0.
2. check cargo-only checkout compiles with empty UI: emptied `ui/dist/` to
   `.gitkeep` only, `cargo build -p dial9-viewer` -> Finished dev profile
   (34s). No npm involvement in build.rs (unchanged, none exists).
3. check no `test_*.js` in the binary: PASS. `cargo build --release -p
   dial9-viewer --features dev-server --bin dev-server` (rust-embed embeds
   only in release; debug reads from disk at runtime), then `strings` on the
   40 MB binary scanned for ALL 29 `test_*.js` filenames: 3 hits total, each
   a code COMMENT inside a legitimately embedded copied file
   (flamegraph.html:163, heatmap.js:21, panel_layout.js) - byte-identical
   copying requires those comments; zero test files embedded. Positive
   controls prove dist content IS embedded: "dial9 Trace Browser" x2,
   "fg-search-bar" x2, "url_state.js" x3, "tokio_stats.html" x2,
   "demo-trace.bin" x6.
4. Rust gates for the mod.rs change: `cargo fmt --check` PASS; `cargo clippy
   --all-targets --features __nonlinux_all_features` PASS for dial9-viewer
   (zero warnings in the touched crate); `cargo nextest run
   --stress-duration 20s` PASS: 810 tests x 2 stress iterations, 0 failed,
   0 skipped, no flakes (32.4s test summary).

## PRE-EXISTING FINDINGS (not fixed, per scope rules)

- Clippy (macOS, `__nonlinux_all_features`) reports pre-existing warnings in
  UNTOUCHED crates: `perf-self-profile/src/rate_limit.rs` (unused macro
  `rate_limited`, unused import, dead `time_since_epoch`) and
  `dial9-tokio-telemetry/src/telemetry/recorder/mod.rs:10` (unused import
  `poll_start_ts_monotonic`). Likely cfg(non-Linux) artifacts. Report-only.

## REMAINING

None on this branch. Elsewhere:
- Execution-plan state table flip (T02 -> gates-passed) lives on the docs
  lineage (T01 branch / orchestrator) - not part of this branch.

## review: ITEMS (for the human reviewer)

- Dependency justification per S1: exactly three dev-deps (vite, typescript,
  vite-plugin-static-copy - the migration aid named by the ticket). No
  runtime deps.
- The deliberate ABSENCE of `"type": "module"` in ui/package.json (H2:
  `node test_*.js` must keep loading root scripts as CJS).
- The `dev-probe.ts` placeholder Vite input pattern (page tickets replace).
- The dist/.gitkeep-via-public/.gitkeep regeneration trick.

## NOTES

- `dev-probe.ts` emits an empty chunk warning from Vite ("Generated an empty
  chunk: dev-probe") - expected: the module exports a constant and has no
  side effects; page tickets replace this input.
- Byte-diff rig: `python3 -m http.server` chosen because the dev-server
  hardcodes its UI dir; T12 replaces this rig with real parity tooling.
