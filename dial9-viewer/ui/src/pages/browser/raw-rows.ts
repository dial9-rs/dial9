// Pure row model for the raw-results table (T15 amendment of features/01
// I2 display; ADR-0004 section 1). Maps a browse object through the typed
// key parser (lib/trace/keys.ts) into what each column displays:
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
