// The persistent inspector sidebar component: one mount(host, store) that
// renders the tabs (Task / Poll / Event / Related / Stack) + the at-cursor
// readout, driven by the selection slice, and owns the P-row mechanics (resize
// -> uiPrefs.sidebarWidth + localStorage, tab families, body scroll, the "what
// is selected" line + Esc/clear affordance).
//
// The shell renders an EMPTY <aside class="d9-inspector"> landmark; this
// component renders its whole interior imperatively via lit-html into that
// aside, so the shell's declarative re-renders never clobber it (no child
// bindings on the aside). It re-renders on its OWN store subscription
// (selection / trace / uiPrefs / transient) inside the scheduler tick, and on
// local UI-state changes (tab switch, section toggle, load-more, frame expand)
// which render directly.
//
// The heavy derivations are the pure inspector-model plus consumed contracts:
// the Task-tab derivation, transient.atCursor (readout), selection.pinnedEvent
// (Event/Related KV), selection.taskDump (async-stack flamegraph), and
// selection.spawnedTasksRange + computeSpawnedTasks. Trace-invariant lookups
// are cached in a store.derived over the trace slice.

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
import type { RegionAnalysisController } from "./region-analysis.js";
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
  resolveTaskDumpCaptures,
  tabAvailability,
  type FrameLine,
  type InspectorTab,
  type RelatedExpandState,
  type RelatedRow,
  type RelatedSection,
  type RelatedUiState,
  type SampleGroupView,
  type PollDetailView,
} from "./inspector-model.js";
import { createFlamegraphHost } from "./flamegraph-host.js";

/** Clamp bounds for the resize drag ([200px, 92vw]). */
const MIN_WIDTH = 200;
const MAX_WIDTH_VW = 0.92;
/** localStorage key for the persisted width. */
const WIDTH_KEY = "dial9.viewer.sidebarWidth";
/** Auto-narrow width on a fresh event pin. */
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
  /** The esc-cascade to register the clear-selection surface into. */
  esc: EscCascade;
  /**
   * The region-analysis panel (flamegraph / blocking-calls / heap). The Stack
   * tab renders a binding-free `[data-region-host]` for a retained region
   * (sel.sidebarRange); this controller populates it imperatively and is synced
   * after every frame render. The inspector owns the Stack CONTAINER; the
   * region panel owns what opens.
   */
  regionPanel: RegionAnalysisController;
}

export interface MountedInspector {
  /** Tear down the store subscription + esc registration + drag listeners. */
  dispose(): void;
}

/** Trace-invariant lookups the inspector reads, cached over the trace slice. */
interface InspectorData {
  laneData: LaneData | null;
  customEvents: readonly CustomTraceEvent[];
  callframeSymbols: CallframeSymbols;
  queueData: QueueData;
  /** Enables the flamegraph's runtime filter dropdown; null before a trace. */
  runtimeWorkers: Map<string, number[]> | null;
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
  // ── Derived caches: rebuilt only when the trace slice is replaced ─────────
  const data = store.derived(["trace"], (s): InspectorData => {
    const trace = s.trace.trace;
    return {
      laneData: trace ? deriveLaneData(trace) : null,
      customEvents: trace?.customEvents ?? [],
      callframeSymbols: trace?.callframeSymbols ?? new Map(),
      queueData: computeQueueData(trace),
      runtimeWorkers: trace?.runtimeWorkers ?? null,
    };
  });
  const taskDetail = createTaskDetailDerivation(store);

  // Per-event task resolver, memoized in a WeakMap and rebuilt only when the
  // lane data changes.
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
  // re-activate the tab). Auto-activation runs only when this changes.
  let lastSelSig: string | null = null;
  // Poll Detail frame-group expansion, keyed "sched-<i>"/"cpu-<i>".
  let expandedGroups = new Set<string>();
  let lastPollRef: SelectionSlice["pollDetail"] = null;
  // Which sample section the poll flamegraph shows when both cpu + sched exist.
  // A within-poll view choice (not a cross-selection pref), so it resets when
  // the clicked poll changes. The list/flame CHOICE itself is the persisted
  // uiPrefs.stacksAsFlamegraph.
  let pollFgSection: "cpu" | "sched" = "cpu";
  // The poll tab's own flamegraph instance, hosted the same way the region
  // panel hosts its own (a SECOND live instance on the page).
  const pollFg = createFlamegraphHost({
    doc: host.ownerDocument,
    className: "d9-poll-fg",
  });
  const taskDumpFg = createFlamegraphHost({
    doc: host.ownerDocument,
    className: "d9-task-dump-fg",
  });
  const traceIds = new WeakMap<object, number>();
  let nextTraceId = 1;
  function traceId(trace: StoreState["trace"]["trace"]): number {
    if (trace === null) return 0;
    const existing = traceIds.get(trace);
    if (existing !== undefined) return existing;
    const id = nextTraceId++;
    traceIds.set(trace, id);
    return id;
  }
  // Related tab UI state; reset when the detail event changes.
  let relatedUi: RelatedUiState = { collapsed: {}, expand: {}, correlate: null };
  let lastDetailEventRef: CustomTraceEvent | null = null;
  // Resize / persistence state.
  let userResized = false;

  // Seed the persisted width: a hand-set width survives reload. When one is
  // stored, treat it as a manual resize so auto-defaults yield to it.
  const storedWidth = readStoredWidth();
  if (storedWidth !== null) {
    userResized = true;
    if (storedWidth !== store.getState().uiPrefs.sidebarWidth) {
      store.update("uiPrefs", { sidebarWidth: storedWidth });
    }
  }

  function state(): StoreState {
    return store.getState();
  }

  // ── Selection-driven tab activation (re-scope in the same action) ─────────

  function selectionSignature(sel: SelectionSlice): string {
    return [
      sel.selectedTaskId ?? "-",
      sel.pollDetail ? `${sel.pollDetail.start}-${sel.pollDetail.end}` : "-",
      sel.taskDump
        ? `${sel.taskDump.taskId}:${sel.taskDump.timestamps.join(",")}`
        : "-",
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

    // Reset Poll Detail expansion + flamegraph section when the clicked poll
    // changes.
    if (sel.pollDetail !== lastPollRef) {
      expandedGroups = new Set();
      pollFgSection = "cpu";
      lastPollRef = sel.pollDetail;
    }
    // Reset Related UI when the detail event changes (persistence is per
    // selection: collapse state cleared on reset).
    const detailEvent = sel.pinnedEvent?.detailEvent ?? null;
    if (detailEvent !== lastDetailEventRef) {
      relatedUi = { collapsed: {}, expand: {}, correlate: null };
      lastDetailEventRef = detailEvent;
    }

    // Activate the tab the just-completed interaction implies, if it has
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

    // Auto-narrow on a fresh single-event pin, unless the user set a width.
    if (!userResized && pref === "event") {
      applyWidth(EVENT_DEFAULT_WIDTH, false);
    }
  }

  // ── Timestamp formatters (honor the rel/abs time mode) ───────────────────

  function formatters(): { fmtTs: (ns: number) => string; fmtDelta: (ns: number) => string } {
    const axis = deriveAxisInputs(state());
    const fmtTs = (ns: number): string => fmtAxisTick(axis, ns, false);
    const fmtDelta = (ns: number): string =>
      (ns >= 0 ? "+" : "-") + formatHumanDuration(Math.abs(ns));
    return { fmtTs, fmtDelta };
  }

  // ── Render ────────────────────────────────────────────────────────────────
  //
  // Two render targets so the transient channel stays cheap: the FRAME (status
  // + tabs + body, incl. the heavy Poll/Related derivations) re-renders only on
  // trace/selection/uiPrefs changes; the at-cursor READOUT re-renders on the
  // high-frequency `transient` channel into its own host, so a hover never
  // re-runs buildPollDetail/buildRelated. The readout host is a binding-free
  // node inside the frame template, so a frame re-render leaves its imperative
  // content intact.

  /** Full frame render (non-transient). Also refreshes the readout host. */
  function renderFrame(): void {
    const s = state();
    reconcileSelection(s.selection);
    host.style.width = `${s.uiPrefs.sidebarWidth}px`;
    render(frameTemplate(s), host);
    renderReadout();
    // Populate the Stack tab's region-analysis host. Idempotent + a no-op
    // unless the Stack tab is showing a retained region; runs after the frame
    // render so the `[data-region-host]` node exists.
    deps.regionPanel.sync();
    // Same, for the Poll tab's flamegraph: the host node exists only after the
    // render above.
    syncPollFlamegraph(s);
    // A task-dump click also renders a flamegraph in the Stack tab from the
    // selection stored by the task-detail track.
    syncTaskDumpFlamegraph(s);
  }

  /** Readout-only render (transient channel; the at-moment surface). */
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

  // ── "What is selected" line + explicit clear affordance ──────────────────

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
    if (sel.taskDump !== null) {
      const count = sel.taskDump.timestamps.length;
      return `Task dump trace selected · ${count} capture${count === 1 ? "" : "s"}`;
    }
    if (sel.spawnedTasksRange !== null) return "Spawn-range selected";
    if (sel.sidebarRange !== null) return "Region selected";
    if (sel.selectedTaskId !== null) {
      return `Task 0x${sel.selectedTaskId.toString(16)} selected · Esc clears`;
    }
    return "No selection";
  }

  // ── At-cursor readout: the ONE at-moment surface ─────────────────────────

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

  // ── Task tab ──────────────────────────────────────────────────────────────

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
          ? kv("idle stacks", `${d.taskDumps.length} captured (flamegraph)`)
          : nothing}
      </div>
    `;
  }

  function kv(k: string, v: string): TemplateResult {
    return html`<div class="d9-kv"><span class="k">${k}</span><span class="v">${v}</span></div>`;
  }

  // ── Poll Detail tab ──────────────────────────────────────────────────────

  function pollTemplate(sel: SelectionSlice): TemplateResult {
    const poll = sel.pollDetail;
    if (poll === null) {
      return html`<p class="d9-inspector-hint">
        Click a poll carrying CPU or scheduling samples to see its blocking
        calls and CPU profile here.
      </p>`;
    }
    const view = buildPollDetail(poll, data().callframeSymbols);
    const hasStacks = view.schedGroups.length > 0 || view.cpuGroups.length > 0;
    const asFlame = state().uiPrefs.stacksAsFlamegraph;
    return html`
      <div class="d9-poll-detail">
        <div class="d9-poll-title">
          <span>${view.title}</span>
          ${hasStacks ? stacksViewToggle(asFlame) : nothing}
        </div>
        ${!hasStacks
          ? html`<p class="d9-inspector-hint">No samples captured for this poll.</p>`
          : asFlame
            ? pollFlamegraphBody(view)
            : pollListBody(view)}
      </div>
    `;
  }

  /** The list ⇄ flamegraph switch shown on any stack-carrying view. Flips the
   *  persisted uiPrefs pref, so every stack surface changes together and the
   *  choice outlives the selection. */
  function stacksViewToggle(asFlame: boolean): TemplateResult {
    return html`<span class="d9-stacks-view" role="group" aria-label="Stack view">
      <button
        type="button"
        class=${classMap({ "d9-stacks-view-btn": true, on: !asFlame })}
        aria-pressed=${!asFlame ? "true" : "false"}
        title="Show samples as a grouped list"
        @click=${() => store.update("uiPrefs", { stacksAsFlamegraph: false })}
      >
        List
      </button>
      <button
        type="button"
        class=${classMap({ "d9-stacks-view-btn": true, on: asFlame })}
        aria-pressed=${asFlame ? "true" : "false"}
        title="Show samples as a flamegraph"
        @click=${() => store.update("uiPrefs", { stacksAsFlamegraph: true })}
      >
        Flame
      </button>
    </span>`;
  }

  function pollListBody(view: PollDetailView): TemplateResult {
    return html`
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
    `;
  }

  /**
   * Flamegraph mode for the poll's samples. One instance can show one tree, so
   * when a poll carries BOTH cpu and sched samples a small section switch picks
   * which feeds the canvas; the switch is hidden when only one kind exists. The
   * `[data-poll-fg-host]` node is binding-free so the post-render sync can own
   * the canvas without lit-html reconciling it away (same technique as the
   * region host).
   */
  function pollFlamegraphBody(view: PollDetailView): TemplateResult {
    const hasCpu = view.cpuSamplesRaw.length > 0;
    const hasSched = view.schedSamplesRaw.length > 0;
    const section = activePollFgSection(view);
    return html`
      ${hasCpu && hasSched
        ? html`<div class="d9-poll-fg-sections" role="tablist">
            <button
              type="button"
              class=${classMap({ "d9-poll-fg-section": true, on: section === "cpu" })}
              role="tab"
              aria-selected=${section === "cpu" ? "true" : "false"}
              @click=${() => setPollFgSection("cpu")}
            >
              CPU (${view.cpuCount})
            </button>
            <button
              type="button"
              class=${classMap({ "d9-poll-fg-section": true, on: section === "sched" })}
              role="tab"
              aria-selected=${section === "sched" ? "true" : "false"}
              @click=${() => setPollFgSection("sched")}
            >
              Blocking (${view.schedCount})
            </button>
          </div>`
        : nothing}
      <div class="d9-poll-fg-host" data-poll-fg-host></div>
    `;
  }

  /** The section actually rendered: the stored choice when its samples exist,
   *  else whichever kind the poll has (a poll can carry only one). */
  function activePollFgSection(view: PollDetailView): "cpu" | "sched" {
    if (pollFgSection === "cpu" && view.cpuSamplesRaw.length > 0) return "cpu";
    if (pollFgSection === "sched" && view.schedSamplesRaw.length > 0) return "sched";
    return view.cpuSamplesRaw.length > 0 ? "cpu" : "sched";
  }

  function setPollFgSection(section: "cpu" | "sched"): void {
    if (pollFgSection === section) return;
    pollFgSection = section;
    renderFrame();
  }

  /**
   * Feed the poll flamegraph after the frame render (the host node exists only
   * then). A no-op unless the Poll tab is active in flame mode with samples; on
   * the list path or other tabs it detaches so the widget parks with its
   * removed host, exactly like the region panel.
   */
  function syncPollFlamegraph(s: StoreState): void {
    const poll = s.selection.pollDetail;
    if (activeTab !== "poll" || poll === null || !s.uiPrefs.stacksAsFlamegraph) {
      pollFg.detach();
      return;
    }
    const hostEl = host.querySelector<HTMLElement>("[data-poll-fg-host]");
    if (hostEl === null) {
      pollFg.detach();
      return;
    }
    const view = buildPollDetail(poll, data().callframeSymbols);
    const section = activePollFgSection(view);
    const samples = section === "cpu" ? view.cpuSamplesRaw : view.schedSamplesRaw;
    if (samples.length === 0) {
      pollFg.detach();
      return;
    }
    const d = data();
    // sig changes only when the tree would: this poll, which section, and the
    // sample count (the poll identity + section fully determine the samples).
    const sig = `${poll.start}-${poll.end}:${section}:${samples.length}`;
    pollFg.sync({
      hostEl,
      sig,
      apply: (instance) =>
        instance.setData(samples, d.callframeSymbols, {
          exportTitle: `${section === "cpu" ? "CPU" : "Blocking"} - ${view.title}`,
          runtimeWorkers: d.runtimeWorkers,
        }),
    });
  }

  function syncTaskDumpFlamegraph(s: StoreState): void {
    const selected = s.selection.taskDump;
    if (activeTab !== "stack" || selected === null) {
      taskDumpFg.detach();
      return;
    }
    const hostEl = host.querySelector<HTMLElement>("[data-task-dump-fg-host]");
    if (hostEl === null) {
      taskDumpFg.detach();
      return;
    }
    const dumps = resolveTaskDumpCaptures(s.trace.trace, selected);
    const samples = dumps.map((dump) => ({
      callchain: dump.callchain,
      workerId: 0,
    }));
    if (samples.length === 0) {
      taskDumpFg.detach();
      return;
    }
    const count = samples.length;
    const sig = `${traceId(s.trace.trace)}:${selected.taskId}:${selected.timestamps.join(",")}`;
    taskDumpFg.sync({
      hostEl,
      sig,
      apply: (instance) =>
        instance.setData(samples, data().callframeSymbols, {
          exportTitle: `Waiting on — ${count} async stack capture${count === 1 ? "" : "s"}`,
        }),
    });
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

  // ── Event tab ─────────────────────────────────────────────────────────────

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

  // ── Related tab ────────────────────────────────────────────────────────────

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
        columnarSpans: d.laneData?.columnarSpans,
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

  // ── Stack tab (task dumps + spawned tasks + region analysis) ─────────────

  function stackTemplate(sel: SelectionSlice): TemplateResult {
    if (sel.taskDump !== null) {
      const dumps = resolveTaskDumpCaptures(state().trace.trace, sel.taskDump);
      const count = dumps.length;
      if (count === 0) {
        return html`<p class="d9-inspector-hint">
          The selected task-dump captures are not present in the current trace.
        </p>`;
      }
      return html`
        <div class="d9-task-dump">
          <div class="d9-spawned-head">
            Waiting on — ${count} async stack capture${count === 1 ? "" : "s"}
          </div>
          <div class="d9-task-dump-fg-host" data-task-dump-fg-host></div>
        </div>
      `;
    }
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
      // Region select -> flamegraph / blocking-calls / heap. This is a
      // binding-free host: the region panel renders its interior (sub-tabs +
      // the embedded flamegraph widget) into it imperatively via
      // deps.regionPanel.sync(), so the inspector's re-renders never clobber
      // the canvas.
      return html`<div class="d9-region-host" data-region-host></div>`;
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
    // Set the correlation field and switch to Related (single-event only).
    relatedUi = { ...relatedUi, correlate: { key, val } };
    activeTab = "related";
    renderFrame();
  }

  function copyValue(e: MouseEvent, value: string): void {
    const btn = e.currentTarget as HTMLButtonElement;
    void navigator.clipboard?.writeText(value);
    // Flash a check, reverting after 800ms - imperative, no store.
    btn.textContent = "✓";
    window.setTimeout(() => {
      btn.textContent = "⎘";
    }, 800);
  }

  // ── Store-dispatch seams (navigation + task links) ───────────────────────

  function navigateRelated(target: RelatedRow["target"]): void {
    if (target === null) return;
    if (target.kind === "span") {
      // Focus the span + center it. Centering the viewport is the interaction
      // layer's job; the inspector dispatches the focus, which the lanes
      // consume. Center via the span's midpoint.
      const ld = data().laneData;
      const span = ld?.spanByIdSingle.get(target.spanId) ?? null;
      store.update("selection", {
        focusedSpanId: target.spanId,
        spanFocus: { spanId: target.spanId, chain: new Set([target.spanId]) },
      });
      if (span !== null) centerViewOn((span.start + span.end) / 2);
      return;
    }
    // Event row: pin + center + mark the event.
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
      taskDump: null,
    });
    centerViewOn(ev.timestamp);
  }

  /** Center the viewport on `ns`, keeping the current zoom. */
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
    // A spawned-task link selects the task (re-scopes to the Task tab).
    store.update("selection", {
      selectedTaskId: taskId,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
    });
  }

  function clearSelection(): void {
    // Explicit clear affordance - reset the inspector to its resting state.
    store.update("selection", {
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
      sidebarRange: null,
      spawnedTasksRange: null,
      hoveredWakerTaskId: null,
    });
  }

  // ── Resize drag + width persistence ──────────────────────────────────────

  function applyWidth(width: number, persist: boolean): void {
    const clamped = clampWidth(width);
    // Apply to the DOM synchronously so the track column (flex sibling)
    // reflows to the new width THIS frame, before the tracks redraw below.
    host.style.width = `${clamped}px`;
    if (clamped !== state().uiPrefs.sidebarWidth) {
      store.update("uiPrefs", { sidebarWidth: clamped });
      // Poke the viewport channel so the track-CONTENT controllers redraw at
      // the new column width in real time: lanes/spans/cpu/... subscribe to
      // `viewport`, not `uiPrefs`, so a bare sidebarWidth change re-renders the
      // shell chrome + resizes the canvases but leaves their pixels stale.
      store.update("viewport", {});
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
    // from the pointer to the aside's right edge.
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
    // Persist the final width: survives reload.
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

  // Esc: the inspector's content selection (poll/event/range) is an escapable
  // surface at the `sidebar` priority band - Esc clears it before the entry's
  // fallback clears the task selection.
  const unregisterEsc = deps.esc.register({
    name: "inspector-selection",
    priority: ESC_PRIORITY.sidebar,
    isOpen: () => {
      const sel = state().selection;
      return (
        sel.pollDetail !== null ||
        sel.taskDump !== null ||
        sel.pinnedEvent !== null ||
        sel.sidebarRange !== null ||
        sel.spawnedTasksRange !== null
      );
    },
    close: () => {
      store.update("selection", {
        pinnedEvent: null,
        pollDetail: null,
        taskDump: null,
        sidebarRange: null,
        spawnedTasksRange: null,
      });
    },
  });

  // Initial paint so the aside is populated before the first store tick.
  renderFrame();

  return {
    dispose(): void {
      unsubFrame();
      unsubReadout();
      unregisterEsc();
      pollFg.destroy();
      taskDumpFg.destroy();
      window.removeEventListener("mousemove", onResizeMove);
      window.removeEventListener("mouseup", onResizeUp);
    },
  };
}

// ── Width persistence helpers ───────────────────────────────────────────────

function clampWidth(width: number): number {
  const maxW =
    typeof window !== "undefined" ? window.innerWidth * MAX_WIDTH_VW : Infinity;
  return Math.round(Math.max(MIN_WIDTH, Math.min(maxW, width)));
}

/** Read the persisted inspector width; null when absent/unavailable. */
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
