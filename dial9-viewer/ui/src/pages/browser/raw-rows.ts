// Pure row model + column sorting for the raw-results table (T15
// amendments of features/01 I2 display and G8; ADR-0004 section 1).
//
// Row model: maps a browse object through the typed key parser
// (lib/trace/keys.ts) into what each column displays:
//
// - known-layout keys render Service / Host / Boot as parsed;
// - unknown-layout keys render RAW: the full key shown across the
//   Service/Host/Boot columns instead of positionally shifted fields
//   (the legacy mislabel this amendment retires - Finding 1).
//
// The filename epoch/segIndex are layout-independent (see keys.ts), so
// Trace Start and Seg # render for both variants, and the default table
// order (trace-start epoch ascending, legacy G3) covers unknown keys too.
//
// Sorting (G8 amendment - the legacy page advertised sortable headers with
// no handler): every column is sortable; NUMERIC for Trace Start (epoch),
// Seg # and Size, LEXICAL otherwise (ticket-decided semantics). A repeated
// click on the active column flips the direction. `sort: null` keeps the
// legacy default order.
//
// Kept free of DOM so the model is unit-testable under the node test env;
// raw-view.ts renders it.

import { parseKey } from "../../lib/trace/keys.js";
import type { BrowseObject } from "./state.js";

export interface RawRow {
  obj: BrowseObject;
  /**
   * Parsed directory columns; null for unknown-layout keys, whose raw key
   * renders across the Service/Host/Boot columns instead.
   */
  parsedCols: { service: string; host: string; bootId: string } | null;
  /** Filename trace-start epoch (unix seconds); 0 when absent. */
  epoch: number;
  /** Filename segment index; "" when absent. */
  segIndex: string;
}

export function toRawRow(obj: BrowseObject): RawRow {
  const p = parseKey(obj.key);
  if (p.layout === "known") {
    return {
      obj,
      parsedCols: { service: p.service, host: p.host, bootId: p.bootId },
      epoch: p.epoch,
      segIndex: p.segIndex,
    };
  }
  return { obj, parsedCols: null, epoch: p.epoch, segIndex: p.segIndex };
}

/** Build the table's row models in the legacy default order (epoch asc). */
export function toRawRows(objects: readonly BrowseObject[]): RawRow[] {
  const rows = objects.map(toRawRow);
  rows.sort((a, b) => a.epoch - b.epoch);
  return rows;
}

/** Column keys - the `data-sort` attributes of the table's headers. */
export type RawSortKey =
  | "service"
  | "host"
  | "bootId"
  | "traceStart"
  | "segIndex"
  | "size"
  | "last_modified";

export interface RawSort {
  key: RawSortKey;
  /** 1 = ascending, -1 = descending. */
  dir: 1 | -1;
}

/**
 * Next sort state after clicking a header (G8): first click sorts that
 * column ascending; clicking the active column again flips the direction.
 */
export function nextSort(current: RawSort | null, clicked: RawSortKey): RawSort {
  if (current && current.key === clicked) {
    return { key: clicked, dir: current.dir === 1 ? -1 : 1 };
  }
  return { key: clicked, dir: 1 };
}

/**
 * The value a column sorts on. Numeric columns return numbers; lexical
 * columns return strings. Missing numerics (no filename epoch/seg#) read
 * as 0 so they group at the numeric bottom rather than interleaving.
 * For unknown-layout rows, the Service/Host/Boot columns DISPLAY the raw
 * key, so the raw key is also what they sort on.
 */
export function sortValue(row: RawRow, key: RawSortKey): number | string {
  switch (key) {
    case "traceStart":
      return row.epoch;
    case "segIndex":
      return row.segIndex === "" ? 0 : parseInt(row.segIndex, 10);
    case "size":
      return row.obj.size;
    case "service":
      return row.parsedCols ? row.parsedCols.service : row.obj.key;
    case "host":
      return row.parsedCols ? row.parsedCols.host : row.obj.key;
    case "bootId":
      return row.parsedCols ? row.parsedCols.bootId : row.obj.key;
    case "last_modified":
      // The raw ISO-8601 string: lexical order IS chronological order,
      // independent of the active TZ display mode.
      return row.obj.last_modified ?? "";
  }
}

/**
 * Sort the row models for display. `sort: null` = the legacy default
 * (trace-start epoch ascending). Ties fall back to epoch-then-key so the
 * order is total and stable across rebuilds.
 */
export function sortRawRows(rows: readonly RawRow[], sort: RawSort | null): RawRow[] {
  const out = [...rows];
  out.sort((a, b) => {
    if (sort) {
      const va = sortValue(a, sort.key);
      const vb = sortValue(b, sort.key);
      const cmp =
        typeof va === "number" && typeof vb === "number"
          ? va - vb
          : String(va).localeCompare(String(vb));
      if (cmp !== 0) return sort.dir * cmp;
    }
    // Default order / tie-break: epoch asc, then key for totality.
    if (a.epoch !== b.epoch) return a.epoch - b.epoch;
    return a.obj.key.localeCompare(b.obj.key);
  });
  return out;
}
