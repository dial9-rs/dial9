// Type declarations for the frozen-core file `flamegraph.js`
// (shared flamegraph widget: canvas rendering, zoom, search, export menu).
// See src/types/decode.d.ts for the declaration-form rationale.

declare module "*/flamegraph.js" {
  import type { CallframeSymbols, CpuSample } from "*/trace_parser.js";
  import type { FlamegraphNode } from "*/trace_analysis.js";

  /**
   * Input sample for setData. CpuSample satisfies this; the heap views pass
   * pseudo-samples carrying explicit weights. `workerId` routes the sample
   * to the worker vs off-worker panel (255 = off-worker sentinel);
   * `spawnLoc` feeds the spawn-location filter dropdown.
   */
  export interface FlamegraphDataSample {
    callchain: string[];
    workerId: number;
    spawnLoc?: string | null;
    weight?: number;
    allocWeight?: number;
  }

  export interface FlamegraphSetDataOptions {
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
  }

  export interface FlamegraphInstance {
    /** Build worker/off-worker trees from samples and render. */
    setData(
      samples: readonly FlamegraphDataSample[],
      callframeSymbols: CallframeSymbols,
      opts?: FlamegraphSetDataOptions
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
  }

  /**
   * Create the flamegraph widget inside `container`. `onZoomChange` fires
   * whenever the zoom stack changes (used for URL state).
   */
  export function createFlamegraph(
    container: HTMLElement,
    onZoomChange?: () => void
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
