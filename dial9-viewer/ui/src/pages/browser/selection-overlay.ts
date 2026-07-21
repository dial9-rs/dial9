// Selection overlay: the #heatmap-sel rectangle - live rubber band during a
// drag (transient channel), persistent selection box after - plus the
// selected host-label row highlights.
//
// MOUNT ORDER: after mountBrowseView. Both subscribe to `browse`; when a
// search rebuilds the label rows, the painter must have rebuilt them within
// the same flush before this component re-applies the `.sel` classes
// (subscribers run in registration order).

import { assertInScheduledRender } from "../../store/store.js";
import { ROW_H } from "./actions.js";
import type { PageCtx } from "./ctx.js";
import { timeToX } from "./format.js";

export function mountSelectionOverlay({ store, els }: PageCtx): void {
  store.subscribe(["browse", "transient"], (state) => {
    assertInScheduledRender("selection-overlay render");
    const sel = state.browse.selection;
    const drag = state.transient.drag;
    const selEl = els.heatmapSel;

    // Highlight the selected host-label rows.
    const labelRows = els.heatmapLabels.querySelectorAll(".row");
    labelRows.forEach((el, i) => {
      const inSel = sel && sel.rows && i >= sel.rows[0] && i <= sel.rows[1];
      el.classList.toggle("sel", !!inSel);
    });

    if (drag) {
      // In-progress drag rubber band. A zoom drag is purely horizontal, so
      // its band spans all rows.
      selEl.classList.toggle("zoom", drag.zooming);
      selEl.style.display = "block";
      const l = Math.min(drag.x0, drag.x1);
      const r = Math.max(drag.x0, drag.x1);
      selEl.style.left = l + "px";
      selEl.style.width = r - l + "px";
      if (drag.zooming) {
        selEl.style.top = "0px";
        selEl.style.height = state.browse.rows.length * ROW_H + "px";
      } else {
        const t = Math.min(drag.y0, drag.y1);
        const b = Math.max(drag.y0, drag.y1);
        selEl.style.top = t + "px";
        selEl.style.height = b - t + "px";
      }
      return;
    }

    selEl.classList.remove("zoom");
    if (!sel || !sel.keys.length) {
      selEl.style.display = "none";
      return;
    }
    if (sel.rows && state.browse.domain) {
      // Whole rows [r0,r1] x time span [t0,t1].
      const W =
        els.heatmapCanvas.clientWidth ||
        parseFloat(els.heatmapCanvas.style.width) ||
        1;
      const { tMin, tMax } = state.browse.domain;
      const x0 = timeToX(sel.t0, tMin, tMax, W);
      const x1 = timeToX(sel.t1, tMin, tMax, W);
      selEl.style.display = "block";
      selEl.style.left = x0 + "px";
      selEl.style.width = Math.max(2, x1 - x0) + "px";
      selEl.style.top = sel.rows[0] * ROW_H + "px";
      selEl.style.height = (sel.rows[1] - sel.rows[0] + 1) * ROW_H + "px";
    }
  });
}
