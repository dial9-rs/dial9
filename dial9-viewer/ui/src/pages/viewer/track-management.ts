// Per-track collapse + drag-reorder for the unified track column. This module
// owns:
//
//   - which tracks are user-manageable (collapse + reorder): the four built-in
//     analysis surfaces plus dynamic field charts. The two structural tracks
//     and the selection-only task-detail track are fixed.
//   - the ordering resolution (uiPrefs.trackOrder -> the ordered TrackSpec
//     list), robust to unknown/missing ids so a stored order predating a new
//     track still resolves.
//   - the reorder swap (drop = swap position).
//   - the collapse predicate + label-only height.
//   - the store actions (toggle collapse / reorder / close) the shell's track
//     controls dispatch.
//   - localStorage persistence (hydrate on boot + persist on change):
//     built-in order/collapse survives reload; dynamic ids remain URL-only.
//
// Track pinning is out of scope. This operates on the track list/order +
// per-track height only; it does not touch the per-track render delegation.

import type { ViewerStore } from "../../store/store.js";
import {
  TRACKS,
  isFieldChartTrackId,
} from "../../lib/canvas/track-layout.js";
import type { TrackId, TrackSpec } from "../../lib/canvas/track-layout.js";
import {
  closeFieldChart,
  fieldChartTrackSpecs,
} from "./field-chart-model.js";

/** Collapsed (label-only) track height in CSS px. */
export const COLLAPSED_TRACK_H = 36;

/**
 * The tracks the user can collapse + reorder: the foldable analysis surfaces
 * (spans/events/cpu/queue; task-detail is NOT foldable). The structural tracks
 * (timeline/lanes) host the shared axis + worker rows and stay pinned at the
 * top. Order here is irrelevant (membership only).
 */
export const MANAGEABLE_TRACK_IDS: readonly TrackId[] = [
  "cpu",
  "queue",
  "spans",
  "events",
];

const MANAGEABLE = new Set<string>(MANAGEABLE_TRACK_IDS);

/** True when a track can be collapsed + reordered (a foldable analysis track). */
export function isManageableTrack(id: string): id is TrackId {
  return MANAGEABLE.has(id) || isFieldChartTrackId(id);
}

/**
 * True when track `id` is currently collapsed: manageable AND explicitly
 * flagged in the collapse map. Non-manageable tracks (timeline/lanes/
 * task-detail) are never collapsed, whatever the map says.
 */
export function isCollapsed(
  collapsed: Readonly<Record<string, boolean>>,
  id: TrackId,
): boolean {
  return isManageableTrack(id) && collapsed[id] === true;
}

/**
 * Resolve `uiPrefs.trackOrder` into the full ordered TrackSpec list. The
 * structural tracks (timeline/lanes) and task-detail keep their catalogue
 * positions; only the manageable analysis tracks are permuted, in the stored
 * order. Robust to a stale/partial/dirty stored order:
 *   - unknown ids and non-manageable ids in `trackOrder` are ignored;
 *   - duplicates are de-duped (first occurrence wins);
 *   - manageable tracks not named in `trackOrder` are appended in catalogue
 *     order (so a stored order predating a newly added track still shows it).
 * The manageable tracks fill their catalogue SLOTS in the resolved order, so
 * the fixed tracks never move.
 */
export function orderedTracks(
  trackOrder: readonly string[],
  dynamicTracks: readonly TrackSpec[] = [],
): readonly TrackSpec[] {
  const catalogue = [
    ...TRACKS,
    ...dynamicTracks.filter((track) => isFieldChartTrackId(track.id)),
  ];
  const catalogueManageable = catalogue.filter((t) => isManageableTrack(t.id));
  const byId = new Map(catalogueManageable.map((t) => [t.id, t] as const));
  const seen = new Set<string>();
  const managed: TrackSpec[] = [];
  for (const id of trackOrder) {
    const spec = byId.get(id as TrackId);
    if (spec !== undefined && !seen.has(id)) {
      managed.push(spec);
      seen.add(id);
    }
  }
  for (const t of catalogueManageable) {
    if (!seen.has(t.id)) managed.push(t);
  }
  // Re-lay the catalogue, drawing manageable slots from `managed` in order.
  let mi = 0;
  return catalogue.map((t) =>
    isManageableTrack(t.id) ? (managed[mi++] as TrackSpec) : t,
  );
}

/** The manageable track ids in their current resolved order (persist target). */
function manageableOrder(
  trackOrder: readonly string[],
  dynamicTracks: readonly TrackSpec[] = [],
): TrackId[] {
  return orderedTracks(trackOrder, dynamicTracks)
    .filter((t) => isManageableTrack(t.id))
    .map((t) => t.id);
}

/**
 * Compute the new trackOrder after dropping `dragged` onto `target`: swap the
 * two tracks' positions. A no-op (returns the current manageable order) when
 * either id is not manageable or they are the same track.
 */
export function computeReorder(
  trackOrder: readonly string[],
  dragged: TrackId,
  target: TrackId,
  dynamicTracks: readonly TrackSpec[] = [],
): TrackId[] {
  const order = manageableOrder(trackOrder, dynamicTracks);
  if (
    dragged === target ||
    !isManageableTrack(dragged) ||
    !isManageableTrack(target)
  ) {
    return order;
  }
  const i = order.indexOf(dragged);
  const j = order.indexOf(target);
  if (i < 0 || j < 0) return order;
  const swapped = order.slice();
  [swapped[i], swapped[j]] = [swapped[j] as TrackId, swapped[i] as TrackId];
  return swapped;
}

// ── Store actions ────────────────────────────────────────────────────────

/** The management dispatchers used by the shell's per-track controls. */
export interface TrackManageActions {
  /** Toggle a track's collapsed state (caret click / Enter / Space). */
  toggleCollapse(id: TrackId): void;
  /** Reorder: drop `dragged` onto `target`, swapping their positions. */
  reorder(dragged: TrackId, target: TrackId): void;
  /** Close a URL-defined dynamic chart and clean every reference to its id. */
  close(id: TrackId): void;
}

/** Bind the track-management actions to a store (dispatch uiPrefs updates). */
export function createTrackManageActions(store: ViewerStore): TrackManageActions {
  return {
    toggleCollapse(id: TrackId): void {
      if (!isManageableTrack(id)) return;
      const cur = store.getState().uiPrefs.collapsed;
      store.update("uiPrefs", {
        collapsed: { ...cur, [id]: !(cur[id] === true) },
      });
    },
    reorder(dragged: TrackId, target: TrackId): void {
      const state = store.getState();
      const cur = state.uiPrefs.trackOrder;
      const dynamicTracks = fieldChartTrackSpecs(state.view.fieldCharts);
      const next = computeReorder(cur, dragged, target, dynamicTracks);
      // Only write on an actual change (no store thrash / needless render).
      // Compare against the RESOLVED current order, not the raw stored value:
      // a same-track drop with an empty stored order must not normalize-write.
      if (sameOrder(manageableOrder(cur, dynamicTracks), next)) return;
      store.update("uiPrefs", { trackOrder: next });
    },
    close(id: TrackId): void {
      closeFieldChart(store, id);
    },
  };
}

function sameOrder(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) return false;
  return a.every((v, i) => v === b[i]);
}

/**
 * Toggle a runtime group's folded state by NAME (a click on its lanes header
 * band). Folding drops that runtime's worker rows from the lanes stack (its
 * header stays, so it can be unfolded); the change persists via the uiPrefs ->
 * trackPrefs subscriber. Standalone (not on TrackManageActions) because the
 * lanes interaction layer dispatches it directly, without the track strip.
 */
export function toggleRuntimeCollapsed(store: ViewerStore, name: string): void {
  const cur = store.getState().uiPrefs.collapsedRuntimes;
  store.update("uiPrefs", {
    collapsedRuntimes: { ...cur, [name]: !(cur[name] === true) },
  });
}

/**
 * Toggle a runtime's SUMMARY-LANE fold by NAME (a click on its metrics lane in
 * the lanes track). Folding shrinks the lane to its one-line strip - the
 * headline numbers stay, the chart's height is reclaimed - leaving the runtime's
 * worker rows untouched. Persists via the uiPrefs -> trackPrefs subscriber, like
 * {@link toggleRuntimeCollapsed}.
 */
export function toggleRuntimeMetricsCollapsed(store: ViewerStore, name: string): void {
  const cur = store.getState().uiPrefs.collapsedRuntimeMetrics;
  store.update("uiPrefs", {
    collapsedRuntimeMetrics: { ...cur, [name]: !(cur[name] === true) },
  });
}

// ── localStorage persistence (survives reload) ─────────────────────────────
//
// A try/catch around localStorage with an in-memory fallback map, so a
// storage-blocked context (private mode, disabled storage) degrades to
// session-scoped prefs instead of throwing. Stored as ONE JSON blob under a
// single key.

/** localStorage key for the serialized track prefs. */
export const TRACK_PREFS_STORAGE_KEY = "dial9.viewer.trackPrefs";

/** The persisted shape: the manageable order + the collapse map + the two
 *  per-runtime fold maps (runtime, summary lane) + the lanes box height + the
 *  issues-rail width. Every field but `trackOrder`/`collapsed` is optional so
 *  prefs written before it existed still parse (the store keeps its default
 *  when absent). */
export interface TrackPrefs {
  trackOrder: readonly string[];
  collapsed: Readonly<Record<string, boolean>>;
  collapsedRuntimes?: Readonly<Record<string, boolean>>;
  collapsedRuntimeMetrics?: Readonly<Record<string, boolean>>;
  lanesHeight?: number;
  railWidth?: number;
  taskColWidths?: Readonly<Record<string, number>>;
  issueColWidths?: Readonly<Record<string, number>>;
}

/** Coerce an unknown value into a `Record<string, boolean>`, keeping only the
 *  boolean-valued entries. A non-object yields an empty map. Shared by the
 *  `collapsed` (track) + `collapsedRuntimes` / `collapsedRuntimeMetrics`
 *  (runtime) fold maps. */
function parseBoolMap(value: unknown): Record<string, boolean> {
  const out: Record<string, boolean> = {};
  if (value !== null && typeof value === "object") {
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (typeof v === "boolean") out[k] = v;
    }
  }
  return out;
}

/** Coerce an unknown value into a `Record<string, number>` of positive finite
 *  widths, dropping everything else. A non-object yields an empty map. */
function parseWidthMap(value: unknown): Record<string, number> {
  const out: Record<string, number> = {};
  if (value !== null && typeof value === "object") {
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (typeof v === "number" && Number.isFinite(v) && v > 0) out[k] = v;
    }
  }
  return out;
}

const memoryFallback = new Map<string, string>();

function storageGet(key: string): string | null {
  try {
    return globalThis.localStorage.getItem(key);
  } catch {
    return memoryFallback.has(key) ? (memoryFallback.get(key) ?? null) : null;
  }
}

function storageSet(key: string, value: string): void {
  try {
    globalThis.localStorage.setItem(key, value);
  } catch {
    memoryFallback.set(key, value);
  }
}

/**
 * Read persisted track prefs, or null when none are stored / the stored value
 * is unusable. Defensive parse: a malformed blob (hand-edited, older schema)
 * yields null rather than throwing, so a corrupt entry degrades to defaults.
 */
export function loadTrackPrefs(): TrackPrefs | null {
  const raw = storageGet(TRACK_PREFS_STORAGE_KEY);
  if (raw === null) return null;
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed === null || typeof parsed !== "object") return null;
    const obj = parsed as { trackOrder?: unknown; collapsed?: unknown };
    const trackOrder = Array.isArray(obj.trackOrder)
      ? obj.trackOrder.filter(
          (v): v is string =>
            typeof v === "string" && !isFieldChartTrackId(v),
        )
      : [];
    const collapsed = Object.fromEntries(
      Object.entries(parseBoolMap(obj.collapsed)).filter(
        ([id]) => !isFieldChartTrackId(id),
      ),
    );
    const cr = (obj as { collapsedRuntimes?: unknown }).collapsedRuntimes;
    const collapsedRuntimes = cr !== undefined ? parseBoolMap(cr) : undefined;
    const crm = (obj as { collapsedRuntimeMetrics?: unknown }).collapsedRuntimeMetrics;
    const collapsedRuntimeMetrics = crm !== undefined ? parseBoolMap(crm) : undefined;
    const lh = (obj as { lanesHeight?: unknown }).lanesHeight;
    const lanesHeight = typeof lh === "number" && Number.isFinite(lh) && lh > 0 ? lh : undefined;
    const rw = (obj as { railWidth?: unknown }).railWidth;
    const railWidth = typeof rw === "number" && Number.isFinite(rw) && rw > 0 ? rw : undefined;
    const tcw = (obj as { taskColWidths?: unknown }).taskColWidths;
    const taskColWidths = tcw !== undefined ? parseWidthMap(tcw) : undefined;
    const icw = (obj as { issueColWidths?: unknown }).issueColWidths;
    const issueColWidths = icw !== undefined ? parseWidthMap(icw) : undefined;
    return {
      trackOrder,
      collapsed,
      ...(collapsedRuntimes !== undefined ? { collapsedRuntimes } : {}),
      ...(collapsedRuntimeMetrics !== undefined ? { collapsedRuntimeMetrics } : {}),
      ...(lanesHeight !== undefined ? { lanesHeight } : {}),
      ...(railWidth !== undefined ? { railWidth } : {}),
      ...(taskColWidths !== undefined ? { taskColWidths } : {}),
      ...(issueColWidths !== undefined ? { issueColWidths } : {}),
    };
  } catch {
    return null;
  }
}

/** Persist track prefs (order + collapse map + the two runtime fold maps + lanes
 *  box height + issues-rail width) to localStorage. */
export function saveTrackPrefs(prefs: TrackPrefs): void {
  const trackOrder = prefs.trackOrder.filter((id) => !isFieldChartTrackId(id));
  const collapsed = Object.fromEntries(
    Object.entries(prefs.collapsed).filter(
      ([id]) => !isFieldChartTrackId(id),
    ),
  );
  storageSet(
    TRACK_PREFS_STORAGE_KEY,
    JSON.stringify({
      trackOrder,
      collapsed,
      ...(prefs.collapsedRuntimes !== undefined
        ? { collapsedRuntimes: prefs.collapsedRuntimes }
        : {}),
      ...(prefs.collapsedRuntimeMetrics !== undefined
        ? { collapsedRuntimeMetrics: prefs.collapsedRuntimeMetrics }
        : {}),
      ...(prefs.lanesHeight !== undefined ? { lanesHeight: prefs.lanesHeight } : {}),
      ...(prefs.railWidth !== undefined ? { railWidth: prefs.railWidth } : {}),
      ...(prefs.taskColWidths !== undefined ? { taskColWidths: prefs.taskColWidths } : {}),
      ...(prefs.issueColWidths !== undefined ? { issueColWidths: prefs.issueColWidths } : {}),
    }),
  );
}

/**
 * Seed the store's uiPrefs from persisted track prefs (call once on boot,
 * BEFORE the shell's first render, so the initial paint reflects the saved
 * order/collapse). A no-op when nothing is stored (the store keeps its resting
 * defaults). Dispatches a single uiPrefs update.
 */
export function hydrateTrackPrefs(store: ViewerStore): void {
  const prefs = loadTrackPrefs();
  if (prefs === null) return;
  store.update("uiPrefs", {
    trackOrder: prefs.trackOrder,
    collapsed: prefs.collapsed,
    // Only override the store defaults when the field was actually stored (prefs
    // written before these existed keep the resting default).
    ...(prefs.collapsedRuntimes !== undefined
      ? { collapsedRuntimes: prefs.collapsedRuntimes }
      : {}),
    ...(prefs.collapsedRuntimeMetrics !== undefined
      ? { collapsedRuntimeMetrics: prefs.collapsedRuntimeMetrics }
      : {}),
    ...(prefs.lanesHeight !== undefined ? { lanesViewportHeight: prefs.lanesHeight } : {}),
    ...(prefs.railWidth !== undefined ? { railWidth: prefs.railWidth } : {}),
    ...(prefs.taskColWidths !== undefined ? { taskColWidths: prefs.taskColWidths } : {}),
    ...(prefs.issueColWidths !== undefined ? { issueColWidths: prefs.issueColWidths } : {}),
  });
}

/**
 * Persist trackOrder/collapsed whenever they change. Subscribes to the
 * uiPrefs slice and writes the two fields on every uiPrefs update (cheap - a
 * small JSON blob). Returns an unsubscribe for teardown/HMR. Pair with
 * hydrateTrackPrefs (hydrate first, then mount this) so the reload round-trip
 * is closed: hydrate reads what a previous session's subscriber wrote.
 */
export function mountTrackPrefsPersistence(store: ViewerStore): () => void {
  return store.subscribe(["uiPrefs"], (state) => {
    const {
      trackOrder,
      collapsed,
      collapsedRuntimes,
      collapsedRuntimeMetrics,
      lanesViewportHeight,
      railWidth,
      taskColWidths,
      issueColWidths,
    } = state.uiPrefs;
    saveTrackPrefs({
      trackOrder,
      collapsed,
      collapsedRuntimes,
      collapsedRuntimeMetrics,
      lanesHeight: lanesViewportHeight,
      railWidth,
      taskColWidths,
      issueColWidths,
    });
  });
}
