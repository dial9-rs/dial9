// lib/canvas/dpr.ts - DPR-aware canvas backing-store sizing (T08;
// docs/ui-inventory/03-performance-findings.md F3, architecture 2.3).
//
// The measured problem (F3): the legacy renderers reassign canvas.width/
// canvas.height on EVERY render invocation (renderLane viewer.html:2963,
// renderCrosshair :4779), which reallocates the backing store - and
// clears the canvas - even when the size is unchanged. The design rule:
// canvas backing stores resize only on geometry change.
//
// Split so the decision is Node-testable without a DOM:
//
// - planBackingStore(prev, next): PURE - given the last-applied geometry
//   and the requested one, decide whether a resize is needed and what the
//   device-pixel size is.
// - createCanvasSizer(canvas): the thin stateful binding that applies the
//   plan to a real canvas. Typed against a minimal structural surface
//   (DprCanvas) so tests drive it with a recording stub;
//   HTMLCanvasElement satisfies it.

/** The inputs that determine a canvas backing store's size. */
export interface CanvasGeometry {
  /** CSS width in px (the layout size). */
  cssWidth: number;
  /** CSS height in px. */
  cssHeight: number;
  /** Device pixel ratio the backing store is scaled by. */
  dpr: number;
}

export interface BackingStorePlan {
  /** True when the backing store must be (re)allocated. */
  changed: boolean;
  /** Backing-store width in device px: round(cssWidth * dpr). */
  deviceWidth: number;
  /** Backing-store height in device px: round(cssHeight * dpr). */
  deviceHeight: number;
}

/**
 * Decide whether a canvas backing store needs resizing. Pure - no DOM.
 *
 * `changed` is an exact comparison against the last-APPLIED geometry
 * (never a read-back of canvas.width, whose integer truncation would
 * make fractional sizes always look "changed"). Device sizes round to
 * the nearest device pixel; the legacy code let the canvas setter
 * truncate instead, which could undersize the store by up to 1px.
 */
export function planBackingStore(
  prev: CanvasGeometry | null,
  next: CanvasGeometry,
): BackingStorePlan {
  const changed =
    prev === null ||
    prev.cssWidth !== next.cssWidth ||
    prev.cssHeight !== next.cssHeight ||
    prev.dpr !== next.dpr;
  return {
    changed,
    deviceWidth: Math.round(next.cssWidth * next.dpr),
    deviceHeight: Math.round(next.cssHeight * next.dpr),
  };
}

/** The one context capability the sizer needs. */
export interface DprTransformContext {
  setTransform(
    a: number,
    b: number,
    c: number,
    d: number,
    e: number,
    f: number,
  ): void;
}

/**
 * Minimal structural surface of HTMLCanvasElement used by the sizer, so
 * the binding is testable with a plain recording object in Node.
 */
export interface DprCanvas<Ctx extends DprTransformContext> {
  width: number;
  height: number;
  style: { width: string; height: string };
  getContext(contextId: "2d"): Ctx | null;
}

export interface CanvasSizer<Ctx extends DprTransformContext> {
  /**
   * Make the canvas backing store match (cssWidth, cssHeight, dpr) and
   * return its 2d context with the DPR transform applied.
   *
   * - Reallocates the backing store ONLY when the geometry differs from
   *   the last ensure() (F3). The unchanged path performs zero canvas
   *   writes besides setTransform.
   * - Because the unchanged path does not touch canvas.width, it does
   *   NOT clear the canvas; renderers must paint their full area (every
   *   panel starts with a background fill, so this holds today).
   * - The transform is reset with setTransform (not scale()) so repeated
   *   calls never compound the scale.
   *
   * `dpr` defaults to the live devicePixelRatio (1 where unavailable),
   * matching the legacy `devicePixelRatio || 1` call sites.
   */
  ensure(cssWidth: number, cssHeight: number, dpr?: number): Ctx;
  /** The last-applied geometry; null before the first ensure(). */
  geometry(): CanvasGeometry | null;
}

function defaultDpr(): number {
  return (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
}

/**
 * Bind a resize-only-on-geometry-change sizer to one canvas. One sizer
 * per canvas: the sizer tracks what it applied, not what the DOM holds.
 */
export function createCanvasSizer<Ctx extends DprTransformContext>(
  canvas: DprCanvas<Ctx>,
): CanvasSizer<Ctx> {
  let applied: CanvasGeometry | null = null;
  let ctx: Ctx | null = null;
  return {
    ensure(cssWidth, cssHeight, dpr = defaultDpr()) {
      const next: CanvasGeometry = { cssWidth, cssHeight, dpr };
      const plan = planBackingStore(applied, next);
      if (plan.changed) {
        canvas.width = plan.deviceWidth;
        canvas.height = plan.deviceHeight;
        canvas.style.width = `${cssWidth}px`;
        canvas.style.height = `${cssHeight}px`;
        applied = next;
      }
      if (ctx === null) {
        ctx = canvas.getContext("2d");
        if (ctx === null) {
          throw new Error("createCanvasSizer: canvas returned no 2d context");
        }
      }
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      return ctx;
    },
    geometry: () => applied,
  };
}
