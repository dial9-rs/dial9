// Viewport resolution + fetch-window quantization, mirroring Perfetto's model
// (ui/src/base/resolution.ts). A track's resolution is nanoseconds-per-pixel
// floored to a power of two, so small pans/zooms produce the SAME resolution
// value -> the mipmap query at that resolution is a cache hit. The fetch window
// is over-fetched (a "skirt") and its edges snapped to the resolution grid so
// the cache key (start, end, resolution) is stable across sub-pixel movement.

/** Largest power of two <= x (x >= 1). Returns 1 for x < 2. */
export function bitFloor(x: number): number {
  if (x < 2) return 1;
  return 2 ** Math.floor(Math.log2(x));
}

/**
 * Resolution (ns/pixel, power-of-two-floored) for a `durationNs` window drawn
 * across `widthPx` pixels. Minimum 1 (one ns per pixel when zoomed in fully).
 */
export function resolutionFor(durationNs: number, widthPx: number): number {
  if (widthPx <= 0) return 1;
  return bitFloor(Math.max(1, Math.floor(durationNs / widthPx)));
}

/** Round n DOWN to a multiple of q (q a power of two, but works for any q>0). */
export function quantFloor(n: number, q: number): number {
  return Math.floor(n / q) * q;
}
/** Round n UP to a multiple of q. */
export function quantCeil(n: number, q: number): number {
  return Math.ceil(n / q) * q;
}

/** A stable per-track fetch window: the visible span padded by a full screen on
 * each side (3x total, the "skirt") and snapped to the resolution grid, so
 * panning within +/-1 screen never changes the key. */
export interface FetchWindow {
  start: number;
  end: number;
  resolution: number;
}

export function fetchWindowFor(
  viewStart: number,
  viewEnd: number,
  widthPx: number
): FetchWindow {
  const dur = viewEnd - viewStart;
  const resolution = resolutionFor(dur, widthPx);
  return {
    start: quantFloor(viewStart - dur, resolution),
    end: quantCeil(viewEnd + dur, resolution),
    resolution,
  };
}

/** True when `w` still covers `[viewStart,viewEnd]` at the same resolution, so
 * the cached derived data can be reused without a refetch. */
export function fetchWindowCovers(
  w: FetchWindow,
  viewStart: number,
  viewEnd: number,
  widthPx: number
): boolean {
  return (
    viewStart >= w.start &&
    viewEnd <= w.end &&
    resolutionFor(viewEnd - viewStart, widthPx) === w.resolution
  );
}
