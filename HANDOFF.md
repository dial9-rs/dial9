# T02 HANDOFF - Vite MPA scaffolding + dist-only embed

## STATUS

BLOCKED on one maintainer decision (asset location, below). Everything
decision-independent is done and committed.

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
  `npx tsc --noEmit` and `npm run build` pass; dist contains the 16 copied
  legacy files.
- Deliberate: NO `"type": "module"` in package.json - `node test_*.js` must
  keep loading the root scripts as CJS (constraint H2).

## BLOCKER / QUESTION (stop-on-gate)

The ticket says `demo-trace.bin` + `flamegraph.css` MOVE to `public/`
("no plugin needed"). Two facts the ticket/DoD do not account for:

1. `dev_ui_dir` serving (`ServeDir`, src/server/mod.rs:376-380) has NO
   fallback: dev-server/`dial9 serve --dev` serve `ui/` from disk, so moving
   the files under `ui/public/` 404s `/flamegraph.css` and `/demo-trace.bin`
   in dev mode until T04 repoints the dev loop at `ui/dist`.
2. `ui/demo-trace.bin` has ~18 DISK readers that would all need updating:
   12+ `test_*.js`/bench_parse.js (`path.join(__dirname, "demo-trace.bin")`),
   Rust `tests/decode_test.rs`, `tests/parser_parity_test.rs`,
   `benches/decode_bench.rs` (`CARGO_MANIFEST_DIR/ui/demo-trace.bin`),
   `scripts/regenerate_demo_trace.sh`, `compose.yml`,
   `.github/workflows/stress-test.yml` (CI = T03's scope), AGENTS.md
   instructions.

Options:
- (A) Literal spec: move both, update all readers (touches Rust tests/bench,
  CI workflow, regeneration pipeline; full Rust gates triggered; crosses the
  "CI wiring is T03" scope fence).
- (B) Deviate minimally: keep both at `ui/` root, add them to the
  static-copy list (2 lines in vite.config.ts). Zero reader churn, dev loop
  keeps working, served dist output byte-identical either way. The public/
  move then lands with T04 (when the dev loop repoints) or as its own
  atomic ticket that also moves the regeneration pipeline.

Recommendation: (B) - the public/ move buys nothing today (identical dist
output) and its real blast radius belongs in a ticket that owns it.

## REMAINING (after the decision)

1. Apply the chosen asset mechanism (A: git mv + reader updates; B: add the
   two static-copy lines).
2. DoD checks: byte-diff all 4 pages + every referenced asset served from
   `ui/` vs `ui/dist/` (two static servers + curl + diff); cargo-only
   compile with empty dist (only .gitkeep); `cargo build -p dial9-viewer`
   then assert no `test_*.js` in the binary (strings).
3. Rust gates for the mod.rs change: `cargo fmt --check`, clippy
   (`--features __nonlinux_all_features` on macOS), `cargo nextest run`
   (+ stress per AGENTS.md final verification).
4. Update HANDOFF + execution-plan state table.

## EVIDENCE SO FAR

- `npx tsc --noEmit`: clean. `npm run build`: 1 module transformed, 16 items
  copied, dist listing = 4 html + 12 js + assets/dev-probe + .gitkeep.
