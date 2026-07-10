// lib/canvas barrel - the shared drawing utilities (T08; architecture
// 2.3): layout geometry, DPR-aware canvas sizing, pixel-bounded
// downsampling (fills via the frozen core, strokes via ./stroke.ts) and
// palettes. Canvas components (components/canvas/*) consume drawing math
// through this barrel; only the modules behind it import the frozen
// core's drawing helpers directly.

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

// flamegraph.ts - the frozen flamegraph widget (canvas rendering, zoom,
// search, tooltip, export menu), typed (T13).
export { createFlamegraph, filterCpuSamples } from "./flamegraph.js";
export type {
  FlamegraphDataSample,
  FlamegraphInstance,
  FlamegraphSetDataOptions,
} from "./flamegraph.js";

// heatmap.ts - the frozen heatmap data helpers (density, host grouping,
// segment math), typed (T14).
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
