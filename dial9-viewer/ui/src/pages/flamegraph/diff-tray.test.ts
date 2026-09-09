import { describe, expect, it, vi } from "vitest";
import { parseDiff } from "../../lib/canvas/flamegraph_diff.js";
import {
  createDiffTray,
  summarizeScope,
  trayModel,
  type TrayModel,
} from "./diff-tray.js";

/** A scope shaped like fullScopeQuery's output for the live view. */
function scope(over: Record<string, string> = {}, hosts: string[] = ["h1"]): URLSearchParams {
  const p = new URLSearchParams({
    api: "1",
    bucket: "traces",
    service: "api",
    start_ns: "1768471200000000000",
    end_ns: "1768471800000000000",
    ...over,
  });
  for (const host of hosts) p.append("host", host);
  return p;
}

describe("summarizeScope", () => {
  it("names the service, host count, window and bucket", () => {
    expect(summarizeScope(scope())).toBe(
      "api · 1 host · 01-15 10:00 UTC · 10m 0.0s · traces",
    );
  });

  it("counts a multi-host scope", () => {
    expect(summarizeScope(scope({}, ["h1", "h2", "h3"]))).toContain("3 hosts");
  });

  it("surfaces the extra scope dimensions that narrow a capture", () => {
    const s = summarizeScope(scope({ thread_class: "blocking", source: "alloc" }));
    expect(s).toContain("blocking");
    expect(s).toContain("alloc");
  });

  // "cpu" is the default source; printing it would just be noise.
  it("omits the default cpu source", () => {
    expect(summarizeScope(scope({ source: "cpu" }))).not.toContain("cpu");
  });

  it("has readable output for an empty or missing scope", () => {
    expect(summarizeScope(null)).toBe("—");
    expect(summarizeScope(new URLSearchParams())).toBe("(empty scope)");
  });
});

describe("trayModel", () => {
  it("hides the tray until something is captured", () => {
    expect(trayModel({ a: null, b: null })).toStrictEqual({
      visible: false,
      a: null,
      b: null,
      canSwap: false,
      canOpen: false,
      presets: null,
    });
  });

  // With only A captured there is nothing to compare against and nothing to
  // swap with, but the tray shows so the user can see what they captured.
  it("shows a half-filled capture without enabling swap or open", () => {
    const model = trayModel({ a: scope(), b: null });
    expect(model.visible).toBe(true);
    expect(model.a).toContain("api");
    expect(model.b).toBeNull();
    expect(model.canSwap).toBe(false);
    expect(model.canOpen).toBe(false);
  });

  it("enables swap and open once both sides are captured", () => {
    const model = trayModel({ a: scope({}, ["h1"]), b: scope({}, ["h2"]) });
    expect(model.canSwap).toBe(true);
    expect(model.canOpen).toBe(true);
  });
});

/**
 * Drive a tray over a scripted sequence of live scopes: each `add()` captures
 * the next one, standing in for the user changing the Host dropdown between
 * captures.
 */
function setup(scopes: URLSearchParams[], knownHosts: string[] = []) {
  let next = 0;
  const models: TrayModel[] = [];
  const openDiff = vi.fn();
  const tray = createDiffTray({
    currentScope: () => scopes[Math.min(next++, scopes.length - 1)]!,
    openDiff,
    knownHosts: () => knownHosts,
    render: (model) => models.push(model),
  });
  return { tray, openDiff, models, last: () => models[models.length - 1]! };
}

/** The two scopes a diff link decodes back to. */
function decode(openDiff: ReturnType<typeof vi.fn>) {
  const search = openDiff.mock.calls[0]![0] as string;
  const parsed = parseDiff(new URLSearchParams(search));
  expect(parsed).not.toBeNull();
  return parsed!;
}

describe("flamegraph diff capture tray", () => {
  it("fills A then B from the live view, and renders each change", () => {
    const a = scope({}, ["h1"]);
    const b = scope({}, ["h2"]);
    const { tray, models } = setup([a, b]);

    // One render at creation, so the tray starts hidden rather than unstyled.
    expect(models).toHaveLength(1);
    expect(models[0]!.visible).toBe(false);

    tray.add();
    expect(tray.capture()).toStrictEqual({ a, b: null });
    tray.add();
    expect(tray.capture()).toStrictEqual({ a, b });

    // Every change repaints, and the last paint offers the diff.
    expect(models).toHaveLength(3);
    expect(models[2]!.canOpen).toBe(true);
  });

  // The point of the feature: the capture follows the Host dropdown, so two
  // adds around a host change give a host-vs-host comparison.
  it("captures whatever the live view currently scopes to", () => {
    const { tray } = setup([scope({}, ["h1"]), scope({}, ["h2"])]);

    tray.add();
    tray.add();

    expect(tray.capture().a!.getAll("host")).toStrictEqual(["h1"]);
    expect(tray.capture().b!.getAll("host")).toStrictEqual(["h2"]);
  });

  it("replaces B on a further add so the comparison side can be re-picked", () => {
    const third = scope({}, ["h3"]);
    const { tray } = setup([scope({}, ["h1"]), scope({}, ["h2"]), third]);

    tray.add();
    tray.add();
    tray.add();

    expect(tray.capture().a!.getAll("host")).toStrictEqual(["h1"]);
    expect(tray.capture().b).toBe(third);
  });

  it("swaps the two sides", () => {
    const a = scope({}, ["h1"]);
    const b = scope({}, ["h2"]);
    const { tray } = setup([a, b]);

    tray.add();
    tray.add();
    tray.swap();

    expect(tray.capture()).toStrictEqual({ a: b, b: a });
  });

  // Removing A promotes B rather than leaving a B-without-A hole, which the
  // link codec cannot represent.
  it("promotes B when A is removed", () => {
    const a = scope({}, ["h1"]);
    const b = scope({}, ["h2"]);
    const { tray } = setup([a, b]);

    tray.add();
    tray.add();
    tray.remove("a");

    expect(tray.capture()).toStrictEqual({ a: b, b: null });
  });

  it("clears both sides", () => {
    const { tray } = setup([scope({}, ["h1"]), scope({}, ["h2"])]);

    tray.add();
    tray.add();
    tray.clear();

    expect(tray.capture()).toStrictEqual({ a: null, b: null });
  });

  it("opens a diff link the diff view can parse back", () => {
    const a = scope({}, ["h1"]);
    const b = scope({}, ["h2"]);
    const { tray, openDiff } = setup([a, b]);

    tray.add();
    tray.add();
    tray.open();

    expect(openDiff).toHaveBeenCalledTimes(1);
    const parsed = decode(openDiff);
    expect(parsed.a.getAll("host")).toStrictEqual(["h1"]);
    expect(parsed.b.getAll("host")).toStrictEqual(["h2"]);
    // Both captured sides already carry api=1, so the diff needs no fix-up.
    expect(parsed.a.get("api")).toBe("1");
    expect(parsed.b.get("api")).toBe("1");
  });

  it("does nothing on open until both sides are captured", () => {
    const { tray, openDiff } = setup([scope()]);

    tray.open();
    tray.add();
    tray.open();

    expect(openDiff).not.toHaveBeenCalled();
  });
});

// The "Quick B" presets (#624): with A captured, derive B in one click
// instead of navigating a second view and capturing it by hand.
describe("flamegraph diff quick-B presets", () => {
  it("offers presets only while A is set and B is empty", () => {
    const { tray, last } = setup(
      [scope({}, ["h1"]), scope({}, ["h2"])],
      ["h1", "h2"],
    );

    // Nothing captured: nothing to derive from.
    expect(last().presets).toBeNull();

    tray.add();
    expect(last().presets).not.toBeNull();
    expect(last().presets!.otherHosts).toStrictEqual(["h2"]);
    expect(last().presets!.canTimeShift).toBe(true);

    // With B already chosen, deriving a new one would discard that choice.
    tray.add();
    expect(last().presets).toBeNull();

    // Dropping B brings the presets back.
    tray.remove("b");
    expect(last().presets).not.toBeNull();
  });

  it("tracks the host facet as it grows across snapshots", () => {
    const hosts: string[] = [];
    let next = 0;
    const scopes = [scope({}, ["h1"])];
    const models: TrayModel[] = [];
    const tray = createDiffTray({
      currentScope: () => scopes[Math.min(next++, scopes.length - 1)]!,
      openDiff: vi.fn(),
      knownHosts: () => hosts,
      render: (model) => models.push(model),
    });

    tray.add();
    expect(models[models.length - 1]!.presets!.otherHosts).toStrictEqual([]);

    // A later snapshot refreshes availability without changing the capture.
    hosts.push("h1", "h2");
    const captured = tray.capture();
    tray.refresh();
    expect(models[models.length - 1]!.presets!.otherHosts).toStrictEqual(["h2"]);
    expect(tray.capture()).toBe(captured);
  });

  it("opens A vs the same window on another host", () => {
    const { tray, openDiff } = setup([scope({}, ["h1"])], ["h1", "h2"]);

    tray.add();
    tray.applyPreset({ kind: "host", host: "h2" });

    const parsed = decode(openDiff);
    expect(parsed.a.getAll("host")).toStrictEqual(["h1"]);
    expect(parsed.b.getAll("host")).toStrictEqual(["h2"]);
    // Same window on both sides - that is what makes it a host comparison.
    expect(parsed.b.get("start_ns")).toBe(parsed.a.get("start_ns"));
    expect(parsed.b.get("end_ns")).toBe(parsed.a.get("end_ns"));
  });

  it("opens A vs the same scope an hour earlier", () => {
    const { tray, openDiff } = setup([scope({}, ["h1"])]);

    tray.add();
    tray.applyPreset({ kind: "shift", shift: "1h" });

    const parsed = decode(openDiff);
    const shift = BigInt(parsed.a.get("start_ns")!) - BigInt(parsed.b.get("start_ns")!);
    expect(shift).toBe(3_600_000_000_000n);
    expect(parsed.b.getAll("host")).toStrictEqual(["h1"]);
  });

  it("leaves the capture alone - a preset opens a link, it does not fill B", () => {
    const a = scope({}, ["h1"]);
    const { tray } = setup([a], ["h1", "h2"]);

    tray.add();
    tray.applyPreset({ kind: "host", host: "h2" });

    expect(tray.capture()).toStrictEqual({ a, b: null });
  });

  it("does nothing without an A to derive from", () => {
    const { tray, openDiff } = setup([scope({}, ["h1"])], ["h1", "h2"]);

    tray.applyPreset({ kind: "host", host: "h2" });

    expect(openDiff).not.toHaveBeenCalled();
  });

  it("does not bypass a captured B when a preset is invoked", () => {
    const { tray, openDiff } = setup([scope({}, ["h1"]), scope({}, ["h2"])]);
    tray.add();
    tray.add();
    tray.applyPreset({ kind: "shift", shift: "1h" });
    expect(openDiff).not.toHaveBeenCalled();
    expect(tray.capture().b!.getAll("host")).toStrictEqual(["h2"]);
  });
});
