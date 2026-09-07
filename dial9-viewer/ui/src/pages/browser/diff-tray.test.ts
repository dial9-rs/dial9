// The landing page's diff tray: the "Quick B" presets (#624) that derive
// side B from side A instead of navigating to a second scope and capturing
// it by hand.

import { describe, expect, it } from "vitest";
import { createDiffActions } from "./diff-actions.js";
import { presetHostOptions } from "./diff-tray.js";
import type { BrowserEls } from "./dom.js";
import { createBrowserStore, type HeatmapRow } from "./state.js";

const START = 1_768_471_200_000_000_000n;
const HOUR_NS = 3_600_000_000_000n;

function scopeA(hosts: string[] = ["h1"], windowed = true): URLSearchParams {
  const p = new URLSearchParams({ bucket: "traces", service: "api" });
  if (windowed) {
    p.set("start_ns", String(START));
    p.set("end_ns", String(START + HOUR_NS));
  }
  for (const host of hosts) p.append("host", host);
  return p;
}

function row(service: string, host: string): HeatmapRow {
  return {
    service,
    host,
    label: `${service} / ${host}`,
    segments: [],
    totalBytes: 0,
    tiled: [],
    gaps: [],
  } as unknown as HeatmapRow;
}

describe("presetHostOptions", () => {
  it("labels each offerable host with its service", () => {
    const options = presetHostOptions(scopeA(["h1"]), [
      row("api", "h1"),
      row("api", "h2"),
      row("web", "h3"),
    ]);

    expect(options).toStrictEqual([
      { value: "h2", label: "api / h2" },
      { value: "h3", label: "web / h3" },
    ]);
  });

  // Comparing a host against itself is a no-op.
  it("excludes the host side A is already pinned to", () => {
    const options = presetHostOptions(scopeA(["h2"]), [row("api", "h1"), row("api", "h2")]);
    expect(options.map((o) => o.value)).toStrictEqual(["h1"]);
  });

  it("offers nothing when the view holds no other host", () => {
    expect(presetHostOptions(scopeA(["h1"]), [row("api", "h1")])).toStrictEqual([]);
    expect(presetHostOptions(scopeA(["h1"]), [])).toStrictEqual([]);
  });

  it("de-duplicates hosts that appear in several rows", () => {
    const options = presetHostOptions(scopeA(["h1"]), [
      row("api", "h2"),
      row("api", "h2"),
    ]);
    expect(options).toStrictEqual([{ value: "h2", label: "api / h2" }]);
  });

  // A multi-host A can meaningfully be narrowed to one of its own hosts.
  it("offers a multi-host A's own hosts", () => {
    const options = presetHostOptions(scopeA(["h1", "h2"]), [
      row("api", "h1"),
      row("api", "h2"),
    ]);
    expect(options.map((o) => o.value)).toStrictEqual(["h1", "h2"]);
  });
});

function setup() {
  const store = createBrowserStore();
  const els = {
    bucketInput: { value: "traces" },
  } as unknown as BrowserEls;
  return { store, actions: createDiffActions(store, els) };
}

describe("applyDiffPreset", () => {
  it("fills B with the same window on another host", () => {
    const { store, actions } = setup();
    const a = scopeA(["h1"]);
    store.update("diff", { a, b: null });

    actions.applyDiffPreset({ kind: "host", host: "h2" });

    const { b } = store.getState().diff;
    expect(b!.getAll("host")).toStrictEqual(["h2"]);
    expect(b!.get("start_ns")).toBe(a.get("start_ns"));
    expect(b!.get("end_ns")).toBe(a.get("end_ns"));
    expect(b!.get("service")).toBe("api");
    // A is left exactly as captured.
    expect(store.getState().diff.a).toBe(a);
  });

  it("fills B with the same scope shifted back", () => {
    const { store, actions } = setup();
    store.update("diff", { a: scopeA(["h1"]), b: null });

    actions.applyDiffPreset({ kind: "shift", shift: "24h" });

    const { b } = store.getState().diff;
    expect(BigInt(b!.get("start_ns")!)).toBe(START - 86_400_000_000_000n);
    expect(b!.getAll("host")).toStrictEqual(["h1"]);
  });

  // Filling the tray (rather than opening a diff) is deliberate: this page
  // can compare in the flamegraph OR tokio-stats, so the target stays the
  // user's choice - the "Compare in:" row.
  it("leaves the tray ready to launch rather than opening anything", () => {
    const { store, actions } = setup();
    store.update("diff", { a: scopeA(["h1"]), b: null });

    actions.applyDiffPreset({ kind: "shift", shift: "1h" });

    const { a, b } = store.getState().diff;
    expect(a).not.toBeNull();
    expect(b).not.toBeNull();
  });

  it("does nothing without an A to derive from", () => {
    const { store, actions } = setup();

    actions.applyDiffPreset({ kind: "shift", shift: "1h" });

    expect(store.getState().diff).toStrictEqual({ a: null, b: null });
  });

  // A preset must never silently discard a B the user picked by hand.
  it("does nothing once B is already captured", () => {
    const { store, actions } = setup();
    const a = scopeA(["h1"]);
    const b = scopeA(["h9"]);
    store.update("diff", { a, b });

    actions.applyDiffPreset({ kind: "host", host: "h2" });

    expect(store.getState().diff.b).toBe(b);
  });
});
