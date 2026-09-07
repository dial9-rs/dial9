// In-page "Add to diff" capture tray for the aggregated flamegraph (#646).
//
// Capture two scopes (A/B) straight from this page's live view - respecting
// the Host dropdown and every other facet - and open a two-sided diff, so a
// host-vs-host comparison needs no second tab and no Copy-link paste.
//
// The capture transitions live in the frozen core (addDiffCapture /
// swapDiffCapture / removeDiffSide), which owns the "A fills before B"
// invariant. This module adds the summary text, the view model and the DOM;
// the first three are pure so the tray's behavior is testable without a
// browser, and `mountDiffTray` is the thin DOM assembly over them.
//
// With A captured and B still open, the tray also offers the one-click "Quick
// B" presets (#624) that derive B from A - see lib/canvas/diff-presets.ts.
//
// The tray is inserted AFTER the filter toolbar rather than inside #f-facets,
// which renderFacets rebuilds on every streamed snapshot - a tray in there
// would be destroyed mid-capture.

import {
  SHIFT_KEYS,
  presetAvailability,
  presetScope,
  shiftLabel,
  type Preset,
  type PresetAvailability,
} from "../../lib/canvas/diff-presets.js";
import {
  addDiffCapture,
  diffSearch,
  removeDiffSide,
  swapDiffCapture,
} from "../../lib/canvas/flamegraph_diff.js";
import { formatHumanDuration } from "../../lib/trace/index.js";

/** A captured pair; either side may be empty, but B never fills before A. */
export interface DiffCapture {
  a: URLSearchParams | null;
  b: URLSearchParams | null;
}

/**
 * One captured scope as a one-line human summary: what was compared, over
 * which window. Deliberately says "1 host" rather than the host name - the
 * host is already visible in the toolbar dropdown, and a long hostname would
 * crowd out the window, which is the part a reader cannot otherwise recover.
 */
export function summarizeScope(scope: URLSearchParams | null): string {
  if (!scope) return "—";
  const bits: string[] = [];
  const service = scope.get("service");
  if (service) bits.push(service);
  const hosts = scope.getAll("host");
  if (hosts.length === 1) bits.push("1 host");
  else if (hosts.length > 1) bits.push(`${hosts.length} hosts`);
  const threadClass = scope.get("thread_class");
  if (threadClass) bits.push(threadClass);
  const source = scope.get("source");
  // "cpu" is the default source and carries no information.
  if (source && source !== "cpu") bits.push(source);
  const startNs = scope.get("start_ns");
  const endNs = scope.get("end_ns");
  if (startNs && endNs) {
    const from = new Date(Number(startNs) / 1e6)
      .toISOString()
      .slice(5, 16)
      .replace("T", " ");
    bits.push(`${from} UTC · ${formatHumanDuration(Number(endNs) - Number(startNs))}`);
  }
  const bucket = scope.get("bucket");
  if (bucket) bits.push(bucket);
  return bits.join(" · ") || "(empty scope)";
}

/** Everything the tray's chrome renders, derived from the capture. */
export interface TrayModel {
  /** The tray is chrome for a capture in progress; hidden when there is none. */
  visible: boolean;
  /** Side summary, or null for the "not captured yet" placeholder. */
  a: string | null;
  b: string | null;
  /** Swapping a lone side would leave A empty, so it needs both. */
  canSwap: boolean;
  /** A diff needs both sides. */
  canOpen: boolean;
  /**
   * The "Quick B" presets (#624), or null when they do not apply. They only
   * make sense with A captured and B still empty: with B already picked,
   * deriving a new one would silently discard the user's choice.
   */
  presets: PresetAvailability | null;
}

export function trayModel(
  capture: DiffCapture,
  knownHosts: Iterable<string> = [],
): TrayModel {
  const both = !!(capture.a && capture.b);
  return {
    visible: !!(capture.a || capture.b),
    a: capture.a ? summarizeScope(capture.a) : null,
    b: capture.b ? summarizeScope(capture.b) : null,
    canSwap: both,
    canOpen: both,
    presets:
      capture.a && !capture.b ? presetAvailability(capture.a, knownHosts) : null,
  };
}

export interface DiffTrayDeps {
  /** This view's full aggregate scope, read fresh at each capture. */
  currentScope(): URLSearchParams;
  /** Open a two-sided diff for the given query string. */
  openDiff(search: string): void;
  /** Paint the tray chrome. Called once at creation and after every change. */
  render(model: TrayModel): void;
  /**
   * Hosts this page knows about, for the "different host" preset. Read fresh
   * on each render: the facet set grows as snapshots stream in.
   */
  knownHosts?(): Iterable<string>;
}

export interface DiffTray {
  /** Capture the current view into the next free side (A, then B). */
  add(): void;
  swap(): void;
  clear(): void;
  remove(side: "a" | "b"): void;
  /** Open the A-vs-B diff; a no-op unless both sides are captured. */
  open(): void;
  /**
   * Derive side B from side A and open the diff directly (#624). A no-op
   * unless A is captured - there is nothing to derive from otherwise.
   */
  applyPreset(preset: Preset): void;
  capture(): DiffCapture;
}

export function createDiffTray(deps: DiffTrayDeps): DiffTray {
  let capture: DiffCapture = { a: null, b: null };

  function render(): void {
    deps.render(trayModel(capture, deps.knownHosts?.() ?? []));
  }

  function set(next: DiffCapture): void {
    capture = next;
    render();
  }

  const tray: DiffTray = {
    add: () => set(addDiffCapture(capture, deps.currentScope())),
    swap: () => set(swapDiffCapture(capture)),
    clear: () => set({ a: null, b: null }),
    remove: (side) => set(removeDiffSide(capture, side)),
    open: () => {
      const { a, b } = capture;
      if (!a || !b) return;
      // Both captured scopes already carry `api=1` (they come from
      // fullScopeQuery over the live query), so no per-side flag fix-up.
      deps.openDiff(diffSearch(a, b));
    },
    applyPreset: (preset) => {
      const a = capture.a;
      if (!a) return;
      // A stays side A; the derived scope is B. Opened directly rather than
      // parked in the tray - a one-click preset is the whole point.
      deps.openDiff(diffSearch(a, presetScope(a, preset)));
    },
    capture: () => capture,
  };
  render();
  return tray;
}

// ── DOM ──

const CHIP_A = { bg: "#1e3050", fg: "#9ec1ff" };
const CHIP_B = { bg: "#4a221c", fg: "#ffb3a0" };
const GHOST =
  "background:#2a2a4a;color:#e0e0e0;border:1px solid #444;padding:4px 14px;" +
  "border-radius:3px;font-weight:600";

function button(label: string, title: string, onClick: () => void, style: string) {
  const b = document.createElement("button");
  b.textContent = label;
  b.title = title;
  b.style.cssText = style;
  b.addEventListener("click", onClick);
  return b;
}

/** Enable/disable a button and match its affordance to that state. */
function setEnabled(b: HTMLButtonElement, enabled: boolean): void {
  b.disabled = !enabled;
  b.style.opacity = enabled ? "1" : "0.4";
  b.style.cursor = enabled ? "pointer" : "not-allowed";
}

interface SideCell {
  root: HTMLDivElement;
  render(summary: string | null): void;
}

function makeSideCell(
  label: string,
  chip: { bg: string; fg: string },
  onRemove: () => void,
): SideCell {
  const root = document.createElement("div");
  root.style.cssText =
    "background:#1a1a2e;border:1px solid #333;border-radius:6px;padding:6px 10px;min-width:0";

  const head = document.createElement("div");
  head.style.cssText = "display:flex;align-items:center;gap:8px";
  const badge = document.createElement("span");
  badge.textContent = label;
  badge.style.cssText =
    "font-size:0.85em;font-weight:700;padding:2px 8px;border-radius:10px;" +
    `background:${chip.bg};color:${chip.fg}`;
  const spacer = document.createElement("span");
  spacer.style.flex = "1";
  const removeBtn = button(
    "✕",
    `Remove ${label}`,
    onRemove,
    "background:none;border:none;color:#888;cursor:pointer;font-size:1em",
  );
  head.append(badge, spacer, removeBtn);

  // textContent, never innerHTML: a scope carries user-supplied service,
  // bucket and host names, which must never be parsed as markup.
  const body = document.createElement("div");
  body.style.cssText = "color:#ccc;margin-top:4px;overflow-wrap:anywhere";
  root.append(head, body);

  return {
    root,
    render(summary) {
      const set = summary != null;
      root.style.borderColor = set ? chip.fg : "#333";
      removeBtn.style.display = set ? "" : "none";
      body.textContent = set ? summary : 'pick a host/scope, then click "Add to diff"';
      body.style.color = set ? "#ccc" : "#666";
    },
  };
}

/**
 * The "Quick B" preset row (#624): with A captured and B still open, derive
 * B from A in one click - the same window on another host, or the same scope
 * one hour / day / week earlier.
 *
 * Rebuilt host options on every render because the host facet grows as
 * snapshots stream in.
 */
interface PresetRow {
  root: HTMLDivElement;
  render(available: PresetAvailability | null): void;
}

function makePresetRow(onPreset: (preset: Preset) => void): PresetRow {
  const root = document.createElement("div");
  root.style.cssText = "display:none;align-items:center;gap:8px;flex-wrap:wrap";

  const label = document.createElement("span");
  label.textContent = "Quick B:";
  label.title = "Derive side B from side A without capturing it by hand";
  label.style.cssText = "color:#888;font-weight:600";

  const hostSelect = document.createElement("select");
  hostSelect.style.cssText =
    "background:#1a1a2e;color:#e0e0e0;border:1px solid #444;padding:2px 6px;border-radius:3px";
  hostSelect.title = "Same window, a different host";
  hostSelect.addEventListener("change", () => {
    const host = hostSelect.value;
    // Reset to the prompt so re-picking the same host fires again.
    hostSelect.value = "";
    if (host) onPreset({ kind: "host", host });
  });

  // Keep each button's enabled title alongside it: the disabled state swaps
  // in an explanatory tooltip, and reading the live `title` back to restore
  // it would latch the explanation permanently.
  const shiftButtons = SHIFT_KEYS.map((shift) => {
    const title = `Same scope, the equivalent window ${shift} earlier`;
    return {
      el: button(
        shiftLabel(shift),
        title,
        () => onPreset({ kind: "shift", shift }),
        GHOST + ";padding:2px 10px",
      ),
      title,
    };
  });

  root.append(label, hostSelect, ...shiftButtons.map((b) => b.el));

  return {
    root,
    render(available) {
      root.style.display = available ? "flex" : "none";
      if (!available) return;

      hostSelect.textContent = "";
      const prompt = document.createElement("option");
      prompt.value = "";
      prompt.textContent = available.otherHosts.length
        ? "different host…"
        : "no other host";
      hostSelect.append(prompt);
      for (const host of available.otherHosts) {
        const option = document.createElement("option");
        option.value = host;
        // textContent, never innerHTML: host names are remote data.
        option.textContent = host;
        hostSelect.append(option);
      }
      hostSelect.disabled = available.otherHosts.length === 0;
      hostSelect.style.opacity = available.otherHosts.length ? "1" : "0.4";

      for (const b of shiftButtons) {
        setEnabled(b.el, available.canTimeShift);
        b.el.title = available.canTimeShift
          ? b.title
          : "This scope has no time window to shift";
      }
    },
  };
}

export interface MountDiffTrayOptions {
  /** The tray is inserted after this element (the filter toolbar). */
  anchor: Element;
  /** The toolbar's "+ Add to diff" button. */
  addButton: HTMLButtonElement;
  currentScope(): URLSearchParams;
  /** Hosts this page knows about, for the "different host" preset. */
  knownHosts?(): Iterable<string>;
}

/** Build the tray chrome, wire it to a capture, and return the capture. */
export function mountDiffTray(opts: MountDiffTrayOptions): DiffTray {
  const root = document.createElement("div");
  root.style.cssText =
    "display:none;align-items:center;gap:12px;padding:6px 12px;background:#14142a;" +
    "border-bottom:1px solid #333;font-size:0.8em;flex-shrink:0;flex-wrap:wrap";

  const title = document.createElement("span");
  title.textContent = "Diff capture:";
  title.style.cssText = "color:#888;font-weight:600";

  // Declared before the cells so their remove handlers can reach it.
  let tray: DiffTray;
  const cellA = makeSideCell("A · left", CHIP_A, () => {
    tray.remove("a");
  });
  const cellB = makeSideCell("B · right", CHIP_B, () => {
    tray.remove("b");
  });

  const swapBtn = button(
    "⇄ Swap",
    "Swap which capture is A vs B",
    () => {
      tray.swap();
    },
    GHOST,
  );
  const openBtn = button(
    "Open diff",
    "Open a two-sided diff of A vs B",
    () => {
      tray.open();
    },
    "background:#6c63ff;color:#fff;border:none;padding:4px 14px;border-radius:3px;font-weight:600",
  );
  const clearBtn = button(
    "Clear",
    "Clear both captured sides",
    () => {
      tray.clear();
    },
    GHOST + ";cursor:pointer",
  );

  const presetRow = makePresetRow((preset) => {
    tray.applyPreset(preset);
  });

  root.append(title, cellA.root, cellB.root, swapBtn, openBtn, clearBtn, presetRow.root);
  opts.anchor.after(root);

  tray = createDiffTray({
    currentScope: opts.currentScope,
    ...(opts.knownHosts ? { knownHosts: opts.knownHosts } : {}),
    openDiff: (search) => {
      window.open(window.location.pathname + "?" + search, "_blank");
    },
    render: (model) => {
      root.style.display = model.visible ? "flex" : "none";
      cellA.render(model.a);
      cellB.render(model.b);
      setEnabled(swapBtn, model.canSwap);
      setEnabled(openBtn, model.canOpen);
      presetRow.render(model.presets);
    },
  });

  opts.addButton.addEventListener("click", () => {
    tray.add();
  });
  return tray;
}
