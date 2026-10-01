// The duration histogram: log-spaced bars, P50/P90/P99/P99.9 guide lines, and
// a drag-to-select duration band.
//
// SVG is built imperatively rather than through lit-html: the brush needs
// direct node handles and pointer capture, and no interpolated value here comes
// from user data (durations and counts are numbers).

import {
  durationAtPercentile,
  fmtNs,
  formatHumanDuration,
  normalizeSpanHistogram,
  spanBrushToBand,
  spanHistogramLayout,
  spanNsToPx,
} from "../../lib/trace/index.js";
import type {
  DurationBand,
  HistogramBarLike,
  SpanHistogramBar,
} from "../../lib/trace/index.js";

const SVG_NS = "http://www.w3.org/2000/svg";
/** Drawing width in viewBox units; the SVG scales to its container. */
const W = 600;
const HIST_H = 42;

const PERCENTILES = [
  { p: 50, label: "P50" },
  { p: 90, label: "P90" },
  { p: 99, label: "P99" },
  { p: 99.9, label: "P99.9" },
] as const;

/** The filtered subset used to split the baseline into two disjoint series. */
export interface HistogramComparison {
  label: string;
  histogram: readonly HistogramBarLike[] | undefined;
  count: number | null;
  coverageLabel: string | null;
  pending: boolean;
  hasSnapshot: boolean;
  /** The filtered and baseline histograms cover the exact same folded files. */
  comparable: boolean;
  error: string | null;
}

/** A native hover tooltip for an SVG node. */
function titleEl(text: string): SVGTitleElement {
  const t = document.createElementNS(SVG_NS, "title");
  t.textContent = text;
  return t;
}

const bucketKey = (bar: Pick<SpanHistogramBar, "lo_ns" | "hi_ns">): string =>
  `${bar.lo_ns}:${bar.hi_ns}`;

/**
 * Project a comparison distribution onto the baseline's duration buckets.
 *
 * Attribute filtering may remove empty tail buckets entirely. Reintroducing
 * them with zero counts keeps both rows on exactly the same x-axis.
 */
export function alignHistogramToBaseline(
  baseline: readonly HistogramBarLike[] | undefined,
  comparison: readonly HistogramBarLike[] | undefined,
): SpanHistogramBar[] {
  const counts = new Map(
    normalizeSpanHistogram(comparison).map((bar) => [
      bucketKey(bar),
      bar.count,
    ]),
  );
  return normalizeSpanHistogram(baseline).map((bar) => ({
    ...bar,
    count: counts.get(bucketKey(bar)) ?? 0,
  }));
}

/** Baseline minus filtered subset, bucket by bucket: exactly `Not(filter)`. */
export function complementHistogram(
  baseline: readonly HistogramBarLike[] | undefined,
  filtered: readonly HistogramBarLike[] | undefined,
): SpanHistogramBar[] {
  const aligned = alignHistogramToBaseline(baseline, filtered);
  return normalizeSpanHistogram(baseline).map((bar, index) => ({
    ...bar,
    count: Math.max(0, bar.count - (aligned[index]?.count ?? 0)),
  }));
}

interface DrawSeries {
  label: string;
  bars: readonly SpanHistogramBar[];
  color: string;
  inactiveColor: string;
  /** Narrower foreground bars keep both overlaid populations visible. */
  widthScale: number;
  percentileY: number;
  percentileDash: string;
}

interface HistogramLayer {
  count: number;
  widthScale: number;
}

/**
 * SVG painter order for one duration bucket, back to front.
 *
 * Taller bars are painted first so the shorter population is never hidden.
 * Equal-height bars retain the useful width treatment: wide first, narrow last.
 */
export function histogramLayerOrder(
  layers: readonly HistogramLayer[],
): number[] {
  return layers
    .map((layer, index) => ({ ...layer, index }))
    .filter((layer) => layer.count > 0)
    .sort(
      (a, b) =>
        b.count - a.count || b.widthScale - a.widthScale || a.index - b.index,
    )
    .map((layer) => layer.index);
}

function appendLegendItem(
  container: HTMLElement,
  label: string,
  color: string,
  detail: string,
): void {
  const item = document.createElement("span");
  item.className = "histogram-series-label";

  const swatch = document.createElement("span");
  swatch.className = "histogram-series-swatch";
  swatch.style.background = color;
  item.appendChild(swatch);

  const name = document.createElement("span");
  name.className = "histogram-series-name";
  name.textContent = label;
  item.appendChild(name);

  const meta = document.createElement("span");
  meta.className = "histogram-series-meta";
  meta.textContent = detail;
  item.appendChild(meta);

  container.appendChild(item);
}

function appendLegend(
  container: HTMLElement,
  items: readonly { label: string; color: string; detail: string }[],
  status: string | null,
): void {
  const legend = document.createElement("div");
  legend.className = "histogram-series-labels";
  for (const item of items) {
    appendLegendItem(legend, item.label, item.color, item.detail);
  }
  if (status) {
    const state = document.createElement("span");
    state.className = "histogram-series-status";
    state.textContent = status;
    legend.appendChild(state);
  }
  container.appendChild(legend);
}

function appendHistogramHelp(
  container: HTMLElement,
  comparison: boolean,
  open: boolean,
): void {
  const legend = container.querySelector<HTMLElement>(
    ".histogram-series-labels",
  );
  if (!legend) return;

  const help = document.createElement("details");
  help.className = "histogram-help";
  help.open = open;

  const trigger = document.createElement("summary");
  trigger.textContent = "i";
  trigger.setAttribute("aria-label", "Histogram help");
  trigger.title = "Histogram help";
  help.appendChild(trigger);

  const body = document.createElement("div");
  body.className = "histogram-help-body";
  body.textContent = comparison
    ? "Each span belongs to either Filter or Not(filter). Bars share a height scale, with the shorter bar in front. Dashed lines mark each group's percentiles. Drag across the chart to select a duration range."
    : "Dashed lines mark P50, P90, P99, and P99.9. Drag across the chart to select a duration range.";
  help.appendChild(body);
  legend.appendChild(help);
}

function seriesColumns(
  axisBars: readonly SpanHistogramBar[],
  series: DrawSeries,
  maxCount: number,
) {
  const axisCols = spanHistogramLayout(axisBars, W, 2).cols;
  return axisCols.map((axis, index) => {
    const bar = series.bars[index] ?? { ...axis, count: 0 };
    const width = axis.w * series.widthScale;
    return {
      ...bar,
      x: axis.x + (axis.w - width) / 2,
      w: width,
      hFrac: maxCount > 0 ? bar.count / maxCount : 0,
    };
  });
}

function appendHistogramSvg(
  container: HTMLElement,
  axisBars: readonly SpanHistogramBar[],
  series: readonly DrawSeries[],
  band: DurationBand,
  emptyMessage: string | null,
  onBand: (band: DurationBand) => void,
): void {
  const maxCount = series.reduce(
    (max, current) =>
      current.bars.reduce((m, bar) => Math.max(m, bar.count), max),
    0,
  );
  const banded = band.min_ns != null || band.max_ns != null;

  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("width", "100%");
  svg.setAttribute("viewBox", `0 0 ${W} ${HIST_H}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.style.cssText =
    "display:block;cursor:crosshair;overflow:visible;max-width:800px";

  const columnsBySeries = series.map((current) => ({
    current,
    columns: seriesColumns(axisBars, current, maxCount),
  }));
  for (let bucketIndex = 0; bucketIndex < axisBars.length; bucketIndex++) {
    const drawOrder = histogramLayerOrder(
      columnsBySeries.map(({ current, columns }) => ({
        count: columns[bucketIndex]?.count ?? 0,
        widthScale: current.widthScale,
      })),
    );
    for (let layerIndex = 0; layerIndex < drawOrder.length; layerIndex++) {
      const seriesIndex = drawOrder[layerIndex]!;
      const { current, columns } = columnsBySeries[seriesIndex]!;
      const c = columns[bucketIndex]!;
      const inBand =
        (band.min_ns == null || c.hi_ns > band.min_ns) &&
        (band.max_ns == null || c.lo_ns < band.max_ns);
      const h = Math.max(1, c.hFrac * (HIST_H - 2));
      const rect = document.createElementNS(SVG_NS, "rect");
      rect.setAttribute("x", String(c.x));
      rect.setAttribute("y", String(HIST_H - h));
      rect.setAttribute("width", String(c.w));
      rect.setAttribute("height", String(h));
      rect.setAttribute(
        "fill",
        banded && !inBand ? current.inactiveColor : current.color,
      );
      const isForeground = layerIndex === drawOrder.length - 1;
      rect.setAttribute(
        "fill-opacity",
        series.length > 1
          ? isForeground
            ? "0.9"
            : "0.55"
          : current.widthScale < 1
            ? "0.88"
            : "0.62",
      );
      rect.appendChild(
        titleEl(
          `${current.label}: ${c.count.toLocaleString()} instances · ${fmtNs(c.lo_ns)}–${fmtNs(c.hi_ns)}`,
        ),
      );
      svg.appendChild(rect);
    }
  }

  for (const current of series) {
    for (const { p, label } of PERCENTILES) {
      const ns = durationAtPercentile(current.bars, p);
      if (ns == null) continue;
      const x = spanNsToPx(axisBars, W, ns);
      if (x == null) continue;
      const tip = `${current.label} ${label}: ${fmtNs(ns)}`;

      const line = document.createElementNS(SVG_NS, "line");
      line.setAttribute("x1", String(x));
      line.setAttribute("x2", String(x));
      line.setAttribute("y1", "0");
      line.setAttribute("y2", String(HIST_H));
      line.setAttribute("class", "pctile-line");
      line.setAttribute("stroke", current.color);
      line.setAttribute("stroke-dasharray", current.percentileDash);
      svg.appendChild(line);

      const hitTarget = document.createElementNS(SVG_NS, "line");
      hitTarget.setAttribute("x1", String(x));
      hitTarget.setAttribute("x2", String(x));
      hitTarget.setAttribute("y1", "0");
      hitTarget.setAttribute("y2", String(HIST_H));
      hitTarget.setAttribute("class", "pctile-hit-target");
      hitTarget.appendChild(titleEl(tip));
      svg.appendChild(hitTarget);

      // Eight labels (four per population) become unreadable when tail
      // percentiles coincide. Keep every colored guide + tooltip, but label
      // only the two anchors needed to compare an overlaid distribution.
      if (series.length > 1 && p !== 50 && p !== 99) continue;

      const nearRight = x > W - 40;
      const text = document.createElementNS(SVG_NS, "text");
      text.setAttribute("x", String(nearRight ? x - 3 : x + 3));
      text.setAttribute("y", String(current.percentileY));
      text.setAttribute("text-anchor", nearRight ? "end" : "start");
      text.setAttribute("class", "pctile-label");
      text.setAttribute("fill", current.color);
      text.textContent = label;
      text.appendChild(titleEl(tip));
      svg.appendChild(text);
    }
  }

  if (emptyMessage) {
    const text = document.createElementNS(SVG_NS, "text");
    text.setAttribute("x", String(W / 2));
    text.setAttribute("y", String(HIST_H / 2 + 3));
    text.setAttribute("text-anchor", "middle");
    text.setAttribute("class", "histogram-empty-label");
    text.textContent = emptyMessage;
    svg.appendChild(text);
  }

  const brush = document.createElementNS(SVG_NS, "rect");
  brush.setAttribute("fill", "rgba(255,255,255,0.12)");
  brush.setAttribute("stroke", "#fff");
  brush.setAttribute("stroke-dasharray", "3 2");
  brush.setAttribute("y", "0");
  brush.setAttribute("height", String(HIST_H));
  brush.style.display = "none";
  svg.appendChild(brush);

  let dragStart: number | null = null;
  const localX = (clientX: number): number => {
    const r = svg.getBoundingClientRect();
    return ((clientX - r.left) / r.width) * W;
  };
  const endDrag = (): void => {
    dragStart = null;
    brush.style.display = "none";
  };

  svg.addEventListener("pointerdown", (e: PointerEvent) => {
    dragStart = localX(e.clientX);
    brush.setAttribute("x", String(dragStart));
    brush.setAttribute("width", "0");
    brush.style.display = "block";
    svg.setPointerCapture(e.pointerId);
    e.preventDefault();
  });

  svg.addEventListener("pointermove", (e: PointerEvent) => {
    if (dragStart == null) return;
    const x = localX(e.clientX);
    brush.setAttribute("x", String(Math.min(dragStart, x)));
    brush.setAttribute("width", String(Math.abs(x - dragStart)));
  });

  svg.addEventListener("pointerup", (e: PointerEvent) => {
    if (dragStart == null) return;
    const from = dragStart;
    endDrag();
    svg.releasePointerCapture(e.pointerId);
    const next = spanBrushToBand(axisBars, W, from, localX(e.clientX));
    if (next) onBand(next);
  });
  svg.addEventListener("lostpointercapture", endDrag);

  container.appendChild(svg);
}

/**
 * Draw one histogram normally, or an overlaid `filter` / `Not(filter)` split.
 *
 * `onBand` fires once per completed drag; near-zero drags are treated as clicks
 * and produce nothing.
 */
export function renderHistogram(
  container: HTMLElement,
  histogram: readonly HistogramBarLike[] | undefined,
  band: DurationBand,
  onBand: (band: DurationBand) => void,
  comparison: HistogramComparison | null = null,
): void {
  // The container is a STATIC node in the panel's lit-html template, so
  // lit reuses it across renders and never clears it for us. Without this the
  // SVG, axis and help control stack up one full set per snapshot.
  const helpOpen =
    container.querySelector<HTMLDetailsElement>(".histogram-help")?.open ??
    false;
  container.replaceChildren();
  const bars = normalizeSpanHistogram(histogram);
  if (bars.length === 0) {
    const empty = document.createElement("div");
    empty.style.cssText = "color:#666;font-size:0.8em;padding:4px 0";
    empty.textContent = "No histogram data.";
    container.appendChild(empty);
    return;
  }

  const baselineCount = bars.reduce((sum, bar) => sum + bar.count, 0);
  if (!comparison) {
    appendLegend(
      container,
      [
        {
          label: "All spans",
          color: "#6c63ff",
          detail: `${baselineCount.toLocaleString()} instances`,
        },
      ],
      null,
    );
    appendHistogramSvg(
      container,
      bars,
      [
        {
          label: "All spans",
          bars,
          color: "#6c63ff",
          inactiveColor: "#3a3a5a",
          widthScale: 1,
          percentileY: 9,
          percentileDash: "2 2",
        },
      ],
      band,
      null,
      onBand,
    );
  } else if (!comparison.comparable) {
    const status =
      comparison.error ??
      (comparison.pending
        ? "loading matching coverage…"
        : "comparison coverage is incomplete");
    appendLegend(
      container,
      [
        {
          label: `Filter: ${comparison.label}`,
          color: "#2fb8ac",
          detail: "waiting…",
        },
        {
          label: `Not(${comparison.label})`,
          color: "#6c63ff",
          detail: "waiting…",
        },
      ],
      comparison.coverageLabel,
    );
    appendHistogramSvg(container, bars, [], band, status, onBand);
  } else {
    const filtered = alignHistogramToBaseline(bars, comparison.histogram);
    const complement = complementHistogram(bars, filtered);
    const filteredCount = Math.min(
      baselineCount,
      comparison.count ?? filtered.reduce((sum, bar) => sum + bar.count, 0),
    );
    const complementCount = Math.max(0, baselineCount - filteredCount);
    const detail = (count: number): string => {
      const pct = baselineCount > 0 ? (count / baselineCount) * 100 : 0;
      return `${count.toLocaleString()} · ${pct.toFixed(1)}%`;
    };
    const filteredLabel = `Filter: ${comparison.label}`;
    const complementLabel = `Not(${comparison.label})`;
    appendLegend(
      container,
      [
        {
          label: filteredLabel,
          color: "#2fb8ac",
          detail: detail(filteredCount),
        },
        {
          label: complementLabel,
          color: "#6c63ff",
          detail: detail(complementCount),
        },
      ],
      [comparison.coverageLabel, comparison.pending ? "updating…" : null]
        .filter((value) => value != null)
        .join(" · ") || null,
    );
    appendHistogramSvg(
      container,
      bars,
      [
        {
          label: complementLabel,
          bars: complement,
          color: "#6c63ff",
          inactiveColor: "#3a3a5a",
          widthScale: 1,
          percentileY: 9,
          percentileDash: "1 3",
        },
        {
          label: filteredLabel,
          bars: filtered,
          color: "#2fb8ac",
          inactiveColor: "#24454a",
          widthScale: 0.62,
          percentileY: 18,
          percentileDash: "3 2",
        },
      ],
      band,
      null,
      onBand,
    );
  }

  appendHistogramHelp(container, comparison !== null, helpOpen);

  const axis = document.createElement("div");
  axis.className = "histogram-axis";
  axis.style.maxWidth = "800px";
  const { cols } = spanHistogramLayout(bars, W, 2);
  const stride = Math.max(
    1,
    Math.ceil(cols.length / Math.max(1, Math.floor(W / 70))),
  );
  cols.forEach((c, i) => {
    if (i % stride !== 0) return;
    const t = document.createElement("span");
    t.textContent = formatHumanDuration(c.lo_ns);
    t.style.cssText = `position:absolute;left:${(c.x / W) * 100}%;white-space:nowrap;transform:translateX(-1px)`;
    axis.appendChild(t);
  });
  container.appendChild(axis);
}
