// Type declarations for `flamegraph_diff_view.js` - the browser-only two-sided
// diff view that self-mounts its DOM and owns the per-side SSE streams. See
// src/types/decode.d.ts for the declaration-form rationale.
//
// Not frozen core, but loaded as a browser global (CommonJS-guard form) and
// consumed by typed src/ through the lib/canvas boundary
// (src/lib/canvas/flamegraph_diff_view.ts) exactly like the core.

declare module "*/flamegraph_diff_view.js" {
  /**
   * The diff view's persisted state (onChange arg / initialState):
   * `zoom` is the merged-root-inclusive frame path, `search` the highlight
   * regex. Matches flamegraph_view_state.js readDiffState/writeDiffState.
   */
  export interface DiffViewState {
    zoom?: readonly string[];
    search?: string;
  }

  export interface DiffViewOptions {
    /** Side A (left) scope. */
    scopeA: URLSearchParams;
    /** Side B (right) scope. */
    scopeB: URLSearchParams;
    /** Credential headers for a side; defaults to none. */
    headersFor?: (side: "a" | "b") => Record<string, string>;
    /** Called on a side's fetch failure (drives the per-side BYOC prompt). */
    onSideError?: (side: "a" | "b", err: Error & { status?: number }) => void;
    /** Called on every user view change, for URL persistence. */
    onChange?: (state: DiffViewState) => void;
    /** Seed the view (zoom + highlight) from a shared link. */
    initialState?: DiffViewState;
  }

  export interface DiffViewHandle {
    /** Abort both streams and remove listeners/tooltip. */
    destroy(): void;
    /** Abort and reopen one side's stream (after supplying B's creds). */
    repollSide(side: "a" | "b"): void;
    /** The two sides' display labels. */
    labels: { a: string; b: string };
  }

  /**
   * Create the two-sided diff view inside `container` (self-mounts its DOM).
   * Returns a handle to control the per-side streams.
   */
  export function createDiffView(container: HTMLElement, opts: DiffViewOptions): DiffViewHandle;

  /** Build one side's `/api/flamegraph` URL from its scope. */
  export function apiUrlFor(args: {
    scope: URLSearchParams;
    origin?: string;
    maxFiles?: number | null;
  }): URL;

  /** Short human label for one side (service @ host · poll band). */
  export function scopeLabel(scope: URLSearchParams, fallback: string): string;

  /** Whether a keydown should focus the highlight/search box. */
  export function isSearchFocusKey(e: KeyboardEvent, activeIsSearch: boolean): boolean;
}
