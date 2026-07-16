// lib/canvas barrel - the shared drawing utilities: layout geometry,
// DPR-aware canvas sizing, pixel-bounded downsampling (fills via the
// frozen core, strokes via ./stroke.ts) and palettes. Canvas components
// consume drawing math through this barrel; only the modules behind it
// import the frozen core's drawing helpers directly.

export {
  LABEL_W,
  laneStackGeometry,
  panelGeometry,
  timePanelLayout,
} from "./layout.js";
export type {
  PanelGeometryOpts,
  TimePanelLayout,
  TimePanelLayoutOpts,
} from "./layout.js";

export { createCanvasSizer, planBackingStore } from "./dpr.js";
export type {
  BackingStorePlan,
  CanvasGeometry,
  CanvasSizer,
  DprCanvas,
  DprTransformContext,
} from "./dpr.js";

export {
  downsampleSeriesToColumns,
  drawStrokeBatches,
  expandSteps,
  makeStrokeBatcher,
} from "./stroke.js";
export type {
  ColumnSeriesOpts,
  StrokeBatch,
  StrokeBatcher,
  StrokePathContext,
  StrokeStyleSpec,
  Vertex,
} from "./stroke.js";

export {
  makeBarCoalescer,
  pixelCoverage,
  pixelDownsampleSpans,
} from "./downsample.js";
export type { BarCoalescer } from "./downsample.js";

export {
  SPAN_COLORS,
  flamegraphColor,
  makeColorAssigner,
  makeColorDimmer,
  pollColor,
  pollHeatmapColor,
  pollHeatmapColorQuantized,
} from "./palette.js";

export { createFlamegraph, filterCpuSamples } from "./flamegraph.js";
export type {
  FlamegraphDataSample,
  FlamegraphInstance,
  FlamegraphSetDataOptions,
  FlamegraphViewState,
} from "./flamegraph.js";

export {
  brushToBand,
  fmtDurationNs,
  histogramLayout,
  normalizeHistogram,
  pxToNs,
  sampleWeightedMedianNs,
} from "./flamegraph_histogram.js";
export type {
  HistogramColumn,
  HistogramLayout,
  PollBand,
  PollHistogramBar,
} from "./flamegraph_histogram.js";

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
} from "./flamegraph_diff.js";
export type {
  DiffScope,
  DiffSides,
  DiffCaptureState,
  DiffMergedNode,
  DiffTarget,
} from "./flamegraph_diff.js";

export {
  createDiffView,
  apiUrlFor,
  scopeLabel,
  isSearchFocusKey,
} from "./flamegraph_diff_view.js";
export type {
  DiffViewOptions,
  DiffViewHandle,
  DiffViewState,
} from "./flamegraph_diff_view.js";

export {
  MAX_OPEN_BYTES,
  MIN_SEGMENT_SECONDS,
  accumulateDensity,
  bootTransitions,
  densityColor,
  groupByHost,
  segmentGaps,
  segmentSpan,
  segmentsOverlapping,
  tileSegments,
  totalBytes,
} from "./heatmap.js";
export type { HostRow, SegmentInput } from "./heatmap.js";
