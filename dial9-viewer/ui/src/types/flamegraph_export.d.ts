// Type declarations for the frozen-core file `flamegraph_export.js`
// (folded stacks + interactive SVG export).
// See src/types/decode.d.ts for the declaration-form rationale.

declare module "*/flamegraph_export.js" {
  import type { FlamegraphNode } from "*/trace_analysis.js";

  /** One export panel: a labeled flamegraph tree (worker / off-worker). */
  export interface ExportPanel {
    label: string;
    tree: FlamegraphNode | null;
  }

  /**
   * Walk a tree into the universal folded-stacks format
   * ("frame1;frame2 count" lines). Empty string for an empty/null tree.
   */
  export function treeToFolded(
    root: FlamegraphNode | null | undefined
  ): string;

  /**
   * Render panels into a single standalone interactive flame graph SVG
   * (flamegraph.pl-compatible behavior). `formatValue` renders a node's
   * weight in hover text (defaults to "N samples").
   */
  export function treeToInteractiveSvg(
    panels: readonly ExportPanel[] | null | undefined,
    opts?: {
      title?: string;
      width?: number;
      formatValue?: (count: number) => string;
    }
  ): string;

  /**
   * Merge panels into one single-rooted tree (the panel tree itself for one
   * panel; a synthesized root wrapping labeled panels for several). Null
   * when there is nothing to draw. Never mutates inputs.
   */
  export function buildExportRoot(
    panels: readonly ExportPanel[] | null | undefined
  ): FlamegraphNode | null;

  export interface LayoutFrame {
    node: FlamegraphNode;
    depth: number;
    /** Cumulative count-space coordinates (flamegraph.pl geometry). */
    sTime: number;
    eTime: number;
  }

  /**
   * Lay a tree out into flat positioned frames; subtrees narrower than
   * `minTime` count-units are pruned (the root is always kept).
   */
  export function layoutTree(
    root: FlamegraphNode,
    minTime: number
  ): { frames: LayoutFrame[]; maxDepth: number };

  /** Sanitize a label into a safe filename stem ("flamegraph" fallback). */
  export function filenameStem(label: string | null | undefined): string;

  /**
   * Same hash-to-warm-color mapping the on-screen canvas uses (re-exported
   * from trace_analysis.js so export colors can't drift).
   */
  export function flamegraphColor(name: string): string;

  /**
   * XML-escape a value for SVG text. Accepts anything (String(s) inside),
   * hence `unknown`.
   */
  export function escapeXml(s: unknown): string;
}
