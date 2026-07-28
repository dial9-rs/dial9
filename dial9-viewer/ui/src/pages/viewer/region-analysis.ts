// The in-viewer region-analysis panel: renders the CPU flamegraph / blocking
// calls / heap flamegraph for a Shift+drag region (or a whole-trace toolbar
// open) into the inspector's Stack tab, driving the flamegraph widget through
// its public surface (createFlamegraph/setData/getZoomPath).
//
// The inspector renders a binding-free `<div data-region-host>`; this controller
// renders the panel interior into it imperatively, so the inspector's lit-html
// re-renders never clobber the widget. A single fgContainer + fgInstance persist
// across mode switches and Stack-tab hide/show (parked while detached, resized
// on re-attach). The heavy derivations live in the pure region-analysis-model.

import { html, render, nothing, type TemplateResult } from "lit-html";
import { classMap } from "lit-html/directives/class-map.js";
import type {
  FlamegraphDataSample,
  FlamegraphInstance,
  FlamegraphSetDataOptions,
} from "../../lib/canvas/index.js";
import { createFlamegraphHost } from "./flamegraph-host.js";
import { deriveLaneData } from "../../components/canvas/lanes/index.js";
import type { LaneData } from "../../components/canvas/lanes/index.js";
import type { FlamegraphNode } from "../../lib/trace/index.js";
import type { Announcer } from "../../lib/interact/announce.js";
import { deriveAxisInputs, fmtAxisTick } from "./axis.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { ParsedTrace } from "../../types/trace.js";
import type { TimeRange } from "../../types/trace.js";
import {
  buildBlockingView,
  collectSchedSamplesInRange,
  cpuRegionView,
  defaultRegionMode,
  formatHeapBytes,
  frameSampleTimeExtent,
  heapRegionView,
  heapSamplesForMode,
  regionCoverage,
  regionModesPresent,
  regionTitle,
  type BlockingGroup,
  type BlockingView,
  type CpuRegionView,
  type HeapRegionView,
  type RegionCoverage,
  type RegionMode,
} from "./region-analysis-model.js";

const MODE_ORDER: readonly RegionMode[] = ["cpu", "blocking", "heap"];
const MODE_LABELS: Record<RegionMode, string> = {
  cpu: "Flamegraph",
  blocking: "Blocking Calls",
  heap: "Heap",
};

/** Callbacks the region panel needs from the page entry. */
export interface RegionAnalysisDeps {
  /** The inspector aside; the Stack tab's `[data-region-host]` lives here. */
  inspectorHost: HTMLElement;
  /** Surface an error (pop-out with no trace URL, etc.) as a toast. */
  notify?(message: string): void;
  /** ARIA/status announcer for the "show in timeline" navigation. */
  announcer?: Announcer;
}

export interface RegionAnalysisController {
  /** Render/refresh the panel into the Stack-tab host (idempotent; no-op when
   *  the Stack tab is not showing a region). Called by the inspector after each
   *  frame render and by this controller's own viewport/segments subscription. */
  sync(): void;
  /** Toolbar: open a whole-trace analysis (activates the Stack tab). */
  openWholeTrace(kind: RegionMode): void;
  dispose(): void;
}

/** The computed view for the active (mode, range), cached by signature. */
type ComputedView =
  | { kind: "cpu"; cpu: CpuRegionView }
  | { kind: "heap"; heap: HeapRegionView }
  // `samples` are the raw, stack-carrying sched samples the flamegraph mode
  // feeds off - the grouped `blocking` view has already dropped the per-sample
  // callchain the widget needs.
  | { kind: "blocking"; blocking: BlockingView; samples: readonly FlamegraphDataSample[] }
  | { kind: "empty" };

export function createRegionAnalysis(
  store: ViewerStore,
  deps: RegionAnalysisDeps,
): RegionAnalysisController {
  // Trace-invariant lane data (workerSpans + attached CPU/sched samples),
  // rebuilt only when the trace slice is replaced.
  const laneData = store.derived(["trace"], (s): LaneData | null =>
    s.trace.trace ? deriveLaneData(s.trace.trace) : null,
  );

  // ── local UI state ──────────────────────────────────────────────────────
  let mode: RegionMode | null = null;
  let pendingMode: RegionMode | null = null; // set by a toolbar open
  let heapMode: "bytes" | "count" = "bytes";
  let groupBy: "leaf" | "full" = "leaf";
  let showExtent: TimeRange | null = null; // extent for the current zoom
  let lastRangeKey: string | null = null;

  // ── widget lifecycle (lazy, single instance reused) ─────────────────────
  // The container/instance/ResizeObserver/appliedSig discipline lives in the
  // shared host helper (the inspector hosts a second instance the same way).
  const fg = createFlamegraphHost({
    doc: deps.inspectorHost.ownerDocument,
    className: "d9-region-fg",
    onZoom: onZoomChange,
  });
  let computedView: ComputedView = { kind: "empty" };
  let computedSig: string | null = null;
  let cpuSamplesForExtent: readonly { callchain: string[]; workerId: number; timestamp: number }[] = [];

  function state(): StoreState {
    return store.getState();
  }

  function currentRange(): TimeRange | null {
    return state().selection.sidebarRange;
  }

  function rangeKey(range: TimeRange): string {
    return `${range.startNs}-${range.endNs}`;
  }

  function viewSig(m: RegionMode | null, range: TimeRange): string {
    const key = rangeKey(range);
    if (m === "heap") return `heap:${key}:${heapMode}`;
    if (m === "blocking") return `blocking:${key}:${groupBy}`;
    if (m === "cpu") return `cpu:${key}`;
    return `empty:${key}`;
  }

  // ── flamegraph widget ───────────────────────────────────────────────────

  /** The widget fires this whenever the zoom stack changes. Recompute the
   *  frame->time extent and re-render the header (no setData). */
  function onZoomChange(): void {
    const trace = state().trace.trace;
    const inst = fg.instance();
    if (inst === null || trace === null || mode !== "cpu") {
      showExtent = null;
    } else {
      showExtent = frameSampleTimeExtent(
        cpuSamplesForExtent,
        inst.getZoomPath(),
        trace.callframeSymbols,
      );
    }
    sync();
  }

  // ── view computation (cached by signature) ──────────────────────────────

  function computeView(m: RegionMode | null, range: TimeRange, trace: ParsedTrace, ld: LaneData): ComputedView {
    if (m === "cpu") return { kind: "cpu", cpu: cpuRegionView(trace, range) };
    if (m === "heap") return { kind: "heap", heap: heapRegionView(trace, range, ld.workerSpans) };
    if (m === "blocking") {
      const entries = collectSchedSamplesInRange(ld.workerSpans, ld.workerIds, range);
      // Retain the raw stack-carrying samples for the flamegraph toggle; the
      // grouped view drops them. `SchedEntry.sample` is a CpuSample, which
      // already satisfies FlamegraphDataSample.
      const samples = entries
        .map((e) => e.sample)
        .filter((s) => s.callchain.length > 0) as FlamegraphDataSample[];
      return {
        kind: "blocking",
        blocking: buildBlockingView(entries, trace.callframeSymbols, groupBy),
        samples,
      };
    }
    return { kind: "empty" };
  }

  // ── heap widget options (tooltip / export labels) ───────────────────────

  function heapFgOpts(view: HeapRegionView, range: TimeRange): FlamegraphSetDataOptions {
    const m = heapMode;
    const durStr = ` · ${((range.endNs - range.startNs) / 1e6).toFixed(1)}ms`;
    return {
      workerLabel:
        m === "bytes" ? "Worker thread allocations (by bytes)" : "Worker thread allocations (by count)",
      offworkerLabel:
        m === "bytes" ? "Non-worker thread allocations (by bytes)" : "Non-worker thread allocations (by count)",
      exportTitle: `Heap flamegraph (${m === "bytes" ? "by bytes" : "by count"})${durStr}`,
      exportFormatValue:
        m === "bytes"
          ? (count: number) => `~${formatHeapBytes(count)}`
          : (count: number) => `~${Math.round(count).toLocaleString()} allocs`,
      formatCount(count: number, total: number, _self: number, treeNode: FlamegraphNode | null | undefined) {
        const pct = ((count / total) * 100).toFixed(1);
        if (m === "bytes") {
          const allocs = treeNode && treeNode.allocCount != null ? Math.round(treeNode.allocCount) : null;
          let s = `~${formatHeapBytes(count)} (${pct}%)`;
          if (allocs != null && treeNode && treeNode.allocCount) {
            s += ` · ~${allocs.toLocaleString()} allocs · ${formatHeapBytes(count / treeNode.allocCount)} avg`;
          }
          return s;
        }
        let s = `~${Math.round(count).toLocaleString()} allocs (${pct}%)`;
        if (treeNode && treeNode.allocCount != null) {
          s += ` · ~${formatHeapBytes(treeNode.allocCount)} · ${formatHeapBytes(treeNode.allocCount / count)} avg`;
        }
        return s;
      },
    };
  }

  // ── sync: the single render entry point ─────────────────────────────────

  function sync(): void {
    const host = deps.inspectorHost.querySelector<HTMLElement>("[data-region-host]");
    const range = currentRange();
    const trace = state().trace.trace;
    if (host === null || range === null || trace === null) {
      // Parked: the Stack tab is not showing a region. The widget stays alive
      // (its container detaches with the removed host); re-resolve on reopen.
      lastRangeKey = null;
      fg.detach();
      return;
    }
    const ld = laneData();
    if (ld === null) return;

    const present = regionModesPresent(trace, ld.workerSpans, ld.workerIds, range);
    const key = rangeKey(range);
    if (key !== lastRangeKey) {
      // A genuinely new region (or a fresh toolbar open): pick the mode by the
      // toolbar's forced kind, else the data-present default (reset to the
      // default on each fresh region, not the previous mode).
      lastRangeKey = key;
      mode = pendingMode ?? defaultRegionMode(present);
      pendingMode = null;
      showExtent = null;
    } else if (mode !== null && !present[mode]) {
      mode = defaultRegionMode(present);
    }

    const coverage = regionCoverage(state().segments, range);
    renderPanel(host, range, trace, ld, present, coverage);
  }

  function renderPanel(
    host: HTMLElement,
    range: TimeRange,
    trace: ParsedTrace,
    ld: LaneData,
    present: { cpu: boolean; blocking: boolean; heap: boolean },
    coverage: RegionCoverage,
  ): void {
    const sig = viewSig(mode, range);
    if (sig !== computedSig) {
      computedView = computeView(mode, range, trace, ld);
      computedSig = sig;
    }
    render(panelTemplate(range, present, coverage), host);

    // Body: the flamegraph canvas (cpu/heap always; blocking when the
    // list/flame toggle is on flame and there are stack-carrying samples) or
    // HTML (in the template).
    const blockingFlame =
      mode === "blocking" &&
      state().uiPrefs.stacksAsFlamegraph &&
      computedView.kind === "blocking" &&
      computedView.samples.length > 0;
    if (mode === "cpu" || mode === "heap" || blockingFlame) {
      const fgHost = host.querySelector<HTMLElement>(".d9-region-fg-host");
      if (fgHost === null) return;
      // The flame pref is part of the signature so switching list<->flame for
      // blocking re-applies setData.
      const fgSig = blockingFlame ? `${sig}:flame` : sig;
      fg.sync({
        hostEl: fgHost,
        sig: fgSig,
        apply: (instance) => applyFlamegraph(instance, trace, range),
      });
    } else {
      // Blocking-list / empty: the widget is detached with its old host; drop
      // the stale attach marker so re-entering a flamegraph re-attaches + resizes.
      fg.detach();
    }
  }

  function applyFlamegraph(instance: FlamegraphInstance, trace: ParsedTrace, range: TimeRange): void {
    if (computedView.kind === "cpu") {
      cpuSamplesForExtent = computedView.cpu.samples;
      instance.setData(computedView.cpu.samples, trace.callframeSymbols, {
        exportTitle: `CPU flamegraph - ${regionTitle(range)}`,
        runtimeWorkers: trace.runtimeWorkers,
      });
    } else if (computedView.kind === "heap") {
      cpuSamplesForExtent = [];
      instance.setData(
        heapSamplesForMode(computedView.heap.baseSamples, heapMode),
        trace.callframeSymbols,
        heapFgOpts(computedView.heap, range),
      );
    } else if (computedView.kind === "blocking") {
      // No frame->time extent for blocking (its samples are off-CPU park
      // points, not an on-CPU profile), so the "show in timeline" link stays off.
      cpuSamplesForExtent = [];
      instance.setData(computedView.samples, trace.callframeSymbols, {
        exportTitle: `Blocking calls - ${regionTitle(range)}`,
        runtimeWorkers: trace.runtimeWorkers,
      });
    }
  }

  // ── template ─────────────────────────────────────────────────────────────

  function panelTemplate(
    range: TimeRange,
    present: { cpu: boolean; blocking: boolean; heap: boolean },
    coverage: RegionCoverage,
  ): TemplateResult {
    const available = MODE_ORDER.filter((m) => present[m]);
    return html`
      <div class="d9-region">
        <div class="d9-region-tabs" role="tablist" aria-label="Region analysis">
          ${available.map((m) => modeTab(m))}
        </div>
        ${coverageBadge(coverage)}
        <div class="d9-region-body">${bodyTemplate(range)}</div>
      </div>
    `;
  }

  function modeTab(m: RegionMode): TemplateResult {
    const on = m === mode;
    return html`<button
      type="button"
      class=${classMap({ "d9-region-tab": true, on })}
      role="tab"
      aria-selected=${on ? "true" : "false"}
      @click=${() => selectMode(m)}
    >
      ${MODE_LABELS[m]}
    </button>`;
  }

  /** The partial-window badge: a non-complete region must not read as the whole
   *  picture. */
  function coverageBadge(coverage: RegionCoverage): TemplateResult | typeof nothing {
    if (coverage === "complete") return nothing;
    const label =
      coverage === "oversized"
        ? "oversized segment - analysis covers resident data only"
        : "partial window - analysis covers resident segments only";
    return html`<div class="d9-region-badge" role="status" title=${label}>
      <span aria-hidden="true">⚠</span> ${label}
    </div>`;
  }

  function bodyTemplate(range: TimeRange): TemplateResult {
    if (computedView.kind === "empty") {
      return html`<p class="d9-inspector-hint">
        No CPU samples, scheduling events, or heap allocations in the selected
        ${regionTitle(range).toLowerCase()}.
      </p>`;
    }
    if (computedView.kind === "cpu") return cpuBody(computedView.cpu, range);
    if (computedView.kind === "heap") return heapBody(computedView.heap);
    return blockingBody(computedView.blocking);
  }

  // ── CPU body ────────────────────────────────────────────────────────────

  function cpuBody(view: CpuRegionView, range: TimeRange): TemplateResult {
    if (view.foldableCount === 0) {
      return html`<p class="d9-inspector-hint">No CPU samples in the selected region.</p>`;
    }
    return html`
      <div class="d9-region-actions">
        <span class="d9-region-count" title="on-CPU samples with a stack (foldable) vs total CPU sample records"
          >${view.countLabel}</span
        >
        ${showInTimelineButton()}
        <button
          type="button"
          class="d9-region-popout"
          title="Open this flamegraph in a new tab (flamegraph.html)"
          @click=${() => popOut(range)}
        >
          ↗ Pop Out
        </button>
      </div>
      <div class="d9-region-fg-host"></div>
    `;
  }

  /** Frame->timeline link: navigate the viewport to the [min,max] timestamp
   *  extent of the zoomed frame's samples within the analyzed region. */
  function showInTimelineButton(): TemplateResult | typeof nothing {
    if (showExtent === null) return nothing;
    const extent = showExtent;
    const label = extentLabel(extent);
    return html`<button
      type="button"
      class="d9-region-showtime"
      title="Move the timeline to this frame's samples (${label})"
      @click=${() => navigateToExtent(extent)}
    >
      ⇲ Show in timeline
    </button>`;
  }

  function extentLabel(extent: TimeRange): string {
    const axis = deriveAxisInputs(state());
    const a = fmtAxisTick(axis, extent.startNs, false);
    if (extent.endNs === extent.startNs) return a;
    return `${a} - ${fmtAxisTick(axis, extent.endNs, false)}`;
  }

  // ── Heap body ───────────────────────────────────────────────────────────

  function heapBody(view: HeapRegionView): TemplateResult {
    if (view.sampleCount === 0) {
      return html`<p class="d9-inspector-hint">No heap samples in the selected region.</p>`;
    }
    return html`
      <div class="d9-region-actions">
        <span class="d9-region-count"
          >${view.sampleCount.toLocaleString()} samples · ~${formatHeapBytes(view.estimatedBytes)} ·
          ~${Math.round(view.estimatedAllocs).toLocaleString()} allocs</span
        >
        <span class="d9-heap-toggle" role="group" aria-label="Heap weight">
          <button
            type="button"
            class=${classMap({ "d9-heap-mode": true, on: heapMode === "bytes" })}
            @click=${() => setHeapMode("bytes")}
          >
            Bytes
          </button>
          <button
            type="button"
            class=${classMap({ "d9-heap-mode": true, on: heapMode === "count" })}
            @click=${() => setHeapMode("count")}
          >
            Count
          </button>
        </span>
      </div>
      <div class="d9-region-fg-host"></div>
    `;
  }

  // ── Blocking body ───────────────────────────────────────────────────────

  function blockingBody(view: BlockingView): TemplateResult {
    if (view.total === 0) {
      return html`<p class="d9-inspector-hint">No scheduling events in the selected region.</p>`;
    }
    const asFlame = state().uiPrefs.stacksAsFlamegraph;
    const canFlame =
      computedView.kind === "blocking" && computedView.samples.length > 0;
    return html`
      <div class="d9-region-actions">
        <label class="d9-block-groupby">
          <span class="d9-sr-only">Group blocking calls by</span>
          <select @change=${onGroupByChange} ?disabled=${asFlame && canFlame}>
            <option value="leaf" ?selected=${groupBy === "leaf"}>Group by blocking call</option>
            <option value="full" ?selected=${groupBy === "full"}>Group by full stack</option>
          </select>
        </label>
        ${canFlame ? blockingViewToggle(asFlame) : nothing}
      </div>
      <p class="d9-block-note">
        Blocking calls that caused the kernel to deschedule a tokio worker thread
        during a poll.
      </p>
      ${asFlame && canFlame
        ? html`<div class="d9-region-fg-host"></div>`
        : html`<div class="d9-block-bars">
              ${view.groups.map((g) => blockingBar(g))}
            </div>
            <div class="d9-block-groups">${view.groups.map(blockingGroup)}</div>`}
    `;
  }

  /** List ⇄ flamegraph switch for the blocking sub-stacks, sharing the same
   *  persisted uiPrefs pref as the poll inspector. */
  function blockingViewToggle(asFlame: boolean): TemplateResult {
    return html`<span class="d9-stacks-view" role="group" aria-label="Stack view">
      <button
        type="button"
        class=${classMap({ "d9-stacks-view-btn": true, on: !asFlame })}
        aria-pressed=${!asFlame ? "true" : "false"}
        title="Show blocking calls as a grouped list"
        @click=${() => store.update("uiPrefs", { stacksAsFlamegraph: false })}
      >
        List
      </button>
      <button
        type="button"
        class=${classMap({ "d9-stacks-view-btn": true, on: asFlame })}
        aria-pressed=${asFlame ? "true" : "false"}
        title="Show blocking calls as a flamegraph"
        @click=${() => store.update("uiPrefs", { stacksAsFlamegraph: true })}
      >
        Flame
      </button>
    </span>`;
  }

  function blockingBar(g: BlockingGroup): TemplateResult {
    return html`<div class=${classMap({ "d9-block-row": true, [`c-${g.color}`]: true })}>
      <span class="d9-block-count">${g.count}</span>
      <span class="d9-block-bar" style="width:${g.barPct}%"></span>
      <span class="d9-block-leaf">${g.leaf}</span>
      <span class="d9-block-pct">${g.pct}%</span>
    </div>`;
  }

  function blockingGroup(g: BlockingGroup): TemplateResult {
    return html`
      <div class=${classMap({ "d9-block-group": true, [`c-${g.color}`]: true })}>
        <div class="d9-block-group-head">
          <span class="d9-block-count">${g.count}×</span>
          <span class="d9-block-leaf">${g.leaf}</span>
          <span class="d9-block-pct">(${g.pct}% of all sched events)</span>
        </div>
        ${g.subStacks.map(
          (sub) => html`
            <div class="d9-block-sub">
              <div class="d9-block-sub-head">
                <strong>${sub.count}×</strong> <span>(${sub.pct}% of group)</span>
              </div>
              ${sub.frames.map(
                (f, i) =>
                  html`<div
                    class=${classMap({ "d9-block-frame": true, leaf: i === 0, tokio: f.isTokio })}
                  >
                    ${f.docsUrl !== null
                      ? html`<a href=${f.docsUrl} target="_blank" rel="noreferrer">${f.text}</a>`
                      : f.text}
                  </div>`,
              )}
            </div>
          `,
        )}
        <div class="d9-block-examples">
          Example polls:
          ${g.examples.map(
            (ex) => html`<button
              type="button"
              class="d9-block-jump"
              @click=${() => jumpToPoll(ex.start, ex.end)}
            >
              W${ex.worker} ${jumpTs(ex.start)} (${ex.durLabel})
            </button>`,
          )}
          ${g.moreExamples > 0 ? html`<span class="d9-block-more">... ${g.moreExamples} more</span>` : nothing}
        </div>
      </div>
    `;
  }

  function jumpTs(ns: number): string {
    return fmtAxisTick(deriveAxisInputs(state()), ns, false);
  }

  // ── interactions ─────────────────────────────────────────────────────────

  function selectMode(m: RegionMode): void {
    if (m === mode) return;
    mode = m;
    showExtent = null;
    sync();
  }

  function setHeapMode(m: "bytes" | "count"): void {
    if (m === heapMode) return;
    heapMode = m;
    sync();
  }

  function onGroupByChange(e: Event): void {
    const v = (e.target as HTMLSelectElement).value;
    groupBy = v === "full" ? "full" : "leaf";
    sync();
  }

  /** Center the viewport on a poll (jump-to-poll), preserving a sensible padded
   *  window (pad = 5x duration). */
  function jumpToPoll(start: number, end: number): void {
    const vp = state().viewport;
    const dur = Math.max(end - start, 1);
    const pad = Math.max(dur * 5, 1e6);
    let viewStart = Math.max(vp.minTs, start - pad * 0.3);
    let viewEnd = Math.min(vp.maxTs, end + pad * 0.3);
    if (viewEnd <= viewStart) viewEnd = Math.min(vp.maxTs, viewStart + 1e6);
    store.update("viewport", { viewStart, viewEnd });
  }

  /** Navigate to the [min,max] extent of the zoomed frame's samples. */
  function navigateToExtent(extent: TimeRange): void {
    const vp = state().viewport;
    const span = extent.endNs - extent.startNs;
    const pad = Math.max(span * 0.15, 1e6);
    let viewStart = Math.max(vp.minTs, extent.startNs - pad);
    let viewEnd = Math.min(vp.maxTs, extent.endNs + pad);
    if (viewEnd <= viewStart) viewEnd = Math.min(vp.maxTs, viewStart + 1e6);
    store.update("viewport", { viewStart, viewEnd });
    deps.announcer?.announce("Timeline moved to the frame's samples");
  }

  /** Pop Out: open flamegraph.html preserving trace URL(s), range, zoom. */
  function popOut(range: TimeRange): void {
    const params = new URLSearchParams(window.location.search);
    const traceUrls = params.getAll("trace").filter((u) => u.length > 0);
    if (traceUrls.length === 0) {
      deps.notify?.("Pop-out requires a URL-loaded trace (open with ?trace=).");
      return;
    }
    const qs = traceUrls.map((u) => `trace=${encodeURIComponent(u)}`).join("&");
    let url = `flamegraph.html?${qs}&start=${Math.round(range.startNs)}&end=${Math.round(range.endNs)}`;
    const fgInstance = fg.instance();
    if (fgInstance !== null) {
      const z = fgInstance.getZoomPath();
      if (z.worker.length > 0) url += `&worker-zoom=${encodeURIComponent(z.worker.join("\t"))}`;
      if (z.offworker.length > 0) url += `&offworker-zoom=${encodeURIComponent(z.offworker.join("\t"))}`;
    }
    window.open(url, "_blank");
  }

  // ── toolbar open ────────────────────────────────────────────────────────

  function openWholeTrace(kind: RegionMode): void {
    const vp = state().viewport;
    if (vp.maxTs <= vp.minTs) {
      deps.notify?.("Load a trace before opening an analysis.");
      return;
    }
    // The whole-trace analysis reuses the retained-region mechanism so the Stack
    // tab activates and the analyzed scope is boxed; the forced kind opens the
    // requested analysis even when other data is present.
    pendingMode = kind;
    store.update("selection", {
      sidebarRange: { startNs: vp.minTs, endNs: vp.maxTs },
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
      spawnedTasksRange: null,
    });
  }

  // Own subscription: viewport + segments drive the badge/extent (the inspector
  // re-renders on trace/selection/uiPrefs and calls sync too).
  const unsubscribe = store.subscribe(["viewport", "segments"], () => sync());

  return {
    sync,
    openWholeTrace,
    dispose(): void {
      unsubscribe();
      fg.destroy();
    },
  };
}
