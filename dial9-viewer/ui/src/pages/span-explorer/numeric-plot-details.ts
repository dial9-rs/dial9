import { html, nothing, render } from "lit-html";
import { ref } from "lit-html/directives/ref.js";
import type { TailRecord, TailSignal, TailValueAxis } from "../../lib/trace/span-tail-analysis.js";
import { duration, fieldValue, number } from "./numeric-presentation.js";

interface NumericPlotDetailsModel {
  signal: Pick<TailSignal, "key" | "points" | "density">;
  axis: TailValueAxis;
  cellBounds: ((index: number) => { x: number; y: number; width: number; height: number }) | null;
  onJump: (record: TailRecord) => void;
}

// One active surface per document; owners release it on hide or disposal.
const activeInspectors = new WeakMap<Document, { pinned: () => boolean; close: () => void }>();

/** Own details for one plotted report. Marks identify their point/cell indices. */
export function numericPlotDetails(model: NumericPlotDetailsModel) {
  let dispose = () => {};
  let generation = 0;
  return ref(element => {
    dispose();
    dispose = () => {};
    const current = ++generation;
    if (!element) return;
    (element as HTMLElement).hidden = true;
    // Lit calls refs before insertion; clipping ancestors exist after the commit.
    queueMicrotask(() => {
      if (current === generation && element.isConnected) {
        // Imperative details belong to this plotted report and its handlers.
        dispose = mountDetails(element as HTMLElement, model);
      }
    });
  });
}

/** Render original strings without interpreting values or collapsing duplicate keys. */
function recordedFields(record: TailRecord) {
  if (record.attributes == null) return html`<small>Recorded fields unavailable</small>`;
  const omitted = record.attributesOmitted;
  return html`<div class="tail-recorded-fields-heading">Recorded fields · original values</div>
    ${record.attributes.length ? html`<div class="tail-recorded-fields" tabindex="0" role="region" aria-label="Recorded fields">
      <dl>${record.attributes.map(([key, value]) => html`<dt>${key}</dt><dd>${value}</dd>`)}</dl>
    </div>` : html`<small>${omitted === 0 ? "No recorded fields"
      : omitted != null && omitted > 0 ? "All recorded fields omitted by response limits" : "No recorded fields included"}</small>`}
    ${omitted == null ? html`<small>Additional field count unavailable</small>`
      : omitted > 0 ? html`<small>${omitted} ${record.attributes.length ? "additional " : ""}recorded ${omitted === 1 ? "field" : "fields"} omitted · inspect the trace for more</small>` : nothing}`;
}

/** Keep narrow bin endpoints distinct when the normal display rounds them alike. */
function interval(edges: readonly number[], bin: number, format: (value: number) => string, unit: string | null) {
  const lower = edges[bin]!, upper = edges[bin + 1]!;
  let start = format(lower), end = format(upper);
  const raw = (value: number) => `${value}${unit ? ` ${unit}` : " (unit not recorded)"}`;
  if (lower !== upper && start === end) { start = raw(lower); end = raw(upper); }
  const closing = bin === edges.length - 2 ? "]" : ")";
  return html`<span title=${`[${raw(lower)}, ${raw(upper)}${closing}`}>[${start}, ${end}${closing}</span>`;
}

function readGeometry(elements: readonly Element[]): string {
  const view = elements[0]?.ownerDocument.defaultView;
  const viewport = view?.visualViewport;
  return elements.map(element => {
    const { left, top, width, height } = element.getBoundingClientRect();
    return `${left},${top},${width},${height}`;
  }).join(";") + `;${view?.innerWidth},${view?.innerHeight},${view?.scrollX},${view?.scrollY}`
    + `;${viewport?.width},${viewport?.height},${viewport?.offsetLeft},${viewport?.offsetTop},${viewport?.scale}`;
}

/** Dismiss placements when their viewport or anchor geometry becomes stale. */
function observeGeometry(elements: readonly Element[], onChange: (geometry: string) => void): () => void {
  const view = elements[0]?.ownerDocument.defaultView;
  if (!view) return () => {};
  let previous = readGeometry(elements);
  let active = true;
  const viewportChanged = () => { if (active) { previous = readGeometry(elements); onChange(previous); } };
  const observer = typeof view.ResizeObserver === "function" ? new view.ResizeObserver(() => {
    if (!active) return;
    const next = readGeometry(elements);
    // ResizeObserver also sends an initial notification, before any change.
    if (next === previous) return;
    previous = next;
    onChange(next);
  }) : null;
  elements.forEach(element => observer?.observe(element));
  view.addEventListener("resize", viewportChanged);
  view.visualViewport?.addEventListener("resize", viewportChanged);
  view.visualViewport?.addEventListener("scroll", viewportChanged);
  return () => {
    active = false;
    observer?.disconnect();
    view.removeEventListener("resize", viewportChanged);
    view.visualViewport?.removeEventListener("resize", viewportChanged);
    view.visualViewport?.removeEventListener("scroll", viewportChanged);
  };
}

function mountDetails(tooltip: HTMLElement, model: NumericPlotDetailsModel): () => void {
  const plot = tooltip.parentElement;
  const svg = plot?.querySelector<SVGSVGElement>(".tail-scatter");
  const doc = tooltip.ownerDocument;
  const view = doc.defaultView;
  if (!plot || !svg || !view) return () => {};
  const anchor = plot.closest(".tail-field, .tail-plot-dialog") ?? plot;
  const dialog = plot.closest<HTMLDialogElement>(".tail-plot-dialog");
  const targets = [plot, svg, anchor, plot.closest(".detail-panel")]
    .filter((target): target is Element => target != null);
  const { signal, axis, onJump } = model;
  const density = signal.density;
  const xBins = density ? density.elapsedEdgesMs.length - 1 : 0;
  const yBins = density ? density.valueEdges.length - 1 : 0;
  const points = () => svg.querySelectorAll<SVGCircleElement>(".tail-trace-point");
  // Lit may reuse circles whose tabindex was changed by the previous report.
  const entry = Array.from(points()).find(point => point === doc.activeElement)
    ?? svg.querySelector<SVGCircleElement>(".tail-trace-point[tabindex='0']") ?? points()[0];
  points().forEach(point => {
    point.setAttribute("tabindex", point === entry ? "0" : "-1");
    point.setAttribute("aria-pressed", "false");
  });
  type Inspection = { index: number | null; cellIndex: number | null; origin: Element; keyboard: boolean; nearby: boolean; unavailable?: string };
  let current: Inspection | null = null;
  let placementGeometry = "";
  let pinned = false;
  let hideTimer: number | null = null;
  let focusFrame: number | null = null;
  let restoringFocus = false;
  let keyboardCell = density ? Math.max(0, density.counts.indexOf(density.maxCount)) : 0;
  const owner = { pinned: () => pinned, close: () => hide(tooltip.contains(doc.activeElement)) };
  const retain = () => { if (hideTimer != null) view.clearTimeout(hideTimer); hideTimer = null; };
  const cancelFocus = () => {
    if (focusFrame != null) view.cancelAnimationFrame(focusFrame);
    focusFrame = null;
  };
  const clearHighlights = () => {
    svg.querySelectorAll("[data-inspected]").forEach(mark => mark.removeAttribute("data-inspected"));
    points().forEach(point => point.setAttribute("aria-pressed", "false"));
  };
  const hide = (restoreFocus = false) => {
    retain();
    cancelFocus();
    const origin = current?.origin;
    current = null;
    pinned = false;
    tooltip.hidden = true;
    tooltip.dataset.pinned = "false";
    clearHighlights();
    if (activeInspectors.get(doc) === owner) {
      activeInspectors.delete(doc);
      view.removeEventListener("keydown", escape);
    }
    const trigger = origin?.matches(".tail-density-cell") ? svg : origin;
    if (restoreFocus && trigger?.isConnected) {
      restoringFocus = true;
      (trigger as HTMLElement).focus({ preventScroll: true });
      restoringFocus = false;
    }
  };
  const escape = (event: KeyboardEvent) => {
    if (event.key === "Escape" && activeInspectors.get(doc) === owner) {
      hide(true);
      event.preventDefault();
      event.stopPropagation();
    }
  };
  const leave = () => {
    if (pinned || hideTimer != null) return;
    hideTimer = view.setTimeout(() => {
      hideTimer = null;
      const focused = doc.activeElement;
      if (!tooltip.contains(focused) && focused !== current?.origin) hide();
    }, 180);
  };
  const place = () => {
    const anchorRect = anchor.getBoundingClientRect();
    const above = Math.max(0, Math.min(view.innerHeight - 16, anchorRect.top - 16));
    const below = Math.max(0, Math.min(view.innerHeight - 16, view.innerHeight - anchorRect.bottom - 16));
    const useAbove = above >= 160 || above >= below;
    const width = Math.min(330, view.innerWidth - 16);
    const height = Math.min(280, useAbove ? above : below);
    /** Keep the field/dialog controls clear, independent of sample or content. */
    tooltip.style.width = `${width}px`;
    tooltip.style.height = `${height}px`;
    tooltip.style.left = `${Math.max(8, Math.min(anchorRect.left, view.innerWidth - 8 - width))}px`;
    tooltip.style.top = `${useAbove
      ? Math.max(8, Math.min(anchorRect.top - 8 - height, view.innerHeight - 8 - height))
      : Math.max(8, anchorRect.bottom + 8)}px`;
    placementGeometry = readGeometry(targets);
  };
  const show = (inspection: Inspection, pin = false) => {
    const active = activeInspectors.get(doc);
    if (active !== owner && active?.pinned() && !pin) return;
    if (active !== owner) {
      active?.close();
      activeInspectors.set(doc, owner);
      view.addEventListener("keydown", escape);
    }
    retain();
    cancelFocus();
    const changed = current?.index !== inspection.index || current?.cellIndex !== inspection.cellIndex || current?.keyboard !== inspection.keyboard
      || current?.nearby !== inspection.nearby || current?.unavailable !== inspection.unavailable || pinned !== pin;
    current = inspection;
    pinned = pin && inspection.index != null;
    tooltip.dataset.pinned = String(pinned);
    if (changed) {
      const point = inspection.index == null ? null : signal.points[inspection.index];
      const cell = inspection.cellIndex;
      render(html`<div class="tail-inspector-heading"><strong>Span details</strong>
          <span>${pinned ? "Pinned" : point ? "Click to pin" : "Unavailable"}</span></div>
        ${density && cell != null ? html`<div class="tail-density-context">
          <strong>Density box · ${number(density.counts[cell]!)} spans</strong>
          <span>Box X · Elapsed: ${interval(density.elapsedEdgesMs, cell % xBins, duration, "ms")}</span>
          <span>Box Y · ${signal.key}: ${interval(density.valueEdges, Math.floor(cell / xBins),
            value => fieldValue(value, axis.unit), axis.unit)}</span>
        </div>` : html`<small class="tail-density-context">${density
          ? "Sample outside returned density bounds" : "Density box unavailable in this cached report"}</small>`}
        ${point ? html`<div class="tail-sample-coordinates">
          <strong>Sample X · ${duration(point[0])} elapsed</strong>
          <span>Sample Y · ${signal.key}: ${fieldValue(point[1] * axis.multiplier, axis.unit)}</span>
          </div>
          ${inspection.nearby ? html`<small>Nearby retained span · no trace sample in this cell</small>` : nothing}
          <div class="tail-inspector-actions">
            <button type="button" class="tail-tooltip-trace" @click=${() => {
              if (current?.index === inspection.index) onJump(point[2]);
            }}>Open trace focused on this span ↗</button>
            <button type="button" class="tail-inspector-pin" aria-label=${pinned ? "Unpin span details" : "Pin span details"}
              aria-pressed=${String(pinned)} @click=${() => {
                if (pinned) hide(true);
                else if (current) show(current, true);
              }}>${pinned ? "Unpin" : "Pin"}</button>
          </div>
          ${recordedFields(point[2])}
          ${inspection.keyboard ? html`<small>Arrows choose samples · Enter pins · Tab inspects fields · Escape clears</small>` : nothing}`
          : html`<small>${inspection.unavailable}</small>`}`, tooltip);
    }
    clearHighlights();
    if (inspection.index != null) {
      const point = points()[inspection.index]!;
      point.dataset.inspected = "true";
      point.setAttribute("aria-pressed", String(pinned));
      const ring = svg.querySelector<SVGCircleElement>(".tail-sample-highlight")!;
      ring.setAttribute("cx", point.getAttribute("cx")!);
      ring.setAttribute("cy", point.getAttribute("cy")!);
      ring.setAttribute("r", String(Number(point.getAttribute("r")) + 2));
      ring.dataset.pointIndex = String(inspection.index);
      ring.dataset.inspected = "true";
    }
    if (inspection.cellIndex != null && model.cellBounds) {
      const outline = svg.querySelector<SVGRectElement>(".tail-density-highlight")!;
      Object.entries(model.cellBounds(inspection.cellIndex)).forEach(([key, value]) => outline.setAttribute(key, String(value)));
      outline.dataset.cellIndex = String(inspection.cellIndex);
      outline.dataset.inspected = "true";
    }
    tooltip.setAttribute("role", "dialog");
    tooltip.setAttribute("aria-label", "Span details");
    tooltip.setAttribute("aria-live", inspection.keyboard ? "polite" : "off");
    tooltip.hidden = false;
    place();
  };
  const cellForPoint = (index: number): number | null => {
    if (!density) return null;
    const bin = (value: number, edges: readonly number[]) => edges.slice(0, -1).findIndex((lower, index) =>
      value >= lower && (value < edges[index + 1]! || index === edges.length - 2 && value <= edges[index + 1]!));
    const [elapsed, raw] = signal.points[index]!;
    const x = bin(elapsed, density.elapsedEdgesMs), y = bin(raw * axis.multiplier, density.valueEdges);
    return x < 0 || y < 0 ? null : y * xBins + x;
  };
  const showPoint = (element: SVGCircleElement, keyboard: boolean, pin = false) => {
    const index = Number(element.dataset.pointIndex);
    if (signal.points[index]) show({ index, cellIndex: cellForPoint(index), origin: element, keyboard, nearby: false }, pin);
  };
  const showCell = (origin: Element, index: number, keyboard: boolean, pin = false, cursor?: { x: number; y: number }) => {
    if (!density) return;
    if (!density.counts[index] || !signal.points.length) {
      show({ index: null, cellIndex: index, origin, keyboard, nearby: false, unavailable: density.counts[index]
        ? "No retained trace sample is available for this plot."
        : "No recorded spans in this cell." });
      return;
    }
    const candidates = signal.points.flatMap((_, pointIndex) => cellForPoint(pointIndex) === index ? [pointIndex] : []);
    const nearby = !candidates.length;
    const cell = svg.querySelector<SVGRectElement>(`.tail-density-cell[data-cell-index='${index}']`)!.getBoundingClientRect();
    const target = cursor ?? { x: cell.left + cell.width / 2, y: cell.top + cell.height / 2 };
    const marks = points();
    let nearest = 0, distance = Infinity;
    for (const pointIndex of nearby ? signal.points.map((_, i) => i) : candidates) {
      const mark = marks[pointIndex]!.getBoundingClientRect();
      const next = (mark.left + mark.width / 2 - target.x) ** 2 + (mark.top + mark.height / 2 - target.y) ** 2;
      if (next < distance) { nearest = pointIndex; distance = next; }
    }
    show({ index: nearest, cellIndex: index, origin, keyboard, nearby }, pin);
  };
  const pointTarget = (event: Event) => event.target instanceof view.SVGCircleElement
    && event.target.matches(".tail-trace-point") ? event.target : null;
  const cellTarget = (event: Event) => event.target instanceof view.Element
    && event.target.matches(".tail-density-cell") ? event.target : null;
  const preview = (event: Event) => {
    if (pinned) return;
    const point = pointTarget(event), cell = cellTarget(event);
    if (point) showPoint(point, false);
    else if (cell) {
      const pointer = event as PointerEvent;
      showCell(cell, Number(cell.getAttribute("data-cell-index")), false, false, { x: pointer.clientX, y: pointer.clientY });
    } else if (event.target === tooltip || tooltip.contains(event.target as Node)) retain();
    else if (event.type === "pointermove" && svg.contains(event.target as Node)) leave();
  };
  const pointerLeave = (event: Event) => {
    if (event.target === svg || event.target === tooltip) leave();
  };
  const focus = (event: Event) => {
    const point = pointTarget(event);
    if (point) points().forEach(other => other.setAttribute("tabindex", other === point ? "0" : "-1"));
    if (restoringFocus || !point && event.target !== svg) return;
    if (pinned) {
      if (point) showPoint(point, true, true);
      return;
    }
    hide();
    const element = point ?? svg;
    // Native focus may scroll a clipped mark; place details after that scroll.
    focusFrame = view.requestAnimationFrame(() => {
      focusFrame = null;
      if (!element.isConnected || doc.activeElement !== element) return;
      if (point) showPoint(point, true);
      else showCell(svg, keyboardCell, true);
    });
  };
  const blur = (event: Event) => {
    cancelFocus();
    if (!pinned && (pointTarget(event) || event.target === svg || tooltip.contains(event.target as Node))
      && !tooltip.contains((event as FocusEvent).relatedTarget as Node | null)) hide();
  };
  const click = (event: Event) => {
    const point = pointTarget(event), cell = cellTarget(event);
    if (point) showPoint(point, false, true);
    else if (cell) showCell(cell, Number(cell.getAttribute("data-cell-index")), false, true,
      { x: (event as MouseEvent).clientX, y: (event as MouseEvent).clientY });
  };
  const keydown = (event: Event) => {
    const key = (event as KeyboardEvent).key;
    if (key === "Escape" && !tooltip.hidden) {
      hide(tooltip.contains(event.target as Node));
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (tooltip.contains(event.target as Node)) return;
    const point = pointTarget(event);
    if (point) {
      const index = Number(point.dataset.pointIndex);
      if (key === "Escape") { hide(); return; }
      if (key === "Enter" || key === " ") showPoint(point, true, true);
      else {
        // One Tab entry per plot; arrow keys move through the bounded trace sample.
        const next = key === "Home" ? 0 : key === "End" ? signal.points.length - 1
          : key === "ArrowLeft" ? Math.max(0, index - 1)
          : key === "ArrowRight" ? Math.min(signal.points.length - 1, index + 1) : null;
        if (next == null) return;
        points()[next]?.focus({ preventScroll: true });
      }
      event.preventDefault();
    } else if (event.target === svg && density) {
      if (key === "Escape") { hide(); return; }
      if (key === "Enter" || key === " ") showCell(svg, keyboardCell, true, true);
      else {
        let x = keyboardCell % xBins, y = Math.floor(keyboardCell / xBins);
        if (key === "ArrowLeft") x = Math.max(0, x - 1);
        else if (key === "ArrowRight") x = Math.min(xBins - 1, x + 1);
        else if (key === "ArrowUp") y = Math.min(yBins - 1, y + 1);
        else if (key === "ArrowDown") y = Math.max(0, y - 1);
        else return;
        keyboardCell = y * xBins + x;
        showCell(svg, keyboardCell, true, pinned);
      }
      event.preventDefault();
    }
  };
  const listeners = { pointerenter: preview, pointermove: preview, pointerleave: pointerLeave, focus, blur, click, keydown };
  Object.entries(listeners).forEach(([type, listener]) => plot.addEventListener(type, listener, true));
  const geometryChanged = (geometry = readGeometry(targets)) => {
    // Native scrolling may notify after pointer entry already placed this preview.
    if (!tooltip.hidden && geometry !== placementGeometry) {
      if (pinned) place();
      else hide(tooltip.contains(doc.activeElement));
    }
  };
  const scroll = (event: Event) => {
    if (!(event.target instanceof view.Node && tooltip.contains(event.target))) geometryChanged();
  };
  const closeDialog = () => hide();
  dialog?.addEventListener("close", closeDialog);
  const disposeGeometry = observeGeometry(targets, geometryChanged);
  view.addEventListener("scroll", scroll, true);
  return () => {
    disposeGeometry();
    dialog?.removeEventListener("close", closeDialog);
    view.removeEventListener("scroll", scroll, true);
    Object.entries(listeners).forEach(([type, listener]) => plot.removeEventListener(type, listener, true));
    hide(tooltip.contains(doc.activeElement));
    render(nothing, tooltip);
  };
}
