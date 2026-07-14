// The versioned URL view-state codec. Maps shareable VIEW state (what the
// reader is looking at, not what data is loaded) to and from the URL HASH, as a
// versioned form-encoded payload:
//
//   #v=1&fg.w=<tab-joined frame path>&tm=abs
//
// Design rules (additive only, old links must keep working):
//
// - LEGACY PARAMS ARE UNTOUCHED. The flamegraph page's `worker-zoom` /
//   `offworker-zoom` QUERY params keep their exact legacy semantics - this
//   module reads them as a fallback (legacyZoomFromQuery) and mirrors state
//   back into them (applyLegacyZoomToQuery) so links copied from the address
//   bar keep working on the legacy page too. The hash is the NEW unified
//   carrier.
// - PRECEDENCE: when both the versioned hash and legacy query params carry the
//   same field, the hash wins PER FIELD; legacy params fill the gaps. The two
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
// Zoom-path values reuse the legacy wire format exactly: frame names joined by
// TAB (\t), root -> target. Tab therefore cannot appear IN a frame name - the
// same limitation the legacy params have always had.
//
// This module is PURE (no window/history/DOM): the browser side lives in
// sync.ts, so the codec is property-testable under plain Node.

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
  /** Focused span id (re-resolved to the span + highlight chain on load). */
  selectedSpanId?: string;
  /** Poll-detail anchor: the clicked poll's start ns (re-resolved on load). */
  pollStartNs?: number;
  /** Pinned-event anchor: the cluster timestamp ns (re-resolved on load). */
  pinnedEventTs?: number;
  /** Retained region (`"startNs-endNs"`) -> selection.sidebarRange. */
  sidebarRange?: string;
  /** Spawned-tasks range (`"startNs-endNs"`) -> selection.spawnedTasksRange. */
  spawnedRange?: string;
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
const KEY_TIME_MODE = "tm";
const KEY_TIME_ZONE = "tz";
const KNOWN_KEYS: readonly string[] = [
  KEY_FG_WORKER,
  KEY_FG_OFFWORKER,
  KEY_TIME_MODE,
  KEY_TIME_ZONE,
];

/** The legacy flamegraph zoom-state QUERY params. */
export const LEGACY_WORKER_ZOOM_PARAM = "worker-zoom";
export const LEGACY_OFFWORKER_ZOOM_PARAM = "offworker-zoom";

/** Frame-path wire format: tab-joined, root -> target. */
function encodeZoomPath(path: readonly string[]): string {
  return path.join("\t");
}

/**
 * Inverse of encodeZoomPath. An empty or all-empty value means "no path"
 * (null): the legacy page never writes empty segments, so any such value
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

/**
 * Read the LEGACY flamegraph zoom params from a query string, with their exact
 * legacy semantics: absent or empty param = no zoom for that tree. Field names
 * line up with ViewState so the result merges directly.
 */
export function legacyZoomFromQuery(
  search: string | URLSearchParams,
): Pick<ViewState, "fgWorkerZoom" | "fgOffworkerZoom"> {
  const p = typeof search === "string" ? new URLSearchParams(search) : search;
  const out: Pick<ViewState, "fgWorkerZoom" | "fgOffworkerZoom"> = {};
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
  return out;
}

/**
 * Mirror the zoom fields of `state` into the LEGACY query params, exactly as
 * the legacy page writes them: set when non-empty, DELETE when empty, touch
 * nothing else in `params`. Keeping the mirror alive is what makes an
 * address-bar copy from the migrated page open correctly on the legacy page.
 */
export function applyLegacyZoomToQuery(params: URLSearchParams, state: ViewState): void {
  if (state.fgWorkerZoom !== undefined && state.fgWorkerZoom.length > 0) {
    params.set(LEGACY_WORKER_ZOOM_PARAM, encodeZoomPath(state.fgWorkerZoom));
  } else {
    params.delete(LEGACY_WORKER_ZOOM_PARAM);
  }
  if (state.fgOffworkerZoom !== undefined && state.fgOffworkerZoom.length > 0) {
    params.set(LEGACY_OFFWORKER_ZOOM_PARAM, encodeZoomPath(state.fgOffworkerZoom));
  } else {
    params.delete(LEGACY_OFFWORKER_ZOOM_PARAM);
  }
}

/**
 * Resolve the effective view state of a URL: legacy query params as the
 * base, hash fields (v=1 only) overriding PER FIELD. `search`/`hash` take
 * the window.location forms ("?..."/"#..." or ""); bare strings work too.
 */
export function resolveViewState(url: { search: string; hash: string }): ViewState {
  const base: ViewState = { ...legacyZoomFromQuery(url.search) };
  const decoded = decodeViewState(url.hash);
  if (decoded === null || decoded.version !== VIEW_STATE_VERSION) return base;
  return { ...base, ...decoded.state };
}
