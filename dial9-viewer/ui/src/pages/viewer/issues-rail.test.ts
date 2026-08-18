// Tests for the issues-rail controller. The lit-html rail markup is exercised
// in the browser (no DOM env here); this suite pins the `n`/`p` stepping wiring
// that dispatches into the store - the store-side of the keyboard nav.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { parseTraceBuffer } from "../../lib/trace/index.js";
import type { ParsedTrace } from "../../types/trace.js";
import type { KeyEventLike, KeyBinding } from "../../lib/interact/keyboard.js";
import { createViewerStore } from "./store.js";
import type { ViewerStore } from "../../store/store.js";
import {
  clampColWidth,
  clampRailWidth,
  createIssuesRail,
} from "./issues-rail.js";

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

/** A store with the demo trace resident and the viewport fitted. */
function loadedStore(): ViewerStore {
  const store = createViewerStore({ scheduler: () => {} });
  store.update("trace", { trace });
  const minTs = trace.minTs ?? 0;
  const maxTs = trace.maxTs ?? 0;
  store.update("viewport", { minTs, maxTs, viewStart: minTs, viewEnd: maxTs });
  return store;
}

const FAKE_KEY: KeyEventLike = {
  key: "",
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  shiftKey: false,
  target: null,
  defaultPrevented: false,
  preventDefault: () => {},
};

function binding(bindings: readonly KeyBinding[], key: string): KeyBinding {
  const b = bindings.find((x) => x.key === key);
  if (b === undefined) throw new Error(`no binding for ${key}`);
  return b;
}

describe("issues-rail n/p stepping", () => {
  it("`n` from nothing-selected lands on the first POI and moves the view", () => {
    const store = loadedStore();
    const rail = createIssuesRail(store);
    const before = store.getState().viewport;

    const consumed = binding(rail.keyBindings, "n").onKey(FAKE_KEY);

    const st = store.getState();
    expect(consumed).toBe(true); // there are sched POIs, so the key is consumed
    expect(st.poi.index).toBe(0);
    // Centering on a POI narrows/moves the view off the full extent.
    const moved =
      st.viewport.viewStart !== before.viewStart ||
      st.viewport.viewEnd !== before.viewEnd;
    expect(moved).toBe(true);
  });

  it("`n` advances and `p` retreats through the list", () => {
    const store = loadedStore();
    const rail = createIssuesRail(store);
    const n = binding(rail.keyBindings, "n");
    const p = binding(rail.keyBindings, "p");

    n.onKey(FAKE_KEY);
    expect(store.getState().poi.index).toBe(0);
    n.onKey(FAKE_KEY);
    expect(store.getState().poi.index).toBe(1);
    p.onKey(FAKE_KEY);
    expect(store.getState().poi.index).toBe(0);
  });

  it("declines (returns false) when no trace is loaded - the key falls through", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const rail = createIssuesRail(store);
    expect(binding(rail.keyBindings, "n").onKey(FAKE_KEY)).toBe(false);
    expect(binding(rail.keyBindings, "p").onKey(FAKE_KEY)).toBe(false);
  });
});

describe("rail width clamp (resize drag bounds)", () => {
  it("clamps below the minimum usable table width", () => {
    expect(clampRailWidth(50, 1200)).toBe(220);
  });

  it("passes through an in-bounds width, rounded to whole px", () => {
    expect(clampRailWidth(420, 1200)).toBe(420);
    expect(clampRailWidth(300.6, 1200)).toBe(301);
  });

  it("caps at 60% of the viewport so the track column stays usable", () => {
    expect(clampRailWidth(2000, 1200)).toBe(720);
  });
});

describe("column width clamp (per-column resize bounds, both rail tables)", () => {
  it("floors at the column header's measured width so the label never clips", () => {
    expect(clampColWidth(10, 64)).toBe(64);
    expect(clampColWidth(100, 64)).toBe(100);
    expect(clampColWidth(63.4, 20)).toBe(63);
  });

  it("keeps the absolute floor when the header measure is degenerate", () => {
    expect(clampColWidth(4, 0)).toBe(24);
    expect(clampColWidth(4, Number.NaN)).toBe(24);
    expect(clampColWidth(4, -10)).toBe(24);
  });
});
