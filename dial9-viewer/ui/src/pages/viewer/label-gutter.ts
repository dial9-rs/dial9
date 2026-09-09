/** Content-fit default for the shared time-track label gutter (CSS px). */
export const DEFAULT_LABEL_WIDTH = 180;
/** Smallest usable gutter: track controls and a metadata key still fit. */
export const MIN_LABEL_WIDTH = 120;
/** Do not let labels consume more than this share of the viewport. */
export const MAX_LABEL_WIDTH_VW = 0.45;

/** Clamp a gutter width to its accessible, viewport-relative bounds. */
export function clampLabelWidth(
  width: number,
  viewportWidth = typeof window === "undefined" ? Infinity : window.innerWidth,
): number {
  const max = Math.max(MIN_LABEL_WIDTH, Math.floor(viewportWidth * MAX_LABEL_WIDTH_VW));
  return Math.round(Math.max(MIN_LABEL_WIDTH, Math.min(max, width)));
}
