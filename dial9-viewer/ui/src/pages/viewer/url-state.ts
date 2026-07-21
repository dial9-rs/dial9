// The trace viewer's shareable URL state.
//
// Projects the store's viewport / selection / uiPrefs into a ViewState so
// bindViewStateToUrl mirrors them into readable query params, and reads them
// back on load. Clock mode + timezone (tm/tz) ride the hash (the ViewState codec
// owns those); everything viewer-specific is a plain query param, so a shared URL
// reads `?start=..&end=..&task=0x..&span-filter=..` rather than an opaque hash
// blob. The viewport is captured as a POSITION (start/end ns): the recipient sees
// the same window but keeps the full trace and can zoom out (distinct from Set
// Range, which reduces the resident data).

import type { ReadonlyState } from "../../store/store.js";
import type { StoreState, PoiSortKey } from "../../types/state.js";
import type { PointOfInterestType } from "../../types/trace.js";
import type { ViewState } from "../../lib/url/index.js";
import { POI_FILTERS } from "./poi.js";

const P_START = "start";
const P_END = "end";
const P_TASK = "task";
const P_SPAN_FILTER = "span-filter";
const P_TRACK_ORDER = "track-order";
const P_COLLAPSED = "collapsed";
const P_SPAN = "span";
const P_SPAN_FOCUS = "span-focus";
const P_POLL = "poll";
const P_EVENT = "event";
const P_REGION = "region";
const P_SPAWNED = "spawned";
const P_ISSUE = "issue";
const P_ISSUE_SORT = "issue-sort";
const P_ISSUE_INDEX = "issue-index";
const P_SPAN_PCT = "span-pct";
const P_SPAN_NAMES = "span-names";
const P_EVENT_NAMES = "event-names";

/** Valid rail sort keys (drop anything else on read). */
const POI_SORT_KEYS: readonly PoiSortKey[] = ["worker", "kind", "time", "duration"];
/** Valid span percentile-filter steps (0/All is the default, never emitted). */
const SPAN_PCTS: readonly number[] = [50, 90, 95, 99];

/** Project the store into the shareable ViewState. */
export function projectViewerState(state: ReadonlyState<StoreState>): ViewState {
  const vs: ViewState = {};
  // tm/tz ride the hash; include them only when non-default so a pristine
  // full-view URL stays clean (`?trace=...` with no hash).
  if (state.uiPrefs.timeMode !== "rel") vs.timeMode = state.uiPrefs.timeMode;
  if (state.uiPrefs.tz !== "utc") vs.timeZone = state.uiPrefs.tz;
  // Viewport only when zoomed in from the full trace extent (a pristine
  // full-view URL stays clean).
  const vp = state.viewport;
  if (
    vp.maxTs > vp.minTs &&
    (vp.viewStart > vp.minTs || vp.viewEnd < vp.maxTs)
  ) {
    vs.viewStart = vp.viewStart;
    vs.viewEnd = vp.viewEnd;
  }
  const sel = state.selection;
  if (sel.selectedTaskId !== null) vs.selectedTaskId = sel.selectedTaskId;
  if (sel.spanFocus !== null) vs.selectedSpanId = sel.spanFocus.spanId;
  if (sel.pollDetail !== null) {
    vs.pollAnchor = `${sel.pollDetail.start}:${sel.pollDetail.taskId}`;
  }
  if (sel.pinnedEvent !== null) vs.pinnedEventTs = sel.pinnedEvent.timestamp;
  if (sel.sidebarRange !== null) {
    vs.sidebarRange = `${sel.sidebarRange.startNs}-${sel.sidebarRange.endNs}`;
  }
  if (sel.spawnedTasksRange !== null) {
    vs.spawnedRange = `${sel.spawnedTasksRange.startNs}-${sel.spawnedTasksRange.endNs}`;
  }
  if (state.uiPrefs.spanFilter !== "") vs.spanFilter = state.uiPrefs.spanFilter;
  if (state.uiPrefs.trackOrder.length > 0) {
    vs.trackOrder = state.uiPrefs.trackOrder;
  }
  const collapsed = Object.keys(state.uiPrefs.collapsed).filter(
    (id) => state.uiPrefs.collapsed[id] === true,
  );
  if (collapsed.length > 0) vs.collapsed = collapsed;
  if (sel.focusedSpanId !== null) vs.focusedSpanId = sel.focusedSpanId;
  // Issues rail: only the deltas from the resting defaults (filter "sched",
  // sort duration/desc, no current POI) so a pristine rail keeps the URL clean.
  const poi = state.poi;
  if (poi.filter !== "sched") vs.poiFilter = poi.filter;
  if (poi.sortKey !== "duration" || poi.sortDir !== "desc") {
    vs.poiSort = `${poi.sortKey},${poi.sortDir}`;
  }
  if (poi.index >= 0) vs.poiIndex = poi.index;
  if (state.uiPrefs.spanPctFilter !== 0) vs.spanPct = state.uiPrefs.spanPctFilter;
  if (state.uiPrefs.selectedSpanNames.size > 0) {
    vs.spanNames = [...state.uiPrefs.selectedSpanNames];
  }
  if (state.uiPrefs.selectedEventNames.size > 0) {
    vs.eventNames = [...state.uiPrefs.selectedEventNames];
  }
  return vs;
}

/** Mirror the viewer fields of `vs` into readable query params (set/delete). */
export function mirrorViewerToQuery(
  params: URLSearchParams,
  vs: ViewState,
): void {
  set(params, P_START, vs.viewStart != null ? String(Math.round(vs.viewStart)) : null);
  set(params, P_END, vs.viewEnd != null ? String(Math.round(vs.viewEnd)) : null);
  set(params, P_TASK, vs.selectedTaskId != null ? `0x${vs.selectedTaskId.toString(16)}` : null);
  set(params, P_SPAN_FILTER, vs.spanFilter && vs.spanFilter.length > 0 ? vs.spanFilter : null);
  set(params, P_TRACK_ORDER, vs.trackOrder && vs.trackOrder.length > 0 ? vs.trackOrder.join(",") : null);
  set(params, P_COLLAPSED, vs.collapsed && vs.collapsed.length > 0 ? vs.collapsed.join(",") : null);
  set(params, P_SPAN, vs.selectedSpanId ?? null);
  set(params, P_SPAN_FOCUS, vs.focusedSpanId ?? null);
  set(params, P_POLL, vs.pollAnchor ?? null);
  set(params, P_EVENT, vs.pinnedEventTs != null ? String(Math.round(vs.pinnedEventTs)) : null);
  set(params, P_REGION, vs.sidebarRange ?? null);
  set(params, P_SPAWNED, vs.spawnedRange ?? null);
  set(params, P_ISSUE, vs.poiFilter ?? null);
  set(params, P_ISSUE_SORT, vs.poiSort ?? null);
  set(params, P_ISSUE_INDEX, vs.poiIndex != null ? String(Math.round(vs.poiIndex)) : null);
  set(params, P_SPAN_PCT, vs.spanPct != null ? String(vs.spanPct) : null);
  set(params, P_SPAN_NAMES, encodeNames(vs.spanNames));
  set(params, P_EVENT_NAMES, encodeNames(vs.eventNames));
}

/**
 * Encode a name-chip list into ONE param value: each name percent-encoded
 * (so a name containing a comma survives), joined by ",". Null when empty, so
 * the param is dropped rather than written blank.
 */
function encodeNames(names?: readonly string[]): string | null {
  if (names === undefined || names.length === 0) return null;
  return names.map((n) => encodeURIComponent(n)).join(",");
}

function set(params: URLSearchParams, key: string, value: string | null): void {
  if (value === null) params.delete(key);
  else params.set(key, value);
}

/** The viewer state parsed from a URL query string. */
export interface ViewerUrlState {
  /** Restore-on-load viewport window (ns), applied to the first loaded trace. */
  viewStart?: number;
  viewEnd?: number;
  selectedTaskId?: number;
  spanFilter?: string;
  trackOrder?: string[];
  collapsed?: string[];
  /** Canvas-selection anchors, re-resolved against the loaded trace on load. */
  selectedSpanId?: string;
  focusedSpanId?: string;
  poll?: { startNs: number; taskId: number };
  pinnedEventTs?: number;
  sidebarRange?: { startNs: number; endNs: number };
  spawnedRange?: { startNs: number; endNs: number };
  /** Issues-rail restore (applied at boot). */
  poiFilter?: PointOfInterestType;
  poiSort?: { key: PoiSortKey; dir: "asc" | "desc" };
  poiIndex?: number;
  spanPct?: number;
  spanNames?: string[];
  eventNames?: string[];
}

/** Read the viewer fields from a URL query string. */
export function readViewerUrlState(search: string): ViewerUrlState {
  const p = new URLSearchParams(search);
  const out: ViewerUrlState = {};
  const start = num(p.get(P_START));
  const end = num(p.get(P_END));
  if (start != null && end != null && end > start) {
    out.viewStart = start;
    out.viewEnd = end;
  }
  const task = intMaybeHex(p.get(P_TASK));
  if (task != null) out.selectedTaskId = task;
  const sf = p.get(P_SPAN_FILTER);
  if (sf != null && sf.length > 0) out.spanFilter = sf;
  const ord = p.get(P_TRACK_ORDER);
  if (ord != null && ord.length > 0) {
    out.trackOrder = ord.split(",").filter((s) => s.length > 0);
  }
  const col = p.get(P_COLLAPSED);
  if (col != null && col.length > 0) {
    out.collapsed = col.split(",").filter((s) => s.length > 0);
  }
  const span = p.get(P_SPAN);
  if (span != null && span.length > 0) out.selectedSpanId = span;
  const spanFocus = p.get(P_SPAN_FOCUS);
  if (spanFocus != null && spanFocus.length > 0) out.focusedSpanId = spanFocus;
  const poll = p.get(P_POLL);
  if (poll != null) {
    const colon = poll.indexOf(":");
    const startNs = num(colon > 0 ? poll.slice(0, colon) : poll);
    const taskId = colon > 0 ? num(poll.slice(colon + 1)) : null;
    if (startNs != null && taskId != null) out.poll = { startNs, taskId };
  }
  const event = num(p.get(P_EVENT));
  if (event != null) out.pinnedEventTs = event;
  const region = rangePair(p.get(P_REGION));
  if (region != null) out.sidebarRange = region;
  const spawned = rangePair(p.get(P_SPAWNED));
  if (spawned != null) out.spawnedRange = spawned;
  const issue = p.get(P_ISSUE);
  if (issue != null && (POI_FILTERS as readonly string[]).includes(issue)) {
    out.poiFilter = issue as PointOfInterestType;
  }
  const issueSort = p.get(P_ISSUE_SORT);
  if (issueSort != null) {
    const comma = issueSort.indexOf(",");
    const key = comma > 0 ? issueSort.slice(0, comma) : "";
    const dir = comma > 0 ? issueSort.slice(comma + 1) : "";
    if ((POI_SORT_KEYS as readonly string[]).includes(key) && (dir === "asc" || dir === "desc")) {
      out.poiSort = { key: key as PoiSortKey, dir };
    }
  }
  const issueIndex = num(p.get(P_ISSUE_INDEX));
  if (issueIndex != null && issueIndex >= 0) out.poiIndex = Math.round(issueIndex);
  const spanPct = num(p.get(P_SPAN_PCT));
  if (spanPct != null && SPAN_PCTS.includes(spanPct)) out.spanPct = spanPct;
  const spanNames = decodeNames(p.get(P_SPAN_NAMES));
  if (spanNames != null) out.spanNames = spanNames;
  const eventNames = decodeNames(p.get(P_EVENT_NAMES));
  if (eventNames != null) out.eventNames = eventNames;
  return out;
}

/** Inverse of encodeNames: split on "," and percent-decode each name. Null
 * when empty so an absent/blank param leaves the field unset. */
function decodeNames(v: string | null): string[] | null {
  if (v == null || v.length === 0) return null;
  const names = v
    .split(",")
    .map((s) => decodeURIComponent(s))
    .filter((s) => s.length > 0);
  return names.length > 0 ? names : null;
}

/** Parse a `"startNs-endNs"` param into a range, or null. */
function rangePair(v: string | null): { startNs: number; endNs: number } | null {
  if (v === null) return null;
  const dash = v.indexOf("-", 1); // skip a leading '-' (negative not expected)
  if (dash <= 0) return null;
  const startNs = num(v.slice(0, dash));
  const endNs = num(v.slice(dash + 1));
  if (startNs == null || endNs == null || endNs <= startNs) return null;
  return { startNs, endNs };
}

function num(v: string | null): number | null {
  if (v === null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function intMaybeHex(v: string | null): number | null {
  if (v === null) return null;
  const n = /^0x/i.test(v) ? parseInt(v.slice(2), 16) : Number(v);
  return Number.isFinite(n) ? n : null;
}
