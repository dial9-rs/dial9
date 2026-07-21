// Typed seam over the frozen two-sided differential-flamegraph core
// (flamegraph_diff.js): the DOM-free logic behind the `?diff=1` branch - tree
// merge, relative-hotness color, per-side layout, the base64url scope-link
// codec (encodeScope/decodeScope/diffSearch/parseDiff), and the poll-band
// label. Pages compose the diff through this re-export instead of importing the
// core module directly.
//
// flamegraph_diff.js is self-contained (only URL/base64 primitives), so it
// needs no global-seed chain like the flamegraph widget does.

export {
  mergeTrees,
  diffColor,
  layoutSide,
  nodeAtPath,
  fullScopeQuery,
  scopeWithHost,
  shiftScopeTime,
  encodeScope,
  decodeScope,
  pollBandLabel,
  diffSearch,
  parseDiff,
  chooseTarget,
  addDiffCapture,
  swapDiffCapture,
  removeDiffSide,
  DIFF_SHIFT_1H,
  DIFF_SHIFT_24H,
  DIFF_SHIFT_7D,
} from "../../../flamegraph_diff.js";
export type {
  DiffScope,
  DiffSides,
  DiffCaptureState,
  DiffMergedNode,
  DiffTarget,
} from "../../../flamegraph_diff.js";
