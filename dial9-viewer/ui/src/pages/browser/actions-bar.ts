// Actions bar component (features/01 H): View Selected / Flamegraph /
// Tokio Stats buttons, the selection count and the size-cap warning.
// The render is the legacy updateSelectionCount (index.html:1888-1922)
// as a pure function of store state.

import { MAX_OPEN_BYTES } from "../../lib/canvas/index.js";
import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { fmtTick, formatSize } from "./format.js";

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

  store.subscribe(["browse", "raw", "ui", "config"], (state) => {
    assertInScheduledRender("actions-bar render");
    const agg = state.config.aggregationEnabled;

    if (state.ui.tab === "browse") {
      const sel = state.browse.selection;
      if (!sel || !sel.keys.length) {
        els.viewBtn.disabled = true;
        els.cpuBtn.disabled = true;
        els.healthBtn.disabled = true;
        els.selectionWarn.textContent = "";
        els.selectionCount.textContent = "";
        return;
      }
      // H4: the selection size cap (MAX_OPEN_BYTES; raised to 200 MB by
      // #600). Since #570, Flamegraph is exempt in aggregation mode (the
      // server samples; no client decode) and Tokio Stats is never capped.
      const over = sel.bytes > MAX_OPEN_BYTES;
      els.viewBtn.disabled = over;
      els.cpuBtn.disabled = over && !agg;
      els.healthBtn.disabled = !agg;
      els.selectionWarn.textContent =
        over && !agg
          ? `Too large to open (${formatSize(sel.bytes)} > ${formatSize(MAX_OPEN_BYTES)}) — narrow your selection.`
          : "";
      const tz = state.ui.useLocalTz;
      const win =
        sel.t0 && sel.t1 ? ` · ${fmtTick(sel.t0, tz)}–${fmtTick(sel.t1, tz)}` : "";
      els.selectionCount.textContent = `${sel.keys.length} segment${sel.keys.length !== 1 ? "s" : ""} · ${formatSize(sel.bytes)}${win}`;
      return;
    }

    // Raw mode. The legacy branch never touched the (hidden) Flamegraph /
    // Tokio Stats buttons here; their disabled state is browse-selection
    // territory and is left untouched.
    const count = state.raw.selected.size;
    els.selectionWarn.textContent = "";
    els.selectionCount.textContent = count ? `${count} selected` : "";
    els.viewBtn.disabled = count === 0;
  });
}
