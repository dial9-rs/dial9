import { describe, expect, it } from "vitest";
import { mountActionsBar, profileButtonLabels } from "./actions-bar.js";
import type { BrowserActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import { createBrowserStore, type BrowserStore } from "./state.js";

class FakeButton {
  textContent = "";
  title = "";
  disabled = false;
  addEventListener(): void {}
}

class FakeSpan {
  textContent = "";
}

async function flushStore(): Promise<void> {
  await Promise.resolve();
}

/**
 * Mount the bar over fake buttons and return the handles to assert on.
 *
 * Renders only run inside a store flush, so `prime()` pushes a no-op update
 * for the first frame the way main.ts's primeRenders does.
 */
function setup(): {
  store: BrowserStore;
  cpuBtn: FakeButton;
  healthBtn: FakeButton;
  prime(): void;
} {
  const cpuBtn = new FakeButton();
  const healthBtn = new FakeButton();
  const els = {
    viewBtn: new FakeButton(),
    cpuBtn,
    healthBtn,
    spansBtn: new FakeButton(),
    selectionWarn: new FakeSpan(),
    selectionCount: new FakeSpan(),
  } as unknown as BrowserEls;

  const store = createBrowserStore();
  mountActionsBar({ store, els, actions: {} as unknown as BrowserActions });
  return {
    store,
    cpuBtn,
    healthBtn,
    prime: () => {
      store.update("diff", {});
    },
  };
}

const scope = (host: string): URLSearchParams =>
  new URLSearchParams({ bucket: "b", host, start_ns: "1000", end_ns: "2000" });

describe("profileButtonLabels", () => {
  it("keeps the plain labels outside diff mode", () => {
    const labels = profileButtonLabels(false);
    expect(labels.flamegraph.text).toBe("🔥 Flamegraph");
    expect(labels.tokio.text).toBe("⚡ Tokio Stats");
  });

  it("says 'diff' when both sides are captured", () => {
    const labels = profileButtonLabels(true);
    expect(labels.flamegraph.text).toBe("🔥 Flamegraph diff");
    expect(labels.tokio.text).toBe("⚡ Tokio Stats diff");
    // The tooltip must not keep promising a single-scope view.
    expect(labels.flamegraph.title).toContain("A/B");
    expect(labels.tokio.title).toContain("A/B");
  });
});

// #626: with a full A/B diff captured, these buttons open the two-sided diff
// rather than a single-scope view, so the labels have to follow the capture
// state - otherwise the button silently does something other than it says.
describe("actions bar diff-mode labels", () => {
  it("relabels once both diff sides are captured, and back again", async () => {
    const { store, cpuBtn, healthBtn, prime } = setup();

    prime();
    await flushStore();
    expect(cpuBtn.textContent).toBe("🔥 Flamegraph");

    // A partial capture still opens the single-scope view.
    store.update("diff", { a: scope("h1"), b: null });
    await flushStore();
    expect(cpuBtn.textContent).toBe("🔥 Flamegraph");
    expect(healthBtn.textContent).toBe("⚡ Tokio Stats");

    store.update("diff", { a: scope("h1"), b: scope("h2") });
    await flushStore();
    expect(cpuBtn.textContent).toBe("🔥 Flamegraph diff");
    expect(healthBtn.textContent).toBe("⚡ Tokio Stats diff");

    store.update("diff", { a: null, b: null });
    await flushStore();
    expect(cpuBtn.textContent).toBe("🔥 Flamegraph");
    expect(healthBtn.textContent).toBe("⚡ Tokio Stats");
  });

  // The per-tab render branches return early when there is nothing selected,
  // so the relabel has to happen ahead of them.
  it("relabels even with no selection to profile", async () => {
    const { store, cpuBtn, healthBtn } = setup();

    store.update("browse", { selection: null, segments: [], fullDomain: null });
    store.update("diff", { a: scope("h1"), b: scope("h2") });
    await flushStore();

    expect(cpuBtn.textContent).toBe("🔥 Flamegraph diff");
    expect(healthBtn.textContent).toBe("⚡ Tokio Stats diff");
  });

  it("relabels on the raw tab too", async () => {
    const { store, cpuBtn } = setup();

    store.update("ui", { tab: "raw" });
    store.update("diff", { a: scope("h1"), b: scope("h2") });
    await flushStore();

    expect(cpuBtn.textContent).toBe("🔥 Flamegraph diff");
  });
});

// The static markup must agree with the non-diff labels, or the buttons
// visibly change text on the first store flush.
describe("actions bar markup", () => {
  it("matches the labels and titles in index.html", async () => {
    const { readFile } = await import("node:fs/promises");
    const html = await readFile(new URL("../../../index.html", import.meta.url), "utf8");
    const plain = profileButtonLabels(false);

    expect(html).toContain(plain.flamegraph.text);
    expect(html).toContain(plain.flamegraph.title);
    expect(html).toContain(plain.tokio.text);
    expect(html).toContain(plain.tokio.title);
  });
});
