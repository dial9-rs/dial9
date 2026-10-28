import { html, nothing, render, svg, type TemplateResult } from "lit-html";
import type { CategoricalHistogram, CategoryLatency } from "../../lib/trace/span-tail-analysis.js";
import { formatHumanDuration } from "../../lib/trace/api_format.js";
import {
  categoryBinCount,
  categoricalHistogramStack,
  histogramBrushRange,
  histogramMarkerRows,
  histogramStackArea,
  latencyToHistogramPosition,
  type CategoricalStack,
  type HistogramRange,
} from "./categorical-histogram-scale.js";
import "./categorical-histograms.css";

export type HistogramViewMode = "overlay" | "stacked";

export interface CategoricalHistogramOptions {
  mode?: HistogramViewMode;
  selection?: HistogramRange | null;
  onSelectRange?: (minMs: number | null, maxMs: number | null) => void;
}

export interface PropertyHistogramOptions extends CategoricalHistogramOptions {
  selectedKey: string | null;
  onSelectProperty: (key: string) => void;
  onModeChange: (mode: HistogramViewMode) => void;
  controls?: TemplateResult | typeof nothing;
}

const colors = [
  "#64c9f3", "#d6a0ff", "#f5bc64", "#79d6af", "#f78da7", "#96a8ff",
  "#d1db73", "#efad86", "#82dbd9", "#e691df", "#b0c1d2", "#e1cdad",
];
/** Keep a property's values consistent across distributions and cohort bars. */
export const propertyValueColor = (index: number) => index < 0 ? "#8393a5" : colors[index % colors.length]!;
const number = (n: number, digits = 0) => n.toLocaleString(undefined, { maximumFractionDigits: digits });
const duration = (ms: number) => formatHumanDuration(ms * 1e6);
const percent = (n: number) => `${number(n * 100, 1)}%`;
const spanCount = (n: number) => `${number(n)} ${n === 1 ? "span" : "spans"}`;

interface Series {
  value: CategoryLatency;
  color: string;
}
type AvailableStack = Extract<CategoricalStack, { available: true }>;

function legend({ value, color }: Series) {
  return html`<div class="tail-category-value">
    <span class="tail-category-name"><i style="background:${color}"></i><strong>${value.value}</strong>
      <span class="tail-muted">${spanCount(value.count)}</span></span>
    <span class="tail-category-p99" title="99% of spans with this value finish within ${duration(value.p99Ms)}. Median: ${duration(value.medianMs)}.">
      p99 <strong style="color:${color}">${duration(value.p99Ms)}</strong>
    </span>
  </div>`;
}

function ticksForHistogram(edges: readonly number[]) {
  const bins = edges.length - 1;
  const lower = edges[1]!;
  const upper = edges[bins]!;
  const ticks = [{ position: 0, label: "0" }];
  const firstExponent = Math.ceil(Math.log10(lower));
  const lastExponent = Math.floor(Math.log10(upper));
  const stride = Math.max(1, Math.ceil((lastExponent - firstExponent + 1) / 5));
  for (let exponent = firstExponent; exponent <= lastExponent; exponent += stride) {
    const latency = 10 ** exponent;
    ticks.push({ position: latencyToHistogramPosition(edges, latency), label: duration(latency) });
  }
  if (ticks.at(-1)!.position < bins) ticks.push({ position: bins, label: duration(upper) });
  // Keep zero and the observed maximum, with room for labels on narrow plots.
  const endpoint = ticks.at(-1)!;
  const spaced = [ticks[0]!];
  for (const tick of ticks.slice(1, -1)) {
    const previous = spaced.at(-1)!;
    // At 10px, allow 6px per character on a conservative 200px mobile canvas.
    // Zero extends right, the endpoint extends left, interior ticks are centered.
    const previousGap = (previous.label.length * (spaced.length === 1 ? 6 : 3)
      + tick.label.length * 3 + 8) / 200;
    const endpointGap = (tick.label.length * 3 + endpoint.label.length * 6 + 8) / 200;
    if ((tick.position - previous.position) / bins >= Math.max(.25, previousGap)
      && (endpoint.position - tick.position) / bins >= Math.max(.25, endpointGap)) spaced.push(tick);
  }
  spaced.push(endpoint);
  return spaced;
}

function binReadout(histogram: CategoricalHistogram, series: readonly Series[], bin: number, stack: AvailableStack | null) {
  const finalBin = bin === histogram.binEdgesMs.length - 2;
  return html`<div class="tail-category-tooltip-range">
      ${duration(histogram.binEdgesMs[bin]!)}–${duration(histogram.binEdgesMs[bin + 1]!)}
      <span>${finalBin ? "inclusive" : "upper bound excluded"}</span>
    </div>
    ${series.map(({ value, color }) => {
      const count = categoryBinCount(value, bin);
      const share = stack ? count! / stack.population : value.shares[bin];
      return html`<div class="tail-category-tooltip-series">
        <span><i style="background:${color}"></i>${value.value}</span>
        <strong>${count == null ? "Unavailable" : spanCount(count)}</strong>
        <span>${share == null ? "—" : percent(share)} of ${number(stack?.population ?? value.count)}</span>
      </div>`;
    })}
    ${stack ? html`<div class="tail-category-tooltip-series tail-category-tooltip-total">
      <span>Total</span><strong>${spanCount(stack.totalCounts[bin]!)}</strong>
      <span>${percent(stack.totalShares[bin]!)} of ${number(stack.population)}</span>
    </div>` : nothing}`;
}

function plot(
  histogram: CategoricalHistogram,
  series: readonly Series[],
  options: CategoricalHistogramOptions,
  stack: AvailableStack | null,
) {
  const edges = histogram.binEdgesMs;
  const bins = edges.length - 1;
  const maximum = Math.max(.1, Math.min(1, Math.ceil((stack?.maximum ?? histogram.maxShare) * 10) / 10));
  const ticks = ticksForHistogram(edges);
  const markerPositions = series.map(s => latencyToHistogramPosition(edges, s.value.p99Ms) / bins);
  const markerRows = histogramMarkerRows(markerPositions);
  const markerHeight = (Math.max(...markerRows) + 1) * 19;
  const selection = options.selection;
  const selectedFrom = selection?.minMs == null ? 0 : latencyToHistogramPosition(edges, selection.minMs);
  const selectedTo = selection?.maxMs == null ? bins : latencyToHistogramPosition(edges, selection.maxMs);
  const banded = selection?.minMs != null || selection?.maxMs != null;
  const step = (shares: readonly number[]) => {
    let path = "M0 1";
    shares.forEach((share, i) => { path += `V${1 - share / maximum}H${i + 1}`; });
    return `${path}V1Z`;
  };
  const totalLine = stack
    ? `M0 ${1 - stack.totalShares[0]! / maximum}${stack.totalShares.map((share, bin) => `V${1 - share / maximum}H${bin + 1}`).join("")}`
    : "";

  let dragStart: number | null = null;
  let dragPointer: number | null = null;
  let keyboardBin = Math.max(0, Math.min(bins - 1, Math.floor(selectedFrom)));
  let keyboardAnchor: number | null = null;

  const canvasFor = (element: SVGSVGElement) => element.parentElement!;
  const localPosition = (element: SVGSVGElement, clientX: number) => {
    const rect = element.getBoundingClientRect();
    return Math.max(0, Math.min(bins, (clientX - rect.left) / rect.width * bins));
  };
  const setBrush = (element: SVGSVGElement, from: number, to: number) => {
    const brush = element.querySelector<SVGRectElement>(".tail-category-brush")!;
    brush.setAttribute("x", String(Math.min(from, to)));
    brush.setAttribute("width", String(Math.abs(to - from)));
    brush.style.display = "block";
  };
  const hideBrush = (element: SVGSVGElement) => {
    element.querySelector<SVGRectElement>(".tail-category-brush")!.style.display = "none";
  };
  const showBin = (element: SVGSVGElement, bin: number, keyboard: boolean) => {
    const tooltip = canvasFor(element).querySelector<HTMLElement>(".tail-category-tooltip")!;
    const highlight = element.querySelector<SVGRectElement>(".tail-category-hover")!;
    const bounded = Math.max(0, Math.min(bins - 1, bin));
    highlight.setAttribute("x", String(bounded));
    highlight.style.display = "block";
    render(binReadout(histogram, series, bounded, stack), tooltip);
    tooltip.hidden = false;
    // Anchor to one half so long value labels remain in bounds.
    tooltip.style.left = bounded < bins / 2 ? "5px" : "auto";
    tooltip.style.right = bounded < bins / 2 ? "auto" : "5px";
    tooltip.setAttribute("aria-live", keyboard ? "polite" : "off");
  };
  const hideBin = (element: SVGSVGElement) => {
    canvasFor(element).querySelector<HTMLElement>(".tail-category-tooltip")!.hidden = true;
    element.querySelector<SVGRectElement>(".tail-category-hover")!.style.display = "none";
  };
  const select = (from: number, to: number) => {
    const range = histogramBrushRange(edges, from, to);
    options.onSelectRange?.(range.minMs, range.maxMs);
  };
  const onPointerDown = (event: PointerEvent) => {
    if (!options.onSelectRange || event.button !== 0) return;
    const element = event.currentTarget as SVGSVGElement;
    dragStart = localPosition(element, event.clientX);
    dragPointer = event.pointerId;
    hideBin(element);
    setBrush(element, dragStart, dragStart);
    element.setPointerCapture(event.pointerId);
    event.preventDefault();
  };
  const onPointerMove = (event: PointerEvent) => {
    const element = event.currentTarget as SVGSVGElement;
    const position = localPosition(element, event.clientX);
    if (dragStart != null) setBrush(element, dragStart, position);
    else showBin(element, Math.floor(position), false);
  };
  const cancelDrag = (event: PointerEvent) => {
    const element = event.currentTarget as SVGSVGElement;
    dragStart = null;
    dragPointer = null;
    hideBrush(element);
  };
  const onPointerUp = (event: PointerEvent) => {
    if (dragStart == null || dragPointer !== event.pointerId) return;
    const element = event.currentTarget as SVGSVGElement;
    const from = dragStart;
    const to = localPosition(element, event.clientX);
    cancelDrag(event);
    element.releasePointerCapture(event.pointerId);
    // Like the main histogram, tiny pointer drags are inspections.
    if (Math.abs(to - from) / bins * element.getBoundingClientRect().width >= 2) select(from, to);
    else showBin(element, Math.floor(to), false);
  };
  const onKeyDown = (event: KeyboardEvent) => {
    const element = event.currentTarget as SVGSVGElement;
    if (event.key === "ArrowLeft" || event.key === "ArrowRight" || event.key === "Home" || event.key === "End") {
      if (event.shiftKey && keyboardAnchor == null) keyboardAnchor = keyboardBin;
      if (!event.shiftKey) keyboardAnchor = null;
      keyboardBin = event.key === "Home" ? 0
        : event.key === "End" ? bins - 1
        : Math.max(0, Math.min(bins - 1, keyboardBin + (event.key === "ArrowRight" ? 1 : -1)));
      if (keyboardAnchor != null) setBrush(element, Math.min(keyboardAnchor, keyboardBin), Math.max(keyboardAnchor, keyboardBin) + 1);
      else hideBrush(element);
      showBin(element, keyboardBin, true);
      event.preventDefault();
    } else if (event.key === "Enter" && options.onSelectRange) {
      const from = Math.min(keyboardAnchor ?? keyboardBin, keyboardBin);
      const to = Math.max(keyboardAnchor ?? keyboardBin, keyboardBin) + 1;
      keyboardAnchor = null;
      hideBrush(element);
      select(from, to);
      event.preventDefault();
    } else if (event.key === "Escape") {
      keyboardAnchor = null;
      hideBrush(element);
      hideBin(element);
    }
  };

  return html`<div class="tail-category-plot">
    <div class="tail-category-y" style="padding-top:${markerHeight}px">
      <span>${percent(maximum)}</span><span>${percent(maximum / 2)}</span><span>0%</span>
    </div>
    <div class="tail-category-canvas">
      <div class="tail-category-markers" style="height:${markerHeight}px">
        ${series.map((s, i) => html`<span class="tail-category-marker"
          style="left:${markerPositions[i]! * 100}%;top:${markerRows[i]! * 19}px;color:${s.color};--marker-shift:${markerPositions[i]! < .12 ? "0%" : markerPositions[i]! > .88 ? "-100%" : "-50%"}"
          title="${s.value.value} p99: ${duration(s.value.p99Ms)}">
          <strong>p99</strong> ${s.value.value}
        </span>`)}
      </div>
      <svg viewBox="0 0 ${bins} 1" preserveAspectRatio="none" role="group" tabindex="0"
        aria-label="${histogram.key}: ${stack ? "stacked contributions to the total elapsed-time distribution" : "overlaid elapsed-time distributions"} for ${series.map(s => s.value.value).join(", ")}. Arrow keys inspect latency bins. Shift and arrow keys extend a selection; Enter selects it."
        @pointerdown=${onPointerDown} @pointermove=${onPointerMove} @pointerup=${onPointerUp}
        @pointercancel=${cancelDrag} @lostpointercapture=${cancelDrag}
        @pointerleave=${(event: PointerEvent) => { if (dragStart == null) hideBin(event.currentTarget as SVGSVGElement); }}
        @keydown=${onKeyDown}
        @focus=${(event: FocusEvent) => showBin(event.currentTarget as SVGSVGElement, keyboardBin, true)}
        @blur=${(event: FocusEvent) => { hideBin(event.currentTarget as SVGSVGElement); hideBrush(event.currentTarget as SVGSVGElement); }}>
        ${[0, .5, 1].map(y => svg`<path d="M0 ${y}H${bins}" class="tail-category-grid"></path>`)}
        ${ticks.map(t => svg`<path d="M${t.position} 0V1" class="tail-category-grid"></path>`)}
        ${series.map((s, i) => svg`<path class="tail-category-distribution"
          d=${stack ? histogramStackArea(stack.layers[i]!.lower, stack.layers[i]!.upper, maximum) : step(s.value.shares)}
          fill=${s.color} stroke=${s.color} fill-opacity=${stack ? .65 : series.length > 1 ? .08 : .16}></path>`)}
        ${stack ? svg`<path class="tail-category-total" d=${totalLine}><title>Total distribution of spans with recorded ${histogram.key}</title></path>` : nothing}
        ${banded ? svg`<rect x="0" y="0" width=${selectedFrom} height="1" class="tail-category-outside"></rect>
          <rect x=${selectedTo} y="0" width=${Math.max(0, bins - selectedTo)} height="1" class="tail-category-outside"></rect>` : nothing}
        ${series.map(s => svg`<line x1=${latencyToHistogramPosition(edges, s.value.p99Ms)} x2=${latencyToHistogramPosition(edges, s.value.p99Ms)}
          y1="0" y2="1" stroke=${s.color} stroke-dasharray="3 2" class="tail-category-percentile">
          <title>${s.value.value} p99: ${duration(s.value.p99Ms)}</title>
        </line>`)}
        <rect x="0" y="0" width="1" height="1" class="tail-category-hover" style="display:none"></rect>
        <rect x="0" y="0" width="0" height="1" class="tail-category-brush" style="display:none"></rect>
      </svg>
      <div class="tail-category-tooltip" role="tooltip" hidden></div>
      <div class="tail-category-x">
        ${ticks.map((tick, i) => html`<span class="${i === 0 ? "first" : i === ticks.length - 1 ? "last" : ""}" style="left:${tick.position / bins * 100}%">${tick.label}</span>`)}
      </div>
      <div class="tail-category-axis-label">Elapsed time <span class="tail-muted">· log scale</span></div>
    </div>
  </div>`;
}

/** Draws backend-computed distributions; the browser only lays out the chart. */
export function renderCategoricalHistogram(
  histogram: CategoricalHistogram,
  options: CategoricalHistogramOptions = {},
) {
  const series = histogram.values.map((value, i) => ({ value, color: propertyValueColor(i) }));
  const overlay = (options.mode ?? "stacked") === "overlay";
  const stack = overlay ? null : categoricalHistogramStack(histogram);
  const availableStack = stack?.available ? stack : null;
  return html`<section class="tail-category-chart ${overlay ? "overlay" : "stacked"}">
    <div class="tail-category-caption"><span>${overlay ? "% of spans within each value" : `% of ${spanCount(histogram.count)} with a recorded value`}</span>
      ${histogram.missingCount ? html`<span>${number(histogram.missingCount)} missing values excluded</span>` : nothing}
      ${options.onSelectRange && (overlay || availableStack) ? html`<span class="tail-category-brush-hint">Drag to select latency</span>` : nothing}
    </div>
    <div class="tail-category-overlay-legend">${series.map(legend)}
      ${availableStack ? html`<span class="tail-category-total-key"><i></i>Total</span>` : nothing}
    </div>
    ${overlay || availableStack ? plot(histogram, series, options, availableStack)
      : html`<p class="tail-category-unavailable" role="status">${stack?.available === false && stack.reason === "incomplete-values"
        ? "Some recorded values are omitted; the total distribution is unavailable."
        : stack?.available === false && stack.reason === "missing-counts"
          ? "Exact bin counts are unavailable in this cached report. Refresh analysis to stack."
          : "Exact bin counts are incomplete; the total distribution is unavailable."}</p>`}
  </section>`;
}

/** One property selector and one view control drive the displayed graph. */
export function renderPropertyHistograms(
  histograms: readonly CategoricalHistogram[],
  options: PropertyHistogramOptions,
) {
  const histogram = histograms.find(h => h.key === options.selectedKey) ?? histograms[0];
  if (!histogram) return nothing;
  const mode = options.mode ?? "stacked";
  return html`<section class="tail-properties">
    <div class="tail-property-heading"><h4>${histograms.length === 1 ? histogram.key : "Properties"}</h4>
      <div class="tail-histogram-modes" role="group" aria-label="Property histogram view">
        <button type="button" aria-pressed=${mode === "overlay"} @click=${() => options.onModeChange("overlay")}>Overlay</button>
        <button type="button" aria-pressed=${mode === "stacked"} @click=${() => options.onModeChange("stacked")}>Stacked</button>
      </div>
    </div>
    ${histograms.length > 1 ? html`<div class="tail-property-filters" role="group" aria-label="Displayed property">
      ${histograms.map(h => html`<button type="button" aria-pressed=${h.key === histogram.key}
        @click=${() => options.onSelectProperty(h.key)} title="Show latency by ${h.key}">${h.key}</button>`)}
    </div>` : nothing}
    ${options.controls ?? nothing}
    ${renderCategoricalHistogram(histogram, options)}
  </section>`;
}
