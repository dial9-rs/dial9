// Header chrome: the timezone toggle button (features/01 B3). The creds
// button lives in creds-panel.ts (it is part of the C-section component).

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";

export function mountHeader({ store, els, actions }: PageCtx): void {
  els.tzBtn.addEventListener("click", () => {
    actions.toggleTz();
  });

  store.subscribe(["ui"], (state) => {
    assertInScheduledRender("header render");
    els.tzBtn.textContent = state.ui.useLocalTz ? "TZ: Local" : "TZ: UTC";
  });
}
