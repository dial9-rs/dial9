// The spans track component. Hosts inside the unified track column: tracks.ts
// delegates the "spans" row's template (label metadata + legend controls +
// canvas) and its canvas paint here. Everything reactive flows through the
// store: filter/percentile/chips write uiPrefs, focus writes selection, and the
// canvas redraws inside the shell's frame tick.
//
// Derivations are cached in two tiers: the trace-invariant span data
// (computeSpanTrackData) is a store.derived(["trace"], ...) cache recomputed only
// when the trace slice is replaced; the per-frame render model
// (buildSpanRenderModel) is memoized on its primitive inputs, so a selection-only
// change (dimming) redraws with the same cached layout. The legend chips are a
// keyed lit-html `repeat`, not an innerHTML rebuild + relisten.
//
// Incomplete spans are surfaced, never dropped - the "N unmatched" warning
// renders whenever buildSpanData reports spans with an enter but no exit.

import { html, render, nothing, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { classMap } from "lit-html/directives/class-map.js";
import {
  clampX,
  createCanvasSizer,
  makeColorAssigner,
  nsToDrawX,
} from "../../lib/canvas/index.js";
import type { CanvasSizer } from "../../lib/canvas/index.js";
import {
  formatFieldValue,
  formatHumanDuration,
} from "../../lib/trace/index.js";
import type {
  ParsedTrace,
  TracingSpan,
  LaneSpans,
} from "../../lib/trace/index.js";
import { sharedSpanData, sharedWorkerSpans } from "../../lib/trace/derived.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { TrackSpec } from "../../lib/canvas/track-layout.js";
import {
  BASE_BAR_H,
  buildSpanRenderModel,
  computeSpanTrackData,
  isFilterActive,
  spanChipModels,
  spanFocusChain,
  spanHighlight,
  spanLabelModel,
  spanMatchesFilter,
  spanPercentileRank,
  type SpanDrawBucket,
  type SpanFilterState,
  type SpanRenderModel,
  type SpanTrackData,
} from "./spans-model.js";

/** Height reserved for the legend/controls strip above the span canvas. */
const CONTROLS_H = 30;
/** Percentile-filter dropdown options. */
const PCT_OPTIONS: readonly { value: number; label: string }[] = [
  { value: 0, label: "All" },
  { value: 50, label: "≥ P50" },
  { value: 90, label: "≥ P90" },
  { value: 95, label: "≥ P95" },
  { value: 99, label: "≥ P99" },
];

// Span-panel canvas colors.
const CANVAS_BG = "#1a1a2e";
const EMPTY_TEXT = "#555";
const TICK_TEXT = "#666";

/** The handle tracks.ts + the shell drive. */
export interface SpansTrackController {
  /** The full `.d9-track` row template for the spans track. */
  rowTemplate(track: TrackSpec): TemplateResult;
  /** Size + draw the span canvas. Called from sizeTracks inside the shell's
   *  frame tick. */
  paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void;
  /** Tear down the tooltip element. */
  dispose(): void;
}

export function createSpansTrack(store: ViewerStore): SpansTrackController {
  // ── Derived caches ─────────────────────────────────────────────────────
  // Trace-invariant span data: recomputed only when the trace slice changes.
  // The worker poll lanes reconstruct each span's active/idle segments and
  // resolve its owning task, so the spans carry taskId directly.
  const spanData = store.derived(["trace"], (s) => {
    const t = s.trace.trace;
    return computeSpanTrackData(
      t?.customEvents,
      buildWorkerLanes(t),
      // Shared buildSpanData output (computed once per trace with the lanes).
      t ? sharedSpanData(t) : undefined,
    );
  });

  // Stable name -> color assignment for this track's lifetime.
  const colorOf = makeColorAssigner();

  // Per-frame render-model memo, keyed on the derived-data reference (trace
  // identity) + its primitive inputs so a selection-only change (dimming)
  // reuses the cached layout but a trace/viewport/filter/focus change
  // recomputes it.
  let modelCache: { data: SpanTrackData; key: string; model: SpanRenderModel } | null =
    null;
  // The buckets last painted, for canvas hit-testing (click, hover).
  let lastHits: readonly SpanDrawBucket[] = [];
  // Nav: the filtered match list + cursor, rebuilt when the filter or trace
  // changes; the cursor is ephemeral track state (not the store).
  let navMatches: { data: SpanTrackData; key: string; spans: TracingSpan[] } | null =
    null;
  let navIndex = -1;
  // One tooltip element for hover, created lazily.
  let tooltipEl: HTMLDivElement | null = null;
  let sizer: CanvasSizer<CanvasRenderingContext2D> | null = null;
  let sizerCanvas: HTMLCanvasElement | null = null;

  function state(): StoreState {
    return store.getState();
  }

  function filterState(s: StoreState): SpanFilterState {
    return {
      text: s.uiPrefs.spanFilter,
      pctFloor: s.uiPrefs.spanPctFilter,
      selectedNames: s.uiPrefs.selectedSpanNames,
    };
  }

  /** A stable, collision-free key for a filter (memo/nav invalidation). */
  function filterKey(f: SpanFilterState): string {
    return JSON.stringify([f.text, f.pctFloor, [...f.selectedNames].sort()]);
  }

  function renderModel(
    data: SpanTrackData,
    s: StoreState,
    drawW: number,
    canvasH: number,
  ): SpanRenderModel {
    const filter = filterState(s);
    const focusedSpanId = s.selection.focusedSpanId;
    const key = [
      s.viewport.viewStart,
      s.viewport.viewEnd,
      drawW,
      canvasH,
      focusedSpanId ?? "",
      filterKey(filter),
    ].join("|");
    if (modelCache !== null && modelCache.data === data && modelCache.key === key) {
      return modelCache.model;
    }
    const model = buildSpanRenderModel({
      data,
      viewStart: s.viewport.viewStart,
      viewEnd: s.viewport.viewEnd,
      drawW,
      canvasH,
      focusedSpanId,
      filter,
    });
    modelCache = { data, key, model };
    return model;
  }

  function getNavMatches(data: SpanTrackData, filter: SpanFilterState): TracingSpan[] {
    const key = filterKey(filter);
    if (navMatches !== null && navMatches.data === data && navMatches.key === key) {
      return navMatches.spans;
    }
    let spans: TracingSpan[] = [];
    if (isFilterActive(filter)) {
      const cs = data.columnarSpans;
      if (cs) {
        // Rows are start-sorted; materialize only the matches (transient, on a
        // filter/nav action).
        for (let r = 0; r < cs.length; r++) {
          const s = cs.at(r);
          if (spanMatchesFilter(s, filter, data.durationsByName)) spans.push(s);
        }
      } else {
        spans = data.allSpans.filter((sp) => spanMatchesFilter(sp, filter, data.durationsByName));
      }
      spans.sort((a, b) => a.start - b.start);
    }
    navMatches = { data, key, spans };
    return spans;
  }

  // ── Store dispatch helpers ─────────────────────────────────────────────

  function setFilterText(text: string): void {
    navIndex = -1;
    store.update("uiPrefs", { spanFilter: text });
  }
  function setPctFilter(pct: number): void {
    navIndex = -1;
    store.update("uiPrefs", { spanPctFilter: pct });
  }
  function toggleName(name: string): void {
    const cur = state().uiPrefs.selectedSpanNames;
    const next = new Set(cur);
    if (next.has(name)) next.delete(name);
    else next.add(name);
    navIndex = -1;
    store.update("uiPrefs", { selectedSpanNames: next });
  }
  function clearNames(): void {
    navIndex = -1;
    store.update("uiPrefs", { selectedSpanNames: new Set<string>() });
  }

  /** Focus a span: pin it in the panel, highlight its ancestor chain, and
   *  select the task polling at its start. */
  function focusSpan(spanId: string): void {
    const data = spanData();
    const s = state();
    if (s.selection.focusedSpanId === spanId) {
      clearFocus();
      return;
    }
    const chain = spanFocusChain(spanId, data);
    const cs = data.columnarSpans;
    const taskId = cs
      ? (() => { const r = cs.spanIdToRow.get(spanId); return r === undefined ? null : cs.taskIdAt(r); })()
      : data.allSpans.find((sp) => sp.spanId === spanId)?.taskId ?? null;
    const patch: Partial<StoreState["selection"]> = {
      focusedSpanId: spanId,
      spanFocus: { spanId, chain },
      pinnedEvent: null,
    };
    if (taskId !== null) patch.selectedTaskId = taskId;
    store.update("selection", patch);
  }

  function clearFocus(): void {
    store.update("selection", {
      focusedSpanId: null,
      spanFocus: null,
      selectedTaskId: null,
      pinnedEvent: null,
    });
  }

  /** Step the filtered-span navigation: center the viewport on the next/prev
   *  match and highlight it + its ancestors. */
  function stepNav(delta: number): void {
    const data = spanData();
    const s = state();
    const matches = getNavMatches(data, filterState(s));
    if (matches.length === 0) return;
    navIndex = (((navIndex + delta) % matches.length) + matches.length) % matches.length;
    const span = matches[navIndex]!;
    const spanDur = span.end - span.start;
    const viewDur = Math.max(spanDur * 10, 1e6);
    const { minTs, maxTs } = s.viewport;
    const viewStart = Math.max(minTs, span.start - viewDur * 0.3);
    const viewEnd = Math.min(maxTs, viewStart + viewDur);
    store.update("viewport", { viewStart, viewEnd });
    store.update("selection", {
      spanFocus: { spanId: span.spanId, chain: spanFocusChain(span.spanId, data) },
    });
  }

  // ── Canvas paint ───────────────────────────────────────────────────────

  function paint(
    canvas: HTMLCanvasElement,
    drawW: number,
    trackHeight: number,
    dpr: number,
    viewStart: number,
    viewEnd: number,
  ): void {
    if (sizer === null || sizerCanvas !== canvas) {
      sizer = createCanvasSizer<CanvasRenderingContext2D>(canvas);
      sizerCanvas = canvas;
    }
    const canvasH = Math.max(0, trackHeight - CONTROLS_H);
    const ctx = sizer.ensure(drawW, canvasH, dpr);
    const s = state();
    const data = spanData();
    const model = renderModel(data, s, drawW, canvasH);
    lastHits = model.buckets;
    drawSpansCanvas(ctx, model, data, s, drawW, canvasH, viewStart, viewEnd, colorOf);
  }

  // ── Templates (label, controls) ────────────────────────────────────────

  function rowTemplate(track: TrackSpec): TemplateResult {
    const s = state();
    const data = spanData();
    const filter = filterState(s);
    const hasSpans = data.spanNames.length > 0 || data.unmatchedSpans.length > 0;
    const label = spanLabelModel(s.selection.focusedSpanId, data);
    return html`
      <div
        class="d9-track d9-track--spans"
        data-track-id=${track.id}
        style="height:${track.height}px"
      >
        <div class="d9-track-label d9-spans-label" id="d9-track-label-spans">
          ${label === null
            ? html`<span class="d9-track-name">${track.label}</span>`
            : spanMetaTemplate(label)}
        </div>
        <div class="d9-track-body d9-spans-body">
          ${hasSpans ? controlsTemplate(data, s, filter) : nothing}
          <div class="d9-spans-canvas-wrap">
            <canvas
              class="d9-track-canvas d9-spans-canvas"
              data-track-canvas=${track.id}
              aria-labelledby="d9-track-label-spans"
              role="img"
              @click=${onCanvasClick}
              @mousemove=${onCanvasHover}
              @mouseleave=${hideTooltip}
            ></canvas>
          </div>
        </div>
      </div>
    `;
  }

  /** Focused-span metadata rows with copy buttons. */
  function spanMetaTemplate(label: NonNullable<ReturnType<typeof spanLabelModel>>): TemplateResult {
    return html`
      <div class="d9-spans-meta-name">${label.name}</div>
      ${repeat(
        label.rows,
        (r) => r.key,
        (r) => html`
          <div class="d9-kv-row">
            <span class="d9-kv-key">${r.key}</span>
            <span class="d9-kv-val" title=${r.display}>${r.display}</span>
            <button
              class="d9-kv-copy"
              type="button"
              title="Copy value"
              @click=${(e: MouseEvent) => copyValue(e, r.copy)}
            >
              ⎘
            </button>
          </div>
        `,
      )}
    `;
  }

  /** Filter, nav, count, percentile, clear, chips, unmatched warning. */
  function controlsTemplate(
    data: SpanTrackData,
    s: StoreState,
    filter: SpanFilterState,
  ): TemplateResult {
    const matches = getNavMatches(data, filter);
    const active = isFilterActive(filter);
    const countText = !active
      ? ""
      : navIndex >= 0
        ? `${navIndex + 1}/${matches.length}`
        : matches.length > 0
          ? `${matches.length} matches`
          : "";
    const chips = spanChipModels(data, s.uiPrefs.selectedSpanNames, colorOf);
    const navDisabled = matches.length === 0;
    return html`
      <div class="d9-spans-controls" role="group" aria-label="Span filter">
        <input
          class="d9-span-filter"
          type="text"
          placeholder="Filter spans"
          aria-label="Filter spans by name or field"
          .value=${filter.text}
          @input=${(e: Event) => setFilterText((e.target as HTMLInputElement).value)}
        />
        ${filter.text.length > 0
          ? html`<button
              class="d9-span-btn"
              type="button"
              title="Clear filter"
              @click=${() => setFilterText("")}
            >
              ✕
            </button>`
          : nothing}
        <button
          class="d9-span-btn"
          type="button"
          title="Previous match"
          aria-label="Previous matching span"
          ?disabled=${navDisabled}
          @click=${() => stepNav(-1)}
        >
          ◀
        </button>
        <button
          class="d9-span-btn"
          type="button"
          title="Next match"
          aria-label="Next matching span"
          ?disabled=${navDisabled}
          @click=${() => stepNav(1)}
        >
          ▶
        </button>
        <span class="d9-span-count">${countText}</span>
        <select
          class="d9-span-pct"
          aria-label="Percentile filter"
          .value=${String(filter.pctFloor)}
          @change=${(e: Event) => setPctFilter(Number((e.target as HTMLSelectElement).value))}
        >
          ${PCT_OPTIONS.map(
            (o) => html`<option value=${o.value}>${o.label}</option>`,
          )}
        </select>
        ${filter.selectedNames.size > 0
          ? html`<button
              class="d9-span-btn"
              type="button"
              @click=${clearNames}
            >
              ✕ Clear
            </button>`
          : nothing}
        <span class="d9-span-chips">
          ${repeat(
            chips,
            (c) => c.name,
            (c) => html`
              <button
                class=${classMap({ "d9-span-chip": true, on: c.active })}
                type="button"
                style="--chip:${c.color}"
                aria-pressed=${c.active ? "true" : "false"}
                @click=${() => toggleName(c.name)}
              >
                ${c.name}
              </button>
            `,
          )}
        </span>
        ${data.unmatchedSpans.length > 0
          ? html`<span
              class="d9-span-unmatched"
              title="Spans with an enter but no exit (trace ended mid-span or a segment rotated)"
              >⚠ ${data.unmatchedSpans.length} unmatched</span
            >`
          : nothing}
      </div>
    `;
  }

  // ── Canvas interaction (click, hover) ──────────────────────────────────

  function bucketAt(canvas: HTMLCanvasElement, ev: MouseEvent): SpanDrawBucket | null {
    const rect = canvas.getBoundingClientRect();
    const mx = ev.clientX - rect.left;
    const my = ev.clientY - rect.top;
    for (let i = lastHits.length - 1; i >= 0; i--) {
      const h = lastHits[i]!;
      if (mx >= h.x1 && mx <= h.x2 && my >= h.y && my <= h.y + h.h) return h;
    }
    return null;
  }

  function onCanvasClick(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const bucket = bucketAt(canvas, ev);
    if (bucket) focusSpan(bucket.representative.spanId);
    else clearFocus();
  }

  function onCanvasHover(ev: MouseEvent): void {
    const canvas = ev.currentTarget as HTMLCanvasElement;
    const bucket = bucketAt(canvas, ev);
    if (bucket === null) {
      hideTooltip();
      return;
    }
    showTooltip(ev, bucket);
  }

  function tooltip(): HTMLDivElement {
    if (tooltipEl === null) {
      tooltipEl = document.createElement("div");
      tooltipEl.className = "d9-span-tooltip";
      tooltipEl.setAttribute("role", "tooltip");
      document.body.appendChild(tooltipEl);
    }
    return tooltipEl;
  }

  function showTooltip(ev: MouseEvent, bucket: SpanDrawBucket): void {
    const el = tooltip();
    render(tooltipContent(bucket, spanData()), el);
    el.style.display = "block";
    // Place near the cursor, flipping above if it would overflow the viewport.
    const pad = 12;
    const rect = el.getBoundingClientRect();
    let x = ev.clientX + pad;
    let y = ev.clientY + pad;
    if (x + rect.width > window.innerWidth) x = ev.clientX - rect.width - pad;
    if (y + rect.height > window.innerHeight) y = ev.clientY - rect.height - pad;
    el.style.left = `${Math.max(0, x)}px`;
    el.style.top = `${Math.max(0, y)}px`;
  }

  function hideTooltip(): void {
    if (tooltipEl !== null) tooltipEl.style.display = "none";
  }

  function tooltipContent(bucket: SpanDrawBucket, data: SpanTrackData): TemplateResult {
    if (bucket.spans.length === 1) {
      const rep = bucket.representative;
      const dur = rep.end - rep.start;
      const idle = dur - rep.activeNs;
      const pct = spanPercentileRank(rep, data.durationsByName);
      const workers = [...new Set(rep.segments.map((sg) => sg.workerId))].sort(
        (a, b) => a - b,
      );
      const fields = Object.entries(rep.fields);
      return html`
        <div>
          <span class="tt-k">Span:</span> ${rep.spanName}
          <span class="tt-dim"
            >${formatHumanDuration(dur)}${pct != null ? ` (P${pct})` : ""}</span
          >
        </div>
        <div>
          <span class="tt-k">Active:</span> ${formatHumanDuration(rep.activeNs)}${idle > 0
            ? html` · <span class="tt-k">Idle:</span> ${formatHumanDuration(idle)}`
            : nothing}${rep.segments.length > 1
            ? html` · <span class="tt-k">Polls:</span> ${rep.segments.length}`
            : nothing}
        </div>
        <div>
          <span class="tt-k">Worker${workers.length > 1 ? "s" : ""}:</span>
          ${workers.join(", ")}
        </div>
        ${fields.length > 0
          ? html`<div>
              <span class="tt-k">Fields:</span>
              ${fields
                .map(([k, v]) => `${k}=${formatFieldValue(v)}`)
                .join(", ")}
            </div>`
          : nothing}
      `;
    }
    // Cluster tooltip: size + top names + min/max duration.
    const names = topNames(bucket.spans);
    let minDur = Infinity;
    let maxDur = 0;
    for (const sp of bucket.spans) {
      const d = sp.end - sp.start;
      if (d < minDur) minDur = d;
      if (d > maxDur) maxDur = d;
    }
    return html`
      <div><span class="tt-k">Cluster:</span> ${bucket.spans.length} spans</div>
      <div><span class="tt-k">Names:</span> ${names}</div>
      <div>
        <span class="tt-k">Duration:</span> ${formatHumanDuration(minDur)} –
        ${formatHumanDuration(maxDur)}
      </div>
    `;
  }

  function copyValue(ev: MouseEvent, value: string): void {
    const btn = ev.currentTarget as HTMLButtonElement;
    void navigator.clipboard?.writeText(value);
    const prev = btn.textContent;
    btn.textContent = "✓";
    window.setTimeout(() => {
      btn.textContent = prev;
    }, 800);
  }

  function dispose(): void {
    if (tooltipEl !== null) {
      tooltipEl.remove();
      tooltipEl = null;
    }
  }

  return { rowTemplate, paint, dispose };
}

// ── Pure helpers (no store) ──────────────────────────────────────────────

/** Build the worker poll lanes for task resolution. Empty when the trace has no
 *  events. */
function buildWorkerLanes(trace: ParsedTrace | null): Record<number, LaneSpans> {
  if (trace === null || trace.maxTs === null) return {};
  return sharedWorkerSpans(trace).workerSpans;
}

/** Most-frequent-first "name xN" list, for cluster tooltips. */
function topNames(spans: readonly TracingSpan[], limit = 5): string {
  const counts = new Map<string, number>();
  for (const s of spans) counts.set(s.spanName, (counts.get(s.spanName) ?? 0) + 1);
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, limit)
    .map(([n, c]) => (c > 1 ? `${n} ×${c}` : n))
    .join(", ");
}

// ── Canvas draw (extracted so the render input is unit-testable) ──────────

/**
 * Draw the span clusters into `ctx` (already DPR-scaled + sized to
 * drawW x canvasH). The representative bar's alpha encodes the dimming: a
 * highlighted cluster is bright, the rest recede when a highlight is active.
 * Draw-area-relative x (0..drawW), exactly the axis's nsToDrawX, so span bars
 * line up with the ticks.
 */
export function drawSpansCanvas(
  ctx: CanvasRenderingContext2D,
  model: SpanRenderModel,
  data: SpanTrackData,
  s: StoreState,
  drawW: number,
  canvasH: number,
  viewStart: number,
  viewEnd: number,
  colorOf: (name: string) => string,
): void {
  ctx.clearRect(0, 0, drawW, canvasH);
  ctx.fillStyle = CANVAS_BG;
  ctx.fillRect(0, 0, drawW, canvasH);
  if (drawW <= 0 || canvasH <= 0) return;

  if (model.emptyReason !== null) {
    ctx.fillStyle = EMPTY_TEXT;
    ctx.font = "12px sans-serif";
    ctx.textAlign = "left";
    const msg =
      model.emptyReason === "no-roots"
        ? "No root spans in view"
        : "No spans in view" +
          (s.uiPrefs.spanFilter ? ` (filter: "${s.uiPrefs.spanFilter}")` : "");
    ctx.fillText(msg, 10, Math.min(60, canvasH / 2));
    return;
  }

  const highlight = spanHighlight(s.selection);
  const span = viewEnd - viewStart || 1;
  const nsToX = (ns: number): number => {
    const x = ((ns - viewStart) / span) * drawW;
    return x < 0 ? 0 : x > drawW ? drawW : x;
  };

  for (const bucket of model.buckets) {
    const rep = bucket.representative;
    const isSel = highlight.set.has(rep.spanId);
    const dim = highlight.active;
    const color = colorOf(rep.spanName);

    // Representative envelope bar.
    ctx.fillStyle = color;
    ctx.globalAlpha = isSel ? 0.4 : dim ? 0.08 : 0.15;
    ctx.fillRect(bucket.x1, bucket.y, Math.max(bucket.x2 - bucket.x1, 1), bucket.h);

    // Active (on-worker) segments, brighter.
    ctx.globalAlpha = isSel ? 1.0 : dim ? 0.3 : 0.8;
    for (const seg of rep.segments) {
      if (seg.end < viewStart || seg.start > viewEnd) continue;
      const sx1 = nsToX(seg.start);
      const sx2 = nsToX(seg.end);
      ctx.fillRect(sx1, bucket.y, Math.max(sx2 - sx1, 1), bucket.h);
    }
  }
  ctx.globalAlpha = 1;

  // Log-scale y-axis duration ticks.
  if (model.minDur > 0 && model.maxDur > 0) {
    const PAD_TOP = 2;
    const usableH = canvasH - PAD_TOP - 2 - BASE_BAR_H;
    const minL = Math.log(model.minDur);
    const maxL = Math.log(model.maxDur);
    const range = maxL - minL || 1;
    ctx.fillStyle = TICK_TEXT;
    ctx.font = "9px sans-serif";
    ctx.textAlign = "left";
    const ticks: number[] = [];
    for (const d of [1, 10, 100, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9]) {
      if (d >= model.minDur && d <= model.maxDur) ticks.push(d);
    }
    if (ticks.length < 2) {
      ticks.length = 0;
      ticks.push(model.minDur, model.maxDur);
    }
    for (const dur of ticks) {
      const norm = (Math.log(dur) - minL) / range;
      const y = PAD_TOP + (1 - norm) * usableH;
      if (y < 2 || y > canvasH - 8) continue;
      ctx.globalAlpha = 0.4;
      ctx.fillRect(0, y, drawW, 0.5);
      ctx.globalAlpha = 0.7;
      ctx.fillText(formatHumanDuration(dur), 2, y - 2);
    }
    ctx.globalAlpha = 1;
  }
}
