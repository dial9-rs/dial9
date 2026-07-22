// Differential-comparison tray (#623): capture two heatmap selections as A/B
// aggregate scopes, then launch a two-sided diff in the flamegraph or
// tokio-stats view. The tray DOM is built here (the static page markup is
// frozen) and appended next to the actions bar; capture state lives in the
// store's `diff` slice, so this component only renders from it and dispatches
// the diff verbs on click.

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { fmtTick } from "./format.js";

// A row of theme-matched inline styles kept local to the tray (the page's CSS
// is frozen, so the tray styles itself the way the legacy markup did).
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

  tray.append(header, sides, launch);
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
