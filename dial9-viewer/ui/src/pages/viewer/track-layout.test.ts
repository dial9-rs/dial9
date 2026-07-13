// Tests for the viewer track catalogue + per-track geometry (T21).
// The load-bearing property: every track shares ONE drawW so their time
// axes line up (the A13 invariant) - a shell regression here silently
// misaligns tracks, so it is asserted explicitly.

import { describe, it, expect } from "vitest";
import { TRACKS, LABEL_W, trackGeometry } from "./track-layout.js";
import type { TrackSpec } from "./track-layout.js";

const opts = { pw: 1000, viewStart: 0, viewEnd: 4_000_000_000, dpr: 1 };

describe("track catalogue", () => {
  it("orders the unified column: axis, lanes, then analysis surfaces", () => {
    expect(TRACKS.map((t) => t.id)).toEqual([
      "timeline",
      "lanes",
      "cpu",
      "queue",
      "spans",
      "events",
      "task-detail",
    ]);
  });

  it("marks only task-detail as selection-only (features/02 N1)", () => {
    const selectionOnly = TRACKS.filter((t) => t.selectionOnly).map((t) => t.id);
    expect(selectionOnly).toEqual(["task-detail"]);
  });

  it("records a downstream owner ticket for every slot", () => {
    for (const t of TRACKS) expect(t.ownedBy).toMatch(/^T\d+$/);
  });
});

describe("trackGeometry - shared axis (A13)", () => {
  it("gives every track the same drawW at a fixed column width", () => {
    const drawWs = new Set(TRACKS.map((t) => trackGeometry(t, opts).time.drawW));
    expect(drawWs.size).toBe(1);
  });

  it("splits width as LABEL_W gutter + drawW (no scrollbar)", () => {
    const t = TRACKS[0] as TrackSpec;
    const g = trackGeometry(t, opts);
    expect(g.time.drawW).toBe(opts.pw - LABEL_W);
  });

  it("subtracts the scrollbar gutter so the right edge matches the lanes", () => {
    const t = TRACKS[0] as TrackSpec;
    const g = trackGeometry(t, { ...opts, scrollbarW: 15 });
    expect(g.time.drawW).toBe(opts.pw - LABEL_W - 15);
  });

  it("carries the track height into the geometry box", () => {
    for (const t of TRACKS) {
      expect(trackGeometry(t, opts).height).toBe(t.height);
    }
  });
});
