// Tab bar component: Browse <-> Raw Search switching and the per-tab
// actions swap (Raw shows Select All/Deselect All; Browse shows the
// Flamegraph and Tokio Stats buttons).

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";

export function mountTabs({ store, els, actions }: PageCtx): void {
  els.tabBrowse.addEventListener("click", () => {
    actions.switchTab("browse");
  });
  els.tabRaw.addEventListener("click", () => {
    actions.switchTab("raw");
  });

  store.subscribe(["ui"], (state) => {
    assertInScheduledRender("tabs render");
    const tab = state.ui.tab;
    els.tabBrowse.classList.toggle("active", tab === "browse");
    els.tabRaw.classList.toggle("active", tab === "raw");
    els.browseView.style.display = tab === "browse" ? "" : "none";
    els.rawView.style.display = tab === "raw" ? "" : "none";
    // Raw mode has Select All/Deselect; browse mode the profiling buttons.
    els.rawActions.style.display = tab === "raw" ? "flex" : "none";
    els.cpuBtn.style.display = tab === "browse" ? "" : "none";
    els.healthBtn.style.display = tab === "browse" ? "" : "none";
  });
}
