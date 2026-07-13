// T23 wheel semantics (features/02 H5): plain wheel scrolls the lanes (null
// intent, the binding leaves the browser default), Ctrl/Cmd+wheel zooms at the
// cursor. Ported mapping from viewer.html:5100-5115.

import { describe, it, expect } from "vitest";
import { wheelZoomIntent, WHEEL_ZOOM_FACTOR, type WheelInput } from "./wheel.js";

const BASE: WheelInput = {
  ctrlKey: false,
  metaKey: false,
  deltaY: 0,
  mouseXInDraw: 500,
  drawW: 1000,
};

describe("wheelZoomIntent", () => {
  it("plain wheel returns null (scroll lanes, no zoom)", () => {
    expect(wheelZoomIntent({ ...BASE, deltaY: 120 })).toBeNull();
    expect(wheelZoomIntent({ ...BASE, deltaY: -120 })).toBeNull();
  });

  it("Ctrl+wheel down zooms OUT (factor 1.3) at the cursor fraction", () => {
    const intent = wheelZoomIntent({ ...BASE, ctrlKey: true, deltaY: 120, mouseXInDraw: 250 });
    expect(intent).toEqual({ factor: WHEEL_ZOOM_FACTOR, centerFrac: 0.25 });
  });

  it("Ctrl+wheel up zooms IN (factor 1/1.3)", () => {
    const intent = wheelZoomIntent({ ...BASE, ctrlKey: true, deltaY: -120, mouseXInDraw: 500 });
    expect(intent).toEqual({ factor: 1 / WHEEL_ZOOM_FACTOR, centerFrac: 0.5 });
  });

  it("Cmd (metaKey) also zooms (macOS)", () => {
    expect(wheelZoomIntent({ ...BASE, metaKey: true, deltaY: 1 })).not.toBeNull();
  });

  it("clamps the cursor fraction to [0,1]", () => {
    expect(wheelZoomIntent({ ...BASE, ctrlKey: true, mouseXInDraw: -50 })?.centerFrac).toBe(0);
    expect(wheelZoomIntent({ ...BASE, ctrlKey: true, mouseXInDraw: 5000 })?.centerFrac).toBe(1);
  });

  it("degenerate drawW <= 0 centres at 0.5", () => {
    expect(wheelZoomIntent({ ...BASE, ctrlKey: true, drawW: 0 })?.centerFrac).toBe(0.5);
  });
});
