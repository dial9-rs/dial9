// Raw search view component (features/01 G): prefix input + search, the
// results table (columns parsed from the key), row/select-all checkboxes.
//
// T15 amendments live here:
// - I2 display: unknown-layout keys render RAW - the full key across the
//   Service/Host/Boot columns (raw-rows.ts) - instead of the legacy
//   positionally shifted fields (Finding 1).
// - G8: the sortable columns the legacy page advertised (data-sort markup,
//   sort-arrow CSS, pointer cursor) but never wired now actually sort:
//   click a header to sort (asc), click again to flip. Numeric for Trace
//   Start / Seg # / Size, lexical otherwise (raw-rows.ts). A sort rebuild
//   preserves the checkbox selection (unlike a search/TZ rebuild, whose
//   selection drop is recorded legacy behavior).

import { assertInScheduledRender } from "../../store/store.js";
import type { PageCtx } from "./ctx.js";
import { formatDate, formatEpochStr, formatSize } from "./format.js";
import {
  nextSort,
  sortRawRows,
  toRawRows,
  type RawSort,
  type RawSortKey,
} from "./raw-rows.js";
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

  // G8 (T15 amendment): sortable column headers.
  const sortHeaders = [
    ...els.rawTable.querySelectorAll<HTMLTableCellElement>("th[data-sort]"),
  ];
  for (const th of sortHeaders) {
    th.addEventListener("click", () => {
      const key = th.dataset["sort"] as RawSortKey;
      store.update("raw", { sort: nextSort(store.getState().raw.sort, key) });
    });
  }

  // Direction indicator (the .sort-arrow slot the legacy CSS always styled)
  // plus aria-sort on the active header.
  function renderSortIndicators(sort: RawSort | null): void {
    for (const th of sortHeaders) {
      const active = sort !== null && th.dataset["sort"] === sort.key;
      let arrow = th.querySelector<HTMLSpanElement>(".sort-arrow");
      if (active) {
        if (!arrow) {
          arrow = document.createElement("span");
          arrow.className = "sort-arrow";
          th.appendChild(arrow);
        }
        arrow.textContent = sort.dir === 1 ? "^" : "v";
        th.setAttribute("aria-sort", sort.dir === 1 ? "ascending" : "descending");
      } else {
        if (arrow) arrow.remove();
        th.removeAttribute("aria-sort");
      }
    }
  }

  // G3: rebuild the table body. Rows in the active sort order (default:
  // trace-start epoch ascending). A search/TZ rebuild starts unchecked
  // (legacy renderRawTable semantics - a TZ toggle drops the selection);
  // a G8 sort rebuild passes the selection mirror to re-check its rows.
  function rebuildRows(
    objects: readonly BrowseObject[],
    localTz: boolean,
    sort: RawSort | null,
    keepChecked: ReadonlySet<string> | null,
  ): void {
    els.rawBody.textContent = "";

    for (const row of sortRawRows(toRawRows(objects), sort)) {
      const { obj } = row;
      const tr = document.createElement("tr");

      const cbCell = document.createElement("td");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.className = "raw-cb";
      cb.dataset["key"] = obj.key;
      if (keepChecked) cb.checked = keepChecked.has(obj.key);
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
  let lastSort: RawSort | null = null;
  store.subscribe(["raw", "ui"], (state) => {
    assertInScheduledRender("raw-view render");
    renderStatus(els.rawStatus, state.raw.status);
    els.rawTable.style.display = state.raw.tableVisible ? "" : "none";
    renderSortIndicators(state.raw.sort);
    // Rebuild only when a search / TZ toggle bumped the render epoch
    // (selection reset - legacy semantics) or a G8 header click changed
    // the sort (selection preserved); selection mirror changes must NOT
    // rebuild (they would wipe the checkboxes the user just clicked).
    const epochChanged = state.raw.renderEpoch !== lastEpoch;
    const sortChanged = state.raw.sort !== lastSort;
    if (epochChanged || sortChanged) {
      lastEpoch = state.raw.renderEpoch;
      lastSort = state.raw.sort;
      rebuildRows(
        state.raw.objects,
        state.ui.useLocalTz,
        state.raw.sort,
        epochChanged ? null : state.raw.selected,
      );
    }
  });
}
