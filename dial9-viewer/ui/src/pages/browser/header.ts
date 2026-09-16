// Header chrome: the timezone toggle button, and the labels that name the
// active zone for values the button cannot annotate. The creds button lives
// in creds-panel.ts.

import { assertInScheduledRender } from "../../store/store.js";
import { tzName } from "./format.js";
import type { PageCtx } from "./ctx.js";

export function mountHeader({ store, els, actions }: PageCtx): void {
  els.tzBtn.addEventListener("click", () => {
    actions.toggleTz();
  });

  store.subscribe(["ui"], (state) => {
    assertInScheduledRender("header render");
    const zone = tzName(state.ui.useLocalTz);
    els.tzBtn.textContent = `TZ: ${zone}`;
    // The range pickers are native datetime-local inputs and the table's date
    // columns repeat per row, so both name the zone on their label instead.
    els.rangeFromLabel.textContent = `From (${zone}):`;
    els.rangeToLabel.textContent = `To (${zone}):`;
    els.thTraceStart.textContent = `Trace Start (${zone})`;
    els.thUploaded.textContent = `Uploaded (${zone})`;
  });
}
