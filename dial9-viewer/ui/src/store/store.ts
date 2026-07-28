// Per-page typed store: update(slice, patch) + slice-filtered subscribe, with
// one RAF-coalesced notification flush per frame. Slices are replaced whole
// (never mutated) so slice identity is the derived()-cache invalidation signal.

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
 * One-level-deep readonly view: slices cannot be reassigned AND a slice's own
 * fields cannot be written (a plain Readonly<S> only guards the first).
 * In-place mutation bypasses dirty tracking, so it must not typecheck.
 */
export type ReadonlyState<S> = { readonly [K in keyof S]: Readonly<S[K]> };

/**
 * A store subscriber. `changed` is the full set of slices that changed in the
 * flushed frame (it may include slices the subscriber did not declare).
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
  /** Cached getter: `compute` reruns only when a `deps` slice identity changes. */
  derived<T, K extends SliceKey<S>>(
    deps: readonly K[],
    compute: (state: ReadonlyState<Pick<S, K>>) => T,
  ): () => T;
}

/** The viewer-page store. */
export type ViewerStore = Store<StoreState>;

// Notification-flush depth on the call stack, shared across all page stores.
let notifyDepth = 0;
let assertCalls = 0;
let assertViolations = 0;

/**
 * Dev-only: throws when a render-marked function runs outside a store
 * notification tick (renders must be triggered by subscriptions, never
 * directly from input events). Compiled out of release builds.
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

/** Snapshot of the render-assertion counters (dev builds only). */
export function devRenderAssertStats(): { calls: number; violations: number } {
  return { calls: assertCalls, violations: assertViolations };
}

interface SubEntry<S> {
  slices: ReadonlySet<SliceKey<S>>;
  fn: Subscriber<S>;
}

export function createStore<S extends { [K in keyof S]: object }>(
  initial: S,
  options?: StoreOptions,
): Store<S> {
  const scheduler: FrameScheduler =
    options?.scheduler ?? ((cb) => { requestAnimationFrame(cb); });
  const state: S = { ...initial };
  if (import.meta.env.DEV) {
    // Freeze every slice so an in-place field write throws instead of
    // silently desyncing subscribers and derived() caches.
    for (const key of Object.keys(state)) {
      Object.freeze(state[key as SliceKey<S>]);
    }
  }
  const subscribers = new Set<SubEntry<S>>();
  let dirty = new Set<SliceKey<S>>();
  let scheduled = false;

  function flush(): void {
    // Reset before dispatch: updates made by subscribers open the next frame.
    scheduled = false;
    const changed: ReadonlySet<SliceKey<S>> = dirty;
    dirty = new Set();
    notifyDepth += 1;
    let completed = false;
    try {
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
      completed = true;
    } finally {
      notifyDepth -= 1;
      if (!completed) {
        // A subscriber threw: re-arm the frame's changed set so the
        // subscribers that never ran are retried next frame.
        for (const slice of changed) dirty.add(slice);
        if (!scheduled) {
          scheduled = true;
          scheduler(flush);
        }
      }
    }
  }

  return {
    getState: () => state,
    update(slice, patch) {
      const next = Object.assign({}, state[slice], patch);
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
