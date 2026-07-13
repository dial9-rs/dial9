// Header chrome: the timezone toggle button. The creds button lives in
// creds-panel.ts.

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
