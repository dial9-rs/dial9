// Store-slice -> URL view-state sync: the browser half of the codec
// (view-state.ts). Subscribes to declared store slices, projects them to a
// ViewState, and writes it into the URL via ONE debounced history.replaceState
// per settled change burst (replaceState, never pushState: view tweaks must not
// pollute Back).
//
// Write rules:
// - the query string is preserved verbatim except what `mirrorToQuery` touches
//   (the flamegraph page passes applyLegacyZoomToQuery);
// - a versioned hash is rewritten with unknown keys preserved; a FOREIGN hash
//   (not ours) is never overwritten - hash carriage is skipped, query mirroring
//   still applies;
// - no-op writes are skipped (URL already says this), so binding at page load
//   never dirties history: restore-on-load produces zero writes.
//
// The debounce timer is injectable for Node tests.

import type { ReadonlyState, SliceKey, Store } from "../../store/store.js";
import {
  VIEW_STATE_VERSION,
  decodeViewState,
  encodeViewState,
  type ViewState,
} from "./view-state.js";

/** The pieces of the URL the sync engine reads (window.location forms). */
export interface UrlParts {
  pathname: string;
  /** "" or "?...", like location.search. */
  search: string;
  /** "" or "#...", like location.hash. */
  hash: string;
}

/** Where URLs are read from and written to; injectable for tests. */
export interface UrlHost {
  read(): UrlParts;
  /** Rewrite the current history entry to `url` (path + search + hash). */
  replace(url: string): void;
}

/** The real thing: window.location + history.replaceState. */
export function windowUrlHost(): UrlHost {
  return {
    read: () => ({
      pathname: window.location.pathname,
      search: window.location.search,
      hash: window.location.hash,
    }),
    replace: (url) => {
      window.history.replaceState(null, "", url);
    },
  };
}

/** Injectable one-shot timer (Node tests drive it manually). */
export interface DebounceTimer {
  set(cb: () => void, ms: number): unknown;
  clear(handle: unknown): void;
}

const defaultTimer: DebounceTimer = {
  set: (cb, ms) => setTimeout(cb, ms),
  clear: (h) => {
    clearTimeout(h as ReturnType<typeof setTimeout>);
  },
};

/**
 * Trailing-edge debounce interval. Short enough that the URL is current
 * by the time a human reaches the address bar or the copy-link button
 * (which flushes anyway); long enough to coalesce a zoom-click burst
 * into one history rewrite.
 */
export const DEFAULT_DEBOUNCE_MS = 150;

export interface BindViewStateOptions<S> {
  /** Store slices whose changes carry view state. */
  slices: Iterable<SliceKey<S>>;
  /** Project the store state to the URL-carried view state. */
  project: (state: ReadonlyState<S>) => ViewState;
  /**
   * Mirror view-state fields into LEGACY query params during each write
   * (the flamegraph page passes applyLegacyZoomToQuery). The hook must
   * only touch its own params.
   */
  mirrorToQuery?: (params: URLSearchParams, state: ViewState) => void;
  host?: UrlHost;
  debounceMs?: number;
  timer?: DebounceTimer;
}

export interface ViewStateBinding {
  /** Project current state NOW (copy-link calls this before copying). */
  flush(): void;
  /** Unsubscribe and cancel any pending write. */
  dispose(): void;
}

/**
 * Bind `store`'s view state to the URL. Returns the binding handle; no
 * write happens until a declared slice actually changes.
 */
export function bindViewStateToUrl<S extends { [K in keyof S]: object }>(
  store: Store<S>,
  options: BindViewStateOptions<S>,
): ViewStateBinding {
  const host = options.host ?? windowUrlHost();
  const timer = options.timer ?? defaultTimer;
  const debounceMs = options.debounceMs ?? DEFAULT_DEBOUNCE_MS;

  let pending: unknown = null;
  let disposed = false;

  function write(): void {
    const state = options.project(store.getState());
    const cur = host.read();

    const params = new URLSearchParams(cur.search);
    options.mirrorToQuery?.(params, state);
    const search = params.toString();

    // Hash carriage: rewrite only what is OURS. An empty hash is writable;
    // a versioned v=1 hash is rewritten with its unknown keys preserved; a
    // foreign or future-version hash stays byte-identical (its state is
    // not ours to strip - see the codec's version rules).
    const curPayload = cur.hash.startsWith("#") ? cur.hash.slice(1) : cur.hash;
    let payload: string;
    if (curPayload === "") {
      payload = encodeViewState(state);
    } else {
      const decoded = decodeViewState(curPayload);
      if (decoded === null || decoded.version !== VIEW_STATE_VERSION) {
        payload = curPayload;
      } else {
        payload = encodeViewState(state, decoded.unknown);
      }
    }

    const url = cur.pathname + (search !== "" ? `?${search}` : "") + (payload !== "" ? `#${payload}` : "");
    const curUrl = cur.pathname + cur.search + cur.hash;
    if (url !== curUrl) host.replace(url);
  }

  function schedule(): void {
    if (pending !== null) timer.clear(pending);
    pending = timer.set(() => {
      pending = null;
      write();
    }, debounceMs);
  }

  const unsubscribe = store.subscribe(options.slices, schedule);

  return {
    flush() {
      if (disposed) return;
      if (pending !== null) {
        timer.clear(pending);
        pending = null;
      }
      write();
    },
    dispose() {
      disposed = true;
      unsubscribe();
      if (pending !== null) {
        timer.clear(pending);
        pending = null;
      }
    },
  };
}
