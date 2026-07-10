// Raw search view component (features/01 G): prefix input + search, the
// results table (columns parsed from the key), row/select-all checkboxes.
//
// G8 column sort is DEAD in the legacy page (headers carry data-sort
// markup and sort-arrow CSS but no click handler) and is ported AS-IS:
// no header click handler here. T15 owns the sortable-table amendment.

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { formatDate, formatEpochStr, formatSize } from "./format.js";
import { parseKeyCompat } from "./legacy-keys.js";
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
    const rows = objects.map((obj) => ({ obj, parsed: parseKeyCompat(obj.key) }));
    rows.sort((a, b) => a.parsed.epoch - b.parsed.epoch);

    for (const { obj, parsed: p } of rows) {
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
      cell(p.service, "service");
      cell(p.host, "host");
      cell(p.bootId || "", "host");
      cell(formatEpochStr(p.epoch, localTz));
      cell(p.segIndex);
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
