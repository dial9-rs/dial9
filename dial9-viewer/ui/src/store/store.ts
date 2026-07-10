// The per-page typed store (T07; docs/ui-inventory/02-architecture.md 2.2,
// docs/adr/0004-viewer-ui-migration.md section 5, perf findings F2/F5).
//
// Contract:
//
// - Plain object + update(slice, patch) + subscribe(sliceSet, fn). No
//   proxies, no reactivity library.
// - A patch is merged into a FRESH slice object (the slice is replaced
//   wholesale, never mutated); the root state object keeps a stable
//   identity for the store's lifetime. Slice identity change is the
//   invalidation signal for derived() caches (F5). The no-mutation rule is
//   enforced two ways (audit finding 1): the exposed state is typed
//   one-level-deep readonly (ReadonlyState), and dev builds Object.freeze
//   every slice so an in-place write throws instead of silently skipping
//   dirty tracking. The freeze is gated on import.meta.env.DEV (like N18)
//   so release builds pay nothing.
// - The scheduler coalesces all notifications into ONE tick per frame and
//   runs each subscriber at most once per frame (F2/N2). Subscribers
//   declare the slices they depend on and run only when one of those
//   slices changed in the flushed frame. Updates made DURING a flush land
//   in the next frame's tick, never the running one.
// - The "transient channel" (architecture 2.2/2.3) is slice-set filtering,
//   not a second mechanism: an update to the `transient` slice notifies
//   only subscribers that declared `transient` (the crosshair overlay), so
//   it can never trigger full renders. One scheduler serves both.
// - The frame scheduler is injectable (StoreOptions.scheduler): the store
//   must be Node-testable and Node has no requestAnimationFrame. Chosen
//   over a stubbable module-level binding so concurrent stores and tests
//   never share stub state. The default schedules via
//   globalThis.requestAnimationFrame and is left to fail loudly
//   (ReferenceError on first update) where none exists.
// - N18 dev assertion: render-marked functions call
//   assertInScheduledRender() at entry; in dev builds it throws when the
//   caller runs outside a store notification tick (F2: no path from input
//   event to render may skip the scheduler). The whole check is gated on
//   import.meta.env.DEV, so Vite compiles it out of release bundles.
//   devRenderAssertStats() exposes the invocation counters for T12's perf
//   probe.

import type { StoreState } from "../types/state.js";

/** Schedules `callback` to run once, on the next animation frame. */
export type FrameScheduler = (callback: () => void) => void;

export interface StoreOptions {
  /** Frame scheduler override; defaults to requestAnimationFrame. */
  scheduler?: FrameScheduler;
}

/** The string keys of a state shape's slices. */
export type SliceKey<S> = Extract<keyof S, string>;

/**
 * One-level-deep readonly view of a state shape: slices cannot be
 * reassigned AND a slice's own fields cannot be written (a plain
 * Readonly<S> only guards the first). In-place slice mutation bypasses
 * dirty tracking and derived() identity invalidation, so it must not
 * typecheck (audit finding 1). Deeper interiors rely on the T06 shapes'
 * own readonly modifiers (ReadonlySet/ReadonlyMap/readonly fields).
 */
export type ReadonlyState<S> = { readonly [K in keyof S]: Readonly<S[K]> };

/**
 * A store subscriber. `changed` is the full set of slices that changed in
 * the flushed frame (it may include slices the subscriber did not declare).
 */
export type Subscriber<S> = (
  state: ReadonlyState<S>,
  changed: ReadonlySet<SliceKey<S>>,
) => void;

export interface Store<S> {
  /** The live state root (stable identity; slices replaced on update). */
  getState(): ReadonlyState<S>;
  /** Merge `patch` into a fresh copy of `slice` and schedule notification. */
  update<K extends SliceKey<S>>(slice: K, patch: Readonly<Partial<S[K]>>): void;
  /**
   * Run `fn` (at most once per frame) whenever any slice in `sliceSet`
   * changes. Returns the unsubscribe function.
   */
  subscribe(sliceSet: Iterable<SliceKey<S>>, fn: Subscriber<S>): () => void;
  /**
   * A cached getter over state (F5): `compute` reruns only when one of the
   * `deps` slices has been replaced since the last call, otherwise the
   * cached value is returned.
   */
  derived<T>(deps: readonly SliceKey<S>[], compute: (state: Readonly<S>) => T): () => T;
}

/** The viewer-page store, composed with the T06 slice shapes. */
export type ViewerStore = Store<StoreState>;

// Depth of store notification flushes currently on the call stack, across
// all stores in the page (module-level so assertInScheduledRender needs no
// store handle; flushes only ever nest through a synchronous injected
// scheduler, and the counter handles that). Dev-only diagnostics state.
let notifyDepth = 0;
let assertCalls = 0;
let assertViolations = 0;

/**
 * N18 dev-build assertion: render-marked functions call this at entry.
 * Throws in dev builds when the caller is running outside a store
 * notification tick; compiled out of release bundles (import.meta.env.DEV).
 */
export function assertInScheduledRender(context?: string): void {
  if (!import.meta.env.DEV) return;
  assertCalls += 1;
  if (notifyDepth === 0) {
    assertViolations += 1;
    const what = context ?? "render function";
    throw new Error(
      `[dial9 N18] ${what} ran outside the store scheduler's notification ` +
        "tick. Renders must be triggered by store subscriptions only " +
        "(perf finding F2); dispatch a store update instead.",
    );
  }
}

/** Snapshot of the N18 hook counters (dev builds only; for T12's probe). */
export function devRenderAssertStats(): { calls: number; violations: number } {
  return { calls: assertCalls, violations: assertViolations };
}

interface SubEntry<S> {
  slices: ReadonlySet<SliceKey<S>>;
  fn: Subscriber<S>;
}

/**
 * Create a store over `initial`. The homomorphic constraint (every property
 * is an object) is what update()'s patch-merge and derived()'s
 * identity-compare rely on; it accepts interface types like StoreState,
 * which a Record<string, object> constraint would reject.
 */
export function createStore<S extends { [K in keyof S]: object }>(
  initial: S,
  options?: StoreOptions,
): Store<S> {
  const scheduler: FrameScheduler =
    options?.scheduler ?? ((cb) => { requestAnimationFrame(cb); });
  const state: S = { ...initial };
  if (import.meta.env.DEV) {
    // Dev-only (finding 1): freeze every slice so an in-place field write
    // throws (strict mode) instead of silently desyncing subscribers and
    // derived() caches. Slices handed to createStore are store-owned from
    // here on, per the replace-wholesale contract above.
    for (const key of Object.keys(state)) {
      Object.freeze(state[key as SliceKey<S>]);
    }
  }
  const subscribers = new Set<SubEntry<S>>();
  let dirty = new Set<SliceKey<S>>();
  let scheduled = false;

  function flush(): void {
    // Reset BEFORE dispatch: updates made by subscribers open a new frame.
    scheduled = false;
    const changed: ReadonlySet<SliceKey<S>> = dirty;
    dirty = new Set();
    notifyDepth += 1;
    try {
      // Snapshot: subscribers added during dispatch join from next frame.
      for (const sub of [...subscribers]) {
        if (!subscribers.has(sub)) continue; // unsubscribed mid-flush
        let affected = false;
        for (const slice of sub.slices) {
          if (changed.has(slice)) {
            affected = true;
            break;
          }
        }
        if (affected) sub.fn(state, changed);
      }
    } finally {
      // A throwing subscriber aborts the rest of the frame loudly (errors
      // are not swallowed), but must not leave the N18 gate open.
      notifyDepth -= 1;
    }
  }

  return {
    getState: () => state,
    update(slice, patch) {
      const next = Object.assign({}, state[slice], patch);
      // Dev-only freeze of the fresh slice (finding 1; see header).
      if (import.meta.env.DEV) Object.freeze(next);
      state[slice] = next;
      dirty.add(slice);
      if (!scheduled) {
        scheduled = true;
        scheduler(flush);
      }
    },
    subscribe(sliceSet, fn) {
      const entry: SubEntry<S> = { slices: new Set(sliceSet), fn };
      subscribers.add(entry);
      return () => {
        subscribers.delete(entry);
      };
    },
    derived(deps, compute) {
      let cache: { seen: readonly unknown[]; value: ReturnType<typeof compute> } | null = null;
      return () => {
        const cur = deps.map((d) => state[d]);
        const c = cache;
        if (c === null || cur.some((slice, i) => slice !== c.seen[i])) {
          cache = { seen: cur, value: compute(state) };
          return cache.value;
        }
        return c.value;
      };
    },
  };
}
