import { afterEach, describe, expect, it, vi } from "vitest";
import { mountActionsBar } from "./actions-bar.js";
import type { BrowserActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import {
  createOpenLinks,
  effectiveProfileSelection,
} from "./open-links.js";
import { createBrowserStore } from "./state.js";
import type { HeatmapSegment } from "./state.js";

class FakeElement {
  disabled = false;
  textContent = "";
  value = "";

  addEventListener(): void {}
}

function segments(): HeatmapSegment[] {
  return [
    {
      key: "traces/2026-04-09/1910/api/host-a/boot/1000-0.bin.gz",
      size: 10,
      start: 100,
      end: 150,
      layout: "known",
      service: "api",
      host: "host-a",
      bootId: "boot",
    },
    {
      key: "traces/2026-04-09/1911/api/host-b/boot/1001-0.bin.gz",
      size: 20,
      start: 150,
      end: 200,
      layout: "known",
      service: "api",
      host: "host-b",
      bootId: "boot",
    },
  ];
}

function fakeEls(): BrowserEls {
  const el = () => new FakeElement();
  return {
    bucketInput: Object.assign(el(), { value: "trace-bucket" }),
    credsRegion: el(),
    viewBtn: el(),
    cpuBtn: el(),
    healthBtn: el(),
    spansBtn: el(),
    selectionWarn: el(),
    selectionCount: el(),
  } as unknown as BrowserEls;
}

async function flushStore(): Promise<void> {
  await Promise.resolve();
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("effectiveProfileSelection", () => {
  it("falls back to every loaded segment and the full service extent", () => {
    const store = createBrowserStore();
    store.update("browse", {
      segments: segments(),
      fullDomain: { tMin: 100, tMax: 200 },
    });

    expect(effectiveProfileSelection(store.getState().browse)).toEqual({
      keys: segments().map((segment) => segment.key),
      bytes: 30,
      t0: 100,
      t1: 200,
    });
  });

  it("prefers an explicit heatmap selection", () => {
    const store = createBrowserStore();
    const selection = {
      keys: [segments()[0]!.key],
      bytes: 10,
      t0: 110,
      t1: 120,
    };
    store.update("browse", {
      segments: segments(),
      fullDomain: { tMin: 100, tMax: 200 },
      selection,
    });

    expect(effectiveProfileSelection(store.getState().browse)).toBe(selection);
  });
});

describe("profile links without a heatmap selection", () => {
  it("opens Flamegraph, Tokio Stats, and Spans over the loaded service", () => {
    const open = vi.fn();
    vi.stubGlobal("window", {
      Dial9Creds: null,
      open,
    });
    vi.stubGlobal("alert", vi.fn());

    const store = createBrowserStore();
    store.update("config", { aggregationEnabled: true });
    store.update("browse", {
      segments: segments(),
      fullDomain: { tMin: 100, tMax: 200 },
    });
    const links = createOpenLinks({
      store,
      els: fakeEls(),
      getSelectedKeys: () => [],
      launchDiff: vi.fn(),
    });

    links.viewCpuProfile();
    links.viewTokioStats();
    links.viewSpanExplorer();

    expect(open).toHaveBeenCalledTimes(3);
    for (const [url] of open.mock.calls) {
      const parsed = new URL(String(url), "https://viewer.example/");
      expect(parsed.searchParams.get("bucket")).toBe("trace-bucket");
      expect(parsed.searchParams.get("service")).toBe("api");
      expect(parsed.searchParams.get("start_ns")).toBe("100000000000");
      expect(parsed.searchParams.get("end_ns")).toBe("200000000000");
      expect(parsed.searchParams.getAll("host")).toEqual(["host-a", "host-b"]);
    }
    expect(String(open.mock.calls[0]![0])).toMatch(/^flamegraph\.html\?/);
    const flamegraph = new URL(String(open.mock.calls[0]![0]), "https://viewer.example/");
    expect(flamegraph.searchParams.get("api")).toBe("1");
    expect(String(open.mock.calls[1]![0])).toMatch(/^tokio_stats\.html\?/);
    expect(String(open.mock.calls[2]![0])).toMatch(/^span_explorer\.html\?/);
  });

  it("enables profile buttons while View Selected remains disabled", async () => {
    vi.stubGlobal("window", { Dial9Creds: null });
    const store = createBrowserStore();
    const els = fakeEls();
    mountActionsBar({
      store,
      els,
      actions: {
        viewSelected: vi.fn(),
        viewCpuProfile: vi.fn(),
        viewTokioStats: vi.fn(),
        viewSpanExplorer: vi.fn(),
      } as unknown as BrowserActions,
    });

    store.update("config", { aggregationEnabled: true });
    store.update("browse", {
      segments: segments(),
      fullDomain: { tMin: 100, tMax: 200 },
    });
    await flushStore();

    expect(els.viewBtn.disabled).toBe(true);
    expect(els.cpuBtn.disabled).toBe(false);
    expect(els.healthBtn.disabled).toBe(false);
    expect(els.spansBtn.disabled).toBe(false);
    expect(els.selectionCount.textContent).toContain("Current service");
  });
});
