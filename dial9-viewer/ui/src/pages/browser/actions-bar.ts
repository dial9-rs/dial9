// Actions bar: View Selected / Flamegraph / Tokio Stats buttons, the
// selection count and the size-cap warning, rendered from store state.

// Leaf heatmap seam, not the lib/canvas barrel (see actions.ts).
import { MAX_OPEN_BYTES } from "../../lib/canvas/heatmap.js";
import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { fmtTick, formatSize } from "./format.js";
import { effectiveProfileSelection } from "./open-links.js";

export function mountActionsBar({ store, els, actions }: PageCtx): void {
  els.viewBtn.addEventListener("click", () => {
    actions.viewSelected();
  });
  els.cpuBtn.addEventListener("click", () => {
    actions.viewCpuProfile();
  });
  els.healthBtn.addEventListener("click", () => {
    actions.viewTokioStats();
  });
  els.spansBtn.addEventListener("click", () => {
    actions.viewSpanExplorer();
  });

  store.subscribe(["browse", "raw", "ui", "config"], (state) => {
    assertInScheduledRender("actions-bar render");
    const agg = state.config.aggregationEnabled;

    if (state.ui.tab === "browse") {
      const sel = state.browse.selection;
      const profileSel = effectiveProfileSelection(state.browse);
      if (!profileSel) {
        els.viewBtn.disabled = true;
        els.cpuBtn.disabled = true;
        els.healthBtn.disabled = true;
        els.spansBtn.disabled = true;
        els.selectionWarn.textContent = "";
        els.selectionCount.textContent = "";
        return;
      }
      // Selection size cap (MAX_OPEN_BYTES). Flamegraph is exempt in
      // aggregation mode (the server samples; no client decode) and Tokio
      // Stats is never capped.
      const over = profileSel.bytes > MAX_OPEN_BYTES;
      els.viewBtn.disabled = !sel || over;
      els.cpuBtn.disabled = over && !agg;
      els.healthBtn.disabled = !agg;
      // Span Explorer is aggregate-only: /api/span-stats is the only source of
      // span statistics, and it never client-decodes.
      els.spansBtn.disabled = !agg;
      els.selectionWarn.textContent =
        over && !agg
          ? `Too large to open (${formatSize(profileSel.bytes)} > ${formatSize(MAX_OPEN_BYTES)}) — narrow your selection.`
          : "";
      const tz = state.ui.useLocalTz;
      const win =
        profileSel.t0 && profileSel.t1
          ? ` · ${fmtTick(profileSel.t0, tz)}–${fmtTick(profileSel.t1, tz)}`
          : "";
      const scopeLabel = sel ? "" : "Current service · ";
      els.selectionCount.textContent = `${scopeLabel}${profileSel.keys.length} segment${profileSel.keys.length !== 1 ? "s" : ""} · ${formatSize(profileSel.bytes)}${win}`;
      return;
    }

    // Raw mode. The (hidden) Flamegraph / Tokio Stats / Spans buttons are
    // browse-selection territory; leave their disabled state untouched.
    const count = state.raw.selected.size;
    els.selectionWarn.textContent = "";
    els.selectionCount.textContent = count ? `${count} selected` : "";
    els.viewBtn.disabled = count === 0;
  });
}
