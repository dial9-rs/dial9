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

import type { ParsedTrace, PollSpan, CustomTraceEvent, TimeRange } from "./trace.js";
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
   * Range retained while the sidebar shows a region analysis (region
   * select -> flamegraph/blocking calls); blocks keyboard selection
   * until the sidebar closes (02 H9/H10). Replaces
   * `sidebarSelStart`/`sidebarSelEnd`.
   */
  sidebarRange: TimeRange | null;
  /** Waker task hovered in the task-detail panel (orange polls, 02 G8). */
  hoveredWakerTaskId: number | null;
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
}

// ── segments slice (architecture 2.8) ───────────────────────────────────

/**
 * Lifecycle of one S3 segment in the two-tier pipeline:
 * listed -> fetching -> parsed -> evicted. Eviction drops the parsed
 * data (the ~10x cost) and falls back to tier-1 rendering; re-entering
 * a window re-parses. Exhaustive switches over this union keep every
 * consumer honest when the lifecycle grows.
 */
export type SegmentLifecycle = "listed" | "fetching" | "parsed" | "evicted";

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
