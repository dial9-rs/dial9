// Resizable service/host label column for the Browse heatmap. The CSS
// variable remains the single source of truth for the label width.

import type { PageCtx } from "./ctx.js";

export const MIN_LABEL_WIDTH = 220;
export const MAX_LABEL_WIDTH = 640;
const MIN_PLOT_WIDTH = 200;
const LABEL_HORIZONTAL_PADDING = 16;

export function clampLabelWidth(width: number, availableWidth: number): number {
  const maximum = Math.max(
    MIN_LABEL_WIDTH,
    Math.min(MAX_LABEL_WIDTH, availableWidth - MIN_PLOT_WIDTH),
  );
  return Math.min(maximum, Math.max(MIN_LABEL_WIDTH, width));
}

export function contentFitLabelWidth(
  labelTextWidths: readonly number[],
  availableWidth: number,
): number {
  const widest = Math.max(0, ...labelTextWidths);
  return clampLabelWidth(widest + LABEL_HORIZONTAL_PADDING, availableWidth);
}

export function mountHeatmapLabelResize({ els }: PageCtx): void {
  let startX = 0;
  let startWidth = 0;
  let resizing = false;

  function repaint(): void {
    // The heatmap interaction owns the debounced resize repaint path.
    window.dispatchEvent(new Event("resize"));
  }

  els.heatmapLabelResizer.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    resizing = true;
    startX = e.clientX;
    startWidth = els.heatmapLabels.getBoundingClientRect().width;
    e.preventDefault();
  });

  window.addEventListener("mousemove", (e) => {
    if (!resizing) return;
    const width = clampLabelWidth(
      startWidth + e.clientX - startX,
      els.heatmapBody.clientWidth,
    );
    document.documentElement.style.setProperty("--heatmap-label-w", `${width}px`);
    repaint();
  });

  window.addEventListener("mouseup", () => {
    if (!resizing) return;
    resizing = false;
    repaint();
  });
}
