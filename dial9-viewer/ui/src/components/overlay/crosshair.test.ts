// Crosshair tests: the overlay's ns<->x mapping stays pixel-exact with the
// lanes/panels across column widths, and drawCrosshair emits the expected
// overlays. Recording 2D-context stub, no DOM.

import { describe, it, expect } from "vitest";
import { crosshairX, drawCrosshair, type CrosshairContext, type CrosshairInput } from "./crosshair.js";
import { TRACKS, LABEL_W, trackGeometry } from "../../pages/viewer/track-layout.js";
import type { TrackSpec } from "../../pages/viewer/track-layout.js";

const trackById = (id: string): TrackSpec => TRACKS.find((t) => t.id === id) as TrackSpec;

// ── Alignment invariant (overlay lines up with every track) ──────────

describe("crosshairX / alignment invariant (A13/N4, three widths)", () => {
  const viewStart = 1_000_000;
  const viewEnd = 4_000_000_000;
  const samples = [viewStart, viewStart + 1, (viewStart + viewEnd) / 2, viewEnd - 7, viewEnd];
  const widths = [
    { pw: 420, scrollbarW: 0 },
    { pw: 1024, scrollbarW: 15 },
    { pw: 2560, scrollbarW: 0 },
  ];

  for (const { pw, scrollbarW } of widths) {
    it(`crosshair x == every track's absolute-in-column x (pw=${pw}, sb=${scrollbarW})`, () => {
      const opts = { pw, scrollbarW, viewStart, viewEnd, dpr: 1 };
      const overlayGeo = trackGeometry(trackById("lanes"), opts);
      const tracks = ["lanes", "cpu", "queue", "spans", "events"].map((id) =>
        trackGeometry(trackById(id), opts),
      );
      for (const ns of samples) {
        // The overlay draws the crosshair at this x (its full-column origin).
        const overlayX = crosshairX(overlayGeo.time, ns);
        for (const g of tracks) {
          // Every track draws content at canvas-local `nsToPanelX(ns) - labelW`,
          // and its canvas sits after the LABEL_W gutter, so its absolute x is:
          const trackAbsoluteX = LABEL_W + (g.time.nsToPanelX(ns) - g.time.labelW);
          expect(overlayX).toBeCloseTo(trackAbsoluteX, 9);
        }
      }
    });
  }

  it("crosshairX is exactly the shared nsToPanelX (no private mapping)", () => {
    const g = trackGeometry(trackById("lanes"), {
      pw: 1000,
      scrollbarW: 0,
      viewStart: 0,
      viewEnd: 1e9,
      dpr: 1,
    });
    for (const ns of [0, 2.5e8, 5e8, 1e9]) {
      expect(crosshairX(g.time, ns)).toBe(g.time.nsToPanelX(ns));
    }
  });
});

// ── Recording context ────────────────────────────────────────────────────

interface StrokeRec {
  x: number;
  dash: number[];
  strokeStyle: string;
  lineWidth: number;
}
interface Recording {
  ctx: CrosshairContext;
  strokes: StrokeRec[];
  clears: number;
  fillRects: number;
  strokeRects: number;
  fillTexts: string[];
}

function recordingCtx(): Recording {
  const rec: Recording = {
    strokes: [],
    clears: 0,
    fillRects: 0,
    strokeRects: 0,
    fillTexts: [],
    ctx: null as unknown as CrosshairContext,
  };
  let curDash: number[] = [];
  let curX = 0;
  rec.ctx = {
    strokeStyle: "",
    fillStyle: "",
    lineWidth: 1,
    font: "",
    textAlign: "left",
    textBaseline: "alphabetic",
    clearRect() {
      rec.clears++;
    },
    beginPath() {},
    moveTo(x: number) {
      curX = x;
    },
    lineTo() {},
    stroke() {
      rec.strokes.push({
        x: curX,
        dash: [...curDash],
        strokeStyle: rec.ctx.strokeStyle,
        lineWidth: rec.ctx.lineWidth,
      });
    },
    fillRect() {
      rec.fillRects++;
    },
    strokeRect() {
      rec.strokeRects++;
    },
    fillText(text: string) {
      rec.fillTexts.push(text);
    },
    measureText(text: string) {
      return { width: text.length * 6 };
    },
    setLineDash(segments: number[]) {
      curDash = segments;
    },
  };
  return rec;
}

// A plain full-width layout (labelW-shifted) for draw-op assertions.
function layoutFor(pw: number, viewStart: number, viewEnd: number): CrosshairInput["time"] {
  return trackGeometry(trackById("lanes"), { pw, scrollbarW: 0, viewStart, viewEnd, dpr: 1 }).time;
}

describe("drawCrosshair (I2-I5)", () => {
  const viewStart = 0;
  const viewEnd = 1_000_000;
  const time = layoutFor(1000, viewStart, viewEnd);
  const base: CrosshairInput = {
    time,
    width: 1000,
    height: 400,
    viewStart,
    viewEnd,
    mouseNs: null,
    keyboardSelection: null,
    hoverEventTs: null,
    pinnedEvent: null,
  };

  it("clears the canvas and draws nothing when idle", () => {
    const rec = recordingCtx();
    drawCrosshair(rec.ctx, base);
    expect(rec.clears).toBe(1);
    expect(rec.strokes.length).toBe(0);
  });

  it("I2: dashed white mouse crosshair at nsToPanelX, only in view", () => {
    const ns = 400_000;
    const rec = recordingCtx();
    drawCrosshair(rec.ctx, { ...base, mouseNs: ns });
    expect(rec.strokes.length).toBe(1);
    const s = rec.strokes[0]!;
    expect(s.x).toBeCloseTo(time.nsToPanelX(ns), 9);
    expect(s.dash).toEqual([4, 4]);
    expect(s.strokeStyle).toBe("rgba(255,255,255,0.3)");

    // Out of range hides it.
    const rec2 = recordingCtx();
    drawCrosshair(rec2.ctx, { ...base, mouseNs: viewEnd + 1 });
    expect(rec2.strokes.length).toBe(0);
  });

  it("I3: solid bright keyboard cursor at cursorNs", () => {
    const rec = recordingCtx();
    drawCrosshair(rec.ctx, {
      ...base,
      keyboardSelection: { kind: "region-select", startNs: 100_000, cursorNs: 250_000 },
    });
    expect(rec.strokes.length).toBe(1);
    const s = rec.strokes[0]!;
    expect(s.x).toBeCloseTo(time.nsToPanelX(250_000), 9);
    expect(s.dash).toEqual([]);
    expect(s.strokeStyle).toBe("rgba(255,255,255,0.8)");
    expect(s.lineWidth).toBe(1.5);
  });

  it("I4: orange dashed custom-event guide at hoverEventTs", () => {
    const rec = recordingCtx();
    drawCrosshair(rec.ctx, { ...base, hoverEventTs: 600_000 });
    expect(rec.strokes.length).toBe(1);
    const s = rec.strokes[0]!;
    expect(s.x).toBeCloseTo(time.nsToPanelX(600_000), 9);
    expect(s.dash).toEqual([3, 3]);
    expect(s.strokeStyle).toBe("rgba(255,140,0,0.4)");
  });

  it("I5: pinned marker draws a line + label chip, clamped inside the overlay", () => {
    const rec = recordingCtx();
    drawCrosshair(rec.ctx, {
      ...base,
      pinnedEvent: { timestamp: 500_000, label: "MyEvent @ 00:00:00" },
    });
    // One marker line + one chip (fillRect bg + strokeRect border + label).
    expect(rec.strokes.length).toBe(1);
    expect(rec.strokes[0]!.strokeStyle).toBe("rgba(255,140,0,0.9)");
    expect(rec.fillRects).toBe(1);
    expect(rec.strokeRects).toBe(1);
    expect(rec.fillTexts).toEqual(["MyEvent @ 00:00:00"]);
  });

  it("draws all four overlays together when all are active", () => {
    const rec = recordingCtx();
    drawCrosshair(rec.ctx, {
      ...base,
      mouseNs: 100_000,
      keyboardSelection: { kind: "zoom-select", startNs: 0, cursorNs: 200_000 },
      hoverEventTs: 300_000,
      pinnedEvent: { timestamp: 500_000, label: "E @ t" },
    });
    // Mouse crosshair + keyboard cursor + event guide + pinned marker = 4 lines.
    expect(rec.strokes.length).toBe(4);
  });
});
