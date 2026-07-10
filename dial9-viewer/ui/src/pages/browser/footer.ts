// Footer component (features/01 A2-A4): the demo-trace button, the "Open
// Trace Viewer" link (static markup) and the whole-page drag-and-drop that
// opens a dropped .bin in the viewer. The relative viewer.html targets
// resolve against <base href="/"> exactly like the root-served legacy page.

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";

export function mountFooter({ store, els }: PageCtx): void {
  // A2: demo trace in a new tab.
  els.loadDemoBtn.addEventListener("click", () => {
    window.open("viewer.html?trace=demo-trace.bin", "_blank");
  });

  // A3: global drag-and-drop -> open viewer; footer border highlights
  // purple while dragging (transient channel).
  document.body.addEventListener("dragover", (e) => {
    e.preventDefault();
    store.update("transient", { footerDragActive: true });
  });
  document.body.addEventListener("dragleave", (e) => {
    const related = e.relatedTarget;
    if (!related || !(related instanceof Node) || !document.body.contains(related)) {
      store.update("transient", { footerDragActive: false });
    }
  });
  document.body.addEventListener("drop", (e) => {
    e.preventDefault();
    store.update("transient", { footerDragActive: false });
    const file = e.dataTransfer?.files[0];
    if (file) {
      const url = URL.createObjectURL(file);
      window.open("viewer.html?trace=" + encodeURIComponent(url), "_blank");
    }
  });

  store.subscribe(["transient"], (state) => {
    assertInScheduledRender("footer render");
    els.footerDrop.style.borderColor = state.transient.footerDragActive
      ? "#6c63ff"
      : "transparent";
  });
}
