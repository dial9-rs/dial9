// src/pages/viewer/inspector-model.ts - the persistent inspector's PURE
// derivations (T31; features/02 sections P/Q/R). NODE-TESTABLE: no store, no
// DOM, no lit-html - every function is referentially transparent so the
// inspector component (inspector.ts) renders from these and the numbers/rows
// behavioral-diff against legacy directly (the DoD's "Poll Detail exactly as
// legacy" + the Q/R row-walker).
//
// The split mirrors the rest of the viewer (task-detail-model, queue-model,
// events-model): this file owns the derivation; inspector.ts owns the tab
// framework, the imperative render, and the P-row mechanics (resize/persist,
// tab families, Esc/clear). Legacy sources ported verbatim minus the DOM:
//   - Poll Detail (R1/R2): showStackPopup (viewer.html:5455-5538).
//   - Event detail (Q1): eventDetailHtml (viewer.html:3976-4006).
//   - Related (Q4-Q8): relatedHtml (viewer.html:4044-4167).
//   - Spawned-tasks (M7/M8): computeSpawnedTasks (queue-model, T29) formatted.

import {
  deduplicateSamples,
  enclosingSpans,
  formatFrame,
  formatFieldValue,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import type {
  CallframeSymbols,
  CustomTraceEvent,
  PollSpan,
  SampleGroup,
  SymbolFrame,
  TracingSpan,
  WorkerLane,
} from "../../lib/trace/index.js";
import type { PinnedCustomEvent, SelectionSlice } from "../../types/state.js";

// ── Frame formatting (legacy renderFrame, viewer.html:1025-1029) ──────────

/** A display frame: text plus an optional docs.rs link (legacy renderFrame). */
export interface FrameLine {
  text: string;
  docsUrl: string | null;
}

/** Format a symbolized frame for the inspector (formatFrame, no HTML). */
export function frameLine(
  frame: SymbolFrame,
  callframeSymbols?: CallframeSymbols,
): FrameLine {
  const f = formatFrame(frame, callframeSymbols);
  return { text: f.text, docsUrl: f.docsUrl };
}

// ── Poll Detail (R1/R2): showStackPopup, deduped sample groups ────────────

/** One deduplicated sample group as the inspector renders it (R1/R2). */
export interface SampleGroupView {
  /** Occurrence count (legacy `g.count`). */
  count: number;
  /** Percent of this section's samples (legacy `(count/total*100).toFixed(0)`). */
  pct: number;
  /** Leaf frame text (legacy `g.leaf`). */
  leaf: string;
  /** Summary bar width in px (legacy `Math.max(2, ratio*200)`; sched only). */
  barW: number;
  /** The context frames shown by default (legacy `frames.slice(...,3)`). */
  headFrames: FrameLine[];
  /** Frames beyond the first three, revealed by the R2 expand toggle. */
  moreFrames: FrameLine[];
}

/** The Poll Detail tab view (R1): title + sched groups (red) + cpu groups. */
export interface PollDetailView {
  durationLabel: string;
  cpuCount: number;
  schedCount: number;
  /** Legacy title `Poll <dur>[ · N CPU samples][ · N sched events]`. */
  title: string;
  /** Deduplicated blocking-sched groups (red); empty when none. */
  schedGroups: SampleGroupView[];
  /** Deduplicated CPU-profile groups (orange); empty when none. */
  cpuGroups: SampleGroupView[];
}

function groupViews(
  groups: readonly SampleGroup[],
  total: number,
  callframeSymbols: CallframeSymbols,
  headStart: number,
  withBar: boolean,
): SampleGroupView[] {
  return groups.map((g) => {
    const ratio = total > 0 ? g.count / total : 0;
    const frames = g.frames;
    return {
      count: g.count,
      pct: Math.round(ratio * 100),
      leaf: g.leaf,
      barW: withBar ? Math.max(2, ratio * 200) : 0,
      // Sched context is frames[0..3]; CPU context is frames[1..3] (legacy).
      headFrames: frames
        .slice(headStart, 3)
        .map((f) => frameLine(f, callframeSymbols)),
      moreFrames:
        frames.length > 3
          ? frames.slice(3).map((f) => frameLine(f, callframeSymbols))
          : [],
    };
  });
}

/**
 * Build the Poll Detail view for a clicked poll (R1/R2). Ports showStackPopup
 * (viewer.html:5455-5538): title, deduplicated blocking-sched groups (leaf +
 * first three frames, expandable), then the same treatment for CPU samples
 * (context starts at frame 1, no summary bar). Percentages are section-local
 * (sched vs cpu), matching legacy.
 */
export function buildPollDetail(
  poll: PollSpan,
  callframeSymbols: CallframeSymbols,
): PollDetailView {
  const durationLabel = formatHumanDuration(poll.end - poll.start);
  const cpuCount = poll.cpuSamples ? poll.cpuSamples.length : 0;
  const schedCount = poll.schedSamples ? poll.schedSamples.length : 0;
  const title =
    `Poll ${durationLabel}` +
    (cpuCount ? ` · ${cpuCount} CPU sample${cpuCount > 1 ? "s" : ""}` : "") +
    (schedCount ? ` · ${schedCount} sched event${schedCount > 1 ? "s" : ""}` : "");

  const schedGroups =
    schedCount > 0
      ? groupViews(
          deduplicateSamples(poll.schedSamples!, callframeSymbols),
          schedCount,
          callframeSymbols,
          0,
          true,
        )
      : [];
  const cpuGroups =
    cpuCount > 0
      ? groupViews(
          deduplicateSamples(poll.cpuSamples!, callframeSymbols),
          cpuCount,
          callframeSymbols,
          1,
          false,
        )
      : [];

  return { durationLabel, cpuCount, schedCount, title, schedGroups, cpuGroups };
}

// ── Event detail (Q1/Q2/Q3): eventDetailHtml ──────────────────────────────

/** One key/value detail row; `corrVal` non-null offers the Q3 correlation. */
export interface KvRow {
  key: string;
  /** Display value (unit-formatted for a single event). */
  value: string;
  /** The raw value to correlate on (Q3), or null when correlation is offered
   *  nowhere (cluster rows, the `@`/`Task` rows, or a value unique to one
   *  event). */
  corrVal: string | null;
}

/** The Event tab view (Q1): kv rows + the resolved task (single or cluster). */
export interface EventDetailView {
  /** Sidebar title (P3): event name (single) or `Cluster · N events`. */
  title: string;
  rows: KvRow[];
  taskId: number | null;
  /** True for a single-event pin: enables the Related tab (Q4 CONDITIONAL). */
  isSingle: boolean;
}

/** How many events carry `key === String(val)` (legacy eventsWithField). */
function countWithField(
  events: readonly CustomTraceEvent[],
  key: string,
  val: string,
): number {
  let n = 0;
  for (const e of events) {
    if (e.fields && String(e.fields[key]) === val) n++;
  }
  return n;
}

/** Most-frequent-first "name ×N" list (legacy topNames), for cluster types. */
export function topEventNames(
  events: readonly CustomTraceEvent[],
  limit = 5,
): string {
  const counts = new Map<string, number>();
  for (const e of events) counts.set(e.name, (counts.get(e.name) ?? 0) + 1);
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, limit)
    .map(([n, c]) => (c > 1 ? `${n} ×${c}` : n))
    .join(", ");
}

/**
 * Build the Event tab view for a pinned custom event (Q1). Ports
 * eventDetailHtml (viewer.html:3976-4006): a single event lists its
 * unit-formatted fields (correlation offered only when another event shares
 * the value, Q3) plus its timestamp; a cluster lists count / top types / the
 * timestamp range. The resolved task appends a `Task` row. `allEvents` is the
 * full custom-event stream for the Q3 correlation-availability check.
 */
export function buildEventDetail(
  pinned: PinnedCustomEvent,
  allEvents: readonly CustomTraceEvent[],
  fmtTs: (ns: number) => string,
): EventDetailView {
  const events = pinned.events;
  const rows: KvRow[] = [];
  const isSingle = events.length === 1;
  let title: string;

  if (isSingle) {
    const ev = events[0]!;
    title = ev.name;
    for (const [k, v] of Object.entries(ev.fields ?? {})) {
      const corrVal = String(v);
      const display = formatFieldValue(v, ev.units?.[k]);
      const shared = countWithField(allEvents, k, corrVal) > 1;
      rows.push({ key: k, value: display, corrVal: shared ? corrVal : null });
    }
    rows.push({ key: "@", value: fmtTs(ev.timestamp), corrVal: null });
  } else {
    const first = events[0]!;
    const last = events[events.length - 1]!;
    title = `Cluster · ${events.length} events`;
    rows.push({ key: "Cluster", value: `${events.length} events`, corrVal: null });
    rows.push({ key: "Types", value: topEventNames(events), corrVal: null });
    const range =
      last.timestamp !== first.timestamp
        ? `${fmtTs(first.timestamp)} – ${fmtTs(last.timestamp)}`
        : fmtTs(first.timestamp);
    rows.push({ key: "@", value: range, corrVal: null });
  }

  if (pinned.taskId != null) {
    const hex = "0x" + pinned.taskId.toString(16);
    rows.push({ key: "Task", value: `${hex} (selected)`, corrVal: null });
  }
  return { title, rows, taskId: pinned.taskId, isSingle };
}

// ── Related (Q4-Q8): relatedHtml ──────────────────────────────────────────

export const RELATED_INITIAL = 5;
export const RELATED_STEP = 25;

/** A clickable Related row target (Q7): navigate to a span or pin an event. */
export type RelatedTarget =
  | { kind: "span"; spanId: string }
  | { kind: "event"; event: CustomTraceEvent };

/** One rendered Related row (Q4/Q7). */
export interface RelatedRow {
  /** Row label text (event name / span name / relabelled). */
  name: string;
  /** Right-aligned time/delta text (Q4 r-delta or span r-time). */
  aside: string;
  /** Left pad in px (enclosing-spans indent by depth). */
  padPx: number;
  /** The self ("this event") anchor row is non-navigable (Q7). */
  self: boolean;
  /** Click target; null for the self row. */
  target: RelatedTarget | null;
}

/** A per-direction "load more" affordance (Q6). */
export interface RelatedLoadMore {
  dir: "before" | "after";
  /** How many rows this click reveals (min(STEP, hidden)). */
  count: number;
  /** How many remain hidden in this direction. */
  hidden: number;
}

/** One Related section (Q4/Q5): collapsible, windowed (Q6). */
export interface RelatedSection {
  /** Stable title key (also the relatedCollapsed / relatedExpand map key). */
  title: string;
  /** Count shown in the header (null = no count, e.g. unresolved task). */
  count: number | null;
  collapsed: boolean;
  /** Empty-state placeholder text (Q8), or null when the section has rows. */
  empty: string | null;
  loadBefore: RelatedLoadMore | null;
  rows: RelatedRow[];
  loadAfter: RelatedLoadMore | null;
}

/** Per-section grow state (Q6): extra rows revealed before/after the anchor. */
export interface RelatedExpandState {
  before: number;
  after: number;
}

/** The Related tab UI state the component owns; the model reads it (Q5/Q6). */
export interface RelatedUiState {
  /** Explicit collapse choices by section title (tri-state; Q5). */
  collapsed: Readonly<Record<string, boolean>>;
  /** Per-section grow counters (Q6). */
  expand: Readonly<Record<string, RelatedExpandState>>;
  /** The Q3 correlation field, or null (set by clicking a correlation button). */
  correlate: { key: string; val: string } | null;
}

/** The full Related tab view (Q4). */
export interface RelatedView {
  sections: RelatedSection[];
}

/** Context the Related derivation reads (trace-invariant, F5-cached). */
export interface RelatedContext {
  allEvents: readonly CustomTraceEvent[];
  allSpans: readonly TracingSpan[];
  /** Resolve the enclosing task for an event (events-model resolveTaskForEvent). */
  taskOf: (ev: CustomTraceEvent) => number | null;
  fmtTs: (ns: number) => string;
  /** Signed offset from the anchor event (legacy fmtDelta). */
  fmtDelta: (ns: number) => string;
}

function eventsWhere(
  events: readonly CustomTraceEvent[],
  pred: (e: CustomTraceEvent) => boolean,
): CustomTraceEvent[] {
  return events.filter(pred).sort((a, b) => a.timestamp - b.timestamp);
}

function isCollapsed(
  ui: RelatedUiState,
  title: string,
  collapseByDefault: boolean,
): boolean {
  const explicit = ui.collapsed[title];
  return explicit === undefined ? collapseByDefault : explicit;
}

/**
 * Build one self-anchored event section (legacy `eventSection`): a small
 * window around the selected event grown per direction via load-more (Q6).
 * `label` derives the row text per event (defaults to the event name).
 */
function eventSection(
  ctx: RelatedContext,
  ui: RelatedUiState,
  anchor: CustomTraceEvent,
  title: string,
  sorted: readonly CustomTraceEvent[],
  label: ((e: CustomTraceEvent) => string) | null,
): RelatedSection {
  if (sorted.length === 0) {
    return {
      title,
      count: 0,
      collapsed: isCollapsed(ui, title, true),
      empty: "none",
      loadBefore: null,
      rows: [],
      loadAfter: null,
    };
  }
  const n = sorted.length;
  const anchorRaw = sorted.indexOf(anchor);
  const anchorIdx = anchorRaw < 0 ? 0 : anchorRaw;
  // Nothing beyond the current event itself -> collapse by default (legacy).
  const collapseByDefault = anchorRaw >= 0 && n === 1;
  const st = ui.expand[title] ?? { before: 0, after: 0 };
  const beforeShown = Math.min(anchorIdx, RELATED_INITIAL + st.before);
  const afterShown = Math.min(n - 1 - anchorIdx, RELATED_INITIAL + st.after);
  const start = anchorIdx - beforeShown;
  const end = anchorIdx + afterShown + 1;

  const rows: RelatedRow[] = [];
  for (const e of sorted.slice(start, end)) {
    const self = e === anchor;
    const name = label ? label(e) : e.name;
    rows.push({
      name: self ? `${name} (this event)` : name,
      aside: ctx.fmtDelta(e.timestamp - anchor.timestamp),
      padPx: 0,
      self,
      target: self ? null : { kind: "event", event: e },
    });
  }
  return {
    title,
    count: n,
    collapsed: isCollapsed(ui, title, collapseByDefault),
    empty: null,
    loadBefore: start > 0 ? loadMore("before", start) : null,
    rows,
    loadAfter: n - end > 0 ? loadMore("after", n - end) : null,
  };
}

function loadMore(dir: "before" | "after", hidden: number): RelatedLoadMore {
  return { dir, count: Math.min(RELATED_STEP, hidden), hidden };
}

/**
 * Build the Related tab view for a single pinned event (Q4-Q8). Ports
 * relatedHtml (viewer.html:4044-4167): optional field-correlation section
 * (Q3), enclosing spans indented by nesting depth, same-span / same-task /
 * same-type self-anchored windows, each collapsible (Q5) and windowed (Q6),
 * with empty placeholders (Q8). Row targets (Q7) ride on each row.
 */
export function buildRelated(
  ev: CustomTraceEvent,
  ctx: RelatedContext,
  ui: RelatedUiState,
): RelatedView {
  const sections: RelatedSection[] = [];
  const taskId = ctx.taskOf(ev);
  const spans = ctx.allSpans.length ? enclosingSpans(ctx.allSpans, ev) : [];
  const innerSpan = spans.length ? spans[spans.length - 1]! : null;

  // Field correlation (Q3), only when the value links to other events.
  if (ui.correlate) {
    const { key, val } = ui.correlate;
    const matches = eventsWhere(
      ctx.allEvents,
      (e) => !!e.fields && String(e.fields[key]) === val,
    );
    if (matches.length > 1) {
      sections.push(eventSection(ctx, ui, ev, `Same ${key}=${val}`, matches, null));
    }
  }

  // Enclosing spans (outermost first, indented by depth).
  if (spans.length === 0) {
    sections.push({
      title: "Enclosing spans",
      count: 0,
      collapsed: isCollapsed(ui, "Enclosing spans", true),
      empty: "none",
      loadBefore: null,
      rows: [],
      loadAfter: null,
    });
  } else {
    const minDepth = Math.min(...spans.map((s) => s.depth));
    const rows: RelatedRow[] = spans.map((s) => ({
      name: s.spanName || `span ${s.spanId}`,
      aside: ctx.fmtTs(s.start),
      padPx: 6 + (s.depth - minDepth) * 12,
      self: false,
      target: { kind: "span", spanId: s.spanId },
    }));
    sections.push({
      title: "Enclosing spans",
      count: spans.length,
      collapsed: isCollapsed(ui, "Enclosing spans", false),
      empty: null,
      loadBefore: null,
      rows,
      loadAfter: null,
    });
  }

  // Same span: events inside the innermost enclosing span.
  if (innerSpan) {
    const inSpan = eventsWhere(
      ctx.allEvents,
      (e) => e.timestamp >= innerSpan.start && e.timestamp <= innerSpan.end,
    );
    sections.push(eventSection(ctx, ui, ev, "Same span", inSpan, null));
  }

  // Same task.
  if (taskId == null) {
    sections.push({
      title: "Same task",
      count: null,
      collapsed: isCollapsed(ui, "Same task", true),
      empty: "task unresolved",
      loadBefore: null,
      rows: [],
      loadAfter: null,
    });
  } else {
    const forTask = eventsWhere(ctx.allEvents, (e) => ctx.taskOf(e) === taskId);
    sections.push(eventSection(ctx, ui, ev, "Same task", forTask, null));
  }

  // Same type: rows share the name, so label by resolved task instead.
  const ofType = eventsWhere(ctx.allEvents, (e) => e.name === ev.name);
  sections.push(
    eventSection(ctx, ui, ev, "Same type", ofType, (e) => {
      const t = ctx.taskOf(e);
      return t != null ? "task 0x" + t.toString(16) : e.name;
    }),
  );

  return { sections };
}

// ── Spawned tasks (M7/M8): computeSpawnedTasks formatting ─────────────────

/** One spawn-location group as the inspector renders it (M8: 5 + "N more"). */
export interface SpawnedGroupView {
  loc: string;
  /** The first `SPAWNED_TASK_HEAD` task ids as clickable hex links (M8). */
  head: readonly { taskId: number; hex: string }[];
  /** Count of tasks beyond the head (the "N more" tail). */
  moreCount: number;
}

/** The Tasks-spawned-in-range view (M7): total + range label + groups. */
export interface SpawnedTasksView {
  total: number;
  rangeLabel: string;
  groups: SpawnedGroupView[];
}

/** Legacy `showSpawnedTasks` shows up to 5 example task links per group. */
export const SPAWNED_TASK_HEAD = 5;

/**
 * Format the M7 spawned-tasks result (queue-model `computeSpawnedTasks`) for
 * the inspector (M8): each group keeps its first five task ids as clickable hex
 * links plus a "N more" tail. `rangeLabel` is the pre-formatted range duration.
 * Returns null when the range found no spawned task (legacy early return).
 */
export function buildSpawnedTasksView(
  result: {
    total: number;
    groups: readonly {
      loc: string;
      tasks: readonly { taskId: number; firstPoll: number }[];
    }[];
  } | null,
  rangeLabel: string,
): SpawnedTasksView | null {
  if (result === null) return null;
  const groups: SpawnedGroupView[] = result.groups.map((g) => ({
    loc: g.loc,
    head: g.tasks.slice(0, SPAWNED_TASK_HEAD).map((t) => ({
      taskId: t.taskId,
      hex: "0x" + t.taskId.toString(16),
    })),
    moreCount: Math.max(0, g.tasks.length - SPAWNED_TASK_HEAD),
  }));
  return { total: result.total, rangeLabel, groups };
}

// ── Tab availability (P4 families; the persistent-inspector re-scope) ──────

/** The inspector tabs (ticket title poll/event/related/stack + the T30 Task). */
export type InspectorTab = "task" | "poll" | "event" | "related" | "stack";

export const INSPECTOR_TABS: readonly InspectorTab[] = [
  "task",
  "poll",
  "event",
  "related",
  "stack",
];

/** Which tabs currently carry content (P4 `showSidebarTabs` availability). */
export interface TabAvailability {
  task: boolean;
  poll: boolean;
  event: boolean;
  related: boolean;
  stack: boolean;
}

/**
 * Compute which tabs have content for the current selection (P4). A single
 * pinned event enables Event + Related; a cluster pin enables Event only
 * (Related is single-event, Q4 CONDITIONAL). A selected task enables Task; a
 * clicked poll enables Poll; a retained range (spawned-tasks or region) enables
 * Stack.
 */
export function tabAvailability(sel: SelectionSlice): TabAvailability {
  const pinned = sel.pinnedEvent;
  return {
    task: sel.selectedTaskId !== null,
    poll: sel.pollDetail !== null,
    event: pinned !== null,
    related: pinned !== null && pinned.detailEvent !== null,
    stack: sel.spawnedTasksRange !== null || sel.sidebarRange !== null,
  };
}

/**
 * The tab a fresh selection should activate (the S4 "re-scope in the same
 * action"). Ordered by the interaction that most likely just happened: a poll
 * click -> Poll; an event pin -> Event; a range drag -> Stack; a task select ->
 * Task. Returns null when nothing is selected (the inspector shows its resting
 * at-cursor readout).
 */
export function preferredTab(sel: SelectionSlice): InspectorTab | null {
  if (sel.pollDetail !== null) return "poll";
  if (sel.pinnedEvent !== null) return "event";
  if (sel.spawnedTasksRange !== null || sel.sidebarRange !== null) return "stack";
  if (sel.selectedTaskId !== null) return "task";
  return null;
}

/** True when NO selection is active (resting inspector: readout only, F7). */
export function hasNoSelection(sel: SelectionSlice): boolean {
  return (
    sel.selectedTaskId === null &&
    sel.pollDetail === null &&
    sel.pinnedEvent === null &&
    sel.spawnedTasksRange === null &&
    sel.sidebarRange === null
  );
}

// ── Worker-lane bundle for task resolution (K7, reused from events-model) ──

/** The trace-invariant lanes the Related task resolver reads. */
export interface InspectorLaneData {
  lanes: Record<number, WorkerLane>;
  workerIds: readonly number[];
}
