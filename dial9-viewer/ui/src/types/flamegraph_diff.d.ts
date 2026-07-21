// Type declarations for `flamegraph_diff.js` - the DOM-free core of the
// two-sided differential flamegraph (`?diff=1`): tree merge, relative-hotness
// color, per-side layout, the base64url scope-link codec, and the poll-band
// label. See src/types/decode.d.ts for the declaration-form rationale.
//
// Not frozen core, but loaded as a browser global (CommonJS-guard form) and
// consumed by typed src/ through the lib/canvas boundary
// (src/lib/canvas/flamegraph_diff.ts) exactly like the core.

declare module "*/flamegraph_diff.js" {
  /** One side's scope: a URLSearchParams of the server-bound aggregate query. */
  export type DiffScope = URLSearchParams;

  /** A parsed diff link: the two sides' scopes. */
  export interface DiffSides {
    a: DiffScope;
    b: DiffScope;
  }

  /** The in-page capture tray state (A fills before B). */
  export interface DiffCaptureState {
    a: DiffScope | null;
    b: DiffScope | null;
  }

  /** A merged diff node carrying BOTH sides' counts. */
  export interface DiffMergedNode {
    name: string;
    a: number;
    b: number;
    selfA: number;
    selfB: number;
    children: Map<string, DiffMergedNode>;
  }

  /** A viz-button routing decision (page + query string). */
  export interface DiffTarget {
    page: string;
    search: string;
  }

  /** Merge two server flamegraph trees (either may be null) into one diff tree. */
  export function mergeTrees(treeA: unknown, treeB: unknown): DiffMergedNode;

  /** Map a node's two counts to an `rgb(...)` string (blue=A, red=B, grey=parity). */
  export function diffColor(a: number, b: number, totalA: number, totalB: number): string;

  /** Lay out one side as a flat box list (see flamegraph_diff.js for the shape). */
  export function layoutSide(
    focus: DiffMergedNode,
    focusPath: readonly string[],
    side: "a" | "b",
    widthPx: number,
    minPx?: number
  ): { boxes: unknown[]; maxDepth: number };

  /** Find a merged node by frame-name path (["(all)", ...]); null if absent. */
  export function nodeAtPath(root: DiffMergedNode, path: readonly string[]): DiffMergedNode | null;

  /**
   * Keep only the allowlisted scope keys (bucket/prefix/service/host/time/
   * poll-band/...) from `params`; drops credentials. Returns fresh params.
   */
  export function fullScopeQuery(params: URLSearchParams): URLSearchParams;

  /** Copy of `scope` with all hosts removed and a single `host` appended. */
  export function scopeWithHost(scope: URLSearchParams, host: string): URLSearchParams;

  /** Copy of `scope` with start_ns/end_ns shifted back by `deltaNs` (BigInt-safe). */
  export function shiftScopeTime(
    scope: URLSearchParams,
    deltaNs: bigint | number | string
  ): URLSearchParams;

  /** base64url-encode one side's scope for a diff link's `a=`/`b=` param. */
  export function encodeScope(query: string | URLSearchParams): string;

  /** Inverse of encodeScope: a base64url blob back to a scope URLSearchParams. */
  export function decodeScope(b64: string): URLSearchParams;

  /**
   * Human label for a poll-duration band from ns bounds (either may be
   * null/empty). "" when neither bound is set.
   */
  export function pollBandLabel(
    minNs: string | number | null | undefined,
    maxNs: string | number | null | undefined
  ): string;

  /** Build the `diff=1&a=..&b=..` query string comparing two scopes. */
  export function diffSearch(
    scopeA: string | URLSearchParams,
    scopeB: string | URLSearchParams
  ): string;

  /** Parse a diff view's location.search into { a, b }, or null when not a diff. */
  export function parseDiff(search: string | URLSearchParams): DiffSides | null;

  /** Decide the page + query a viz button opens (single scope vs captured A/B). */
  export function chooseTarget(
    kind: "flamegraph" | "tokio",
    opts?: {
      hasDiff?: boolean;
      diffA?: string | URLSearchParams;
      diffB?: string | URLSearchParams;
      singleQuery?: string | URLSearchParams;
    }
  ): DiffTarget;

  /** Capture one more scope into the tray (fill A first, then B). */
  export function addDiffCapture(
    state: DiffCaptureState | null,
    scope: DiffScope
  ): DiffCaptureState;

  /** Swap which capture is A vs B (no-op unless both are set). */
  export function swapDiffCapture(state: DiffCaptureState | null): DiffCaptureState;

  /** Remove one side (clearing A promotes B to A). */
  export function removeDiffSide(
    state: DiffCaptureState | null,
    side: "a" | "b"
  ): DiffCaptureState;

  /** Backward time-shift deltas (ns) for the "earlier window" diff presets. */
  export const DIFF_SHIFT_1H: bigint;
  export const DIFF_SHIFT_24H: bigint;
  export const DIFF_SHIFT_7D: bigint;
}
