// Differential-comparison tray (#623): capture two heatmap selections as A/B
// aggregate scopes, then launch a two-sided diff in the flamegraph or
// tokio-stats view. The tray DOM is built here (the static page markup is
// frozen) and appended next to the actions bar; capture state lives in the
// store's `diff` slice, so this component only renders from it and dispatches
// the diff verbs on click.
//
// With A captured and B still open, the tray also offers the "Quick B"
// presets (#624) that derive B from A - the same window on another host in
// the browse view, or the same scope 1h/24h/7d earlier. They fill B in the
// tray rather than launching, so the user still picks the comparison view.

import {
  SHIFT_KEYS,
  presetAvailability,
  shiftLabel,
  type Preset,
} from "../../lib/canvas/diff-presets.js";
import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { fmtTick } from "./format.js";
import type { HeatmapRow } from "./state.js";

// A row of theme-matched inline styles kept local to the tray because the
// page's CSS is frozen.
const CHIP_A = { bg: "#1e3050", fg: "#9ec1ff" };
const CHIP_B = { bg: "#4a221c", fg: "#ffb3a0" };
const ACCENT = "#6c63ff";

function button(label: string, onClick: () => void, style: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = label;
  b.style.cssText = style;
  b.addEventListener("click", onClick);
  return b;
}

/** A captured scope as a one-line human summary for the tray's A/B rows. */
function summarize(scope: URLSearchParams, localTz: boolean): string {
  const bits: string[] = [];
  const svc = scope.get("service");
  if (svc) bits.push(svc);
  const hosts = scope.getAll("host");
  if (hosts.length === 1) bits.push(hosts[0]!);
  else if (hosts.length > 1) bits.push(`${hosts.length} hosts`);
  else bits.push("all hosts");
  const s = scope.get("start_ns");
  const e = scope.get("end_ns");
  if (s && e) {
    const t0 = Number(s) / 1e9;
    const t1 = Number(e) / 1e9;
    bits.push(`${fmtTick(t0, localTz, true)} · ${formatDuration(t1 - t0)}`);
  }
  const bucket = scope.get("bucket");
  if (bucket) bits.push(bucket);
  return bits.join(" · ") || "(empty scope)";
}

/** One option in the "different host" preset dropdown. */
export interface HostOption {
  /** The host to scope side B to. */
  value: string;
  /** "service / host" where the service is known, else the bare host. */
  label: string;
}

/**
 * Hosts offerable as side B, labelled from the browse view's rows.
 *
 * Rows are the hosts currently on screen, which is exactly the set the user
 * could otherwise select and capture by hand. Availability itself comes from
 * the shared preset rules, so this tray and the flamegraph's agree.
 */
export function presetHostOptions(
  scopeA: URLSearchParams,
  rows: readonly HeatmapRow[],
): HostOption[] {
  const services = new Map<string, string>();
  const serviceA = scopeA.get("service");
  for (const row of rows) {
    // A host preset preserves A's service filter; other services would yield
    // an empty comparison even though that host has data in the browse view.
    if (serviceA && row.service !== serviceA) continue;
    if (row.host && !services.has(row.host)) services.set(row.host, row.service);
  }
  const { otherHosts } = presetAvailability(scopeA, services.keys());
  return otherHosts.map((host) => {
    const service = services.get(host);
    return { value: host, label: service ? `${service} / ${host}` : host };
  });
}

function formatDuration(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${(seconds / 60).toFixed(1)}m`;
  return `${(seconds / 3600).toFixed(1)}h`;
}

export function mountDiffTray({ store, els, actions }: PageCtx): void {
  // "Add to diff" button, placed after the existing action buttons.
  const addBtn = button("➕ Add to diff", () => actions.addToDiff(), "");
  addBtn.title = "Capture this selection as one side (A, then B) of a diff";
  addBtn.disabled = true;
  els.actionsBar.insertBefore(addBtn, els.selectionWarn);

  // Tray shell.
  const tray = document.createElement("div");
  tray.style.cssText =
    `display:none;margin:0 20px 12px;padding:10px 14px;background:#161b2e;` +
    `border:1px solid ${ACCENT};border-radius:8px;font-size:0.85em`;

  const header = document.createElement("div");
  header.style.cssText = "display:flex;align-items:center;gap:10px;margin-bottom:8px";
  const title = document.createElement("b");
  title.textContent = "Diff";
  title.style.color = "#c7c2ff";
  const legend = document.createElement("span");
  legend.textContent = "blue = heavier in A · red = heavier in B";
  legend.style.color = "#8b949e";
  const spacer = document.createElement("span");
  spacer.style.flex = "1";
  const ghost =
    "background:#2a2a44;color:#e0e0e0;border:1px solid #444;padding:3px 10px;" +
    "border-radius:3px;cursor:pointer";
  const swapBtn = button("⇄ Swap", () => actions.swapDiff(), ghost);
  swapBtn.title = "Swap A and B";
  const clearBtn = button("Clear", () => actions.clearDiff(), ghost);
  clearBtn.title = "Clear both sides";
  header.append(title, legend, spacer, swapBtn, clearBtn);

  const sides = document.createElement("div");
  sides.style.cssText = "display:grid;grid-template-columns:1fr 1fr;gap:10px";
  const cellA = makeSideCell("A · left", CHIP_A, () => actions.clearDiffSide("a"));
  const cellB = makeSideCell("B · right", CHIP_B, () => actions.clearDiffSide("b"));
  sides.append(cellA.root, cellB.root);

  const launch = document.createElement("div");
  launch.style.cssText = "display:none;margin-top:10px;align-items:center;gap:8px";
  const launchLabel = document.createElement("span");
  launchLabel.textContent = "Compare in:";
  launchLabel.style.color = "#8b949e";
  const launchStyle =
    `background:${ACCENT};color:#fff;border:none;padding:4px 14px;border-radius:3px;` +
    "cursor:pointer;font-weight:600";
  launch.append(
    launchLabel,
    button("🔥 Flamegraph", () => actions.launchDiff("flamegraph"), launchStyle),
    button("⚡ Tokio Stats", () => actions.launchDiff("tokio"), launchStyle),
  );

  // "Quick B" presets (#624), between the sides and the launch row: they are
  // a way to finish filling the capture, so they read before "Compare in:".
  const presets = document.createElement("div");
  presets.style.cssText =
    "display:none;margin-top:10px;align-items:center;gap:8px;flex-wrap:wrap";
  const presetLabel = document.createElement("span");
  presetLabel.textContent = "Quick B:";
  presetLabel.style.color = "#8b949e";

  const applyPreset = (preset: Preset): void => {
    actions.applyDiffPreset(preset);
  };
  const shiftBtns = SHIFT_KEYS.map((shift) => {
    const b = button(shiftLabel(shift), () => applyPreset({ kind: "shift", shift }), ghost);
    b.title = `Side B = side A, the equivalent window ${shift} earlier`;
    return b;
  });

  const hostSelect = document.createElement("select");
  hostSelect.style.cssText =
    "background:#14142a;color:#e0e0e0;border:1px solid #444;padding:3px 8px;border-radius:3px";
  hostSelect.title = "Side B = side A on a different host, same time window";
  hostSelect.addEventListener("change", () => {
    const host = hostSelect.value;
    // Back to the prompt so the same host can be re-picked after a Clear.
    hostSelect.value = "";
    if (host) applyPreset({ kind: "host", host });
  });

  presets.append(presetLabel, ...shiftBtns, hostSelect);

  /** Enable a control and match its affordance to that state. */
  const setEnabled = (el: HTMLButtonElement | HTMLSelectElement, on: boolean): void => {
    el.disabled = !on;
    el.style.opacity = on ? "1" : "0.4";
    el.style.cursor = on ? "pointer" : "not-allowed";
  };

  tray.append(header, sides, presets, launch);
  els.actionsBar.after(tray);

  store.subscribe(["diff", "browse", "ui"], (state) => {
    assertInScheduledRender("diff-tray render");
    const { a, b } = state.diff;
    const localTz = state.ui.useLocalTz;

    const sel = state.browse.selection;
    addBtn.disabled = state.ui.tab !== "browse" || !sel || !sel.keys.length;

    tray.style.display = a || b ? "" : "none";
    cellA.render(a ? summarize(a, localTz) : null);
    cellB.render(b ? summarize(b, localTz) : null);
    launch.style.display = a && b ? "flex" : "none";
    swapBtn.disabled = !(a && b);

    // Presets derive B from A, so they only apply with A set and B open.
    presets.style.display = a && !b ? "flex" : "none";
    if (a && !b) {
      const options = presetHostOptions(a, state.browse.rows);
      hostSelect.textContent = "";
      const prompt = document.createElement("option");
      prompt.value = "";
      prompt.textContent = options.length
        ? "same time, different host…"
        : "no other host in view";
      hostSelect.append(prompt);
      for (const option of options) {
        const el = document.createElement("option");
        el.value = option.value;
        // textContent, never innerHTML: service/host names are remote data.
        el.textContent = option.label;
        hostSelect.append(el);
      }
      setEnabled(hostSelect, options.length > 0);

      const canShift = presetAvailability(a, []).canTimeShift;
      for (const btn of shiftBtns) setEnabled(btn, canShift);
    }
  });
}

interface SideCell {
  root: HTMLDivElement;
  /** `summary` null => the "not captured yet" placeholder. */
  render(summary: string | null): void;
}

function makeSideCell(
  label: string,
  chip: { bg: string; fg: string },
  onRemove: () => void,
): SideCell {
  const root = document.createElement("div");
  root.style.cssText =
    "background:#14142a;border:1px solid #333;border-radius:6px;padding:8px 10px;min-width:0";

  const head = document.createElement("div");
  head.style.cssText = "display:flex;align-items:center;gap:8px";
  const badge = document.createElement("span");
  badge.textContent = label;
  badge.style.cssText =
    `font-size:0.72em;font-weight:700;padding:2px 8px;border-radius:10px;` +
    `background:${chip.bg};color:${chip.fg}`;
  const headSpacer = document.createElement("span");
  headSpacer.style.flex = "1";
  const removeBtn = button("✕", onRemove, "background:none;border:none;color:#888;cursor:pointer;font-size:1em");
  removeBtn.title = `Remove ${label}`;
  head.append(badge, headSpacer, removeBtn);

  // textContent (never innerHTML) so host/service names can never inject markup.
  const body = document.createElement("div");
  body.style.cssText = "color:#ccc;margin-top:6px;overflow-wrap:anywhere";
  root.append(head, body);

  return {
    root,
    render(summary) {
      const set = summary != null;
      root.style.borderColor = set ? chip.fg : "#333";
      removeBtn.style.display = set ? "" : "none";
      if (set) {
        body.textContent = summary;
        body.style.color = "#ccc";
      } else {
        body.textContent = 'select a region and click "Add to diff"';
        body.style.color = "#666";
      }
    },
  };
}
