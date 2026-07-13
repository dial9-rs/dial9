// App-level store-state vocabulary (T06; docs/ui-inventory/02-architecture.md
// 2.2 store slices, 2.8 segments slice, 2.3 layout geometry).
//
// TYPES ONLY, same form and rules as src/types/trace.d.ts (see its header):
// importable module .d.ts with no runtime backing; consumers must
// `import type`. The store IMPLEMENTATION (update/subscribe mechanics,
// actions, persistence) is T07/T08 -- this file defines only the shapes
// those tickets and every component/interact module share.
//
// Slice contents follow architecture 2.2 verbatim, with field shapes
// verified against the viewer.html globals they replace (the ~68-global
// inventory, docs/ui-inventory/features/02-viewer-html.md).

import type {
  ParsedTrace,
  PollSpan,
  CustomTraceEvent,
  SegmentEdgePolls,
  TimeRange,
} from "./trace.js";
import type { TimePanelLayout } from "../../panel_layout.js";

// ── Panel vocabulary ────────────────────────────────────────────────────

/**
 * The four foldable analysis panels, named by their real DOM
 * `data-panel-key` values (viewer.html) -- "events" is the custom-events
 * panel. These keys are load-bearing: localStorage persistence uses
 * `dial9.viewer.panelCollapsed.<key>` (features 02 O4).
 */
export type FoldablePanelKind = "spans" | "events" | "cpu" | "queue";

/**
 * Every analysis panel below the worker lanes, discriminating render
 * targets in the renderer registry (architecture 2.3). "task-detail" is
 * shown only while a task is selected and is NOT foldable (features 02 N).
 */
export type PanelKind = FoldablePanelKind | "task-detail";

// ── Clock display vocabulary (features 02 E1/E2, F) ─────────────────────

/**
 * Clock display mode for the time axis and all timestamps (features 02 E1):
 * "rel" shows offsets from the trace start (`+1.23s`), "abs" shows
 * wall-clock time via the trace's clock-sync anchors. Replaces the legacy
 * `useAbsoluteTime` boolean; the same "rel"/"abs" vocabulary the URL codec
 * carries (lib/url/view-state.ts TimeMode).
 */
export type TimeMode = "rel" | "abs";

/**
 * Timezone for absolute timestamps (features 02 E2): "utc" vs the viewer's
 * local zone. Only meaningful when timeMode === "abs". Replaces the legacy
 * `useLocalTz` boolean.
 */
export type TimeZoneMode = "utc" | "local";

// ── trace slice ─────────────────────────────────────────────────────────

/**
 * The parsed trace produced by the frozen core. Replaced WHOLESALE on
 * load/reparse, never mutated (architecture 2.2); derived analyses
 * (worker spans, span data, flamegraph trees) are computed from it, not
 * stored in it.
 */
export interface TraceSlice {
  /** null until a trace has been loaded. */
  trace: ParsedTrace | null;
}

// ── viewport slice ──────────────────────────────────────────────────────

/**
 * The visible time window over the trace. Zoom/pan ops (store actions,
 * T07/T08) keep the existing clamps: a 100ns minimum view span and
 * clamping to [minTs, maxTs] (features 02 H1-H12). All fields are 0
 * until a trace loads (matching today's globals); the slice is only
 * meaningful alongside a non-null TraceSlice.trace.
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
 * across all lanes and backs the sidebar Event/Related tabs (features
 * 02 I5, K4). Replaces the `selectedEvent` global.
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
   * shown. Explicitly null for cluster pins (Related is single-event
   * only) -- replaces the `selectedEventRef` global.
   */
  detailEvent: CustomTraceEvent | null;
}

/**
 * The focused-span chain (features 02 G7, J): a clicked span bar plus
 * its ancestor chain, highlighted across the lanes. Replaces the
 * `selectedSpanId` + `selectedSpanIds` globals.
 */
export interface SpanFocus {
  /** The clicked span. */
  spanId: string;
  /** The clicked span plus its ancestor chain (highlight set). */
  chain: ReadonlySet<string>;
}

/**
 * Cross-highlight state that is scattered globals today (architecture
 * 2.2; features 02 G6-G8, I4-I5). All fields are independently
 * clearable, hence all explicitly nullable.
 */
export interface SelectionSlice {
  /** Yellow-highlighted task across all lanes (02 G6). */
  selectedTaskId: number | null;
  /** Focused span + ancestor chain (02 G7). */
  spanFocus: SpanFocus | null;
  /**
   * Span whose subtree the span panel is filtered to (span + descendants,
   * 02 J) -- distinct from spanFocus, which is the lane highlight.
   */
  focusedSpanId: string | null;
  /** Pinned custom event + marker (02 I5). */
  pinnedEvent: PinnedCustomEvent | null;
  /**
   * The clicked poll shown in the Poll Detail inspector tab (features 02
   * R1 / G15). Set by the lane-interaction click when a poll carrying CPU or
   * scheduling samples is clicked (`resolveLaneClick.openStackFor`); null when
   * no poll detail is open. T31's inspector renders the deduplicated
   * blocking-sched + CPU-sample groups from it. Additive selection field
   * (the T29 `spawnedTasksRange` precedent): the legacy showStackPopup carried
   * the clicked poll in a local, so the store gains an explicit field the
   * persistent inspector re-scopes to in the same click action (04 S4).
   */
  pollDetail: PollSpan | null;
  /**
   * Range retained while the sidebar shows a region analysis (region
   * select -> flamegraph/blocking calls); blocks keyboard selection
   * until the sidebar closes (02 H9/H10). Replaces
   * `sidebarSelStart`/`sidebarSelEnd`.
   */
  sidebarRange: TimeRange | null;
  /** Waker task hovered in the task-detail panel (orange polls, 02 G8). */
  hoveredWakerTaskId: number | null;
  /**
   * Time range drag-selected on the queue track (features 02 M7). T29
   * dispatches it here on drag-release; T31's inspector RENDERS the "tasks
   * spawned in range" list from it (queue-model.ts `computeSpawnedTasks` is
   * the shared derivation - T29 dispatches, T31 renders). Distinct from
   * `sidebarRange` (region -> flamegraph/blocking, T32): M7 is the
   * spawn-location task listing, a different sidebar surface. null when no
   * range is active.
   */
  spawnedTasksRange: TimeRange | null;
}

// ── uiPrefs slice ───────────────────────────────────────────────────────

/**
 * View preferences persisted to localStorage as today (architecture 2.2;
 * features 02 O4, P). Persistence mechanics are the store's concern
 * (T07/T08), not encoded here.
 */
export interface UiPrefsSlice {
  /** Foldable-panel collapsed state (localStorage-backed, 02 O4). */
  panelCollapsed: Readonly<Record<FoldablePanelKind, boolean>>;
  /** Stack-sidebar width in CSS px (drag-resizable, 02 P2). */
  sidebarWidth: number;
  /**
   * Legend chip toggles (02 J9, K5): span / custom-event names currently
   * selected for display filtering. Empty set = no name filter.
   */
  selectedSpanNames: ReadonlySet<string>;
  selectedEventNames: ReadonlySet<string>;
  /**
   * Span filter text (02 J7): case-insensitive substring over span name or
   * field key/value. Empty string = no text filter. AND-combined with the
   * name chips and the percentile filter (spanMatchesFilter). Lives in the
   * store (not component-local) so the spans track re-renders reactively and
   * a filter keystroke coalesces through the RAF scheduler to <= 1 render per
   * frame (perf finding F2) instead of the legacy synchronous renderAll.
   */
  spanFilter: string;
  /**
   * Span percentile filter (02 J8): 0 = All, else 50 / 90 / 95 / 99 - show
   * only spans at/above that percentile of their name's duration
   * distribution. AND-combined with the text + name filters.
   */
  spanPctFilter: number;
  /**
   * Clock display mode for the time axis + timestamps (02 E1). Default
   * "rel" (legacy `useAbsoluteTime` = false). The toolbar toggle drives it
   * (T33); the time-axis track (T25) and every timestamp formatter READ it.
   */
  timeMode: TimeMode;
  /**
   * Timezone for absolute timestamps (02 E2). Default "utc" (legacy
   * `useLocalTz` = false); only consulted when timeMode === "abs".
   */
  tz: TimeZoneMode;
}

// ── transient slice ─────────────────────────────────────────────────────

/**
 * In-flight drag gestures on the lanes (architecture 2.5): plain drag
 * pans, Shift+drag region-selects, Alt+drag zoom-selects. A drag only
 * becomes "moved" past the 3px intent threshold (02 H6-H8).
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
   * area). Equals startNs at press; the pointer machine (T23) advances it on
   * every move so the H10 selection overlay can render the region box from
   * [startNs, curNs] without re-reading the pointer position. For a "pan"
   * drag it is unused (stays at startNs) - pan updates the viewport, not a box.
   */
  curNs: number;
  /** True once movement exceeded the 3px drag-intent threshold. */
  moved: boolean;
}

/**
 * Keyboard-driven Shift/Alt selection (02 H9): cursor seeded at the
 * mouse position (or view center), extended by arrow keys, confirmed
 * with Enter. Mirrors DragKind's selection modes.
 */
export interface KeyboardSelection {
  kind: "region-select" | "zoom-select";
  /** Anchor timestamp (trace-monotonic ns). */
  startNs: number;
  /** Moving cursor timestamp (trace-monotonic ns). */
  cursorNs: number;
}

/**
 * The "at this instant" stats mirrored into ONE persistent surface (04 S4;
 * the store contract T24 DEFINES and T31's inspector RENDERS). It carries the
 * values the legacy floating top-right `#info-panel` showed (features/02 I6 -
 * global injection-queue depth, max local-queue depth across workers, and the
 * active-task count) plus the worker + timestamp the cursor resolved to.
 *
 * Written to `transient.atCursor` by the crosshair/hover channel (T24) on
 * every hover frame; read by the inspector (T31) - and, until T31 lands, by
 * T24's minimal readout stub in the inspector slot - so "at-moment stats" live
 * in the inspector, not a floating corner div. Because it rides the transient
 * slice, updating it never triggers a full track redraw (only subscribers that
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
   * trace carries no task tracking (matches the legacy info-panel omitting the
   * "Active tasks" line when `activeTaskSamples` is empty).
   */
  activeTaskCount: number | null;
  /**
   * Windowed-data completeness at the cursor (T17 segment-windowing carried
   * obligation - T17-audit notes 6-7): "complete" when the segment covering
   * `ns` is fully resident (or the whole trace is resident on the
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
 * never triggers full renders (architecture 2.2/2.3 overlay layer).
 */
export interface TransientSlice {
  /** Timestamp under the mouse; null when outside the lanes (02 I2). */
  mouseNs: number | null;
  /** Hovered custom event's timestamp for the guide line (02 I4). */
  hoverEventTs: number | null;
  /** Active drag gesture; null when not dragging. */
  drag: DragState | null;
  /** Active keyboard selection; null when none (02 I3). */
  keyboardSelection: KeyboardSelection | null;
  /**
   * At-cursor stats readout (02 I6 + 04 S4), null when the cursor is outside
   * the draw area or no trace is loaded. The store contract T31's inspector
   * renders; populated by the T24 hover channel. Distinct from the ephemeral
   * hover TOOLTIP (which is rendered imperatively from LaneHoverData, not
   * stored) - this is the persistent mirror.
   */
  atCursor: AtCursorReadout | null;
}

// ── segments slice (architecture 2.8) ───────────────────────────────────

/**
 * Lifecycle of one S3 segment in the two-tier pipeline:
 * listed -> fetching -> parsed -> evicted, plus the terminal "oversized"
 * refusal. Eviction drops the parsed data (the ~10x cost) and falls back
 * to tier-1 rendering; re-entering a window re-parses.
 *
 * "oversized" (T17-audit finding 2, additive per the T06 precedent): the
 * segment's DECOMPRESSED size, learned from its one and only parse,
 * exceeds the resident budget - it can never be resident, so admission
 * defers it instead of spinning through parse -> evict on every viewport
 * tick. Explicitly distinct from "listed" (not yet fetched) and
 * "evicted" (fits, will re-parse on re-entry): consumers must render it
 * as unavailable-at-this-budget (tier-1 fallback + badge, chunk 2).
 * Exhaustive switches over this union keep every consumer honest when
 * the lifecycle grows.
 */
export type SegmentLifecycle =
  | "listed"
  | "fetching"
  | "parsed"
  | "evicted"
  | "oversized";

/**
 * Parse-derived invariants retained ACROSS eviction (architecture 2.8:
 * "parsed-window invariants (min/max ts, worker set) are retained so
 * lanes/axes stay stable"). Written when a segment first parses; never
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

  // ── T17 additive fields (all absent until first parse) ───────────────

  /** The segment's own parse; present iff state === "parsed". */
  trace?: ParsedTrace;
  /**
   * Decompressed (raw) byte size, learned from the first parse and
   * RETAINED after eviction: it is the segment's resident-budget cost
   * (N19 accounts raw bytes as the proxy for the ~10x parsed heap) and
   * upgrades the pre-fetch gzip-size estimate for re-entry planning.
   */
  rawByteLength?: number;
  /** Retained across eviction (see SegmentParseInvariants). */
  invariants?: SegmentParseInvariants;
  /**
   * Boundary-poll evidence at the segment's edges (types/trace.d.ts).
   * Written at first parse and RETAINED across eviction (they are tiny -
   * at most one open + one close per worker): a poll crossing an evicted
   * neighbor still needs that neighbor's edge evidence to surface as
   * explicitly truncated instead of vanishing (T17-audit finding 1).
   * Absent only before the first parse.
   */
  edgePolls?: SegmentEdgePolls;
}

/**
 * The segment-windowed loading state (architecture 2.8): segment key
 * (S3 object key) -> entry. The viewport drives transitions; budgets,
 * prefetch, and eviction policy live in lib/trace/segments.ts.
 */
export interface SegmentsSlice {
  segments: ReadonlyMap<string, SegmentEntry>;
}

// ── Store shape ─────────────────────────────────────────────────────────

/**
 * The full per-page store state: one property per slice (architecture
 * 2.2 + the 2.8 segments slice). Subscribers declare dependencies as
 * sets of StoreSliceName; the scheduler coalesces notifications per RAF
 * tick (implementation: T07/T08).
 */
export interface StoreState {
  trace: TraceSlice;
  viewport: ViewportSlice;
  selection: SelectionSlice;
  uiPrefs: UiPrefsSlice;
  transient: TransientSlice;
  segments: SegmentsSlice;
}

/** "trace" | "viewport" | "selection" | "uiPrefs" | "transient" | "segments" */
export type StoreSliceName = keyof StoreState;

// ── Layout geometry (architecture 2.3) ──────────────────────────────────
//
// lib/canvas/layout.ts is the single producer of these; the frozen
// panel_layout.js invariant (LABEL_W gutter, drawW, scrollbar
// compensation) is the single source of the ns<->x mapping.

/** Re-export of the frozen core's time-panel layout (ns<->x mapping). */
export type { TimePanelLayout } from "../../panel_layout.js";

/** Geometry of one worker lane row within the lanes stack. */
export interface LaneGeometry {
  workerId: number;
  /** Row index within the lanes stack (top = 0). */
  index: number;
  /** Top edge in CSS px, lanes-local (before scroll offset). */
  y: number;
  /** Row height in CSS px. */
  height: number;
}

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
