# T07 audit - store/scheduler (integration/chunk-1 @ 3004ca2)

**Verdict: FINDINGS** (no blocking defects; core mechanics verified correct,
five contract-level hazards found)

Audited files: `dial9-viewer/ui/src/store/store.ts`,
`dial9-viewer/ui/src/store/store.test.ts`,
`dial9-viewer/ui/src/types/state.d.ts` (T06 dependency),
`dial9-viewer/ui/tsconfig.json`. Authorities: T07 section of
`docs/tickets/chunk-1-foundation.md` + SHARED DECISIONS block,
`docs/ui-inventory/02-architecture.md` 2.2, `03-performance-findings.md`
F2/F5. Line numbers below are against the integration tree.

## Findings

### 1. Shallow `Readonly<S>`: in-place slice mutation typechecks and silently skips notification

`getState(): Readonly<S>` and `Subscriber<S>`'s `state: Readonly<S>`
(store.ts:52-59) protect only top-level slice reassignment. Slice
INTERIORS are mutable: `ViewportSlice.viewStart` etc. are plain `number`
fields (state.d.ts:62-71), so `store.getState().viewport.viewStart = 123`
and, inside a subscriber, `state.viewport.viewEnd = 456` both pass
`tsc --noEmit` (verified with a probe file against the project tsconfig,
exit 0). There is also no dev-mode `Object.freeze` on replaced slices to
catch it at runtime.

Failure scenario: chunk-2 code mutates `viewport` in place -> no dirty
mark, no notification, and `derived()` caches (keyed on slice identity,
store.ts:180) return stale values indefinitely; panels desync from state
with zero diagnostics.

Fix direction: type `getState`/`Subscriber` as one-level-deep readonly
(`{ readonly [K in keyof S]: Readonly<S[K]> }`), and/or `Object.freeze`
the fresh slice in dev builds inside `update()`.

### 2. `undefined` can be stamped into non-optional fields via `update()` patches

`update(slice, patch: Readonly<Partial<S[K]>>)` (store.ts:61) +
`Object.assign({}, state[slice], patch)` (store.ts:161), with
`exactOptionalPropertyTypes` NOT set in tsconfig.json (tsconfig.json:15-24
has `strict` but not that flag): `store.update("viewport", { viewStart:
undefined })` and `store.update("selection", { spanFocus: undefined })`
both typecheck (probe verified, exit 0), and `Object.assign` copies the
explicit `undefined`, overwriting the field.

Failure scenario: `noUncheckedIndexedAccess` (enabled) makes map/array
lookups produce `T | undefined`; passing such a value straight into a
patch writes `undefined` into a field typed `number` or `X | null` ->
every downstream `=== null` check and arithmetic silently misbehaves
while the types claim it cannot.

Fix direction: enable `exactOptionalPropertyTypes`, or strip
`undefined`-valued keys in `update()` (with a dev assertion).

### 3. A throwing subscriber permanently drops the frame's changed set for all later subscribers

`flush()` swaps `dirty` out before dispatch (store.ts:135-136) and never
restores it: if subscriber N throws, subscribers N+1.. are skipped
(store.ts:140-151) and the `changed` set is lost - nothing is
re-scheduled for it (the `finally` only fixes `notifyDepth`,
store.ts:154). The HANDOFF documents "aborts the rest of the frame
loudly" but not that delivery is never retried.

Failure scenario: a subscriber registered before the lane renderer throws
once during the `trace` slice's load notification -> the lane renderer
never learns the trace changed and stays blank until some unrelated
update happens to re-dirty a slice it subscribes to.

Fix direction: on throw, merge `changed` back into `dirty` and re-arm the
scheduler (still rethrowing), so delivery is retried next frame; or
document the drop as an accepted contract and require top-level
subscribers to be non-throwing.

### 4. N18 gate checks "inside ANY flush", not "fired by YOUR subscription"

`notifyDepth` is a single module-level counter shared by every store and
every subscriber (store.ts:82, checked at store.ts:94). Any render
function invoked synchronously from inside any notification tick passes,
including a render called directly by an unrelated subscriber (or by a
subscriber of a different store on the same page).

Failure scenario: panel A's subscriber directly calls panel B's render
function - exactly the scoping bypass F2's renderer-registry rule exists
to prevent - and N18 raises nothing, in dev or CI. The hook matches its
own doc ("outside a store notification tick") but is materially weaker
than F2's "renderer registry maps changed slices to affected canvases
only"; chunk-2 reviewers should not treat N18-clean as proof of correct
render wiring.

Severity: low (design limitation, correctly documented in the header
comment at store.ts:78-81); recorded so chunk 2 does not over-trust it.

### 5. `derived()` compute receives full state; undeclared-dep reads are silently stale

`compute: (state: Readonly<S>) => T` (store.ts:72) hands the whole state
to the compute function while invalidation is keyed only on the declared
`deps` identities (store.ts:178-180). Declared deps ARE all checked
(verified), but a compute that reads an undeclared slice caches stale
data with no dev-build detection.

Failure scenario: `derived(["viewport"], s => window(s.viewport,
s.selection))` -> selection changes, getter keeps returning the old
selection-derived value; the bug ships silently because deps look
plausible.

Fix direction: dev-build proxy over `state` in `compute` that asserts
every slice read is in `deps` (DEV-gated like N18), or narrow the compute
argument to `Pick<S, deps>`.

## Verified clean (audit axes with no defect)

- RAF coalescing: one scheduled flush per frame (`scheduled` flag,
  store.ts:163-166); `scheduled`/`dirty` reset BEFORE dispatch
  (store.ts:134-136), so updates during a flush land in the next frame's
  tick and re-arm the scheduler - no subscriber can run twice in one
  frame and no mid-flush update is lost (modulo finding 3's throw path).
  Subscriber snapshot + liveness check handles mid-flush
  subscribe/unsubscribe (store.ts:140-141). Tests cover re-arm, deferral,
  and mid-flush unsubscribe (store.test.ts:138-189).
- Slice isolation: notification requires slice-set intersection with the
  frame's changed set (store.ts:142-149); B-only updates never fire an
  A-only subscriber (store.test.ts:74-87). Slices are replaced wholesale
  via fresh objects; sibling slices keep identity (store.test.ts:89-101).
  The mutation loophole is finding 1, a type-surface issue, not a
  notification-logic one.
- Derived cache freshness: `update()` replaces the slice object
  synchronously, before any flush, so a derived read in the same frame as
  its dep's update sees the new identity and recomputes - no same-frame
  staleness. All declared deps compared (store.ts:178-180).
- Transient channel: pure slice-set filtering on the one scheduler
  (store.ts header 17-20); `transient` updates reach only declarers
  (store.test.ts:206-226, against the real StoreState). Shared single
  flush means transient traffic cannot starve or reorder normal-slice
  notifications - both deliver in the same tick, subscriber insertion
  order.
- N18 existence + firing: `assertInScheduledRender` throws when
  `notifyDepth === 0` (store.ts:91-103); traced: an event handler calling
  a render-marked function directly throws, the same function inside a
  subscription passes (store.test.ts:252-280). Gate closes on subscriber
  throw via try/finally (store.test.ts:289-300). Release gating: entire
  body behind `import.meta.env.DEV` (store.ts:92) -> statically false in
  Vite production builds, body dead-code-eliminated; counters only
  increment after the DEV check. DEV=false no-op tested via
  `vi.stubEnv` (store.test.ts:282-287).
- Test honesty: re-ran `npx vitest run src/store --coverage.enabled
  --coverage.include='src/store/**'` in the integration worktree:
  14/14 pass, 100% statements (54/54), 100% BRANCHES (20/20), 100%
  functions (13/13), 100% lines (48/48) - the HANDOFF's claim reproduces
  exactly. Fake RAF is a plain queue with explicit `frame()` (no timers,
  no real RAF, test time 5ms); callbacks scheduled during a frame wait
  for the next, matching real RAF semantics. Deterministic.
- Types: `npx tsc --noEmit` clean on the integration tree (strict,
  noUncheckedIndexedAccess). No `any` in store.ts. Slice shapes imported
  from `src/types/state.js` (T06), not redeclared (store.ts:35,76;
  store.test.ts:15,45-66 constructs a value CHECKED against the T06
  types). Root-slice reassignment via `getState()` is correctly rejected
  (probe `@ts-expect-error` held).
