# T07 HANDOFF - Store: typed slices + RAF scheduler

(Replaces the T06 HANDOFF inherited through the branch chain; T06's own
record lives at commit f106b6c / its worktree.)

## STATUS

DONE - all DoD checks pass (evidence below). No STOP-gate hit: the one
spec point with multiple readings ("transient channel") had a clearly
preferable reading, documented below and in the store header. Store only;
no page wiring, no component code, no frozen-core .js, no page HTML, no
Rust touched.

## COMPLETED (commits on `ticket/T07-store-scheduler`, on top of f106b6c)

- `892be3e`: src/store/store.ts (the store core, 133 code lines) +
  src/vite-env.d.ts (standard Vite client-type shim for import.meta.env).
- `3e51e56`: src/store/store.test.ts (14 tests) + @vitest/coverage-v8
  devDependency (package.json / package-lock.json).
- (final commit): this HANDOFF.

## WHAT WAS BUILT (and the decisions inside it)

`src/store/store.ts` - one file, 133 lines excluding comments/blanks
(<200 target, architecture 2.2):

- `createStore<S>(initial, options?)` -> `Store<S>` with `getState()`,
  `update(slice, patch)`, `subscribe(sliceSet, fn) -> unsubscribe`,
  `derived(deps, compute) -> getter`. Generic over the slice shape with a
  homomorphic constraint (`S extends { [K in keyof S]: object }`) because
  a `Record<string, object>` constraint rejects interface types like
  StoreState. Composes with T06's types: `ViewerStore = Store<StoreState>`
  (imported from src/types/state.js, nothing redeclared).
- Update semantics: patch is merged into a FRESH slice object (slice
  replaced wholesale, never mutated); the root state object keeps stable
  identity. Slice identity change is the derived-cache invalidation
  signal (F5), verified by the isolation tests.
- Scheduler: one tick per frame; `update` marks the slice dirty and
  schedules at most one flush; the flush runs each subscriber whose slice
  set intersects the frame's changed set, at most once, passing
  `(state, changedSet)`. Updates made DURING a flush open a new frame
  (dirty set swapped before dispatch), never re-enter the running one.
  A throwing subscriber aborts the rest of its frame loudly (errors are
  not swallowed) but cannot leave the N18 gate open (try/finally).
- INJECTABLE scheduler (`StoreOptions.scheduler`), chosen over a
  stubbable module-level binding: Node has no requestAnimationFrame, and
  injection means concurrent stores/tests never share stub state. Default
  schedules via `requestAnimationFrame` and fails loudly (ReferenceError
  on first update) where none exists - no silent setTimeout fallback.
- TRANSIENT CHANNEL reading: architecture 2.2/2.3 says transient updates
  ride "the crosshair RAF channel" and "never trigger full renders". This
  is implemented as slice-set filtering, not a second mechanism: an
  update to `transient` notifies only subscribers that declared
  `transient` (the crosshair overlay). Observable behavior is identical
  to a physically separate channel (the DoD test asserts it) at a
  fraction of the code; a separate queue can be split out later without
  changing the API. Not treated as a STOP-gate for that reason.
- `derived(deps, compute)` (F5): caches the computed value plus the dep
  slice objects seen at compute time; recomputes only when a dep slice's
  identity changed. No version counters needed given the
  replaced-wholesale invariant.
- N18 hook: `assertInScheduledRender(context?)` - render-marked functions
  call it at entry; in dev builds it throws when `notifyDepth === 0`
  (i.e. the caller is not inside a store notification tick; F2: no path
  from input event to render may skip the scheduler). Entirely gated on
  `import.meta.env.DEV`, so Vite compiles it out of release bundles.
  `devRenderAssertStats()` returns `{ calls, violations }` - the
  externally readable counters T12's perf probe plugs into. The depth
  counter is module-level (shared across stores) so render fns need no
  store handle; it is a depth, not a boolean, so a synchronous injected
  scheduler that nests flushes still balances.

`src/vite-env.d.ts`: standard Vite scaffold shim (`/// <reference
types="vite/client" />`) so `import.meta.env.DEV` typechecks in src/.
First consumer is the store; page tickets (T13/T14) would have added it
anyway.

## DoD EVIDENCE

All run in dial9-viewer/ui after `npm ci` (plus the coverage-v8 install):

1. `npx tsc --noEmit`: clean (exit 0).
2. `npm run test`: 10 files, 142 tests, all pass (8 migrated legacy
   suites + T06 exhaustive test + the new store suite's 14 tests).
3. Coverage (`npx vitest run --coverage.enabled
   --coverage.include='src/store/**'`, provider @vitest/coverage-v8):
   src/store/store.ts = 100% statements (54/54), 100% BRANCHES (20/20),
   100% functions (13/13), 100% lines (48/48). DoD's 100%-branch
   requirement met. Coverage invoked via CLI flags; vite.config.ts is
   untouched (avoids conflicts with sibling tickets).
4. DoD test axes all present in src/store/store.test.ts: slice isolation
   (3 tests), one-notification-per-frame under a fake injected RAF
   (5 tests incl. re-arm, mid-flush update deferral, mid-flush
   unsubscribe, default-RAF path via vi.stubGlobal), transient channel
   bypassing full notification (composed with the REAL StoreState via
   ViewerStore - doubles as the T06-composition proof), derived-cache
   invalidation, N18 hook (4 tests incl. DEV=false via vi.stubEnv and
   gate-closure on a throwing subscriber).
5. `npm run build`: dist listing unchanged - 19 files before and after,
   `diff` empty (src/store is imported by no Vite entry; nothing new can
   ship until a page wires it). Note: running vite build locally deletes
   the tracked `dist/.gitkeep`; restored via git checkout before
   committing (pre-existing quirk, not from this change).
6. Not run: cargo build/nextest/clippy (no .rs touched, no trace-format
   change; rust-embed embeds ui/dist, whose listing is unchanged - same
   justification as T06 per the AGENTS.md JS-only rule). No test_*.js
   added, so no scripts/e2e-trace-tests.sh registration needed (vitest CI
   runs the new suite via the existing src/**/*.test.ts include).

## REMAINING

None for T07. Intentionally left for later tickets:

- T08: store actions (zoom/pan clamps, selection ops), initial-state
  construction, localStorage persistence for uiPrefs.
- T12: perf probe reading devRenderAssertStats().
- Renderer registry mapping changed slices -> affected canvases (2.3
  ticket); it subscribes per slice set like any other subscriber.

## BLOCKERS

None.

## NOTES FOR THE INTEGRATOR / SUCCESSORS

- LOCKFILE: this branch adds `@vitest/coverage-v8@^4.1.10` to
  devDependencies (package.json + package-lock.json). T12's playwright
  addition will conflict in package-lock.json; regenerate the lockfile
  after merging both rather than hand-merging hunks.
- API notes for T08+: `update()` is also the "replace wholesale" op
  (patch with the full slice content, e.g.
  `update("trace", { trace: parsed })`). Subscribers receive the frame's
  FULL changed-slice set, which may include slices they did not declare.
  An empty patch still replaces the slice object, i.e. it signals change.
- Render functions should call `assertInScheduledRender("<name>")` at
  entry (the context string names the offender in the error). Remember it
  throws in dev: a direct render call from an event handler will blow up
  the handler - that is the point (F2).
- The store never persists anything and never touches localStorage; keep
  it that way (persistence is an action-layer concern, T08).
