// src/pages/viewer/inspector.ts - the persistent inspector sidebar component
// (T31; features/02 sections P/Q/R + amendments S4/F7). Concept-1's PERSISTENT
// inspector (settled after the sidebar-drift history #291/#304/#305): one
// mount(host, store) that renders the tabs (Task / Poll / Event / Related /
// Stack) + the at-cursor readout, driven by the selection slice, and owns the
// P-row mechanics (resize -> uiPrefs.sidebarWidth + localStorage, tab families,
// body scroll, the F7 "what is selected" line + Esc/clear affordance).
//
// Rendering model (mirrors toasts / the T24 overlay): the shell renders an
// EMPTY <aside class="d9-inspector"> landmark; this component renders its whole
// interior imperatively via lit-html into that aside, so the shell's
// declarative re-renders never clobber it (no child bindings on the aside).
// It re-renders on its OWN store subscription (selection / trace / uiPrefs /
// transient) - inside the scheduler tick - and on local UI-state changes (tab
// switch, section toggle, load-more, frame expand) which render directly, like
// the events-track tooltip and toasts (not a store-driven track redraw, so no
// N18 assertion applies).
//
// The heavy derivations are the pure inspector-model (Poll Detail, Event,
// Related) + the CONSUMED contracts (never reimplemented here): T30's
// createTaskDetailDerivation (Task tab), T24's transient.atCursor (readout,
// 04 S4 - this replaces T24's stub), T27's selection.pinnedEvent (Event/Related
// KV), T29's selection.spawnedTasksRange + computeSpawnedTasks (M7/M8). Trace-
// invariant lookups (lanes, custom events, symbols, queue data) are cached in a
// store.derived over the trace slice (F5): a pan/selection never rebuilds them.

import { html, render, nothing, type TemplateResult } from "lit-html";
import { classMap } from "lit-html/directives/class-map.js";
import { deriveLaneData } from "../../components/canvas/lanes/index.js";
import type { LaneData } from "../../components/canvas/lanes/index.js";
import { resolveTaskForEvent } from "./events-model.js";
import { computeQueueData, computeSpawnedTasks } from "./queue-model.js";
import type { QueueData } from "./queue-model.js";
import { createTaskDetailDerivation } from "./task-detail-track.js";
import type { TaskDetailData } from "./task-detail-model.js";
import { deriveAxisInputs, fmtAxisTick } from "./axis.js";
import { formatHumanDuration } from "../../lib/trace/index.js";
import type {
  CallframeSymbols,
  CustomTraceEvent,
} from "../../lib/trace/index.js";
import { ESC_PRIORITY } from "./esc-cascade.js";
import type { EscCascade } from "./esc-cascade.js";
import type { ViewerStore } from "../../store/store.js";
import type { AtCursorReadout, SelectionSlice, StoreState } from "../../types/state.js";
import {
  INSPECTOR_TABS,
  buildEventDetail,
  buildPollDetail,
  buildRelated,
  buildSpawnedTasksView,
  hasNoSelection,
  preferredTab,
  tabAvailability,
  type FrameLine,
  type InspectorTab,
  type RelatedExpandState,
  type RelatedRow,
  type RelatedSection,
  type RelatedUiState,
  type SampleGroupView,
} from "./inspector-model.js";

/** Clamp bounds for the resize drag (legacy [200px, 92vw], viewer.html:P2). */
const MIN_WIDTH = 200;
const MAX_WIDTH_VW = 0.92;
/** localStorage key for the persisted width (P8; the legacy dial9.viewer.* namespace). */
const WIDTH_KEY = "dial9.viewer.sidebarWidth";
/** Auto-narrow width on a fresh event pin (legacy EVENT_DEFAULT_WIDTH, P5). */
const EVENT_DEFAULT_WIDTH = 350;

const TAB_LABELS: Record<InspectorTab, string> = {
  task: "Task",
  poll: "Poll",
  event: "Event",
  related: "Related",
  stack: "Stack",
};

/** Callbacks the inspector needs from the page entry. */
export interface InspectorDeps {
  /** The esc-cascade to register the F7 clear-selection surface into. */
  esc: EscCascade;
}

export interface MountedInspector {
  /** Force one render (used after mount). */
  refresh(): void;
  /** Tear down the store subscription + esc registration + drag listeners. */
  dispose(): void;
}

/** Trace-invariant lookups the inspector reads, cached over the trace slice. */
interface InspectorData {
  laneData: LaneData | null;
  customEvents: readonly CustomTraceEvent[];
  callframeSymbols: CallframeSymbols;
  queueData: QueueData;
}

/**
 * Mount the persistent inspector into `host` (the shell's empty aside), wired
 * to `store`. Returns teardown handles. Idempotent renders; a single mount per
 * page (like the other content components in main.ts).
 */
export function mountInspector(
  host: HTMLElement,
  store: ViewerStore,
  deps: InspectorDeps,
): MountedInspector {
  // ── Derived caches (F5): rebuilt only when the trace slice is replaced ────
  const data = store.derived(["trace"], (s): InspectorData => {
    const trace = s.trace.trace;
    return {
      laneData: trace ? deriveLaneData(trace) : null,
      customEvents: trace?.customEvents ?? [],
      callframeSymbols: trace?.callframeSymbols ?? new Map(),
      queueData: computeQueueData(trace),
    };
  });
  const taskDetail = createTaskDetailDerivation(store);

  // Per-event task resolver (K7), memoized in a WeakMap and rebuilt only when
  // the lane data changes (mirrors events-track's taskCache).
  let taskCache: { key: LaneData; fn: (ev: CustomTraceEvent) => number | null } | null =
    null;
  function taskResolver(): (ev: CustomTraceEvent) => number | null {
    const ld = data().laneData;
    if (ld === null) return () => null;
    if (taskCache !== null && taskCache.key === ld) return taskCache.fn;
    const memo = new WeakMap<CustomTraceEvent, number | null>();
    const fn = (ev: CustomTraceEvent): number | null => {
      const cached = memo.get(ev);
      if (cached !== undefined) return cached;
      const t = resolveTaskForEvent(ev, ld.workerSpans, ld.workerIds);
      memo.set(ev, t);
      return t;
    };
    taskCache = { key: ld, fn };
    return fn;
  }

  // ── Local UI state (not store state) ─────────────────────────────────────
  let activeTab: InspectorTab = "task";
  // Selection "activation signature": the fields that decide which tab to open
  // on a fresh selection (NOT hoveredWakerTaskId - a waker hover must not
  // re-activate the tab). Auto-activation runs only when this changes (S4).
  let lastSelSig: string | null = null;
  // Poll Detail frame-group expansion (R2), keyed "sched-<i>"/"cpu-<i>".
  let expandedGroups = new Set<string>();
  let lastPollRef: SelectionSlice["pollDetail"] = null;
  // Related tab UI state (Q3/Q5/Q6); reset when the detail event changes.
  let relatedUi: RelatedUiState = { collapsed: {}, expand: {}, correlate: null };
  let lastDetailEventRef: CustomTraceEvent | null = null;
  // Resize / persistence state (P2/P8/P5).
  let userResized = false;

  // Seed the persisted width (P8): a hand-set width survives reload. When one
  // is stored, treat it as a manual resize so P5/P6 auto-defaults yield to it.
  const storedWidth = readStoredWidth();
  if (storedWidth !== null) {
    userResized = true;
    if (storedWidth !== store.getState().uiPrefs.sidebarWidth) {
      store.update("uiPrefs", { sidebarWidth: storedWidth });
    }
  }

  function state(): StoreState {
    return store.getState() as StoreState;
  }

  // ── Selection-driven tab activation (S4: re-scope in the same action) ─────

  function selectionSignature(sel: SelectionSlice): string {
    return [
      sel.selectedTaskId ?? "-",
      sel.pollDetail ? `${sel.pollDetail.start}-${sel.pollDetail.end}` : "-",
      sel.pinnedEvent ? `${sel.pinnedEvent.timestamp}:${sel.pinnedEvent.events.length}` : "-",
      sel.pinnedEvent?.detailEvent ? sel.pinnedEvent.detailEvent.timestamp : "-",
      sel.spawnedTasksRange ? `${sel.spawnedTasksRange.startNs}-${sel.spawnedTasksRange.endNs}` : "-",
      sel.sidebarRange ? `${sel.sidebarRange.startNs}-${sel.sidebarRange.endNs}` : "-",
    ].join("|");
  }

  /** React to a genuinely-new selection: reset per-selection UI, auto-activate. */
  function reconcileSelection(sel: SelectionSlice): void {
    const sig = selectionSignature(sel);
    if (sig === lastSelSig) return;
    lastSelSig = sig;

    // Reset Poll Detail expansion when the clicked poll changes (R2).
    if (sel.pollDetail !== lastPollRef) {
      expandedGroups = new Set();
      lastPollRef = sel.pollDetail;
    }
    // Reset Related UI when the detail event changes (Q5 persistence is per
    // selection: relatedCollapsed cleared on reset, legacy).
    const detailEvent = sel.pinnedEvent?.detailEvent ?? null;
    if (detailEvent !== lastDetailEventRef) {
      relatedUi = { collapsed: {}, expand: {}, correlate: null };
      lastDetailEventRef = detailEvent;
    }

    // S4: activate the tab the just-completed interaction implies, if it has
    // content. A manual tab switch (no selection change) is preserved because
    // this only runs on a signature change.
    const pref = preferredTab(sel);
    const avail = tabAvailability(sel);
    if (pref !== null && avail[pref]) {
      activeTab = pref;
    } else if (!avail[activeTab] && pref === null) {
      // Selection fully cleared: fall back to Task (resting readout otherwise).
      activeTab = "task";
    }

    // P5: auto-narrow on a fresh single-event pin, unless the user set a width.
    if (!userResized && pref === "event") {
      applyWidth(EVENT_DEFAULT_WIDTH, false);
    }
  }

  // ── Timestamp formatters (W1: honor the rel/abs time mode) ───────────────

  function formatters(): { fmtTs: (ns: number) => string; fmtDelta: (ns: number) => string } {
    const axis = deriveAxisInputs(state());
    const fmtTs = (ns: number): string => fmtAxisTick(axis, ns, false);
    const fmtDelta = (ns: number): string =>
      (ns >= 0 ? "+" : "-") + formatHumanDuration(Math.abs(ns));
    return { fmtTs, fmtDelta };
  }

  // ── Render ────────────────────────────────────────────────────────────────
  //
  // Two render targets so the transient channel stays cheap (architecture
  // 2.2/2.3): the FRAME (status + tabs + body, incl. the heavy Poll/Related
  // derivations) re-renders only on trace/selection/uiPrefs changes; the
  // at-cursor READOUT re-renders on the high-frequency `transient` channel into
  // its own host, so a hover never re-runs buildPollDetail/buildRelated. The
  // readout host is a binding-free node inside the frame template, so a frame
  // re-render leaves its imperative content intact (the toast-region technique).

  /** Full frame render (non-transient). Also refreshes the readout host. */
  function renderFrame(): void {
    const s = state();
    reconcileSelection(s.selection);
    host.style.width = `${s.uiPrefs.sidebarWidth}px`;
    render(frameTemplate(s), host);
    renderReadout();
  }

  /** Readout-only render (transient channel; the S4 at-moment surface). */
  function renderReadout(): void {
    const slot = host.querySelector<HTMLElement>(".d9-atcursor-host");
    if (slot === null) return;
    render(readoutTemplate(state().transient.atCursor), slot);
  }

  function frameTemplate(s: StoreState): TemplateResult {
    const avail = tabAvailability(s.selection);
    return html`
      <div
        class="d9-inspector-resize"
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize inspector"
        @mousedown=${onResizeDown}
      ></div>
      <div class="d9-inspector-inner">
        ${statusTemplate(s.selection)}
        <div class="d9-atcursor-host"></div>
        <div class="d9-inspector-tabs" role="tablist" aria-label="Inspector tabs">
          ${INSPECTOR_TABS.map((t) => tabButton(t, avail[t]))}
        </div>
        <div
          class="d9-inspector-body"
          role="tabpanel"
          aria-label="${TAB_LABELS[activeTab]} detail"
          tabindex="0"
        >
          ${bodyTemplate(s)}
        </div>
      </div>
    `;
  }

  function tabButton(tab: InspectorTab, enabled: boolean): TemplateResult {
    const on = tab === activeTab;
    return html`<button
      type="button"
      class=${classMap({ "d9-inspector-tab": true, on, disabled: !enabled })}
      role="tab"
      aria-selected=${on ? "true" : "false"}
      ?disabled=${!enabled}
      @click=${() => selectTab(tab)}
    >
      ${TAB_LABELS[tab]}
    </button>`;
  }

  // ── F7: "what is selected" line + explicit clear affordance ──────────────

  function statusTemplate(sel: SelectionSlice): TemplateResult {
    const label = selectionLabel(sel);
    const empty = hasNoSelection(sel);
    return html`
      <div class="d9-inspector-status" role="status">
        <span class="d9-inspector-sel">${label}</span>
        ${empty
          ? nothing
          : html`<button
              type="button"
              class="d9-inspector-clear"
              title="Clear selection (Esc)"
              aria-label="Clear selection"
              @click=${clearSelection}
            >
              ✕ clear
            </button>`}
      </div>
    `;
  }

  function selectionLabel(sel: SelectionSlice): string {
    if (sel.pollDetail !== null) {
      return `Poll ${formatHumanDuration(sel.pollDetail.end - sel.pollDetail.start)} selected`;
    }
    if (sel.pinnedEvent !== null) {
      const p = sel.pinnedEvent;
      return p.events.length > 1
        ? `Cluster of ${p.events.length} events selected`
        : `Event ${p.name} selected`;
    }
    if (sel.spawnedTasksRange !== null) return "Spawn-range selected";
    if (sel.sidebarRange !== null) return "Region selected";
    if (sel.selectedTaskId !== null) {
      return `Task 0x${sel.selectedTaskId.toString(16)} selected · Esc clears`;
    }
    return "No selection";
  }

  // ── S4 at-cursor readout (I6): the ONE at-moment surface ─────────────────

  function readoutTemplate(readout: AtCursorReadout | null): TemplateResult {
    if (readout === null) {
      return html`<div class="d9-atcursor" aria-label="At-cursor stats">
        <span class="d9-atcursor-empty">Hover the timeline for at-cursor stats</span>
      </div>`;
    }
    const num = (v: number | null): string => (v !== null ? String(v) : "-");
    const cov =
      readout.coverage === "truncated"
        ? html`<span class="d9-atcursor-warn">partial window</span>`
        : readout.coverage === "oversized"
          ? html`<span class="d9-atcursor-warn">oversized segment</span>`
          : nothing;
    return html`
      <div class="d9-atcursor" aria-label="At-cursor stats">
        <span class="d9-atcursor-title">at cursor</span>
        <span class="k">worker</span><span class="v">${num(readout.workerId)}</span>
        <span class="k">global Q</span><span class="v">${num(readout.globalQueue)}</span>
        <span class="k">local max</span><span class="v">${num(readout.localMax)}</span>
        ${readout.activeTaskCount !== null
          ? html`<span class="k">active tasks</span><span class="v"
                >${readout.activeTaskCount}</span
              >`
          : nothing}
        ${cov}
      </div>
    `;
  }

  // ── Tab body ──────────────────────────────────────────────────────────────

  function bodyTemplate(s: StoreState): TemplateResult {
    switch (activeTab) {
      case "task":
        return taskTemplate(taskDetail());
      case "poll":
        return pollTemplate(s.selection);
      case "event":
        return eventTemplate(s.selection);
      case "related":
        return relatedTemplate(s.selection);
      case "stack":
        return stackTemplate(s.selection);
    }
  }

  // ── Task tab (T30 derivation; N2/N3/N4) ──────────────────────────────────

  function taskTemplate(d: TaskDetailData): TemplateResult {
    if (d.taskId === null || !d.hasPolls) {
      return html`<p class="d9-inspector-hint">
        Click a task in the worker lanes to inspect its polls, wakes, and
        lifetime here.
      </p>`;
    }
    return html`
      <div class="d9-task-detail">
        <div class="d9-task-detail-head">
          Task 0x${d.taskId.toString(16)}
          ${d.isInstrumented
            ? nothing
            : html`<span class="d9-badge" title="Spawned via raw tokio::spawn"
                >uninstrumented</span
              >`}
        </div>
        ${d.spawnLocation != null ? kv("spawn", d.spawnLocation) : nothing}
        ${kv("polls", String(d.pollCount))}
        ${d.isInstrumented ? kv("wakes", String(d.wakeCount)) : nothing}
        ${d.lifetimeNs != null ? kv("lifetime", formatHumanDuration(d.lifetimeNs)) : nothing}
        ${kv("status", d.hasTerminate ? "completed ✓" : "running")}
        ${d.taskDumps.length > 0
          ? kv("idle stacks", `${d.taskDumps.length} captured (flamegraph: T32)`)
          : nothing}
      </div>
    `;
  }

  function kv(k: string, v: string): TemplateResult {
    return html`<div class="d9-kv"><span class="k">${k}</span><span class="v">${v}</span></div>`;
  }

  // ── Poll Detail tab (R1/R2): the behavioral-diff gate ────────────────────

  function pollTemplate(sel: SelectionSlice): TemplateResult {
    const poll = sel.pollDetail;
    if (poll === null) {
      return html`<p class="d9-inspector-hint">
        Click a poll carrying CPU or scheduling samples to see its blocking
        calls and CPU profile here.
      </p>`;
    }
    const view = buildPollDetail(poll, data().callframeSymbols);
    return html`
      <div class="d9-poll-detail">
        <div class="d9-poll-title">${view.title}</div>
        ${view.schedGroups.length > 0
          ? html`<div class="d9-poll-sched-head">
                ⚠ Blocking during poll (${view.schedCount} sched
                event${view.schedCount > 1 ? "s" : ""})
              </div>
              ${view.schedGroups.map((g, i) => sampleGroup(g, "sched", i))}`
          : nothing}
        ${view.cpuGroups.length > 0
          ? html`${view.schedCount > 0
                ? html`<div class="d9-poll-cpu-head">
                    CPU Profile (${view.cpuCount} sample${view.cpuCount > 1 ? "s" : ""})
                  </div>`
                : nothing}
              ${view.cpuGroups.map((g, i) => sampleGroup(g, "cpu", i))}`
          : nothing}
        ${view.schedGroups.length === 0 && view.cpuGroups.length === 0
          ? html`<p class="d9-inspector-hint">No samples captured for this poll.</p>`
          : nothing}
      </div>
    `;
  }

  function sampleGroup(g: SampleGroupView, kind: "sched" | "cpu", idx: number): TemplateResult {
    const key = `${kind}-${idx}`;
    const expanded = expandedGroups.has(key);
    return html`
      <div class=${classMap({ "d9-sample-group": true, sched: kind === "sched", cpu: kind === "cpu" })}>
        <div class="d9-sample-head">
          <span class="d9-sample-count">${g.count}×</span>
          <span class="d9-sample-leaf">${g.leaf}</span>
          <span class="d9-sample-pct">(${g.pct}%)</span>
          ${kind === "sched"
            ? html`<span class="d9-sample-bar" style="width:${g.barW}px"></span>`
            : nothing}
        </div>
        ${g.headFrames.map((f, i) => frameRow(f, kind === "sched" && i === 0))}
        ${g.moreFrames.length > 0
          ? html`<button
                type="button"
                class="d9-frame-toggle"
                @click=${() => toggleGroup(key)}
              >
                ${expanded ? "▼ collapse" : `▶ ${g.moreFrames.length} more frames`}
              </button>
              ${expanded ? g.moreFrames.map((f) => frameRow(f, false, true)) : nothing}`
          : nothing}
      </div>
    `;
  }

  function frameRow(f: FrameLine, leaf: boolean, deep = false): TemplateResult {
    const cls = classMap({ "d9-frame": true, leaf, deep });
    const text =
      f.docsUrl !== null
        ? html`<a href=${f.docsUrl} target="_blank" rel="noreferrer">${f.text}</a>`
        : f.text;
    return html`<div class=${cls}>${text}</div>`;
  }

  // ── Event tab (Q1/Q2/Q3) ─────────────────────────────────────────────────

  function eventTemplate(sel: SelectionSlice): TemplateResult {
    const pinned = sel.pinnedEvent;
    if (pinned === null) {
      return html`<p class="d9-inspector-hint">
        Click a custom-event marker to see its fields here.
      </p>`;
    }
    const { fmtTs } = formatters();
    const view = buildEventDetail(pinned, data().customEvents, fmtTs);
    return html`
      <div class="d9-event-detail">
        <div class="d9-event-title">${view.title}</div>
        ${view.rows.map((r) => eventRow(r.key, r.value, r.corrVal))}
      </div>
    `;
  }

  function eventRow(key: string, value: string, corrVal: string | null): TemplateResult {
    return html`<div class="d9-kv-row">
      <span class="k">${key}</span><span class="v">${value}</span>
      ${corrVal !== null
        ? html`<button
            type="button"
            class="d9-kv-corr"
            title="Find events with ${key}=${corrVal}"
            aria-label="Correlate ${key}"
            @click=${() => correlate(key, corrVal)}
          >
            ↔
          </button>`
        : nothing}
      <button
        type="button"
        class="d9-kv-copy"
        title="Copy value"
        aria-label="Copy ${key}"
        @click=${(e: MouseEvent) => copyValue(e, value)}
      >
        ⎘
      </button>
    </div>`;
  }

  // ── Related tab (Q4-Q8) ──────────────────────────────────────────────────

  function relatedTemplate(sel: SelectionSlice): TemplateResult {
    const ev = sel.pinnedEvent?.detailEvent ?? null;
    if (ev === null) {
      return html`<p class="d9-inspector-hint">
        Select a single custom event to see related events, spans, and tasks.
      </p>`;
    }
    const d = data();
    const { fmtTs, fmtDelta } = formatters();
    const view = buildRelated(
      ev,
      {
        allEvents: d.customEvents,
        allSpans: d.laneData?.allSpans ?? [],
        taskOf: taskResolver(),
        fmtTs,
        fmtDelta,
      },
      relatedUi,
    );
    return html`<div class="d9-related">${view.sections.map(relatedSection)}</div>`;
  }

  function relatedSection(sec: RelatedSection): TemplateResult {
    return html`
      <div class="d9-related-section">
        <button
          type="button"
          class="d9-related-head"
          aria-expanded=${sec.collapsed ? "false" : "true"}
          @click=${() => toggleSection(sec.title, !sec.collapsed)}
        >
          ${sec.collapsed ? "▶" : "▼"} ${sec.title}${sec.count != null ? ` (${sec.count})` : ""}
        </button>
        ${sec.collapsed
          ? nothing
          : html`<div class="d9-related-body">
              ${sec.empty !== null
                ? html`<div class="d9-related-empty">${sec.empty}</div>`
                : nothing}
              ${sec.loadBefore
                ? loadMoreRow(sec.title, "before", sec.loadBefore.count, sec.loadBefore.hidden)
                : nothing}
              ${sec.rows.map(relatedRow)}
              ${sec.loadAfter
                ? loadMoreRow(sec.title, "after", sec.loadAfter.count, sec.loadAfter.hidden)
                : nothing}
            </div>`}
      </div>
    `;
  }

  function relatedRow(row: RelatedRow): TemplateResult {
    const cls = classMap({ "d9-related-row": true, "r-self": row.self });
    if (row.self || row.target === null) {
      return html`<div class=${cls} style="padding-left:${row.padPx}px">
        <span class="r-name">${row.name}</span><span class="r-aside">${row.aside}</span>
      </div>`;
    }
    const target = row.target;
    return html`<div
      class=${cls}
      role="button"
      tabindex="0"
      style="padding-left:${row.padPx}px"
      @click=${() => navigateRelated(target)}
      @keydown=${(e: KeyboardEvent) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          navigateRelated(target);
        }
      }}
    >
      <span class="r-name">${row.name}</span><span class="r-aside">${row.aside}</span>
    </div>`;
  }

  function loadMoreRow(
    title: string,
    dir: "before" | "after",
    count: number,
    hidden: number,
  ): TemplateResult {
    const arrow = dir === "before" ? "↑" : "↓";
    const word = dir === "before" ? "earlier" : "later";
    return html`<button
      type="button"
      class="d9-related-loadmore"
      @click=${() => loadMore(title, dir)}
    >
      ${arrow} load ${count} more ${word} (${hidden} hidden)
    </button>`;
  }

  // ── Stack tab (M7/M8 spawned tasks; the T32 region-analysis seam) ────────

  function stackTemplate(sel: SelectionSlice): TemplateResult {
    if (sel.spawnedTasksRange !== null) {
      const range = sel.spawnedTasksRange;
      const result = computeSpawnedTasks(data().queueData, range);
      const rangeLabel = formatHumanDuration(range.endNs - range.startNs);
      const view = buildSpawnedTasksView(result, rangeLabel);
      if (view === null) {
        return html`<p class="d9-inspector-hint">
          No tasks were first polled in the selected ${rangeLabel} range.
        </p>`;
      }
      return html`
        <div class="d9-spawned">
          <div class="d9-spawned-head">
            ${view.total} task${view.total > 1 ? "s" : ""} spawned in ${view.rangeLabel}
          </div>
          ${view.groups.map(
            (g) => html`
              <div class="d9-spawned-group">
                <div class="d9-spawned-loc">${g.loc}</div>
                <div class="d9-spawned-tasks">
                  ${g.head.map(
                    (t) => html`<button
                      type="button"
                      class="d9-task-link"
                      @click=${() => selectTask(t.taskId)}
                    >
                      ${t.hex}
                    </button>`,
                  )}
                  ${g.moreCount > 0
                    ? html`<span class="d9-spawned-more">+${g.moreCount} more</span>`
                    : nothing}
                </div>
              </div>
            `,
          )}
        </div>
      `;
    }
    if (sel.sidebarRange !== null) {
      // Region select -> flamegraph / blocking-calls / heap is T32's surface
      // (it owns "what opens by data present"; deps on this inspector shell).
      const range = sel.sidebarRange;
      return html`<p class="d9-inspector-hint">
        Region selected (${formatHumanDuration(range.endNs - range.startNs)}).
        Flamegraph / blocking-calls / heap analysis opens here (T32).
      </p>`;
    }
    return html`<p class="d9-inspector-hint">
      Drag-select on the queue track to list tasks spawned in a range, or
      Shift-drag the timeline for a region analysis.
    </p>`;
  }

  // ── UI-state mutations (local; render directly, not via the store) ───────

  function selectTab(tab: InspectorTab): void {
    if (tab === activeTab) return;
    activeTab = tab;
    renderFrame();
  }

  function toggleGroup(key: string): void {
    if (expandedGroups.has(key)) expandedGroups.delete(key);
    else expandedGroups.add(key);
    renderFrame();
  }

  function toggleSection(title: string, collapsed: boolean): void {
    relatedUi = {
      ...relatedUi,
      collapsed: { ...relatedUi.collapsed, [title]: collapsed },
    };
    renderFrame();
  }

  function loadMore(title: string, dir: "before" | "after"): void {
    const cur: RelatedExpandState = relatedUi.expand[title] ?? { before: 0, after: 0 };
    const next: RelatedExpandState =
      dir === "before"
        ? { before: cur.before + 25, after: cur.after }
        : { before: cur.before, after: cur.after + 25 };
    relatedUi = { ...relatedUi, expand: { ...relatedUi.expand, [title]: next } };
    renderFrame();
  }

  function correlate(key: string, val: string): void {
    // Q3: set the correlation field and switch to Related (single-event only).
    relatedUi = { ...relatedUi, correlate: { key, val } };
    activeTab = "related";
    renderFrame();
  }

  function copyValue(e: MouseEvent, value: string): void {
    const btn = e.currentTarget as HTMLButtonElement;
    void navigator.clipboard?.writeText(value);
    // Flash a check (Q2), reverting after 800ms - imperative, no store.
    btn.textContent = "✓";
    window.setTimeout(() => {
      btn.textContent = "⎘";
    }, 800);
  }

  // ── Store-dispatch seams (Q7 navigation, M8 task links) ──────────────────

  function navigateRelated(target: RelatedRow["target"]): void {
    if (target === null) return;
    if (target.kind === "span") {
      // Focus the span + center it (Q7). Centering the viewport is the
      // interaction layer's job; the inspector dispatches the focus, which the
      // lanes consume. Center via the span's midpoint.
      const ld = data().laneData;
      const span = ld?.spanByIdSingle.get(target.spanId) ?? null;
      store.update("selection", {
        focusedSpanId: target.spanId,
        spanFocus: { spanId: target.spanId, chain: new Set([target.spanId]) },
      });
      if (span !== null) centerViewOn((span.start + span.end) / 2);
      return;
    }
    // Event row: pin + center + mark the event (Q7).
    const ev = target.event;
    const taskId = taskResolver()(ev);
    store.update("selection", {
      pinnedEvent: {
        events: [ev],
        timestamp: ev.timestamp,
        taskId,
        name: ev.name,
        poll: null,
        detailEvent: ev,
      },
      selectedTaskId: null,
      pollDetail: null,
    });
    centerViewOn(ev.timestamp);
  }

  /** Center the viewport on `ns`, keeping the current zoom (legacy centerView). */
  function centerViewOn(ns: number): void {
    const vp = state().viewport;
    const span = vp.viewEnd - vp.viewStart;
    if (span <= 0) return;
    let start = ns - span / 2;
    let end = ns + span / 2;
    if (start < vp.minTs) {
      start = vp.minTs;
      end = start + span;
    }
    if (end > vp.maxTs) {
      end = vp.maxTs;
      start = end - span;
    }
    store.update("viewport", { viewStart: Math.max(vp.minTs, start), viewEnd: Math.min(vp.maxTs, end) });
  }

  function selectTask(taskId: number): void {
    // M8: a spawned-task link selects the task (re-scopes to the Task tab).
    store.update("selection", { selectedTaskId: taskId, pinnedEvent: null, pollDetail: null });
  }

  function clearSelection(): void {
    // F7: explicit clear affordance - reset the inspector to its resting state.
    store.update("selection", {
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      sidebarRange: null,
      spawnedTasksRange: null,
      hoveredWakerTaskId: null,
    });
  }

  // ── Resize drag (P2) + width persistence (P8) ────────────────────────────

  function applyWidth(width: number, persist: boolean): void {
    const clamped = clampWidth(width);
    if (clamped !== state().uiPrefs.sidebarWidth) {
      store.update("uiPrefs", { sidebarWidth: clamped });
    }
    if (persist) writeStoredWidth(clamped);
  }

  let dragging = false;
  function onResizeDown(e: MouseEvent): void {
    e.preventDefault();
    dragging = true;
    userResized = true;
    document.body.style.userSelect = "none";
    document.body.style.cursor = "col-resize";
    window.addEventListener("mousemove", onResizeMove);
    window.addEventListener("mouseup", onResizeUp);
  }
  function onResizeMove(e: MouseEvent): void {
    if (!dragging) return;
    // The inspector is on the right; dragging its left handle sets the width
    // from the pointer to the aside's right edge (legacy P2).
    const right = host.getBoundingClientRect().right;
    applyWidth(right - e.clientX, false);
  }
  function onResizeUp(): void {
    if (!dragging) return;
    dragging = false;
    document.body.style.userSelect = "";
    document.body.style.cursor = "";
    window.removeEventListener("mousemove", onResizeMove);
    window.removeEventListener("mouseup", onResizeUp);
    // Persist the final width (P8): survives reload.
    writeStoredWidth(state().uiPrefs.sidebarWidth);
  }

  // ── Store subscription + esc registration ────────────────────────────────

  // Frame re-render on the content slices; readout-only re-render on the
  // high-frequency transient channel (so a hover never re-runs the tab
  // derivations - the split above).
  const unsubFrame = store.subscribe(
    ["trace", "selection", "uiPrefs"],
    () => renderFrame(),
  );
  const unsubReadout = store.subscribe(["transient"], () => renderReadout());

  // F7 / Esc: the inspector's content selection (poll/event/range) is an
  // escapable surface at the `sidebar` priority band - Esc clears it before the
  // entry's fallback clears the task selection (the legacy D9 order).
  const unregisterEsc = deps.esc.register({
    name: "inspector-selection",
    priority: ESC_PRIORITY.sidebar,
    isOpen: () => {
      const sel = state().selection;
      return (
        sel.pollDetail !== null ||
        sel.pinnedEvent !== null ||
        sel.sidebarRange !== null ||
        sel.spawnedTasksRange !== null
      );
    },
    close: () => {
      store.update("selection", {
        pinnedEvent: null,
        pollDetail: null,
        sidebarRange: null,
        spawnedTasksRange: null,
      });
    },
  });

  // Initial paint so the aside is populated before the first store tick.
  renderFrame();

  return {
    refresh: renderFrame,
    dispose(): void {
      unsubFrame();
      unsubReadout();
      unregisterEsc();
      window.removeEventListener("mousemove", onResizeMove);
      window.removeEventListener("mouseup", onResizeUp);
    },
  };
}

// ── Width persistence helpers (P8) ──────────────────────────────────────────

function clampWidth(width: number): number {
  const maxW =
    typeof window !== "undefined" ? window.innerWidth * MAX_WIDTH_VW : Infinity;
  return Math.round(Math.max(MIN_WIDTH, Math.min(maxW, width)));
}

/** Read the persisted inspector width (P8); null when absent/unavailable. */
function readStoredWidth(): number | null {
  try {
    const raw = window.localStorage?.getItem(WIDTH_KEY);
    if (raw === null || raw === undefined) return null;
    const n = Number(raw);
    return Number.isFinite(n) && n >= MIN_WIDTH ? n : null;
  } catch {
    // localStorage can throw (private mode / disabled). One-time read on mount,
    // not a loop; degrade to the default width silently.
    return null;
  }
}

function writeStoredWidth(width: number): void {
  try {
    window.localStorage?.setItem(WIDTH_KEY, String(width));
  } catch {
    // Persistence unavailable (private mode): the width still applies in-store
    // for this session, it just will not survive reload. One-time per drag-end.
  }
}
