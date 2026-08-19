// The trace viewer's shareable URL state.
//
// Projects the store's viewport / selection / uiPrefs into a ViewState so
// bindViewStateToUrl mirrors them into readable query params, and reads them
// back on load. Clock mode + timezone (tm/tz) ride the hash (the ViewState codec
// owns those); everything viewer-specific is a plain query param, so a shared URL
// reads `?start=..&end=..&task=0x..&span-filter=..` rather than an opaque hash
// blob. The viewport is captured as a POSITION (start/end ns): the recipient sees
// the same window but keeps the full trace and can zoom out (distinct from Set
// Range, which reduces the resident data).

import type { ReadonlyState, ViewerStore } from "../../store/store.js";
import type {
  StoreState,
  StoreSliceName,
  PoiSortKey,
  TaskSortKey,
  InspectorTab,
  RegionAnalysisMode,
  FieldChartSpec,
} from "../../types/state.js";
import type { PointOfInterestType } from "../../types/trace.js";
import type { ViewState } from "../../lib/url/index.js";
import {
  DEFAULT_INSPECTOR_WIDTH,
  DEFAULT_LANES_HEIGHT,
  DEFAULT_RAIL_WIDTH,
} from "./store.js";
import { POI_FILTERS } from "./poi.js";
import {
  FIELD_CHART_KINDS,
  FIELD_CHART_URL_SEPARATOR,
  isFieldChartNameSupported,
} from "./field-chart-model.js";
import {
  FIELD_CHART_TRACK_ID_PREFIX,
  isFieldChartTrackId,
} from "../../lib/canvas/track-layout.js";

const P_START = "start";
const P_END = "end";
const P_TASK = "task";
const P_SPAN_FILTER = "span-filter";
const P_TRACK_ORDER = "track-order";
const P_COLLAPSED = "collapsed";
const P_FIELD_CHART = "field-chart";
const P_SPAN = "span";
const P_SPAN_FOCUS = "span-focus";
const P_POLL = "poll";
const P_TASK_DUMP = "task-dump";
const P_EVENT = "event";
const P_REGION = "region";
const P_SPAWNED = "spawned";
const P_ISSUE = "issue";
const P_ISSUE_SORT = "issue-sort";
const P_ISSUE_INDEX = "issue-index";
const P_SPAN_PCT = "span-pct";
const P_SPAN_NAMES = "span-names";
const P_EVENT_NAMES = "event-names";
const P_RAIL_TAB = "rail";
const P_TASK_SORT = "task-sort";
const P_TASK_INDEX = "task-index";
const P_TASK_COLS = "task-cols";
const P_ISSUE_COLS = "issue-cols";
const P_RUNTIME_COLLAPSED = "runtime-collapsed";
const P_RUNTIME_METRICS_COLLAPSED = "runtime-metrics-collapsed";
const P_INSPECTOR_WIDTH = "inspector-width";
const P_RAIL_WIDTH = "rail-width";
const P_LANES_HEIGHT = "lanes-height";
const P_LANES_SCROLL = "lanes-scroll";
const P_STACK_VIEW = "stack-view";
const P_INSPECTOR_TAB = "inspector";
const P_POLL_SECTION = "poll-section";
const P_POLL_EXPANDED = "poll-expanded";
const P_POLL_WORKER_ZOOM = "poll-worker-zoom";
const P_POLL_OFFWORKER_ZOOM = "poll-offworker-zoom";
const P_RELATED_COLLAPSED = "related-collapsed";
const P_RELATED_EXPAND = "related-expand";
const P_RELATED_KEY = "related-key";
const P_RELATED_VALUE = "related-value";
const P_ANALYSIS = "analysis";
const P_HEAP_WEIGHT = "heap-weight";
const P_BLOCKING_GROUP = "blocking-group";
const P_ANALYSIS_WORKER_ZOOM = "analysis-worker-zoom";
const P_ANALYSIS_OFFWORKER_ZOOM = "analysis-offworker-zoom";
const P_ANALYSIS_INSPECT = "analysis-inspect";
const P_SPAN_INDEX = "span-index";
const P_DATA_START = "data-start";
const P_DATA_END = "data-end";

/** Valid rail sort keys (drop anything else on read). */
const POI_SORT_KEYS: readonly PoiSortKey[] = ["worker", "kind", "time", "duration"];
/** Valid span percentile-filter steps (0/All is the default, never emitted). */
const SPAN_PCTS: readonly number[] = [50, 90, 95, 99];

const TASK_SORT_KEYS: readonly TaskSortKey[] = ["id", "loc", "polls", "total", "longest", "lifetime"];
/** Valid issue-cols keys: the severity-dot column plus the sortable four. */
const ISSUE_COL_KEYS: readonly string[] = ["dot", ...POI_SORT_KEYS];
const INSPECTOR_TABS: readonly InspectorTab[] = ["task", "poll", "event", "related", "stack"];
const REGION_MODES: readonly RegionAnalysisMode[] = ["cpu", "blocking", "heap"];

type FieldOwnership =
  | { readonly kind: "url"; readonly params: readonly string[] }
  | { readonly kind: "derived" | "source" | "transient" | "retired" };
type StateOwnership = {
  readonly [S in keyof StoreState]: {
    readonly [F in keyof StoreState[S]]: FieldOwnership;
  };
};

const url = (...params: string[]): FieldOwnership => ({ kind: "url", params });
const derived: FieldOwnership = { kind: "derived" };
const source: FieldOwnership = { kind: "source" };
const transient: FieldOwnership = { kind: "transient" };
const retired: FieldOwnership = { kind: "retired" };

/**
 * Exhaustive ownership gate for the viewer store. Adding any store field fails
 * TypeScript until the author classifies it here. Durable fields must name
 * their URL keys; non-durable fields must state why they are excluded.
 */
export const VIEWER_STATE_OWNERSHIP = {
  trace: {
    trace: url(P_DATA_START, P_DATA_END),
  },
  viewport: {
    viewStart: url(P_START),
    viewEnd: url(P_END),
    minTs: derived,
    maxTs: derived,
  },
  selection: {
    selectedTaskId: url(P_TASK),
    spanFocus: url(P_SPAN),
    focusedSpanId: url(P_SPAN_FOCUS),
    pinnedEvent: url(P_EVENT),
    pollDetail: url(P_POLL),
    taskDump: url(P_TASK_DUMP),
    sidebarRange: url(P_REGION),
    hoveredWakerTaskId: transient,
    spawnedTasksRange: url(P_SPAWNED),
  },
  poi: {
    filter: url(P_ISSUE),
    sortKey: url(P_ISSUE_SORT),
    sortDir: url(P_ISSUE_SORT),
    index: url(P_ISSUE_INDEX),
    railTab: url(P_RAIL_TAB),
    taskSort: url(P_TASK_SORT),
    taskSortDir: url(P_TASK_SORT),
    taskIndex: url(P_TASK_INDEX),
  },
  uiPrefs: {
    panelCollapsed: retired,
    trackOrder: url(P_TRACK_ORDER),
    collapsed: url(P_COLLAPSED),
    collapsedRuntimes: url(P_RUNTIME_COLLAPSED),
    collapsedRuntimeMetrics: url(P_RUNTIME_METRICS_COLLAPSED),
    sidebarWidth: url(P_INSPECTOR_WIDTH),
    railWidth: url(P_RAIL_WIDTH),
    taskColWidths: url(P_TASK_COLS),
    issueColWidths: url(P_ISSUE_COLS),
    lanesViewportHeight: url(P_LANES_HEIGHT),
    lanesScrollTop: url(P_LANES_SCROLL),
    selectedSpanNames: url(P_SPAN_NAMES),
    selectedEventNames: url(P_EVENT_NAMES),
    spanFilter: url(P_SPAN_FILTER),
    spanPctFilter: url(P_SPAN_PCT),
    timeMode: url("#tm"),
    tz: url("#tz"),
    stacksAsFlamegraph: url(P_STACK_VIEW),
  },
  view: {
    fieldCharts: url(P_FIELD_CHART),
    inspectorTab: url(P_INSPECTOR_TAB),
    expandedPollGroups: url(P_POLL_EXPANDED),
    pollFlamegraphSection: url(P_POLL_SECTION),
    pollWorkerZoom: url(P_POLL_WORKER_ZOOM),
    pollOffworkerZoom: url(P_POLL_OFFWORKER_ZOOM),
    relatedCollapsed: url(P_RELATED_COLLAPSED),
    relatedExpand: url(P_RELATED_EXPAND),
    relatedCorrelate: url(P_RELATED_KEY, P_RELATED_VALUE),
    regionMode: url(P_ANALYSIS),
    regionHeapMode: url(P_HEAP_WEIGHT),
    regionGroupBy: url(P_BLOCKING_GROUP),
    regionWorkerZoom: url(P_ANALYSIS_WORKER_ZOOM),
    regionOffworkerZoom: url(P_ANALYSIS_OFFWORKER_ZOOM),
    regionInspectFocus: url(P_ANALYSIS_INSPECT),
    spanNavIndex: url(P_SPAN_INDEX),
  },
  transient: {
    mouseNs: transient,
    hoverEventTs: transient,
    drag: transient,
    keyboardSelection: transient,
    atCursor: transient,
  },
  segments: {
    segments: source,
  },
} satisfies StateOwnership;

function urlOwnedSlices(): StoreSliceName[] {
  return (Object.keys(VIEWER_STATE_OWNERSHIP) as StoreSliceName[]).filter(
    (slice) =>
      Object.values(VIEWER_STATE_OWNERSHIP[slice]).some(
        (field) => field.kind === "url",
      ),
  );
}

/** Store slices that can change the shareable analytical view. */
export const VIEWER_URL_SLICES: readonly StoreSliceName[] = urlOwnedSlices();

/** Stable readable query vocabulary owned by `/viewer.html`. */
export const VIEWER_VIEW_QUERY_PARAMS: readonly string[] = [
  ...new Set(
    Object.values(VIEWER_STATE_OWNERSHIP)
      .flatMap((slice) => Object.values(slice))
      .flatMap((field) => (field.kind === "url" ? field.params : []))
      .filter((param) => !param.startsWith("#")),
  ),
];

/** Project the store into the shareable ViewState. */
export function projectViewerState(state: ReadonlyState<StoreState>): ViewState {
  const vs: ViewState = {};
  // tm/tz ride the hash; include them only when non-default so a pristine
  // full-view URL stays clean (`?trace=...` with no hash).
  if (state.uiPrefs.timeMode !== "rel") vs.timeMode = state.uiPrefs.timeMode;
  if (state.uiPrefs.tz !== "utc") vs.timeZone = state.uiPrefs.tz;
  // Viewport only when zoomed in from the full trace extent (a pristine
  // full-view URL stays clean).
  const vp = state.viewport;
  if (
    vp.maxTs > vp.minTs &&
    (vp.viewStart > vp.minTs || vp.viewEnd < vp.maxTs)
  ) {
    vs.viewStart = vp.viewStart;
    vs.viewEnd = vp.viewEnd;
  }
  const sel = state.selection;
  if (sel.selectedTaskId !== null) vs.selectedTaskId = sel.selectedTaskId;
  if (sel.spanFocus !== null) vs.selectedSpanId = sel.spanFocus.spanId;
  if (sel.pollDetail !== null) {
    vs.pollAnchor = `${sel.pollDetail.start}:${sel.pollDetail.taskId}`;
  }
  if (sel.taskDump !== null && sel.taskDump.timestamps.length > 0) {
    vs.taskDumpAnchor =
      `${sel.taskDump.taskId}:${sel.taskDump.timestamps.join(",")}`;
  }
  if (sel.pinnedEvent !== null) vs.pinnedEventTs = sel.pinnedEvent.timestamp;
  if (sel.sidebarRange !== null) {
    vs.sidebarRange = `${sel.sidebarRange.startNs}-${sel.sidebarRange.endNs}`;
  }
  if (sel.spawnedTasksRange !== null) {
    vs.spawnedRange = `${sel.spawnedTasksRange.startNs}-${sel.spawnedTasksRange.endNs}`;
  }
  if (state.uiPrefs.spanFilter !== "") vs.spanFilter = state.uiPrefs.spanFilter;
  if (state.uiPrefs.trackOrder.length > 0) {
    vs.trackOrder = state.uiPrefs.trackOrder;
  }
  const collapsed = Object.keys(state.uiPrefs.collapsed).filter(
    (id) => state.uiPrefs.collapsed[id] === true,
  ).sort();
  if (collapsed.length > 0) vs.collapsed = collapsed;
  if (state.view.fieldCharts.length > 0) {
    vs.fieldCharts = state.view.fieldCharts;
  }
  if (sel.focusedSpanId !== null) vs.focusedSpanId = sel.focusedSpanId;
  // Issues rail: only the deltas from the resting defaults (filter "sched",
  // sort duration/desc, no current POI) so a pristine rail keeps the URL clean.
  const poi = state.poi;
  if (poi.filter !== "sched") vs.poiFilter = poi.filter;
  if (poi.sortKey !== "duration" || poi.sortDir !== "desc") {
    vs.poiSort = `${poi.sortKey},${poi.sortDir}`;
  }
  if (poi.index >= 0) vs.poiIndex = poi.index;
  if (state.uiPrefs.spanPctFilter !== 0) vs.spanPct = state.uiPrefs.spanPctFilter;
  if (state.uiPrefs.selectedSpanNames.size > 0) {
    vs.spanNames = [...state.uiPrefs.selectedSpanNames].sort();
  }
  if (state.uiPrefs.selectedEventNames.size > 0) {
    vs.eventNames = [...state.uiPrefs.selectedEventNames].sort();
  }
  if (poi.railTab !== "issues") vs.railTab = poi.railTab;
  if (poi.taskSort !== "total" || poi.taskSortDir !== "desc") {
    vs.taskSort = `${poi.taskSort},${poi.taskSortDir}`;
  }
  if (poi.taskIndex >= 0) vs.taskIndex = poi.taskIndex;

  const runtimeCollapsed = Object.keys(state.uiPrefs.collapsedRuntimes)
    .filter((name) => state.uiPrefs.collapsedRuntimes[name] === true)
    .sort();
  if (runtimeCollapsed.length > 0) vs.collapsedRuntimes = runtimeCollapsed;
  const runtimeMetricsCollapsed = Object.keys(state.uiPrefs.collapsedRuntimeMetrics)
    .filter((name) => state.uiPrefs.collapsedRuntimeMetrics[name] === true)
    .sort();
  if (runtimeMetricsCollapsed.length > 0) {
    vs.collapsedRuntimeMetrics = runtimeMetricsCollapsed;
  }
  if (state.uiPrefs.sidebarWidth !== DEFAULT_INSPECTOR_WIDTH) {
    vs.inspectorWidth = state.uiPrefs.sidebarWidth;
  }
  if (state.uiPrefs.railWidth !== DEFAULT_RAIL_WIDTH) {
    vs.railWidth = state.uiPrefs.railWidth;
  }
  const taskColEntries = sortedWidthEntries(state.uiPrefs.taskColWidths);
  if (taskColEntries.length > 0) {
    vs.taskColWidths = Object.fromEntries(taskColEntries);
  }
  const issueColEntries = sortedWidthEntries(state.uiPrefs.issueColWidths);
  if (issueColEntries.length > 0) {
    vs.issueColWidths = Object.fromEntries(issueColEntries);
  }
  if (state.uiPrefs.lanesViewportHeight !== DEFAULT_LANES_HEIGHT) {
    vs.lanesHeight = state.uiPrefs.lanesViewportHeight;
  }
  if (state.uiPrefs.lanesScrollTop > 0) vs.lanesScrollTop = state.uiPrefs.lanesScrollTop;
  if (state.uiPrefs.stacksAsFlamegraph) vs.stackView = "flame";

  const view = state.view;
  const inferredInspectorTab: InspectorTab =
    sel.pollDetail !== null
      ? "poll"
      : sel.pinnedEvent !== null
        ? "event"
        : sel.taskDump !== null ||
            sel.spawnedTasksRange !== null ||
            sel.sidebarRange !== null
          ? "stack"
          : "task";
  if (view.inspectorTab !== inferredInspectorTab) vs.inspectorTab = view.inspectorTab;
  if (view.pollFlamegraphSection !== "cpu") vs.pollSection = view.pollFlamegraphSection;
  if (view.expandedPollGroups.size > 0) {
    vs.expandedPollGroups = [...view.expandedPollGroups].sort();
  }
  if (view.pollWorkerZoom.length > 0) vs.pollWorkerZoom = view.pollWorkerZoom;
  if (view.pollOffworkerZoom.length > 0) vs.pollOffworkerZoom = view.pollOffworkerZoom;
  const relatedCollapsed = Object.keys(view.relatedCollapsed)
    .filter((title) => view.relatedCollapsed[title] === true)
    .sort();
  if (relatedCollapsed.length > 0) vs.relatedCollapsed = relatedCollapsed;
  const relatedExpand = Object.entries(view.relatedExpand)
    .filter(([, amount]) => amount.before > 0 || amount.after > 0)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([title, amount]) => `${title}\t${amount.before}\t${amount.after}`);
  if (relatedExpand.length > 0) vs.relatedExpand = relatedExpand;
  if (view.relatedCorrelate !== null) {
    vs.relatedCorrelateKey = view.relatedCorrelate.key;
    vs.relatedCorrelateValue = view.relatedCorrelate.val;
  }
  if (view.regionMode !== null) vs.regionMode = view.regionMode;
  if (view.regionHeapMode !== "bytes") vs.regionHeapMode = view.regionHeapMode;
  if (view.regionGroupBy !== "leaf") vs.regionGroupBy = view.regionGroupBy;
  if (view.regionWorkerZoom.length > 0) vs.regionWorkerZoom = view.regionWorkerZoom;
  if (view.regionOffworkerZoom.length > 0) vs.regionOffworkerZoom = view.regionOffworkerZoom;
  if (view.regionInspectFocus !== null) {
    vs.regionInspectFocus = view.regionInspectFocus;
  }
  if (view.spanNavIndex >= 0) vs.spanNavIndex = view.spanNavIndex;

  const trace = state.trace.trace;
  if (trace !== null) {
    if (trace.filterStartTime != null && Number.isFinite(trace.filterStartTime)) {
      vs.dataStart = trace.filterStartTime;
    }
    if (trace.filterEndTime != null && Number.isFinite(trace.filterEndTime)) {
      vs.dataEnd = trace.filterEndTime;
    }
  }
  return vs;
}

/** Mirror the viewer fields of `vs` into readable query params (set/delete). */
export function mirrorViewerToQuery(
  params: URLSearchParams,
  vs: ViewState,
): void {
  set(params, P_START, vs.viewStart != null ? String(Math.round(vs.viewStart)) : null);
  set(params, P_END, vs.viewEnd != null ? String(Math.round(vs.viewEnd)) : null);
  set(params, P_TASK, vs.selectedTaskId != null ? `0x${vs.selectedTaskId.toString(16)}` : null);
  set(params, P_SPAN_FILTER, vs.spanFilter && vs.spanFilter.length > 0 ? vs.spanFilter : null);
  set(params, P_TRACK_ORDER, vs.trackOrder && vs.trackOrder.length > 0 ? vs.trackOrder.join(",") : null);
  set(params, P_COLLAPSED, vs.collapsed && vs.collapsed.length > 0 ? vs.collapsed.join(",") : null);
  params.delete(P_FIELD_CHART);
  for (const chart of vs.fieldCharts ?? []) {
    const encoded = encodeFieldChart(chart);
    if (encoded !== null) params.append(P_FIELD_CHART, encoded);
  }
  set(params, P_SPAN, vs.selectedSpanId ?? null);
  set(params, P_SPAN_FOCUS, vs.focusedSpanId ?? null);
  set(params, P_POLL, vs.pollAnchor ?? null);
  set(params, P_TASK_DUMP, vs.taskDumpAnchor ?? null);
  set(params, P_EVENT, vs.pinnedEventTs != null ? String(Math.round(vs.pinnedEventTs)) : null);
  set(params, P_REGION, vs.sidebarRange ?? null);
  set(params, P_SPAWNED, vs.spawnedRange ?? null);
  set(params, P_ISSUE, vs.poiFilter ?? null);
  set(params, P_ISSUE_SORT, vs.poiSort ?? null);
  set(params, P_ISSUE_INDEX, vs.poiIndex != null ? String(Math.round(vs.poiIndex)) : null);
  set(params, P_SPAN_PCT, vs.spanPct != null ? String(vs.spanPct) : null);
  set(params, P_SPAN_NAMES, encodeList(vs.spanNames));
  set(params, P_EVENT_NAMES, encodeList(vs.eventNames));
  set(params, P_RAIL_TAB, vs.railTab ?? null);
  set(params, P_TASK_SORT, vs.taskSort ?? null);
  set(params, P_TASK_INDEX, finiteString(vs.taskIndex));
  set(params, P_RUNTIME_COLLAPSED, encodeList(vs.collapsedRuntimes));
  set(params, P_RUNTIME_METRICS_COLLAPSED, encodeList(vs.collapsedRuntimeMetrics));
  set(params, P_INSPECTOR_WIDTH, finiteString(vs.inspectorWidth));
  set(params, P_RAIL_WIDTH, finiteString(vs.railWidth));
  set(params, P_TASK_COLS, encodeWidthMap(vs.taskColWidths));
  set(params, P_ISSUE_COLS, encodeWidthMap(vs.issueColWidths));
  set(params, P_LANES_HEIGHT, finiteString(vs.lanesHeight));
  set(params, P_LANES_SCROLL, finiteString(vs.lanesScrollTop));
  set(params, P_STACK_VIEW, vs.stackView ?? null);
  set(params, P_INSPECTOR_TAB, vs.inspectorTab ?? null);
  set(params, P_POLL_SECTION, vs.pollSection ?? null);
  set(params, P_POLL_EXPANDED, encodeList(vs.expandedPollGroups));
  set(params, P_POLL_WORKER_ZOOM, encodePath(vs.pollWorkerZoom));
  set(params, P_POLL_OFFWORKER_ZOOM, encodePath(vs.pollOffworkerZoom));
  set(params, P_RELATED_COLLAPSED, encodeList(vs.relatedCollapsed));
  set(params, P_RELATED_EXPAND, encodeEntries(vs.relatedExpand));
  set(params, P_RELATED_KEY, vs.relatedCorrelateKey ?? null);
  set(params, P_RELATED_VALUE, vs.relatedCorrelateValue ?? null);
  set(params, P_ANALYSIS, vs.regionMode ?? null);
  set(params, P_HEAP_WEIGHT, vs.regionHeapMode ?? null);
  set(params, P_BLOCKING_GROUP, vs.regionGroupBy ?? null);
  set(params, P_ANALYSIS_WORKER_ZOOM, encodePath(vs.regionWorkerZoom));
  set(params, P_ANALYSIS_OFFWORKER_ZOOM, encodePath(vs.regionOffworkerZoom));
  set(params, P_ANALYSIS_INSPECT, vs.regionInspectFocus ?? null);
  set(params, P_SPAN_INDEX, finiteString(vs.spanNavIndex));
  set(params, P_DATA_START, finiteString(vs.dataStart));
  set(params, P_DATA_END, finiteString(vs.dataEnd));
}

/**
 * Encode arbitrary labels without delimiter ambiguity. `v1:` distinguishes the
 * modern TAB-separated grammar from the legacy comma/pre-encoded form. Values
 * are passed to URLSearchParams unescaped so they are encoded exactly once.
 */
function encodeList(values?: readonly string[]): string | null {
  return values !== undefined && values.length > 0 ? `v1:${values.join("\t")}` : null;
}

/** The defined entries of a column-width map, sorted by key for a stable URL. */
function sortedWidthEntries(
  widths: Readonly<Partial<Record<string, number>>>,
): [string, number][] {
  return Object.entries(widths)
    .filter((entry): entry is [string, number] => typeof entry[1] === "number")
    .sort(([a], [b]) => a.localeCompare(b));
}

/** Encode a column-width map as a v1 list of `<key>,<px>` entries. */
function encodeWidthMap(widths?: Record<string, number>): string | null {
  return encodeList(
    widths !== undefined
      ? sortedWidthEntries(widths).map(([key, px]) => `${key},${px}`)
      : undefined,
  );
}

/** Structured Related entries already contain TAB fields, so separate entries
 * with newline under the same modern marker. */
function encodeEntries(values?: readonly string[]): string | null {
  return values !== undefined && values.length > 0 ? `v1:${values.join("\n")}` : null;
}

function encodePath(path?: readonly string[]): string | null {
  return path !== undefined && path.length > 0 ? path.join("\t") : null;
}

function finiteString(value?: number): string | null {
  return value !== undefined && Number.isFinite(value) ? String(Math.round(value)) : null;
}

function set(params: URLSearchParams, key: string, value: string | null): void {
  if (value === null) params.delete(key);
  else params.set(key, value);
}

/** The viewer state parsed from a URL query string. */
export interface ViewerUrlState {
  /** Restore-on-load viewport window (ns), applied to the first loaded trace. */
  viewStart?: number;
  viewEnd?: number;
  selectedTaskId?: number;
  spanFilter?: string;
  trackOrder?: string[];
  collapsed?: string[];
  fieldCharts?: FieldChartSpec[];
  /** Canvas-selection anchors, re-resolved against the loaded trace on load. */
  selectedSpanId?: string;
  focusedSpanId?: string;
  poll?: { startNs: number; taskId: number };
  taskDump?: { taskId: number; timestamps: number[] };
  pinnedEventTs?: number;
  sidebarRange?: { startNs: number; endNs: number };
  spawnedRange?: { startNs: number; endNs: number };
  /** Issues-rail restore (applied at boot). */
  poiFilter?: PointOfInterestType;
  poiSort?: { key: PoiSortKey; dir: "asc" | "desc" };
  poiIndex?: number;
  spanPct?: number;
  spanNames?: string[];
  eventNames?: string[];
  railTab?: "issues" | "tasks";
  taskSort?: { key: TaskSortKey; dir: "asc" | "desc" };
  taskIndex?: number;
  collapsedRuntimes?: string[];
  collapsedRuntimeMetrics?: string[];
  inspectorWidth?: number;
  railWidth?: number;
  taskColWidths?: Record<string, number>;
  issueColWidths?: Record<string, number>;
  lanesHeight?: number;
  lanesScrollTop?: number;
  stacksAsFlamegraph?: boolean;
  inspectorTab?: InspectorTab;
  pollSection?: "cpu" | "sched";
  expandedPollGroups?: string[];
  pollWorkerZoom?: string[];
  pollOffworkerZoom?: string[];
  relatedCollapsed?: string[];
  relatedExpand?: Record<string, { before: number; after: number }>;
  relatedCorrelate?: { key: string; val: string };
  regionMode?: RegionAnalysisMode;
  regionHeapMode?: "bytes" | "count";
  regionGroupBy?: "leaf" | "full";
  regionWorkerZoom?: string[];
  regionOffworkerZoom?: string[];
  regionInspectFocus?: string;
  spanNavIndex?: number;
  dataRange?: { startNs?: number; endNs?: number };
}

/**
 * Apply every trace-independent URL field to the store before components
 * mount. Trace-dependent semantic anchors are resolved later by
 * resolveUrlSelection, after the first trace exists.
 */
export function hydrateViewerStore(
  store: ViewerStore,
  urlView: ViewerUrlState,
  sharedView: ViewState,
): void {
  const uiPrefs: Partial<StoreState["uiPrefs"]> = {};
  if (sharedView.timeMode !== undefined) uiPrefs.timeMode = sharedView.timeMode;
  if (sharedView.timeZone !== undefined) uiPrefs.tz = sharedView.timeZone;
  if (urlView.spanFilter !== undefined) uiPrefs.spanFilter = urlView.spanFilter;
  if (urlView.trackOrder !== undefined) uiPrefs.trackOrder = urlView.trackOrder;
  if (urlView.collapsed !== undefined) {
    uiPrefs.collapsed = Object.fromEntries(
      urlView.collapsed.map((id) => [id, true]),
    );
  }
  if (urlView.spanPct !== undefined) uiPrefs.spanPctFilter = urlView.spanPct;
  if (urlView.spanNames !== undefined) {
    uiPrefs.selectedSpanNames = new Set(urlView.spanNames);
  }
  if (urlView.eventNames !== undefined) {
    uiPrefs.selectedEventNames = new Set(urlView.eventNames);
  }
  if (urlView.collapsedRuntimes !== undefined) {
    uiPrefs.collapsedRuntimes = Object.fromEntries(
      urlView.collapsedRuntimes.map((name) => [name, true]),
    );
  }
  if (urlView.collapsedRuntimeMetrics !== undefined) {
    uiPrefs.collapsedRuntimeMetrics = Object.fromEntries(
      urlView.collapsedRuntimeMetrics.map((name) => [name, true]),
    );
  }
  if (urlView.inspectorWidth !== undefined) {
    uiPrefs.sidebarWidth = urlView.inspectorWidth;
  }
  if (urlView.railWidth !== undefined) {
    uiPrefs.railWidth = urlView.railWidth;
  }
  if (urlView.taskColWidths !== undefined) {
    uiPrefs.taskColWidths = urlView.taskColWidths;
  }
  if (urlView.issueColWidths !== undefined) {
    uiPrefs.issueColWidths = urlView.issueColWidths;
  }
  if (urlView.lanesHeight !== undefined) {
    uiPrefs.lanesViewportHeight = urlView.lanesHeight;
  }
  if (urlView.lanesScrollTop !== undefined) {
    uiPrefs.lanesScrollTop = urlView.lanesScrollTop;
  }
  if (urlView.stacksAsFlamegraph !== undefined) {
    uiPrefs.stacksAsFlamegraph = urlView.stacksAsFlamegraph;
  }
  if (Object.keys(uiPrefs).length > 0) store.update("uiPrefs", uiPrefs);

  const view: Partial<StoreState["view"]> = {};
  if (urlView.fieldCharts !== undefined) {
    view.fieldCharts = urlView.fieldCharts;
  }
  if (urlView.inspectorTab !== undefined) view.inspectorTab = urlView.inspectorTab;
  if (urlView.pollSection !== undefined) {
    view.pollFlamegraphSection = urlView.pollSection;
  }
  if (urlView.expandedPollGroups !== undefined) {
    view.expandedPollGroups = new Set(urlView.expandedPollGroups);
  }
  if (urlView.pollWorkerZoom !== undefined) {
    view.pollWorkerZoom = urlView.pollWorkerZoom;
  }
  if (urlView.pollOffworkerZoom !== undefined) {
    view.pollOffworkerZoom = urlView.pollOffworkerZoom;
  }
  if (urlView.relatedCollapsed !== undefined) {
    view.relatedCollapsed = Object.fromEntries(
      urlView.relatedCollapsed.map((title) => [title, true]),
    );
  }
  if (urlView.relatedExpand !== undefined) {
    view.relatedExpand = urlView.relatedExpand;
  }
  if (urlView.relatedCorrelate !== undefined) {
    view.relatedCorrelate = urlView.relatedCorrelate;
  }
  if (urlView.regionMode !== undefined) view.regionMode = urlView.regionMode;
  if (urlView.regionHeapMode !== undefined) {
    view.regionHeapMode = urlView.regionHeapMode;
  }
  if (urlView.regionGroupBy !== undefined) {
    view.regionGroupBy = urlView.regionGroupBy;
  }
  if (urlView.regionWorkerZoom !== undefined) {
    view.regionWorkerZoom = urlView.regionWorkerZoom;
  }
  if (urlView.regionOffworkerZoom !== undefined) {
    view.regionOffworkerZoom = urlView.regionOffworkerZoom;
  }
  if (urlView.regionInspectFocus !== undefined) {
    view.regionInspectFocus = urlView.regionInspectFocus;
  }
  if (urlView.spanNavIndex !== undefined) {
    view.spanNavIndex = urlView.spanNavIndex;
  }
  if (Object.keys(view).length > 0) store.update("view", view);

  const poi: Partial<StoreState["poi"]> = {};
  if (urlView.poiFilter !== undefined) poi.filter = urlView.poiFilter;
  if (urlView.poiSort !== undefined) {
    poi.sortKey = urlView.poiSort.key;
    poi.sortDir = urlView.poiSort.dir;
  }
  if (urlView.poiIndex !== undefined) poi.index = urlView.poiIndex;
  if (urlView.railTab !== undefined) poi.railTab = urlView.railTab;
  if (urlView.taskSort !== undefined) {
    poi.taskSort = urlView.taskSort.key;
    poi.taskSortDir = urlView.taskSort.dir;
  }
  if (urlView.taskIndex !== undefined) poi.taskIndex = urlView.taskIndex;
  if (Object.keys(poi).length > 0) store.update("poi", poi);
}

/** Read the viewer fields from a URL query string. */
export function readViewerUrlState(search: string): ViewerUrlState {
  const p = new URLSearchParams(search);
  const out: ViewerUrlState = {};
  const start = num(p.get(P_START));
  const end = num(p.get(P_END));
  if (start != null && end != null && end > start) {
    out.viewStart = start;
    out.viewEnd = end;
  }
  const task = intMaybeHex(p.get(P_TASK));
  if (task != null) out.selectedTaskId = task;
  const sf = p.get(P_SPAN_FILTER);
  if (sf != null && sf.length > 0) out.spanFilter = sf;
  const ord = p.get(P_TRACK_ORDER);
  if (ord != null && ord.length > 0) {
    out.trackOrder = ord.split(",").filter((s) => s.length > 0);
  }
  const col = p.get(P_COLLAPSED);
  if (col != null && col.length > 0) {
    out.collapsed = col.split(",").filter((s) => s.length > 0);
  }
  const fieldCharts: FieldChartSpec[] = [];
  const fieldChartIds = new Set<string>();
  for (const value of p.getAll(P_FIELD_CHART)) {
    const chart = decodeFieldChart(value);
    if (chart !== null && !fieldChartIds.has(chart.id)) {
      fieldCharts.push(chart);
      fieldChartIds.add(chart.id);
    }
  }
  if (fieldCharts.length > 0) out.fieldCharts = fieldCharts;
  const span = p.get(P_SPAN);
  if (span != null && span.length > 0) out.selectedSpanId = span;
  const spanFocus = p.get(P_SPAN_FOCUS);
  if (spanFocus != null && spanFocus.length > 0) out.focusedSpanId = spanFocus;
  const poll = p.get(P_POLL);
  if (poll != null) {
    const colon = poll.indexOf(":");
    const startNs = num(colon > 0 ? poll.slice(0, colon) : poll);
    const taskId = colon > 0 ? nonNegativeInt(poll.slice(colon + 1)) : null;
    if (startNs != null && taskId != null) out.poll = { startNs, taskId };
  }
  const taskDump = taskDumpAnchor(p.get(P_TASK_DUMP));
  if (taskDump !== null) out.taskDump = taskDump;
  const event = num(p.get(P_EVENT));
  if (event != null) out.pinnedEventTs = event;
  const region = rangePair(p.get(P_REGION));
  if (region != null) out.sidebarRange = region;
  const spawned = rangePair(p.get(P_SPAWNED));
  if (spawned != null) out.spawnedRange = spawned;
  const issue = p.get(P_ISSUE);
  if (issue != null && (POI_FILTERS as readonly string[]).includes(issue)) {
    out.poiFilter = issue as PointOfInterestType;
  }
  const issueSort = p.get(P_ISSUE_SORT);
  if (issueSort != null) {
    const comma = issueSort.indexOf(",");
    const key = comma > 0 ? issueSort.slice(0, comma) : "";
    const dir = comma > 0 ? issueSort.slice(comma + 1) : "";
    if ((POI_SORT_KEYS as readonly string[]).includes(key) && (dir === "asc" || dir === "desc")) {
      out.poiSort = { key: key as PoiSortKey, dir };
    }
  }
  const issueIndex = nonNegativeInt(p.get(P_ISSUE_INDEX));
  if (issueIndex != null) out.poiIndex = issueIndex;
  const spanPct = num(p.get(P_SPAN_PCT));
  if (spanPct != null && SPAN_PCTS.includes(spanPct)) out.spanPct = spanPct;
  const spanNames = decodeList(p.get(P_SPAN_NAMES));
  if (spanNames != null) out.spanNames = spanNames;
  const eventNames = decodeList(p.get(P_EVENT_NAMES));
  if (eventNames != null) out.eventNames = eventNames;

  const rail = p.get(P_RAIL_TAB);
  if (rail === "issues" || rail === "tasks") out.railTab = rail;
  const taskSort = sortPair<TaskSortKey>(p.get(P_TASK_SORT), TASK_SORT_KEYS);
  if (taskSort !== null) out.taskSort = taskSort;
  const taskIndex = nonNegativeInt(p.get(P_TASK_INDEX));
  if (taskIndex !== null) out.taskIndex = taskIndex;
  const collapsedRuntimes = decodeList(p.get(P_RUNTIME_COLLAPSED));
  if (collapsedRuntimes !== null) out.collapsedRuntimes = collapsedRuntimes;
  const collapsedRuntimeMetrics = decodeList(p.get(P_RUNTIME_METRICS_COLLAPSED));
  if (collapsedRuntimeMetrics !== null) {
    out.collapsedRuntimeMetrics = collapsedRuntimeMetrics;
  }
  const inspectorWidth = positiveInt(p.get(P_INSPECTOR_WIDTH));
  if (inspectorWidth !== null) out.inspectorWidth = inspectorWidth;
  const railWidth = positiveInt(p.get(P_RAIL_WIDTH));
  if (railWidth !== null) out.railWidth = railWidth;
  const taskCols = decodeWidthMap(
    p.get(P_TASK_COLS),
    TASK_SORT_KEYS as readonly string[],
  );
  if (taskCols !== null) out.taskColWidths = taskCols;
  const issueCols = decodeWidthMap(p.get(P_ISSUE_COLS), ISSUE_COL_KEYS);
  if (issueCols !== null) out.issueColWidths = issueCols;
  const lanesHeight = positiveInt(p.get(P_LANES_HEIGHT));
  if (lanesHeight !== null) out.lanesHeight = lanesHeight;
  const lanesScrollTop = nonNegativeInt(p.get(P_LANES_SCROLL));
  if (lanesScrollTop !== null) out.lanesScrollTop = lanesScrollTop;
  const stackView = p.get(P_STACK_VIEW);
  if (stackView === "list" || stackView === "flame") {
    out.stacksAsFlamegraph = stackView === "flame";
  }
  const inspectorTab = p.get(P_INSPECTOR_TAB);
  if (inspectorTab !== null && (INSPECTOR_TABS as readonly string[]).includes(inspectorTab)) {
    out.inspectorTab = inspectorTab as InspectorTab;
  }
  const pollSection = p.get(P_POLL_SECTION);
  if (pollSection === "cpu" || pollSection === "sched") out.pollSection = pollSection;
  const expandedPollGroups = decodeList(p.get(P_POLL_EXPANDED));
  if (expandedPollGroups !== null) out.expandedPollGroups = expandedPollGroups;
  const pollWorkerZoom = decodePath(p.get(P_POLL_WORKER_ZOOM));
  if (pollWorkerZoom !== null) out.pollWorkerZoom = pollWorkerZoom;
  const pollOffworkerZoom = decodePath(p.get(P_POLL_OFFWORKER_ZOOM));
  if (pollOffworkerZoom !== null) out.pollOffworkerZoom = pollOffworkerZoom;
  const relatedCollapsed = decodeList(p.get(P_RELATED_COLLAPSED));
  if (relatedCollapsed !== null) out.relatedCollapsed = relatedCollapsed;
  const relatedExpandEntries = decodeEntries(p.get(P_RELATED_EXPAND));
  if (relatedExpandEntries !== null) {
    const expanded: Record<string, { before: number; after: number }> = {};
    for (const entry of relatedExpandEntries) {
      const [title, beforeRaw, afterRaw, ...extra] = entry.split("\t");
      const before = nonNegativeInt(beforeRaw ?? null);
      const after = nonNegativeInt(afterRaw ?? null);
      if (title && extra.length === 0 && before !== null && after !== null) {
        expanded[title] = { before, after };
      }
    }
    if (Object.keys(expanded).length > 0) out.relatedExpand = expanded;
  }
  const relatedKey = p.get(P_RELATED_KEY);
  const relatedValue = p.get(P_RELATED_VALUE);
  if (relatedKey && relatedValue !== null) {
    out.relatedCorrelate = { key: relatedKey, val: relatedValue };
  }
  const analysis = p.get(P_ANALYSIS);
  if (analysis !== null && (REGION_MODES as readonly string[]).includes(analysis)) {
    out.regionMode = analysis as RegionAnalysisMode;
  }
  const heapWeight = p.get(P_HEAP_WEIGHT);
  if (heapWeight === "bytes" || heapWeight === "count") out.regionHeapMode = heapWeight;
  const blockingGroup = p.get(P_BLOCKING_GROUP);
  if (blockingGroup === "leaf" || blockingGroup === "full") out.regionGroupBy = blockingGroup;
  const regionWorkerZoom = decodePath(p.get(P_ANALYSIS_WORKER_ZOOM));
  if (regionWorkerZoom !== null) out.regionWorkerZoom = regionWorkerZoom;
  const regionOffworkerZoom = decodePath(p.get(P_ANALYSIS_OFFWORKER_ZOOM));
  if (regionOffworkerZoom !== null) out.regionOffworkerZoom = regionOffworkerZoom;
  const regionInspectFocus = p.get(P_ANALYSIS_INSPECT);
  if (regionInspectFocus !== null && regionInspectFocus.length > 0) {
    out.regionInspectFocus = regionInspectFocus;
  }
  const spanNavIndex = nonNegativeInt(p.get(P_SPAN_INDEX));
  if (spanNavIndex !== null) out.spanNavIndex = spanNavIndex;
  const dataStart = num(p.get(P_DATA_START));
  const dataEnd = num(p.get(P_DATA_END));
  if (dataStart !== null || dataEnd !== null) {
    if (dataStart === null || dataEnd === null || dataEnd > dataStart) {
      out.dataRange = {
        ...(dataStart !== null ? { startNs: dataStart } : {}),
        ...(dataEnd !== null ? { endNs: dataEnd } : {}),
      };
    }
  }
  return out;
}

interface EncodedFieldChart {
  id: string;
  eventName: string;
  fieldName: string;
  kind: string;
}

function isValidFieldChart(chart: EncodedFieldChart): chart is FieldChartSpec {
  return (
    isValidFieldChartTrackId(chart.id) &&
    isFieldChartNameSupported(chart.eventName) &&
    isFieldChartNameSupported(chart.fieldName) &&
    (FIELD_CHART_KINDS as readonly string[]).includes(chart.kind)
  );
}

function encodeFieldChart(chart: FieldChartSpec): string | null {
  if (!isValidFieldChart(chart)) return null;
  return [
    chart.id,
    chart.eventName,
    chart.fieldName,
    chart.kind,
  ].join(FIELD_CHART_URL_SEPARATOR);
}

function decodeFieldChart(value: string): FieldChartSpec | null {
  const parts = value.split(FIELD_CHART_URL_SEPARATOR);
  if (parts.length !== 4) return null;
  const chart: EncodedFieldChart = {
    id: parts[0]!,
    eventName: parts[1]!,
    fieldName: parts[2]!,
    kind: parts[3]!,
  };
  return isValidFieldChart(chart) ? chart : null;
}

const FIELD_CHART_TRACK_ID_SUFFIX = /^[A-Za-z0-9_-]+$/;

function isValidFieldChartTrackId(id: string): boolean {
  return (
    isFieldChartTrackId(id) &&
    FIELD_CHART_TRACK_ID_SUFFIX.test(
      id.slice(FIELD_CHART_TRACK_ID_PREFIX.length),
    )
  );
}

/** Decode the modern marked list, falling back to the previously emitted
 * comma-separated percent-encoded grammar for old links. */
/** Decode a v1 `<key>,<px>` width list; tolerant reader - unknown keys and
 *  non-positive widths are dropped, an all-junk value reads as absent. */
function decodeWidthMap(
  v: string | null,
  validKeys: readonly string[],
): Record<string, number> | null {
  const entries = decodeList(v);
  if (entries === null) return null;
  const widths: Record<string, number> = {};
  for (const entry of entries) {
    const comma = entry.lastIndexOf(",");
    if (comma <= 0) continue;
    const key = entry.slice(0, comma);
    const px = positiveInt(entry.slice(comma + 1));
    if (px !== null && validKeys.includes(key)) widths[key] = px;
  }
  return Object.keys(widths).length > 0 ? widths : null;
}

function decodeList(v: string | null): string[] | null {
  if (v == null || v.length === 0) return null;
  if (v.startsWith("v1:")) {
    const values = v.slice(3).split("\t");
    return values.length > 0 && values.every((value) => value.length > 0) ? values : null;
  }
  return decodeLegacyList(v);
}

function decodeEntries(v: string | null): string[] | null {
  if (v == null || v.length === 0) return null;
  if (v.startsWith("v1:")) {
    const values = v.slice(3).split("\n");
    return values.length > 0 && values.every((value) => value.length > 0) ? values : null;
  }
  return decodeLegacyList(v);
}

function decodeLegacyList(v: string): string[] | null {
  try {
    const values = v
      .split(",")
      .map((part) => decodeURIComponent(part))
      .filter((part) => part.length > 0);
    return values.length > 0 ? values : null;
  } catch {
    return null;
  }
}

function decodePath(v: string | null): string[] | null {
  if (v === null || v.length === 0) return null;
  const path = v.split("\t");
  return path.every((part) => part.length > 0) ? path : null;
}

function taskDumpAnchor(
  value: string | null,
): { taskId: number; timestamps: number[] } | null {
  if (value === null) return null;
  const colon = value.indexOf(":");
  if (colon <= 0 || colon === value.length - 1) return null;
  const taskId = nonNegativeInt(value.slice(0, colon));
  const timestamps = value.slice(colon + 1).split(",").map(nonNegativeInt);
  if (
    taskId === null ||
    timestamps.length === 0 ||
    timestamps.some((timestamp) => timestamp === null)
  ) {
    return null;
  }
  return { taskId, timestamps: timestamps as number[] };
}

function sortPair<K extends string>(
  value: string | null,
  keys: readonly K[],
): { key: K; dir: "asc" | "desc" } | null {
  if (value === null) return null;
  const comma = value.indexOf(",");
  const key = comma > 0 ? value.slice(0, comma) : "";
  const dir = comma > 0 ? value.slice(comma + 1) : "";
  if (!(keys as readonly string[]).includes(key) || (dir !== "asc" && dir !== "desc")) {
    return null;
  }
  return { key: key as K, dir };
}

function nonNegativeInt(v: string | null): number | null {
  const n = num(v);
  return n !== null && Number.isInteger(n) && n >= 0 ? n : null;
}

function positiveInt(v: string | null): number | null {
  const n = nonNegativeInt(v);
  return n !== null && n > 0 ? n : null;
}

/** Parse a `"startNs-endNs"` param into a range, or null. */
function rangePair(v: string | null): { startNs: number; endNs: number } | null {
  if (v === null) return null;
  const dash = v.indexOf("-", 1); // skip a leading '-' (negative not expected)
  if (dash <= 0) return null;
  const startNs = num(v.slice(0, dash));
  const endNs = num(v.slice(dash + 1));
  if (startNs == null || endNs == null || endNs <= startNs) return null;
  return { startNs, endNs };
}

function num(v: string | null): number | null {
  if (v === null || !/^-?(?:\d+(?:\.\d+)?|\.\d+)$/.test(v)) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function intMaybeHex(v: string | null): number | null {
  if (v === null || !/^(?:0x[0-9a-f]+|\d+)$/i.test(v)) return null;
  const n = /^0x/i.test(v) ? Number.parseInt(v.slice(2), 16) : Number(v);
  return Number.isFinite(n) && Number.isInteger(n) && n >= 0 ? n : null;
}
