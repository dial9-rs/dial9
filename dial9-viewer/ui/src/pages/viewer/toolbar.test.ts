// Tests for the toolbar's pure logic. The lit-html templates are exercised in
// the browser (no DOM env here); this suite pins the goto-time math, the
// file-info stats line, and the uninstrumented count.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "../../lib/trace/index.js";
import type { ParsedTrace } from "../../types/trace.js";
import type { ViewportSlice } from "../../types/state.js";
import { parseGotoTime } from "../../lib/interact/goto-time.js";
import {
  fileMetaText,
  gotoTargetNs,
  gotoWindow,
  uninstrumentedCount,
} from "./toolbar.js";

let trace: ParsedTrace;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)),
  );
  const raw =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  trace = await parseTraceBuffer(raw);
});

const vp: ViewportSlice = { viewStart: 4e8, viewEnd: 6e8, minTs: 0, maxTs: 1e9 };

describe("goto-time", () => {
  it("resolves absolute seconds as an offset from the trace start", () => {
    const g = parseGotoTime("0.5")!;
    expect(gotoTargetNs(g, vp)).toBe(0.5e9); // minTs(0) + 0.5s
  });
  it("resolves +/- relative seconds from the current view center", () => {
    const center = (vp.viewStart + vp.viewEnd) / 2; // 5e8
    expect(gotoTargetNs(parseGotoTime("+0.1")!, vp)).toBe(center + 0.1e9);
    expect(gotoTargetNs(parseGotoTime("-0.1")!, vp)).toBe(center - 0.1e9);
  });
  it("centers the view on the target, preserving duration", () => {
    const win = gotoWindow(5e8, vp);
    const dur = vp.viewEnd - vp.viewStart;
    expect(win.viewEnd - win.viewStart).toBe(dur);
    expect((win.viewStart + win.viewEnd) / 2).toBe(5e8);
  });
  it("clamps to the bounds without shrinking the window", () => {
    const dur = vp.viewEnd - vp.viewStart;
    const nearStart = gotoWindow(vp.minTs, vp);
    expect(nearStart.viewStart).toBe(vp.minTs);
    expect(nearStart.viewEnd - nearStart.viewStart).toBe(dur);
    const nearEnd = gotoWindow(vp.maxTs, vp);
    expect(nearEnd.viewEnd).toBe(vp.maxTs);
    expect(nearEnd.viewEnd - nearEnd.viewStart).toBe(dur);
  });
});

describe("file-info stats line", () => {
  it("reads 'no trace loaded' before a trace", () => {
    expect(fileMetaText(null)).toBe("no trace loaded");
  });
  it("shows events, workers and a duration for the demo trace", () => {
    const text = fileMetaText(trace);
    expect(text).toMatch(/events/);
    expect(text).toMatch(/workers/);
    // Duration segment is present (a human duration unit).
    expect(text.split(" · ").length).toBeGreaterThanOrEqual(3);
  });
});

describe("uninstrumented count", () => {
  it("counts tasks flagged not-instrumented, never below zero", () => {
    const n = uninstrumentedCount(trace);
    expect(n).toBeGreaterThanOrEqual(0);
    const manual = [...trace.taskInstrumented.values()].filter((v) => !v).length;
    expect(n).toBe(manual);
  });
});
