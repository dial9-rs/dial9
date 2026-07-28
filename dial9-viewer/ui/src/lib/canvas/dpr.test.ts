// The DPR wrapper no-ops when geometry is unchanged (backing stores resize
// only on geometry change). The decision logic is pure (planBackingStore);
// the binding is driven with a recording stub that counts every canvas write.

import { describe, it, expect, vi, afterEach } from "vitest";
import { createCanvasSizer, planBackingStore } from "./dpr.js";
import type { CanvasGeometry, DprCanvas, DprTransformContext } from "./dpr.js";

const geom = (
  cssWidth: number,
  cssHeight: number,
  dpr: number,
): CanvasGeometry => ({ cssWidth, cssHeight, dpr });

// Compile-time proof that a real canvas satisfies the structural surface
// the sizer is typed against (so browser call sites need no casts).
type _RealCanvasIsDprCanvas =
  HTMLCanvasElement extends DprCanvas<CanvasRenderingContext2D>
    ? true
    : never;
const _realCanvasOk: _RealCanvasIsDprCanvas = true;
void _realCanvasOk;

describe("planBackingStore (pure)", () => {
  it("first plan (no previous geometry) always resizes", () => {
    const p = planBackingStore(null, geom(800, 60, 2));
    expect(p).toEqual({ changed: true, deviceWidth: 1600, deviceHeight: 120 });
  });

  it("DoD: identical geometry -> no resize", () => {
    const g = geom(800, 60, 2);
    const p = planBackingStore(g, geom(800, 60, 2));
    expect(p.changed).toBe(false);
    // Device sizes are still reported (callers may need them for clears).
    expect(p.deviceWidth).toBe(1600);
    expect(p.deviceHeight).toBe(120);
  });

  it("any single field change -> resize", () => {
    const g = geom(800, 60, 2);
    expect(planBackingStore(g, geom(801, 60, 2)).changed).toBe(true);
    expect(planBackingStore(g, geom(800, 61, 2)).changed).toBe(true);
    expect(planBackingStore(g, geom(800, 60, 1.5)).changed).toBe(true);
  });

  it("device size rounds to the nearest device pixel", () => {
    // 733 * 1.25 = 916.25 -> 916; 733 * 1.5 = 1099.5 -> 1100.
    expect(planBackingStore(null, geom(733, 30, 1.25)).deviceWidth).toBe(916);
    expect(planBackingStore(null, geom(733, 30, 1.5)).deviceWidth).toBe(1100);
  });

  it("fractional CSS change with identical device size is still a change", () => {
    // dpr 1: 800.2 and 800.4 both round to device 800, but the CSS box
    // differs.
    const p = planBackingStore(geom(800.2, 60, 1), geom(800.4, 60, 1));
    expect(p.deviceWidth).toBe(800);
    // style.width must still update (the CSS box changed), so this is
    // "changed" - the comparison is on inputs, not device pixels.
    expect(p.changed).toBe(true);
  });
});

// ── Recording stub canvas ─────────────────────────────────────────────────

interface StubCanvas extends DprCanvas<DprTransformContext> {
  /** Every write/call performed on the canvas or its context. */
  log: string[];
  /** Log entries that allocate/clear the backing store. */
  backingStoreWrites(): string[];
}

function makeStubCanvas(opts?: { nullContext?: boolean }): StubCanvas {
  const log: string[] = [];
  let w = 0;
  let h = 0;
  let styleW = "";
  let styleH = "";
  const ctx: DprTransformContext = {
    setTransform: (...args: number[]) => {
      log.push(`setTransform(${args.join(",")})`);
    },
  };
  return {
    log,
    backingStoreWrites: () =>
      log.filter((e) => e.startsWith("width=") || e.startsWith("height=")),
    get width() {
      return w;
    },
    set width(v: number) {
      w = v;
      log.push(`width=${v}`);
    },
    get height() {
      return h;
    },
    set height(v: number) {
      h = v;
      log.push(`height=${v}`);
    },
    style: {
      get width() {
        return styleW;
      },
      set width(v: string) {
        styleW = v;
        log.push(`style.width=${v}`);
      },
      get height() {
        return styleH;
      },
      set height(v: string) {
        styleH = v;
        log.push(`style.height=${v}`);
      },
    },
    getContext(id: "2d") {
      log.push(`getContext(${id})`);
      return opts?.nullContext === true ? null : ctx;
    },
  };
}

describe("createCanvasSizer (binding)", () => {
  it("first ensure() sizes the store, the CSS box, and the transform", () => {
    const canvas = makeStubCanvas();
    const sizer = createCanvasSizer(canvas);
    sizer.ensure(800, 60, 2);
    expect(canvas.log).toEqual([
      "width=1600",
      "height=120",
      "style.width=800px",
      "style.height=60px",
      "getContext(2d)",
      "setTransform(2,0,0,2,0,0)",
    ]);
    expect(sizer.geometry()).toEqual(geom(800, 60, 2));
  });

  it("DoD: unchanged geometry -> zero canvas writes (no realloc, no clear)", () => {
    const canvas = makeStubCanvas();
    const sizer = createCanvasSizer(canvas);
    sizer.ensure(800, 60, 2);
    canvas.log.length = 0;

    for (let frame = 0; frame < 3; frame++) sizer.ensure(800, 60, 2);

    expect(canvas.backingStoreWrites()).toEqual([]);
    // The ONLY per-frame operation is the transform reset (needed because
    // the unchanged path never clears canvas state via a width= write).
    expect(canvas.log).toEqual([
      "setTransform(2,0,0,2,0,0)",
      "setTransform(2,0,0,2,0,0)",
      "setTransform(2,0,0,2,0,0)",
    ]);
  });

  it("geometry change (window resize / monitor swap) reallocates once", () => {
    const canvas = makeStubCanvas();
    const sizer = createCanvasSizer(canvas);
    sizer.ensure(800, 60, 2);
    canvas.log.length = 0;

    sizer.ensure(800, 60, 1); // dragged to a 1x monitor
    expect(canvas.backingStoreWrites()).toEqual(["width=800", "height=60"]);
    canvas.log.length = 0;

    sizer.ensure(800, 60, 1); // steady state again
    expect(canvas.backingStoreWrites()).toEqual([]);
  });

  it("caches the context: getContext runs once across ensures", () => {
    const canvas = makeStubCanvas();
    const sizer = createCanvasSizer(canvas);
    const a = sizer.ensure(800, 60, 2);
    const b = sizer.ensure(800, 60, 2);
    const c = sizer.ensure(400, 60, 2);
    expect(a).toBe(b);
    expect(b).toBe(c);
    expect(canvas.log.filter((e) => e.startsWith("getContext"))).toHaveLength(1);
  });

  it("setTransform never compounds: dpr is absolute each frame", () => {
    const canvas = makeStubCanvas();
    const sizer = createCanvasSizer(canvas);
    sizer.ensure(800, 60, 2);
    sizer.ensure(800, 60, 2);
    const transforms = canvas.log.filter((e) => e.startsWith("setTransform"));
    expect(transforms).toEqual([
      "setTransform(2,0,0,2,0,0)",
      "setTransform(2,0,0,2,0,0)",
    ]);
  });

  it("throws loudly when the canvas yields no 2d context", () => {
    const sizer = createCanvasSizer(makeStubCanvas({ nullContext: true }));
    expect(() => sizer.ensure(800, 60, 2)).toThrow(/no 2d context/);
  });

  it("defaults dpr to 1 where devicePixelRatio is unavailable (Node)", () => {
    const canvas = makeStubCanvas();
    const sizer = createCanvasSizer(canvas);
    sizer.ensure(800, 60);
    expect(sizer.geometry()).toEqual(geom(800, 60, 1));
    expect(canvas.log).toContain("setTransform(1,0,0,1,0,0)");
  });

  describe("with a live devicePixelRatio global", () => {
    afterEach(() => vi.unstubAllGlobals());

    it("defaults dpr to devicePixelRatio (legacy `devicePixelRatio || 1`)", () => {
      vi.stubGlobal("devicePixelRatio", 2);
      const canvas = makeStubCanvas();
      createCanvasSizer(canvas).ensure(800, 60);
      expect(canvas.log).toContain("width=1600");
      expect(canvas.log).toContain("setTransform(2,0,0,2,0,0)");
    });

    it("guards a zero devicePixelRatio back to 1", () => {
      vi.stubGlobal("devicePixelRatio", 0);
      const canvas = makeStubCanvas();
      const sizer = createCanvasSizer(canvas);
      sizer.ensure(800, 60);
      expect(sizer.geometry()).toEqual(geom(800, 60, 1));
    });
  });
});
