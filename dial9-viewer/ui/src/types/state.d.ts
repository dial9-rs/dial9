// App-level store-state vocabulary.
//
// TYPES ONLY, same form and rules as src/types/trace.d.ts (see its header):
// importable module .d.ts with no runtime backing; consumers must
// `import type`. This file defines only the shapes the store implementation
// and every component/interact module share.

import type {
  ParsedTrace,
  PollSpan,
  CustomTraceEvent,
  PointOfInterestType,
  SegmentEdgePolls,
  TimeRange,
} from "./trace.js";
import type { TimePanelLayout } from "../../panel_layout.js";

// ── Panel vocabulary ────────────────────────────────────────────────────

/**
 * The four foldable analysis panels, named by their real DOM
 * `data-panel-key` values -- "events" is the custom-events panel. These
 * keys are load-bearing: localStorage persistence uses
 * `dial9.viewer.panelCollapsed.<key>`.
 */
export type FoldablePanelKind = "spans" | "events" | "cpu" | "queue";

/**
 * Every analysis panel below the worker lanes, discriminating render
 * targets in the renderer registry. "task-detail" is shown only while a
 * task is selected and is NOT foldable.
 */
export type PanelKind = FoldablePanelKind | "task-detail";

// ── Clock display vocabulary ────────────────────────────────────────────

/**
 * Clock display mode for the time axis and all timestamps: "rel" shows
 * offsets from the trace start (`+1.23s`), "abs" shows wall-clock time via
 * the trace's clock-sync anchors. The same "rel"/"abs" vocabulary the URL
 * codec carries (lib/url/view-state.ts TimeMode).
 */
export type TimeMode = "rel" | "abs";

/**
 * Timezone for absolute timestamps: "utc" vs the viewer's local zone. Only
 * meaningful when timeMode === "abs".
 */
export type TimeZoneMode = "utc" | "local";

// ── trace slice ─────────────────────────────────────────────────────────

/**
 * The parsed trace produced by the frozen core. Replaced WHOLESALE on
 * load/reparse, never mutated; derived analyses (worker spans, span data,
 * flamegraph trees) are computed from it, not stored in it.
 */
export interface TraceSlice {
  /** null until a trace has been loaded. */
  trace: ParsedTrace | null;
}

// ── viewport slice ──────────────────────────────────────────────────────

/**
 * The visible time window over the trace. Zoom/pan ops keep the existing
 * clamps: a 100ns minimum view span and clamping to [minTs, maxTs]. All
 * fields are 0 until a trace loads; the slice is only meaningful alongside
 * a non-null TraceSlice.trace.
 */
export interface ViewportSlice {
  /** Left edge of the visible window (trace-monotonic ns). */
  viewStart: number;
  /** Right edge of the visible window (trace-monotonic ns). */
  viewEnd: number;
  /** Navigable lower bound (the trace's earliest timestamp). */
  minTs: number;
  /** Navigable upper bound (the trace's latest timestamp). */
  maxTs: number;
}

// ── selection slice ─────────────────────────────────────────────────────

/**
 * A pinned (clicked) custom event: draws the persistent orange marker
 * across all lanes and backs the sidebar Event/Related tabs.
 */
export interface PinnedCustomEvent {
  /** All events at the clicked tick (a cluster pins several). */
  events: CustomTraceEvent[];
  /** Marker timestamp (trace-monotonic ns). */
  timestamp: number;
  /** Task resolved from the event's enclosing poll; null when none. */
  taskId: number | null;
  /** Display name of the (first) pinned event. */
  name: string;
  /** The poll the event ran inside; null when it ran outside any poll. */
  poll: PollSpan | null;
  /**
   * Detail pin for the Related tab: the single event whose detail is
   * shown. Explicitly null for cluster pins (Related is single-event only).
   */
  detailEvent: CustomTraceEvent | null;
}

/**
 * The focused-span chain: a clicked span bar plus its ancestor chain,
 * highlighted across the lanes.
 */
export interface SpanFocus {
  /** The clicked span. */
  spanId: string;
  /** The clicked span plus its ancestor chain (highlight set). */
  chain: ReadonlySet<string>;
}

/**
 * Async stack captures selected by clicking a dump-bearing idle span in the
 * task-detail track. Store semantic timestamps rather than trace-owned objects
 * so a reparse resolves against the replacement trace.
 */
export interface TaskDumpSelection {
  /** Task whose idle span produced these captures. */
  taskId: number;
  /** Capture timestamps attributed to the clicked idle span. */
  timestamps: readonly number[];
}

/**
 * Cross-highlight state. All fields are independently clearable, hence all
 * explicitly nullable.
 */
export interface SelectionSlice {
  /** Yellow-highlighted task across all lanes. */
  selectedTaskId: number | null;
  /** Focused span + ancestor chain. */
  spanFocus: SpanFocus | null;
  /**
   * Span whose subtree the span panel is filtered to (span + descendants)
   * -- distinct from spanFocus, which is the lane highlight.
   */
  focusedSpanId: string | null;
  /** Pinned custom event + marker. */
  pinnedEvent: PinnedCustomEvent | null;
  /**
   * The clicked poll shown in the Poll Detail inspector tab. Set by the
   * lane-interaction click when a poll carrying CPU or scheduling samples is
   * clicked (`resolveLaneClick.openStackFor`); null when no poll detail is
   * open. The inspector renders the deduplicated blocking-sched + CPU-sample
   * groups from it.
   */
  pollDetail: PollSpan | null;
  /**
   * Task-dump captures shown in the Stack inspector after clicking a dumped
   * idle span in the task-detail track.
   */
  taskDump: TaskDumpSelection | null;
  /**
   * Range retained while the sidebar shows a region analysis (region
   * select -> flamegraph/blocking calls); blocks keyboard selection until
   * the sidebar closes.
   */
  sidebarRange: TimeRange | null;
  /** Waker task hovered in the task-detail panel (orange polls). */
  hoveredWakerTaskId: number | null;
  /**
   * Time range drag-selected on the queue track. Dispatched on drag-release;
   * the inspector RENDERS the "tasks spawned in range" list from it
   * (queue-model.ts `computeSpawnedTasks` is the shared derivation). Distinct
   * from `sidebarRange` (region -> flamegraph/blocking): this is the
   * spawn-location task listing, a different sidebar surface. null when no
   * range is active.
   */
  spawnedTasksRange: TimeRange | null;
}

// ── poi slice (points-of-interest / issues rail) ────────────────────────

/**
 * How the issues rail orders its rows. The four sortable columns: the
 * worker id, the detector KIND, the event TIME, or the detector's severity
 * value shown in the DURATION column. The "Worst first" mode maps to
 * `duration`/`desc`; unchecked maps to `time`/`asc` (chronological), exactly
 * as `filterPointsOfInterest({ sortByWorst })` does. Column-header clicks
 * pick any of the four directly - a re-sort of the SAME rows, so the row
 * COUNT is sort-independent.
 */
export type PoiSortKey = "worker" | "kind" | "time" | "duration";

/** Issues-table columns: the severity dot plus the four sortable columns. */
export type IssueColKey = "dot" | PoiSortKey;

/** The rail's active tab: the POI issues list or the task index. */
export type RailTab = "issues" | "tasks";

/** Tasks-tab sort column. */
export type TaskSortKey = "id" | "loc" | "polls" | "total" | "longest" | "lifetime";

/**
 * Points-of-interest navigation state. The detectors are the frozen
 * `filterPointsOfInterest` set (trace_analysis.js, consumed via
 * lib/trace/analysis); this slice holds only the VIEW controls - which
 * detector is active, how the rail is ordered, and which row is the
 * "current" one the `n`/`p` keys step through and the timeline centers on.
 * The filtered list itself is DERIVED from the trace + these controls
 * (pages/viewer/poi.ts), never stored.
 */
export interface PoiSlice {
  /**
   * Active detector filter. Default "sched" (the "Kernel Scheduling Delays"
   * option).
   */
  filter: PointOfInterestType;
  /** Rail sort column. Default "duration" (worst-first). */
  sortKey: PoiSortKey;
  /** Sort direction. Default "desc" (worst-first = highest severity first). */
  sortDir: "asc" | "desc";
  /**
   * Index of the "current" POI within the DERIVED filtered+sorted list, or
   * -1 when none is selected. Reset to -1 whenever the filter changes (the
   * list is rebuilt). The `n`/`p` keys and a rail row click set it; the
   * status/rail read it for the "N/total" position.
   */
  index: number;
  /**
   * Which rail tab is showing. Lives in the store (not a rail-local) because
   * the rail is a pure controller: the shell only re-renders it when a
   * subscribed slice changes, so a local `activeTab` would never repaint.
   */
  railTab: RailTab;
  /** Tasks-tab sort column. Default "total" (heaviest first). */
  taskSort: TaskSortKey;
  /** Tasks-tab sort direction. Default "desc". */
  taskSortDir: "asc" | "desc";
  /**
   * Index of the current task within the DERIVED sorted list, or -1. The
   * Tasks-tab analogue of `index`; kept separate so switching tabs preserves
   * each tab's cursor.
   */
  taskIndex: number;
}

// ── uiPrefs slice ───────────────────────────────────────────────────────

/**
 * View preferences persisted to localStorage. Persistence mechanics are the
 * store's concern, not encoded here.
 */
export interface UiPrefsSlice {
  /**
   * Deprecated foldable-panel collapsed state. Superseded by `collapsed`: the
   * one-line-fold presentation is retired and per-track collapse now lives in
   * `collapsed` (keyed by any track id, not just the four foldable panels).
   * Retained as an additive no-op holder so existing defaults are undisturbed;
   * no live surface reads it.
   */
  panelCollapsed: Readonly<Record<FoldablePanelKind, boolean>>;
  /**
   * Track order for the unified column. The user drag-reorders the
   * manageable built-in and dynamic analysis tracks by the track-label grip;
   * this is their resulting id order. Empty = the catalogue order
   * (track-layout.ts TRACKS). Resolution is robust to unknown or missing ids,
   * so an order stored before a new track was added still resolves (the new
   * track appears in its catalogue slot). The full order is URL state; only
   * built-in ids persist in localStorage (`dial9.viewer.trackPrefs`).
   */
  trackOrder: readonly string[];
  /**
   * Per-track collapsed state. Track id -> true when the user collapsed it to
   * label-only height via the track-label caret. Absent or false = expanded
   * (analysis surfaces visible by default). Built-in and dynamic analysis
   * tracks are collapsible. The full map is URL state; only built-in ids
   * persist in localStorage (`dial9.viewer.trackPrefs`).
   */
  collapsed: Readonly<Record<string, boolean>>;
  /**
   * Per-runtime collapsed state: runtime-group name -> true when the user folded
   * that runtime to header-only (its worker rows hidden), clicked on the runtime
   * header band in the lanes track. Only meaningful with more than one runtime
   * group (headers exist only then). Absent or false = expanded. localStorage-
   * backed (dial9.viewer.trackPrefs), so a fold survives reload.
   */
  collapsedRuntimes: Readonly<Record<string, boolean>>;
  /**
   * Per-runtime SUMMARY-LANE collapsed state: runtime-group name -> true when the
   * user folded that runtime's metrics summary lane (queue depth + alive tasks)
   * down to its one-line strip by clicking it. Independent of
   * {@link collapsedRuntimes}, which folds the whole runtime: a reader can keep
   * the worker lanes while reclaiming the chart's height. Absent or false = the
   * full chart. localStorage-backed (dial9.viewer.trackPrefs), so a fold survives
   * reload.
   */
  collapsedRuntimeMetrics: Readonly<Record<string, boolean>>;
  /** Stack-sidebar width in CSS px (drag-resizable). */
  sidebarWidth: number;
  /** Issues/Tasks rail width in CSS px (drag-resizable via its right edge). */
  railWidth: number;
  /**
   * Tasks-table column widths in CSS px, keyed by column sort key. Empty =
   * automatic (content-fit) layout. The first divider drag seeds every column
   * from its measured width and the table switches to fixed layout, so the
   * dragged column changes while the rest hold; the table scrolls
   * horizontally inside the rail when the sum exceeds the rail width.
   */
  taskColWidths: Readonly<Partial<Record<TaskSortKey, number>>>;
  /**
   * Issues-table column widths in CSS px; same mechanics as
   * {@link taskColWidths}. `dot` is the severity-dot column: seeded and
   * pinned with the rest so fixed layout holds it steady, but it carries no
   * drag handle (nothing in it to read).
   */
  issueColWidths: Readonly<Partial<Record<IssueColKey, number>>>;
  /**
   * Height in CSS px of the worker-lanes box (the scroll container holding the
   * fixed-height worker rows). Drag-resizable via the lanes bottom gutter and
   * persisted (dial9.viewer.trackPrefs). Rows are a fixed height and the box
   * scrolls internally, so this only sizes the visible window.
   */
  lanesViewportHeight: number;
  /** Current vertical scroll offset of the worker-lanes viewport (CSS px). */
  lanesScrollTop: number;
  /**
   * Legend chip toggles: span / custom-event names currently selected for
   * display filtering. Empty set = no name filter.
   */
  selectedSpanNames: ReadonlySet<string>;
  selectedEventNames: ReadonlySet<string>;
  /**
   * Span filter text: case-insensitive substring over span name or field
   * key/value. Empty string = no text filter. AND-combined with the name
   * chips and the percentile filter (spanMatchesFilter). Lives in the store
   * (not component-local) so the spans track re-renders reactively and a
   * filter keystroke coalesces through the RAF scheduler to <= 1 render per
   * frame.
   */
  spanFilter: string;
  /**
   * Span percentile filter: 0 = All, else 50 / 90 / 95 / 99 - show only spans
   * at/above that percentile of their name's duration distribution.
   * AND-combined with the text + name filters.
   */
  spanPctFilter: number;
  /**
   * Clock display mode for the time axis + timestamps. Default "rel". The
   * toolbar toggle drives it; the time-axis track and every timestamp
   * formatter READ it.
   */
  timeMode: TimeMode;
  /**
   * Timezone for absolute timestamps. Default "utc"; only consulted when
   * timeMode === "abs".
   */
  tz: TimeZoneMode;
  /**
   * Render stack-frame sample lists (the poll inspector's CPU/sched samples,
   * the region panel's blocking sub-stacks) as a flamegraph instead of a
   * grouped list. Lives here rather than component-local because the inspector
   * resets its per-selection expansion state on every new selection - a local
   * toggle would reset with it, so the preference has to outlive the selection.
   */
  stacksAsFlamegraph: boolean;
}

// ── durable view slice ──────────────────────────────────────────────────

/** Inspector surface currently visible. */
export type InspectorTab = "task" | "poll" | "event" | "related" | "stack";
/** Region analysis selected for a retained range. */
export type RegionAnalysisMode = "cpu" | "blocking" | "heap";
/** Rendering semantics for a custom-event numeric field chart. */
export type FieldChartKind = "gauge" | "counter" | "updown-counter";
/**
 * One URL-defined custom-event field chart. The id also keys track order and
 * collapse state; event + field names resolve against the loaded trace.
 */
export interface FieldChartSpec {
  readonly id: string;
  /** Non-empty custom-event name; commas are not supported by the URL tuple. */
  readonly eventName: string;
  /** Non-empty numeric field name; commas are not supported by the URL tuple. */
  readonly fieldName: string;
  readonly kind: FieldChartKind;
}
/** One Related-section expansion counter. */
export interface RelatedExpansion {
  before: number;
  after: number;
}

/**
 * Durable controls that change what analysis is visible. Unlike TransientSlice,
 * every field here is part of the deep-link contract. Components must dispatch
 * changes through this slice rather than keeping local UI truth, so agents can
 * construct the same view without replaying pointer/keyboard actions.
 */
export interface ViewerViewSlice {
  /** Dynamic numeric-field tracks, persisted only through URL state. */
  fieldCharts: readonly FieldChartSpec[];
  inspectorTab: InspectorTab;
  /** Poll sample groups expanded in list mode (`cpu-N` / `sched-N`). */
  expandedPollGroups: ReadonlySet<string>;
  /** Which sample family the poll flamegraph displays when both are present. */
  pollFlamegraphSection: "cpu" | "sched";
  pollWorkerZoom: readonly string[];
  pollOffworkerZoom: readonly string[];
  relatedCollapsed: Readonly<Record<string, boolean>>;
  relatedExpand: Readonly<Record<string, RelatedExpansion>>;
  relatedCorrelate: { key: string; val: string } | null;
  /** null means choose the data-present default for a newly selected region. */
  regionMode: RegionAnalysisMode | null;
  regionHeapMode: "bytes" | "count";
  regionGroupBy: "leaf" | "full";
  regionWorkerZoom: readonly string[];
  regionOffworkerZoom: readonly string[];
  /** Butterfly/inspect focus in the region flamegraph, by full frame key. */
  regionInspectFocus: string | null;
  /** Current next/previous cursor in the filtered span list. */
  spanNavIndex: number;
}

// ── transient slice ─────────────────────────────────────────────────────

/**
 * In-flight drag gestures on the lanes: plain drag pans, Shift+drag
 * region-selects, Alt+drag zoom-selects. A drag only becomes "moved" past
 * the 3px intent threshold.
 */
export type DragKind = "pan" | "region-select" | "zoom-select";

export interface DragState {
  kind: DragKind;
  /** Pointer x at drag start (CSS px, client coords). */
  startX: number;
  /** Timestamp under the pointer at drag start (trace-monotonic ns). */
  startNs: number;
  /**
   * Timestamp under the pointer NOW (trace-monotonic ns, clamped to the draw
   * area). Equals startNs at press; the pointer machine advances it on every
   * move so the selection overlay can render the region box from
   * [startNs, curNs] without re-reading the pointer position. For a "pan"
   * drag it is unused (stays at startNs) - pan updates the viewport, not a box.
   */
  curNs: number;
  /** True once movement exceeded the 3px drag-intent threshold. */
  moved: boolean;
}

/**
 * Keyboard-driven Shift/Alt selection: cursor seeded at the mouse position
 * (or view center), extended by arrow keys, confirmed with Enter. Mirrors
 * DragKind's selection modes.
 */
export interface KeyboardSelection {
  kind: "region-select" | "zoom-select";
  /** Anchor timestamp (trace-monotonic ns). */
  startNs: number;
  /** Moving cursor timestamp (trace-monotonic ns). */
  cursorNs: number;
}

/**
 * The "at this instant" stats mirrored into ONE persistent surface. It carries
 * the values the floating info panel showed - global injection-queue depth,
 * max local-queue depth across workers, and the active-task count - plus the
 * worker + timestamp the cursor resolved to.
 *
 * Written to `transient.atCursor` by the crosshair/hover channel on every
 * hover frame; read by the inspector - so "at-moment stats" live in the
 * inspector, not a floating corner div. Because it rides the transient slice,
 * updating it never triggers a full track redraw (only subscribers that
 * declared `transient` re-run - the readout surface and the crosshair overlay).
 */
export interface AtCursorReadout {
  /** Timestamp under the cursor (trace-monotonic ns). */
  ns: number;
  /** Worker row under the cursor; null when the cursor is off any lane. */
  workerId: number | null;
  /** Global injection-queue depth at ns (nearest sample); null when none. */
  globalQueue: number | null;
  /** Max local-queue depth across all workers at ns; null when none. */
  localMax: number | null;
  /**
   * Active-task count at ns (step: latest sample with t <= ns); null when the
   * trace carries no task tracking (activeTaskSamples empty).
   */
  activeTaskCount: number | null;
  /**
   * Windowed-data completeness at the cursor: "complete" when the segment
   * covering `ns` is fully resident (or the whole trace is resident on the
   * non-segmented path); "truncated" when the covering segment is only
   * partially resident (listed/fetching/evicted - a window edge, not whole
   * data); "oversized" when the covering segment can never be resident at the
   * budget (SegmentLifecycle "oversized"). Consumers MUST surface a
   * non-"complete" state rather than presenting a truncated window as whole.
   */
  coverage: "complete" | "truncated" | "oversized";
}

/**
 * High-frequency interaction state, updated on the crosshair RAF channel;
 * never triggers full renders (overlay layer).
 */
export interface TransientSlice {
  /** Timestamp under the mouse; null when outside the lanes. */
  mouseNs: number | null;
  /** Hovered custom event's timestamp for the guide line. */
  hoverEventTs: number | null;
  /** Active drag gesture; null when not dragging. */
  drag: DragState | null;
  /** Active keyboard selection; null when none. */
  keyboardSelection: KeyboardSelection | null;
  /**
   * At-cursor stats readout, null when the cursor is outside the draw area or
   * no trace is loaded. Distinct from the ephemeral hover TOOLTIP (which is
   * rendered imperatively from LaneHoverData, not stored) - this is the
   * persistent mirror.
   */
  atCursor: AtCursorReadout | null;
}

// ── segments slice ──────────────────────────────────────────────────────

/**
 * Lifecycle of one S3 segment in the two-tier pipeline:
 * listed -> fetching -> parsed -> evicted, plus the terminal "oversized"
 * refusal. Eviction drops the parsed data (the ~10x cost) and falls back
 * to tier-1 rendering; re-entering a window re-parses.
 *
 * "oversized": the segment's DECOMPRESSED size, learned from its one and only
 * parse, exceeds the resident budget - it can never be resident, so admission
 * defers it instead of spinning through parse -> evict on every viewport
 * tick. Explicitly distinct from "listed" (not yet fetched) and "evicted"
 * (fits, will re-parse on re-entry): consumers must render it as
 * unavailable-at-this-budget (tier-1 fallback + badge). Exhaustive switches
 * over this union keep every consumer honest when the lifecycle grows.
 */
export type SegmentLifecycle =
  | "listed"
  | "fetching"
  | "parsed"
  | "evicted"
  | "oversized";

/**
 * Parse-derived invariants (min/max ts, worker set) retained ACROSS eviction
 * so lanes/axes stay stable. Written when a segment first parses; never
 * cleared while the segment stays listed.
 */
export interface SegmentParseInvariants {
  /** Event-time bounds of the segment's parse; null when it had no events. */
  minTs: number | null;
  maxTs: number | null;
  /** Distinct runtime worker ids observed in the segment. */
  workerIds: readonly number[];
}

/** Per-segment state tracked by the viewport-driven window machinery. */
export interface SegmentEntry {
  state: SegmentLifecycle;
  /**
   * Segment time extent mapped into trace-monotonic ns (derived from S3
   * listing metadata by lib/trace/segments.ts). Known from listing time,
   * BEFORE any raw bytes are fetched -- tier-1 rendering depends on it.
   */
  extent: TimeRange;
  /** Raw (gzipped) object size from the listing, bytes. */
  sizeBytes: number;

  // ── Additive fields (all absent until first parse) ───────────────────

  /** The segment's own parse; present iff state === "parsed". */
  trace?: ParsedTrace;
  /**
   * Decompressed (raw) byte size, learned from the first parse and
   * RETAINED after eviction: it is the segment's resident-budget cost
   * (raw bytes are the proxy for the ~10x parsed heap) and upgrades the
   * pre-fetch gzip-size estimate for re-entry planning.
   */
  rawByteLength?: number;
  /** Retained across eviction (see SegmentParseInvariants). */
  invariants?: SegmentParseInvariants;
  /**
   * Boundary-poll evidence at the segment's edges (SegmentEdgePolls).
   * Written at first parse and RETAINED across eviction (they are tiny -
   * at most one open + one close per worker): a poll crossing an evicted
   * neighbor still needs that neighbor's edge evidence to surface as
   * explicitly truncated instead of vanishing. Absent only before the first
   * parse.
   */
  edgePolls?: SegmentEdgePolls;
}

/**
 * The segment-windowed loading state: segment key (S3 object key) -> entry.
 * The viewport drives transitions; budgets, prefetch, and eviction policy
 * live in lib/trace/segments.ts.
 */
export interface SegmentsSlice {
  segments: ReadonlyMap<string, SegmentEntry>;
}

// ── Store shape ─────────────────────────────────────────────────────────

/**
 * The full per-page store state: one property per slice. Subscribers declare
 * dependencies as sets of StoreSliceName; the scheduler coalesces
 * notifications per RAF tick.
 */
export interface StoreState {
  trace: TraceSlice;
  viewport: ViewportSlice;
  selection: SelectionSlice;
  poi: PoiSlice;
  uiPrefs: UiPrefsSlice;
  view: ViewerViewSlice;
  transient: TransientSlice;
  segments: SegmentsSlice;
}

export type StoreSliceName = keyof StoreState;

// ── Layout geometry ──────────────────────────────────────────────────────
//
// lib/canvas/layout.ts is the single producer of these; the frozen
// panel_layout.js invariant (LABEL_W gutter, drawW, scrollbar
// compensation) is the single source of the ns<->x mapping.

/** Re-export of the frozen core's time-panel layout (ns<->x mapping). */
export type { TimePanelLayout } from "../../panel_layout.js";

/**
 * Geometry handed to a canvas panel's `render(ctx, state, layout)`:
 * the shared time mapping plus this panel's box. Wraps the
 * panel_layout.js output so every panel's time axis lines up.
 */
export interface PanelGeometry {
  kind: PanelKind;
  /** Shared ns<->x mapping (frozen-core invariant). */
  time: TimePanelLayout;
  /** Panel canvas height in CSS px. */
  height: number;
  /** Device pixel ratio the backing store is scaled by. */
  dpr: number;
}
