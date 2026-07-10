// Raw search view component (features/01 G): prefix input + search, the
// results table (columns parsed from the key), row/select-all checkboxes.
//
// T15 amendments live here:
// - I2 display: unknown-layout keys render RAW - the full key across the
//   Service/Host/Boot columns (raw-rows.ts) - instead of the legacy
//   positionally shifted fields (Finding 1).

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { formatDate, formatEpochStr, formatSize } from "./format.js";
import { toRawRows } from "./raw-rows.js";
import type { BrowseObject } from "./state.js";
import { renderStatus } from "./status-render.js";

export function mountRawView({ store, els, actions }: PageCtx): void {
  // G2: Enter in the prefix field triggers the search.
  els.rawSearchInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") void actions.doRawSearch();
  });
  // A6: the raw query rides the URL as `q`.
  els.rawSearchInput.addEventListener("input", () => {
    actions.syncUrl();
  });
  els.rawSearchBtn.addEventListener("click", () => {
    void actions.doRawSearch();
  });

  // G5: select-all header checkbox toggles all row checkboxes.
  els.rawSelectAll.addEventListener("change", () => {
    actions.rawSelectAll(els.rawSelectAll.checked);
  });
  // G6: Select All / Deselect All actions-bar buttons (Raw tab only).
  els.rawSelectAllBtn.addEventListener("click", () => {
    actions.rawSelectAll(true);
  });
  els.rawDeselectAllBtn.addEventListener("click", () => {
    actions.rawSelectAll(false);
  });

  // G3: rebuild the table body. Rows sorted by trace-start epoch; every
  // rebuild starts unchecked (legacy renderRawTable semantics - a TZ
  // toggle drops the selection).
  function rebuildRows(objects: readonly BrowseObject[], localTz: boolean): void {
    els.rawBody.textContent = "";

    for (const row of toRawRows(objects)) {
      const { obj } = row;
      const tr = document.createElement("tr");

      const cbCell = document.createElement("td");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.className = "raw-cb";
      cb.dataset["key"] = obj.key;
      cb.addEventListener("change", () => {
        actions.syncRawSelectionFromDom();
      });
      cbCell.appendChild(cb);
      tr.appendChild(cbCell);

      const cell = (text: string, className?: string) => {
        const td = document.createElement("td");
        if (className) td.className = className;
        td.textContent = text;
        tr.appendChild(td);
      };
      if (row.parsedCols) {
        cell(row.parsedCols.service, "service");
        cell(row.parsedCols.host, "host");
        cell(row.parsedCols.bootId || "", "host");
      } else {
        // I2 amendment: unknown-layout key - show the raw key across the
        // Service/Host/Boot columns instead of guessed (shifted) fields.
        const td = document.createElement("td");
        td.className = "rawkey";
        td.colSpan = 3;
        td.textContent = obj.key;
        td.title = "unrecognized key layout - raw key shown";
        tr.appendChild(td);
      }
      cell(formatEpochStr(row.epoch, localTz));
      cell(row.segIndex);
      cell(formatSize(obj.size), "size");
      cell(formatDate(obj.last_modified, localTz), "date");

      els.rawBody.appendChild(tr);
    }
  }

  let lastEpoch = -1;
  store.subscribe(["raw", "ui"], (state) => {
    assertInScheduledRender("raw-view render");
    renderStatus(els.rawStatus, state.raw.status);
    els.rawTable.style.display = state.raw.tableVisible ? "" : "none";
    // Rebuild only when a search / TZ toggle bumped the render epoch;
    // selection mirror changes must NOT rebuild (they would wipe the
    // checkboxes the user just clicked).
    if (state.raw.renderEpoch !== lastEpoch) {
      lastEpoch = state.raw.renderEpoch;
      rebuildRows(state.raw.objects, state.ui.useLocalTz);
    }
  });
}
