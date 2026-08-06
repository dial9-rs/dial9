// The viewer toolbar: file info, analysis buttons, time display + range. A
// store-wired controller (createToolbar) rendered into the shell's toolbar
// slots; every handler dispatches store actions (or an injected seam) and never
// renders. Every control has a `title` + accessible name.
//
// The analysis buttons open region-analysis surfaces via injected callbacks, NOT
// `selection.sidebarRange` directly, because setting that range with no sidebar
// to clear it would soft-lock keyboard selection. Set/Clear Range reparse the
// retained buffer via injected callbacks too. Time toggles and goto are fully
// wired here.

import { html, type TemplateResult } from "lit-html";
import { classMap } from "lit-html/directives/class-map.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState, ViewportSlice } from "../../types/state.js";
import type { ParsedTrace, SegmentIdentity } from "../../types/trace.js";
import type { KeyBinding } from "../../lib/interact/keyboard.js";
import { parseGotoTime, type GotoTime } from "../../lib/interact/goto-time.js";
import {
  formatHumanDuration,
  hasCpuProfileSamples,
  readSegmentIdentity,
  readSegmentMetadataEntries,
  reconcileIdentity,
  type IdentityField,
  type ReconciledIdentity,
} from "../../lib/trace/index.js";
import { poiSourceFor, kindLabel, redFlagCounts } from "./poi.js";
import type { PointOfInterestType } from "../../types/trace.js";

/** Which whole-trace analysis an analysis button opens. */
export type AnalysisKind = "cpu" | "blocking" | "heap";

/** Injected surface seams for controls whose panels other components own. */
export interface ToolbarDeps {
  /** Open the trace-wide catalogue of graphable custom-event fields. */
  onOpenFieldCharts(): void;
  /** Open a whole-trace analysis in the inspector. */
  onOpenAnalysis(kind: AnalysisKind): void;
  /** Set a time-range filter from the current viewport (reparse). */
  onSetRange(range: { startNs: number; endNs: number }): void;
  /** Clear the active time-range filter (reparse). */
  onClearRange(): void;
  /**
   * The S3-key-derived svc/host identity from the viewer URL. A boot constant
   * read once from `location.search` by the page entry; the file-info surface
   * reconciles it against the trace-EMBEDDED metadata (embedded wins,
   * disagreeing key-derived value tooltipped). Omitted (or `{}`) when the viewer
   * was not opened from the S3 browser handoff.
   */
  keyDerivedIdentity?: SegmentIdentity;
}

export interface ToolbarController {
  /** File info: filename + structured meta. Fills the file-info slot. */
  fileInfoTemplate(state: StoreState, sourceLabel: string): TemplateResult;
  /** Red-flags chip + analysis buttons + info menu. Fills the Analysis slot. */
  analysisTemplate(state: StoreState, sourceLabel: string): TemplateResult;
  /** Time toggles + goto + Set/Clear Range. Fills the Time slot. */
  timeTemplate(state: StoreState): TemplateResult;
  /** `g` goto-time accelerator - focuses the goto input. */
  keyBindings: readonly KeyBinding[];
  dispose(): void;
}

// ── goto-time math ────────────────────────────────────────────────────────

/**
 * Resolve a parsed goto input to an absolute trace timestamp (ns). Absolute
 * seconds are an offset from the trace start (`minTs`, matching the default
 * relative axis); a `+`/`-` relative offset is measured from the current view
 * CENTER (the "current position" the goto grammar refers to).
 */
export function gotoTargetNs(goto: GotoTime, vp: ViewportSlice): number {
  const offsetNs = goto.seconds * 1e9;
  if (goto.relative) return (vp.viewStart + vp.viewEnd) / 2 + offsetNs;
  return vp.minTs + offsetNs;
}

/**
 * Center the viewport on `target` (ns), PRESERVING the current view duration
 * and clamping to [minTs, maxTs] (so a goto near a bound butts against it and
 * keeps its width, like the arrow pan). Returns the new window.
 */
export function gotoWindow(
  target: number,
  vp: ViewportSlice,
): { viewStart: number; viewEnd: number } {
  const dur = vp.viewEnd - vp.viewStart || 1e6;
  let viewStart = target - dur / 2;
  let viewEnd = viewStart + dur;
  if (viewStart < vp.minTs) {
    viewStart = vp.minTs;
    viewEnd = viewStart + dur;
  }
  if (viewEnd > vp.maxTs) {
    viewEnd = vp.maxTs;
    viewStart = Math.max(vp.minTs, viewEnd - dur);
  }
  return { viewStart, viewEnd };
}

// ── derived toolbar flags (from the resident trace) ─────────────────────

/** Count of tasks spawned without wake tracking (raw `tokio::spawn`). */
export function uninstrumentedCount(trace: ParsedTrace): number {
  let n = 0;
  for (const instrumented of trace.taskInstrumented.values()) {
    if (!instrumented) n += 1;
  }
  return n;
}

// ── controller ───────────────────────────────────────────────────────────

export function createToolbar(
  store: ViewerStore,
  deps: ToolbarDeps,
): ToolbarController {
  /** Apply the goto input's value: parse -> move viewport, or ignore junk. */
  function applyGoto(raw: string): void {
    const goto = parseGotoTime(raw);
    if (goto === null) return; // invalid input: the field re-prompts, never guess
    const vp = store.getState().viewport;
    if (vp.maxTs <= vp.minTs) return; // no trace loaded
    const target = gotoTargetNs(goto, vp);
    store.update("viewport", gotoWindow(target, vp));
  }

  function onGotoKey(e: KeyboardEvent): void {
    if (e.key !== "Enter") return;
    const input = e.target as HTMLInputElement;
    applyGoto(input.value);
    input.value = "";
  }

  function toggleTimeMode(): void {
    const mode = store.getState().uiPrefs.timeMode === "rel" ? "abs" : "rel";
    store.update("uiPrefs", { timeMode: mode });
  }
  function toggleTz(): void {
    const tz = store.getState().uiPrefs.tz === "utc" ? "local" : "utc";
    store.update("uiPrefs", { tz });
  }
  function setRange(): void {
    const vp = store.getState().viewport;
    deps.onSetRange({ startNs: vp.viewStart, endNs: vp.viewEnd });
  }

  const keyBindings: readonly KeyBinding[] = [
    {
      // `g`: focus the goto-time input. Declines (falls through) when the input
      // is not on the page (no trace yet).
      key: "g",
      onKey: () => {
        const input = document.querySelector<HTMLInputElement>("[data-goto-input]");
        if (input === null) return false;
        input.focus();
        input.select();
      },
    },
  ];

  const keyDerivedIdentity = deps.keyDerivedIdentity ?? {};

  return {
    fileInfoTemplate: (state, sourceLabel) =>
      fileInfoTemplate(state, sourceLabel, keyDerivedIdentity),
    analysisTemplate: (state, sourceLabel) =>
      analysisTemplate(state, sourceLabel, deps),
    timeTemplate: (state) =>
      timeTemplate(state, { toggleTimeMode, toggleTz, onGotoKey, setRange, onClearRange: deps.onClearRange }),
    keyBindings,
    dispose(): void {
      /* no live listeners: templates own their handlers */
    },
  };
}

// ── file info + identity ──────────────────────────────────────────────────

function fileInfoTemplate(
  state: StoreState,
  sourceLabel: string,
  keyDerived: SegmentIdentity,
): TemplateResult {
  const trace = state.trace.trace;
  const identity = reconcileIdentity(readSegmentIdentity(trace), keyDerived);
  return html`
    <span class="d9-file-name" title=${sourceLabel}>${sourceLabel}</span>
    ${identityTemplate(identity)}
    <span class="d9-file-meta" data-file-meta id="toolbar-row-data"
      >${fileMetaText(trace)}</span
    >
  `;
}

/**
 * The trace-embedded service/host identity chips. Renders nothing when neither
 * the trace metadata nor the URL params yield a value (e.g. an old trace loaded
 * without an S3-key handoff). Each chip shows the embedded metadata value (which
 * wins) and, when the S3-key-derived value disagrees, tooltips that key-derived
 * value.
 */
function identityTemplate(identity: ReconciledIdentity): TemplateResult | string {
  const chips: TemplateResult[] = [];
  if (identity.service) chips.push(identityChip("service", identity.service));
  if (identity.host) chips.push(identityChip("host", identity.host));
  if (chips.length === 0) return "";
  return html`<span class="d9-file-identity" data-file-identity>${chips}</span>`;
}

function identityChip(
  field: "service" | "host",
  info: IdentityField,
): TemplateResult {
  const title =
    info.keyDerived !== undefined
      ? `${field}: ${info.value} (trace metadata; S3 key says "${info.keyDerived}")`
      : `${field}: ${info.value} (from ${info.fromMetadata ? "trace metadata" : "S3 key"})`;
  return html`<span class="d9-file-identity-item" data-identity=${field} title=${title}
    ><span class="d9-file-identity-key">${field}</span>${info.value}</span
  >`;
}

/** The stats line: events, workers, duration, plus truncation/filter notes. */
export function fileMetaText(trace: ParsedTrace | null): string {
  if (trace === null) return "no trace loaded";
  const workers = new Set(trace.tidToWorker.values()).size;
  const parts = [`${trace.events.length.toLocaleString()} events`, `${workers} workers`];
  if (trace.minTs !== null && trace.maxTs !== null) {
    parts.push(formatHumanDuration(trace.maxTs - trace.minTs));
  }
  if (trace.truncated) parts.push("truncated");
  if (trace.timeFiltered) parts.push("range-filtered");
  return parts.join(" · ");
}

// ── red-flags chip + analysis buttons + info menu ─────────────────────────

function analysisTemplate(
  state: StoreState,
  sourceLabel: string,
  deps: ToolbarDeps,
): TemplateResult {
  const trace = state.trace.trace;
  if (trace === null) {
    return html`<span class="d9-slot-hint">No analysis yet</span>`;
  }
  const source = poiSourceFor(trace);
  const schedCount = source.schedDelays.length;
  const cpuCount = trace.cpuSamples.length;
  const heapCount = trace.allocEvents.length;
  const uninstrumented = uninstrumentedCount(trace);

  return html`
    ${redFlagsChip(trace)}
    ${fieldChartsButton(deps.onOpenFieldCharts)}
    ${schedCount > 0
      ? analysisButton(
          "d9-btn-blocking",
          "⚠ Blocking Calls",
          "Show scheduling delays and blocking-call analysis for the whole trace",
          () => deps.onOpenAnalysis("blocking"),
        )
      : ""}
    ${hasCpuProfileSamples(trace.cpuSamples)
      ? analysisButton(
          "d9-btn-flamegraph",
          `🔥 Flamegraph (${cpuCount.toLocaleString()})`,
          "Show a CPU flamegraph for the whole trace",
          () => deps.onOpenAnalysis("cpu"),
        )
      : ""}
    ${heapCount > 0
      ? analysisButton(
          "d9-btn-heap",
          `🔥 Heap (${heapCount.toLocaleString()})`,
          "Show a heap-allocation flamegraph for the whole trace",
          () => deps.onOpenAnalysis("heap"),
        )
      : ""}
    ${infoMenu(trace, sourceLabel, uninstrumented)}
  `;
}

function fieldChartsButton(onClick: () => void): TemplateResult {
  const title = "Browse graphable custom-event fields";
  return html`
    <button
      type="button"
      class="d9-toolbar-btn d9-field-charts-btn"
      id="d9-btn-field-charts"
      title=${title}
      aria-label=${title}
      @click=${onClick}
    >
      <svg
        viewBox="0 0 16 16"
        fill="none"
        stroke="currentColor"
        stroke-width="1.5"
        stroke-linecap="round"
        stroke-linejoin="round"
        aria-hidden="true"
        focusable="false"
      >
        <path d="M2.5 2.5v11h11"></path>
        <path d="m4 10 2.5-3 2.2 1.8L12.5 4"></path>
      </svg>
      Field charts
    </button>
  `;
}

/** The red-flags summary chip: existing detectors' COUNTS only. Non-zero
 *  detectors only. */
function redFlagsChip(trace: ParsedTrace): TemplateResult | string {
  const summary = redFlagCounts(poiSourceFor(trace)).filter((r) => r.count > 0);
  if (summary.length === 0) return "";
  const plural = (s: { type: PointOfInterestType; count: number }): string =>
    `${s.count} ${kindLabel(s.type)}${s.count === 1 ? "" : "s"}`;
  const title = summary.map(plural).join(", ");
  return html`
    <span
      class="d9-redflags"
      data-redflags
      role="status"
      title=${`Detected issues: ${title}`}
    >
      <span aria-hidden="true">⚠</span>
      ${summary.map(
        (s, i) =>
          html`${i > 0 ? html`<span class="d9-redflags-sep">·</span>` : ""}<span
              class="d9-redflags-item"
              >${plural(s)}</span
            >`,
      )}
    </span>
  `;
}

function analysisButton(
  id: string,
  label: string,
  title: string,
  onClick: () => void,
): TemplateResult {
  return html`
    <button
      type="button"
      class="d9-toolbar-btn"
      id=${id}
      title=${title}
      aria-label=${title}
      @click=${onClick}
    >
      ${label}
    </button>
  `;
}

/** The info menu: demoted "Parse perf" + uninstrumented details. */
function infoMenu(
  trace: ParsedTrace,
  sourceLabel: string,
  uninstrumented: number,
): TemplateResult {
  const metadata = readSegmentMetadataEntries(trace);
  const infoTitle =
    metadata.length > 0
      ? "Trace details and segment metadata"
      : "Trace and load details (parse performance, uninstrumented tasks)";
  const workers = new Set(trace.tidToWorker.values()).size;
  const duration =
    trace.minTs !== null && trace.maxTs !== null
      ? formatHumanDuration(trace.maxTs - trace.minTs)
      : "-";
  return html`
    <details class="d9-info-menu" data-info-menu>
      <summary
        class="d9-toolbar-btn d9-info-summary"
        title=${infoTitle}
        aria-label=${infoTitle}
      >
        ⓘ
      </summary>
      <div
        class=${metadata.length > 0
          ? "d9-info-menu-body d9-info-menu-body-wide"
          : "d9-info-menu-body"}
        role="group"
        aria-label="Trace details"
      >
        <div class="d9-info-grid">
          <div class="d9-info-heading">Load &amp; trace</div>
          <div class="d9-info-row"><span>source</span><span>${sourceLabel}</span></div>
          <div class="d9-info-row"><span>events</span><span>${trace.events.length.toLocaleString()}</span></div>
          <div class="d9-info-row"><span>workers</span><span>${workers}</span></div>
          <div class="d9-info-row"><span>duration</span><span>${duration}</span></div>
          <div class="d9-info-row"><span>truncated</span><span>${trace.truncated ? "yes" : "no"}</span></div>
          <div class="d9-info-note">
            Detailed fetch/parse timing (Parse perf) is recorded by the load
            pipeline.
          </div>
          ${uninstrumented > 0
            ? html`<div class="d9-info-heading">Uninstrumented</div>
                <div class="d9-info-row">
                  <span>tasks</span><span>${uninstrumented}</span>
                </div>
                <div class="d9-info-note">
                  Spawned via raw <code>tokio::spawn</code> (no wake tracking).
                  Use <code>TelemetryHandle::spawn</code> for full data.
                </div>`
            : ""}
        </div>
        ${metadata.length > 0
          ? html`<div class="d9-info-heading" id="d9-segment-metadata-heading">
                Segment metadata
              </div>
              <table
                class="d9-info-metadata-table"
                aria-labelledby="d9-segment-metadata-heading"
              >
                <thead>
                  <tr><th scope="col">Key</th><th scope="col">Value</th></tr>
                </thead>
                <tbody>
                  ${metadata.map(
                    ([key, value]) => html`
                      <tr>
                        <td><code title=${key}>${key}</code></td>
                        <td><code title=${value}>${value}</code></td>
                      </tr>
                    `,
                  )}
                </tbody>
              </table>`
          : ""}
      </div>
    </details>
  `;
}

// ── time display + range + goto ───────────────────────────────────────────

interface TimeHandlers {
  toggleTimeMode(): void;
  toggleTz(): void;
  onGotoKey(e: KeyboardEvent): void;
  setRange(): void;
  onClearRange(): void;
}

function timeTemplate(state: StoreState, h: TimeHandlers): TemplateResult {
  const { timeMode, tz } = state.uiPrefs;
  const hasTrace = state.trace.trace !== null;
  const rangeActive = state.trace.trace?.timeFiltered === true;
  return html`
    <button
      type="button"
      class="d9-toolbar-btn"
      id="d9-btn-time-mode"
      title="Toggle between relative offsets and wall-clock timestamps"
      aria-label="Toggle time display mode"
      @click=${h.toggleTimeMode}
    >
      Time: ${timeMode === "rel" ? "Relative" : "Absolute"}
    </button>
    ${timeMode === "abs"
      ? html`<button
          type="button"
          class="d9-toolbar-btn"
          id="d9-btn-tz-mode"
          title="Toggle between UTC and your local timezone"
          aria-label="Toggle timezone"
          @click=${h.toggleTz}
        >
          TZ: ${tz === "utc" ? "UTC" : "Local"}
        </button>`
      : ""}
    <label class="d9-goto-label">
      <span class="d9-sr-only">Go to time</span>
      <input
        type="text"
        class="d9-goto-input"
        data-goto-input
        placeholder="go to time (s or +s)"
        title="Jump the view to a time - absolute seconds, or +/- relative (press g to focus)"
        aria-label="Go to time (seconds, or +/- relative)"
        ?disabled=${!hasTrace}
        @keydown=${h.onGotoKey}
      />
    </label>
    <button
      type="button"
      class="d9-toolbar-btn"
      id="d9-btn-set-range"
      title="Filter the trace to the current view (re-parses to only events in range)"
      aria-label="Set time-range filter"
      ?disabled=${!hasTrace}
      @click=${h.setRange}
    >
      Set Range
    </button>
    <button
      type="button"
      class=${classMap({ "d9-toolbar-btn": true, "d9-hidden": !rangeActive })}
      id="d9-btn-clear-range"
      title="Remove the time-range filter and re-parse the full trace"
      aria-label="Clear time-range filter"
      @click=${h.onClearRange}
    >
      Clear Range
    </button>
  `;
}
