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
import type { StoreState } from "../../types/state.js";
import type { ViewState } from "../../lib/url/index.js";

const P_START = "start";
const P_END = "end";
const P_TASK = "task";
const P_SPAN_FILTER = "span-filter";
const P_TRACK_ORDER = "track-order";
const P_COLLAPSED = "collapsed";
const P_SPAN = "span";
const P_POLL = "poll";
const P_EVENT = "event";
const P_REGION = "region";
const P_SPAWNED = "spawned";

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
  if (sel.pollDetail !== null) vs.pollStartNs = sel.pollDetail.start;
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
  set(params, P_POLL, vs.pollStartNs != null ? String(Math.round(vs.pollStartNs)) : null);
  set(params, P_EVENT, vs.pinnedEventTs != null ? String(Math.round(vs.pinnedEventTs)) : null);
  set(params, P_REGION, vs.sidebarRange ?? null);
  set(params, P_SPAWNED, vs.spawnedRange ?? null);
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
  pollStartNs?: number;
  pinnedEventTs?: number;
  sidebarRange?: { startNs: number; endNs: number };
  spawnedRange?: { startNs: number; endNs: number };
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
  const poll = num(p.get(P_POLL));
  if (poll != null) out.pollStartNs = poll;
  const event = num(p.get(P_EVENT));
  if (event != null) out.pinnedEventTs = event;
  const region = rangePair(p.get(P_REGION));
  if (region != null) out.sidebarRange = region;
  const spawned = rangePair(p.get(P_SPAWNED));
  if (spawned != null) out.spawnedRange = spawned;
  return out;
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
