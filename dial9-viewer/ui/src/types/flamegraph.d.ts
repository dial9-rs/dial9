// Type declarations for the frozen-core file `flamegraph.js`
// (shared flamegraph widget: canvas rendering, zoom, search, export menu).
// See src/types/decode.d.ts for the declaration-form rationale.

declare module "*/flamegraph.js" {
  import type { CallframeSymbols, CpuSample } from "*/trace_parser.js";
  import type { FlamegraphNode, FlamegraphTreeOptions } from "*/trace_analysis.js";

  /**
   * Input sample for setData. CpuSample satisfies this; the heap views pass
   * pseudo-samples carrying explicit weights. `workerId` routes the sample
   * to the worker vs off-worker panel (255 = off-worker sentinel);
   * `spawnLoc` feeds the spawn-location filter dropdown.
   */
  export interface FlamegraphDataSample {
    callchain: readonly string[];
    workerId: number;
    spawnLoc?: string | null;
    weight?: number;
    allocWeight?: number;
  }

  export interface FlamegraphSetDataOptions<
    S extends FlamegraphDataSample = FlamegraphDataSample
  > {
    /**
     * Tooltip weight formatter. `treeNode` is the hovered tree node (for
     * heap views to read allocCount); null-ish for synthetic rows.
     */
    formatCount?: (
      count: number,
      total: number,
      self: number,
      treeNode: FlamegraphNode | null | undefined
    ) => string;
    /** Section label for the worker panel (default "Worker threads"). */
    workerLabel?: string;
    /** Section label for the off-worker panel. */
    offworkerLabel?: string;
    /** Title used for exported SVG/folded files. */
    exportTitle?: string;
    /** Hover-text weight formatter for the exported SVG. */
    exportFormatValue?: (count: number) => string;
    /** runtime name -> worker ids, enables the runtime filter dropdown. */
    runtimeWorkers?: Map<string, number[]> | null;
    /** Alternate sample fields used by weighted profiles such as heap views. */
    treeOptions?: FlamegraphTreeOptions<S>;
  }

  /**
   * The widget's complete, serializable view state (getViewState /
   * applyViewState) - the exact shape the URL codec reads/writes. Every field
   * is omitted when inactive, so consumers must treat each as possibly-absent.
   * `inspect` carries the name/symbol split so a restored link re-derives the
   * same identity key (fullName || name).
   */
  export interface FlamegraphViewState {
    workerZoom?: readonly string[] | undefined;
    offworkerZoom?: readonly string[] | undefined;
    inspect?: { name: string; fullName: string } | undefined;
    search?: string | undefined;
    spawn?: string | undefined;
    runtime?: string | undefined;
  }

  export interface FlamegraphInstance {
    /** Build worker/off-worker trees from samples and render. */
    setData<S extends FlamegraphDataSample>(
      samples: readonly S[],
      callframeSymbols: CallframeSymbols,
      opts?: FlamegraphSetDataOptions<S>
    ): void;
    /**
     * API mode: render a pre-built tree directly (no worker/off-worker
     * split, filters hidden). Preserves the current zoom by node name.
     */
    setTreeDirect(tree: FlamegraphNode, totalCount: number): void;
    /** Re-render after a container resize. */
    resize(): void;
    /** Detach listeners and empty the container. */
    destroy(): void;
    /**
     * Handle an Escape press (unpin tooltip -> close menus -> clear search
     * -> reset zoom). True when it consumed the key.
     */
    handleEscape(): boolean;
    isZoomed(): boolean;
    /** Frame-name paths of the current zoom, per panel. */
    getZoomPath(): { worker: string[]; offworker: string[] };
    /** Restore a zoom from a frame-name path. */
    zoomToPath(key: "worker" | "offworker", names: readonly string[]): void;
    /** The complete, serializable view state (see FlamegraphViewState). */
    getViewState(): FlamegraphViewState;
    /**
     * Restore a view state produced by getViewState. Silent by default: the
     * restore must NOT fire onViewChange (that would rewrite the URL
     * mid-restore). Full restore, not a merge - absent fields are cleared.
     */
    applyViewState(state: FlamegraphViewState, opts?: { silent?: boolean }): void;
    /** The current inspect (butterfly) focus key (fullName||name), or null. */
    getInspectFocus(): string | null;
    /** Restore inspect focus by frame key; false when the frame is absent. */
    focusInspectByKey(key: string): boolean;
    /** The current frames-search query ("" when empty). */
    getSearch(): string;
    /** Set the frames-search query (used by view-state restore). */
    setSearch(q: string): void;
    /** The active spawn-location filter value ("" = none, or API mode). */
    getSpawnFilter(): string;
    /** The active runtime filter value ("" = none, or API mode). */
    getRuntimeFilter(): string;
  }

  /**
   * Create the flamegraph widget inside `container`. `onViewChange` fires
   * whenever the view changes (zoom, inspect focus, search, or a filter),
   * used to drive the URL view-state sync.
   */
  export function createFlamegraph(
    container: HTMLElement,
    onViewChange?: () => void
  ): FlamegraphInstance;

  /**
   * On-CPU samples (non-empty callchain, source !== 1), optionally
   * restricted to [startNs, endNs]. Generic so callers keep their sample
   * type (viewer passes ParsedTrace.cpuSamples).
   */
  export function filterCpuSamples<
    S extends Pick<CpuSample, "callchain" | "source" | "timestamp">
  >(
    cpuSamples: readonly S[],
    startNs?: number | null,
    endNs?: number | null
  ): S[];
}
