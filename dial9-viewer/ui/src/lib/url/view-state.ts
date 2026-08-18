// The versioned URL view-state codec. Maps shareable VIEW state (what the
// reader is looking at, not what data is loaded) to and from the URL HASH, as a
// versioned form-encoded payload:
//
//   #v=1&fg.w=<tab-joined frame path>&tm=abs
//
// Design rules (additive only, old links must keep working):
//
// - STABLE QUERY PARAMS ARE UNTOUCHED. The flamegraph page's `worker-zoom` /
//   `offworker-zoom` params retain their established semantics. This module
//   reads them as a fallback (legacyZoomFromQuery) and mirrors state back into
//   them (applyLegacyZoomToQuery), preserving existing shared links. The hash
//   is the versioned carrier.
// - PRECEDENCE: when both the versioned hash and query params carry the same
//   field, the hash wins PER FIELD; query params fill the gaps. The two
//   only diverge on hand-edited URLs - the sync layer always writes them
//   together.
// - TOLERANT READER: unrecognized keys in a v=1 payload are preserved verbatim
//   (`unknown`) so an older deployed viewer rewriting the URL does not strip
//   state a newer viewer put there. Invalid VALUES of known keys are dropped,
//   never propagated.
// - FOREIGN HASHES are not ours: a non-empty hash without a well-formed `v`
//   integer decodes to null, and callers must leave it alone.
// - VERSION: `v` bumps ONLY on incompatible reinterpretation of an existing
//   key (expected: never). New fields are added within v=1. A payload with a
//   different version decodes to { version, empty state, no unknowns } -
//   callers must not restore from it nor overwrite it.
//
// Zoom-path values reuse the historical wire format exactly: frame names
// joined by TAB (\t), root -> target. Tab therefore cannot appear IN a frame
// name - the same limitation the query params have always had.
//
// This module is PURE (no window/history/DOM): the browser side lives in
// sync.ts, so the codec is property-testable under plain Node.

import type { FieldChartSpec } from "../../types/state.js";

/** Current schema version. Bump ONLY on incompatible reinterpretation. */
export const VIEW_STATE_VERSION = 1;

/** Clock display mode: relative offsets vs absolute wall-clock. */
export type TimeMode = "rel" | "abs";
/** Timezone for absolute timestamps (meaningful with tm=abs). */
export type TimeZoneMode = "utc" | "local";

/**
 * The decoded view state. Every field optional: absent means "the URL does not
 * carry this field" (pages fall back to their own defaults). New fields extend
 * this interface additively.
 */
export interface ViewState {
  /** Flamegraph worker-tree zoom path, root -> target frame names. */
  fgWorkerZoom?: readonly string[];
  /** Flamegraph off-worker-tree zoom path, root -> target frame names. */
  fgOffworkerZoom?: readonly string[];
  /** Flamegraph inspect (butterfly) focus display name (legacy `inspect`). */
  fgInspect?: string | undefined;
  /**
   * Flamegraph inspect focus symbol (legacy `inspect_full`). The identity key
   * is `fgInspectFull || fgInspect`; the legacy mirror only writes it when it
   * differs from the display name.
   */
  fgInspectFull?: string | undefined;
  /** Flamegraph frames-search query (legacy `search`). */
  fgSearch?: string | undefined;
  /** Flamegraph spawn-location filter value (legacy `spawn`), exact mode only. */
  fgSpawn?: string | undefined;
  /** Flamegraph runtime filter value (legacy `runtime`), exact mode only. */
  fgRuntime?: string | undefined;
  /** Clock display mode (key `tm`). */
  timeMode?: TimeMode;
  /** Timezone for absolute timestamps (key `tz`). */
  timeZone?: TimeZoneMode;
  // Trace-viewer shareable state. Carried in READABLE query params by the
  // viewer's mirrorToQuery, NOT the hash: the hash codec above ignores these
  // fields (it only serializes the keys it knows), so a shared viewer URL reads
  // `?start=..&end=..&task=..` rather than an opaque hash blob.
  /** Viewport window start (absolute ns); set only when zoomed in. */
  viewStart?: number;
  /** Viewport window end (absolute ns); set only when zoomed in. */
  viewEnd?: number;
  /** Selected task id, when one is selected. */
  selectedTaskId?: number;
  /** Span-filter text, when non-empty. */
  spanFilter?: string;
  /** Track display order, when reordered from the default. */
  trackOrder?: readonly string[];
  /** Ids of the collapsed tracks, when any. */
  collapsed?: readonly string[];
  /** URL-defined custom-event numeric-field tracks. */
  fieldCharts?: readonly FieldChartSpec[];
  /** Focused span id (re-resolved to the span + highlight chain on load). */
  selectedSpanId?: string;
  /**
   * Poll-detail anchor: `"<startNs>:<taskId>"`. Start ns alone is ambiguous -
   * two workers can start a poll at the same instant - so the task qualifies
   * it (a task's poll at a given start is unique). Re-resolved on load.
   */
  pollAnchor?: string;
  taskDumpAnchor?: string;
  /** Pinned-event anchor: the cluster timestamp ns (re-resolved on load). */
  pinnedEventTs?: number;
  /** Retained region (`"startNs-endNs"`) -> selection.sidebarRange. */
  sidebarRange?: string;
  /** Spawned-tasks range (`"startNs-endNs"`) -> selection.spawnedTasksRange. */
  spawnedRange?: string;
  /** Span-panel subtree focus id, re-resolved on load (distinct from the
   * lane-highlight `selectedSpanId`). */
  focusedSpanId?: string;
  /** Issues-rail detector filter (a PointOfInterestType), when not the default. */
  poiFilter?: string;
  /** Issues-rail sort as `"<key>,<dir>"`, when not the default `duration,desc`. */
  poiSort?: string;
  /** Current POI index in the filtered+sorted rail list, when >= 0. */
  poiIndex?: number;
  /** Span percentile filter (50/90/95/99), when not 0 (All). */
  spanPct?: number;
  /** Span legend name chips toggled on for display filtering. */
  spanNames?: readonly string[];
  /** Custom-event legend name chips toggled on for display filtering. */
  eventNames?: readonly string[];
  /** Active rail tab and Tasks-tab ordering/cursor. */
  railTab?: string;
  taskSort?: string;
  taskIndex?: number;
  /** Runtime groups hidden in the worker lanes. */
  collapsedRuntimes?: readonly string[];
  /** Runtimes whose summary lane is folded to its one-line strip. */
  collapsedRuntimeMetrics?: readonly string[];
  /** Shareable layout geometry and vertical lane position. */
  inspectorWidth?: number;
  railWidth?: number;
  /** Rail-table column widths (px by column key), when user-resized. */
  taskColWidths?: Record<string, number>;
  issueColWidths?: Record<string, number>;
  lanesHeight?: number;
  lanesScrollTop?: number;
  /** Stack sample presentation shared by poll and blocking analyses. */
  stackView?: string;
  /** Inspector and poll-detail view controls. */
  inspectorTab?: string;
  pollSection?: string;
  expandedPollGroups?: readonly string[];
  pollWorkerZoom?: readonly string[];
  pollOffworkerZoom?: readonly string[];
  /** Related-tab disclosure and correlation state. */
  relatedCollapsed?: readonly string[];
  relatedExpand?: readonly string[];
  relatedCorrelateKey?: string;
  relatedCorrelateValue?: string;
  /** Region-analysis controls and embedded flamegraph zoom. */
  regionMode?: string;
  regionHeapMode?: string;
  regionGroupBy?: string;
  regionWorkerZoom?: readonly string[];
  regionOffworkerZoom?: readonly string[];
  regionInspectFocus?: string;
  /** Span match navigation cursor. */
  spanNavIndex?: number;
  /** Active parse filter, distinct from the viewport window. */
  dataStart?: number;
  dataEnd?: number;
}

/** An unrecognized-but-versioned hash entry, preserved for rewrite. */
export type UnknownEntry = readonly [key: string, value: string];

/** Result of decoding a versioned hash payload. */
export interface DecodedViewState {
  /** The payload's declared schema version. */
  version: number;
  /**
   * Decoded fields. Empty when version !== VIEW_STATE_VERSION (a future
   * schema may have reinterpreted the keys; restoring would lie).
   */
  state: ViewState;
  /**
   * v=1 entries this build does not recognize, in payload order. Encoders
   * must carry them through (tolerant-reader rule). Empty for foreign
   * versions: preserving THOSE is the caller's leave-the-hash-alone rule.
   */
  unknown: readonly UnknownEntry[];
}

// The v1 key registry. Order here is the canonical emit order (after `v`).
const KEY_FG_WORKER = "fg.w";
const KEY_FG_OFFWORKER = "fg.o";
const KEY_FG_INSPECT = "fg.i";
const KEY_FG_INSPECT_FULL = "fg.if";
const KEY_FG_SEARCH = "fg.s";
const KEY_FG_SPAWN = "fg.sp";
const KEY_FG_RUNTIME = "fg.rt";
const KEY_TIME_MODE = "tm";
const KEY_TIME_ZONE = "tz";
const KNOWN_KEYS: readonly string[] = [
  KEY_FG_WORKER,
  KEY_FG_OFFWORKER,
  KEY_FG_INSPECT,
  KEY_FG_INSPECT_FULL,
  KEY_FG_SEARCH,
  KEY_FG_SPAWN,
  KEY_FG_RUNTIME,
  KEY_TIME_MODE,
  KEY_TIME_ZONE,
];

/** The historical flamegraph zoom-state query params. */
export const LEGACY_WORKER_ZOOM_PARAM = "worker-zoom";
export const LEGACY_OFFWORKER_ZOOM_PARAM = "offworker-zoom";
// Historical flamegraph inspect/search/filter query names. They stay stable so
// existing shared links keep restoring in the canonical page.
export const LEGACY_INSPECT_PARAM = "inspect";
export const LEGACY_INSPECT_FULL_PARAM = "inspect_full";
export const LEGACY_SEARCH_PARAM = "search";
export const LEGACY_SPAWN_PARAM = "spawn";
export const LEGACY_RUNTIME_PARAM = "runtime";

/** Frame-path wire format: tab-joined, root -> target. */
function encodeZoomPath(path: readonly string[]): string {
  return path.join("\t");
}

/**
 * Inverse of encodeZoomPath. An empty or all-empty value means "no path"
 * (null): the writer never emits empty segments, so any such value
 * is hand-mangled and restoring from it would zoom nowhere.
 */
function decodeZoomPath(value: string): readonly string[] | null {
  if (value === "") return null;
  const parts = value.split("\t");
  if (parts.some((p) => p === "")) return null;
  return parts;
}

function isTimeMode(v: string): v is TimeMode {
  return v === "rel" || v === "abs";
}

function isTimeZoneMode(v: string): v is TimeZoneMode {
  return v === "utc" || v === "local";
}

/**
 * Encode `state` (plus preserved unknown entries) into a hash payload
 * (form-encoded, WITHOUT the leading "#"). Returns "" when there is nothing to
 * carry - callers then omit the hash entirely, so a pristine page keeps a clean
 * URL.
 */
export function encodeViewState(
  state: ViewState,
  unknown: readonly UnknownEntry[] = [],
): string {
  const p = new URLSearchParams();
  if (state.fgWorkerZoom !== undefined && state.fgWorkerZoom.length > 0) {
    p.set(KEY_FG_WORKER, encodeZoomPath(state.fgWorkerZoom));
  }
  if (state.fgOffworkerZoom !== undefined && state.fgOffworkerZoom.length > 0) {
    p.set(KEY_FG_OFFWORKER, encodeZoomPath(state.fgOffworkerZoom));
  }
  // Inspect/search/filters ride verbatim (drop empties). `fg.if` carries the
  // symbol only when it differs from the display name, matching the legacy
  // mirror so both carriers agree on when the pair is redundant.
  if (state.fgInspect) p.set(KEY_FG_INSPECT, state.fgInspect);
  if (state.fgInspectFull && state.fgInspectFull !== state.fgInspect) {
    p.set(KEY_FG_INSPECT_FULL, state.fgInspectFull);
  }
  if (state.fgSearch) p.set(KEY_FG_SEARCH, state.fgSearch);
  if (state.fgSpawn) p.set(KEY_FG_SPAWN, state.fgSpawn);
  if (state.fgRuntime) p.set(KEY_FG_RUNTIME, state.fgRuntime);
  if (state.timeMode !== undefined) p.set(KEY_TIME_MODE, state.timeMode);
  if (state.timeZone !== undefined) p.set(KEY_TIME_ZONE, state.timeZone);
  for (const [k, v] of unknown) p.append(k, v);
  if ([...p.keys()].length === 0) return "";
  // `v` first, purely for human-readable URLs; the parser accepts any order.
  return `v=${VIEW_STATE_VERSION}&${p.toString()}`;
}

/**
 * Decode a hash payload (with or without the leading "#"). Returns null
 * for anything that is not OURS: empty hashes and foreign/unversioned
 * hashes (e.g. `#section-anchor`). Callers must leave a non-empty foreign
 * hash alone on rewrite.
 */
export function decodeViewState(hashPayload: string): DecodedViewState | null {
  const payload = hashPayload.startsWith("#") ? hashPayload.slice(1) : hashPayload;
  if (payload === "") return null;
  const p = new URLSearchParams(payload);
  const v = p.get("v");
  if (v === null || !/^\d+$/.test(v)) return null;
  const version = Number(v);
  if (version !== VIEW_STATE_VERSION) {
    return { version, state: {}, unknown: [] };
  }

  const state: ViewState = {};
  const wz = p.get(KEY_FG_WORKER);
  if (wz !== null) {
    const path = decodeZoomPath(wz);
    if (path !== null) state.fgWorkerZoom = path;
  }
  const oz = p.get(KEY_FG_OFFWORKER);
  if (oz !== null) {
    const path = decodeZoomPath(oz);
    if (path !== null) state.fgOffworkerZoom = path;
  }
  const inspect = p.get(KEY_FG_INSPECT);
  if (inspect) {
    state.fgInspect = inspect;
    // Symbol defaults to the display name (legacy readState semantics), so the
    // identity key `fgInspectFull || fgInspect` is always well-formed.
    const full = p.get(KEY_FG_INSPECT_FULL);
    state.fgInspectFull = full || inspect;
  }
  const search = p.get(KEY_FG_SEARCH);
  if (search) state.fgSearch = search;
  const spawn = p.get(KEY_FG_SPAWN);
  if (spawn) state.fgSpawn = spawn;
  const runtime = p.get(KEY_FG_RUNTIME);
  if (runtime) state.fgRuntime = runtime;
  const tm = p.get(KEY_TIME_MODE);
  if (tm !== null && isTimeMode(tm)) state.timeMode = tm;
  const tz = p.get(KEY_TIME_ZONE);
  if (tz !== null && isTimeZoneMode(tz)) state.timeZone = tz;

  const unknown: UnknownEntry[] = [];
  for (const [k, val] of p.entries()) {
    if (k === "v" || KNOWN_KEYS.includes(k)) continue;
    unknown.push([k, val]);
  }
  return { version, state, unknown };
}

/** The ViewState fields mirrored by the historical flamegraph query params. */
type LegacyMirroredState = Pick<
  ViewState,
  | "fgWorkerZoom"
  | "fgOffworkerZoom"
  | "fgInspect"
  | "fgInspectFull"
  | "fgSearch"
  | "fgSpawn"
  | "fgRuntime"
>;

/**
 * Read the historical flamegraph view params (zoom, inspect, search,
 * spawn/runtime filters) from a query string. An absent or empty param means
 * that field is unset. Field names line up with ViewState for direct merging.
 */
export function legacyZoomFromQuery(
  search: string | URLSearchParams,
): LegacyMirroredState {
  const p = typeof search === "string" ? new URLSearchParams(search) : search;
  const out: LegacyMirroredState = {};
  const wz = p.get(LEGACY_WORKER_ZOOM_PARAM);
  if (wz !== null) {
    const path = decodeZoomPath(wz);
    if (path !== null) out.fgWorkerZoom = path;
  }
  const oz = p.get(LEGACY_OFFWORKER_ZOOM_PARAM);
  if (oz !== null) {
    const path = decodeZoomPath(oz);
    if (path !== null) out.fgOffworkerZoom = path;
  }
  const inspect = p.get(LEGACY_INSPECT_PARAM);
  if (inspect) {
    out.fgInspect = inspect;
    // Symbol defaults to the display name (only serialized when it differs).
    out.fgInspectFull = p.get(LEGACY_INSPECT_FULL_PARAM) || inspect;
  }
  const search2 = p.get(LEGACY_SEARCH_PARAM);
  if (search2) out.fgSearch = search2;
  const spawn = p.get(LEGACY_SPAWN_PARAM);
  if (spawn) out.fgSpawn = spawn;
  const runtime = p.get(LEGACY_RUNTIME_PARAM);
  if (runtime) out.fgRuntime = runtime;
  return out;
}

/**
 * Mirror the flamegraph view fields of `state` into the stable query params:
 * set when non-empty, delete when empty, and touch nothing else in `params`.
 * `inspect_full` is emitted only when it differs from `inspect`.
 */
export function applyLegacyZoomToQuery(params: URLSearchParams, state: ViewState): void {
  setOrDelete(
    params,
    LEGACY_WORKER_ZOOM_PARAM,
    state.fgWorkerZoom && state.fgWorkerZoom.length > 0
      ? encodeZoomPath(state.fgWorkerZoom)
      : null,
  );
  setOrDelete(
    params,
    LEGACY_OFFWORKER_ZOOM_PARAM,
    state.fgOffworkerZoom && state.fgOffworkerZoom.length > 0
      ? encodeZoomPath(state.fgOffworkerZoom)
      : null,
  );
  if (state.fgInspect) {
    params.set(LEGACY_INSPECT_PARAM, state.fgInspect);
    setOrDelete(
      params,
      LEGACY_INSPECT_FULL_PARAM,
      state.fgInspectFull && state.fgInspectFull !== state.fgInspect
        ? state.fgInspectFull
        : null,
    );
  } else {
    params.delete(LEGACY_INSPECT_PARAM);
    params.delete(LEGACY_INSPECT_FULL_PARAM);
  }
  setOrDelete(params, LEGACY_SEARCH_PARAM, state.fgSearch || null);
  setOrDelete(params, LEGACY_SPAWN_PARAM, state.fgSpawn || null);
  setOrDelete(params, LEGACY_RUNTIME_PARAM, state.fgRuntime || null);
}

/** Set `key` to `value`, or delete it when `value` is null/empty. */
function setOrDelete(params: URLSearchParams, key: string, value: string | null): void {
  if (value != null && value !== "") params.set(key, value);
  else params.delete(key);
}

/**
 * Resolve the effective view state of a URL: historical query params as the
 * base, hash fields (v=1 only) overriding PER FIELD. `search`/`hash` take
 * the window.location forms ("?..."/"#..." or ""); bare strings work too.
 */
export function resolveViewState(url: { search: string; hash: string }): ViewState {
  const base: ViewState = { ...legacyZoomFromQuery(url.search) };
  const decoded = decodeViewState(url.hash);
  if (decoded === null || decoded.version !== VIEW_STATE_VERSION) return base;
  return { ...base, ...decoded.state };
}
