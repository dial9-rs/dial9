// One-click diff presets (#624): derive side B of a comparison from side A,
// so the common comparisons need no second tab and no Copy-link paste.
//
// Two presets, both pure scope rewrites over the frozen core's helpers:
//   - "same time, different host"  -> scopeWithHost
//   - "same scope, earlier window" -> shiftScopeTime (-1h / -24h / -7d)
//
// This module decides which presets a given scope can offer and builds the
// resulting B scope. It is DOM-free so the flamegraph tray and the landing
// page's tray share one implementation and one set of tests.

import {
  DIFF_SHIFT_1H,
  DIFF_SHIFT_24H,
  DIFF_SHIFT_7D,
  scopeWithHost,
  shiftScopeTime,
} from "../../../flamegraph_diff.js";

/** Backward window shifts offered by the "earlier window" preset. */
export const SHIFT_KEYS = ["1h", "24h", "7d"] as const;
export type ShiftKey = (typeof SHIFT_KEYS)[number];

const SHIFT_DELTAS: Record<ShiftKey, bigint> = {
  "1h": DIFF_SHIFT_1H,
  "24h": DIFF_SHIFT_24H,
  "7d": DIFF_SHIFT_7D,
};

/** Which side-B presets a scope can offer. */
export interface PresetAvailability {
  /**
   * Hosts worth offering as the "different host" side, sorted. Empty means
   * the preset cannot be offered - there is no other host to compare against.
   */
  otherHosts: string[];
  /** Whether the scope carries a time window to shift backwards. */
  canTimeShift: boolean;
}

/**
 * Decide the presets available for `scope`, given the hosts the page knows
 * about (a facet response, or the rows in the browse view).
 *
 * A scope pinned to exactly one host excludes that host: comparing it against
 * itself is a no-op. A scope spanning several hosts offers all of them, since
 * narrowing a multi-host aggregate down to one host is a real comparison.
 * `knownHosts` is unioned with the scope's own hosts so a page that has not
 * yet seen a facet response still offers what the scope already names.
 */
export function presetAvailability(
  scope: URLSearchParams,
  knownHosts: Iterable<string> = [],
): PresetAvailability {
  const scopeHosts = scope.getAll("host");
  const all = new Set(scopeHosts);
  for (const host of knownHosts) all.add(host);
  const sorted = [...all].sort();
  const pinned = scopeHosts.length === 1 ? scopeHosts[0] : null;
  return {
    otherHosts: pinned === null ? sorted : sorted.filter((host) => host !== pinned),
    canTimeShift: !!(scope.get("start_ns") && scope.get("end_ns")),
  };
}

/** A preset choice: which side-B scope to derive from side A. */
export type Preset =
  | { kind: "host"; host: string }
  | { kind: "shift"; shift: ShiftKey };

/**
 * Build side B from side A. Returns a new URLSearchParams; `scope` is never
 * mutated. A shift over a scope with no window returns an unchanged copy (the
 * caller gates that on `canTimeShift`).
 */
export function presetScope(scope: URLSearchParams, preset: Preset): URLSearchParams {
  if (preset.kind === "host") return scopeWithHost(scope, preset.host);
  return shiftScopeTime(scope, SHIFT_DELTAS[preset.shift]);
}

/** Button label for a backward-shift preset ("-1h"). */
export function shiftLabel(shift: ShiftKey): string {
  return `-${shift}`;
}
