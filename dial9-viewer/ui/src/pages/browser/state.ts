// Browser-page store state (T14; architecture 2.2 applied to features/01).
//
// One typed store for the page, replacing the legacy inline script's page
// globals (useLocalTz, allObjects, heatmap*, rawObjects, currentTab,
// aggregationEnabled, currentQuickRange, serverHasPrefix, ...). Slices are
// replaced wholesale via store.update(); every DOM render subscribes to the
// slices it reads and runs through the store scheduler (src/store/store.ts).
//
// Text inputs (bucket/prefix/pickers/raw query/creds fields) stay
// DOM-owned (uncontrolled), exactly like the legacy page: user-typed values
// are not state the store renders. The single input mirrored into state is
// the prefix (FormSlice) because the Search button's disabled state renders
// from it (features/01 D9 `updateSearchReady`).

import { createStore, type Store } from "../../store/store.js";
import type { HostRow } from "../../lib/canvas/index.js";

/** One object row from GET /api/browse (`objects[]`). */
export interface BrowseObject {
  key: string;
  size: number;
  last_modified?: string | undefined;
}

/** Normalized heatmap segment (legacy `heatmapSegments` element shape). */
export interface HeatmapSegment {
  key: string;
  size: number;
  /** Trace-start epoch seconds (from the key filename). */
  start: number;
  /** last_modified epoch seconds (upload time), or start when missing. */
  end: number;
  service: string;
  host: string;
  bootId: string;
}

/** A grouped host row plus the density inputs the legacy `renderHeatmap`
 * precomputed onto each row (tiled segments + coverage gaps). */
export type HeatmapRow = HostRow<HeatmapSegment> & {
  tiled: readonly HeatmapSegment[];
  gaps: readonly { start: number; end: number }[];
};

export interface TimeDomain {
  tMin: number;
  tMax: number;
}

/** Legacy `heatmapSelection` shape. `rows` = [firstRow, lastRow] indices. */
export interface HeatmapSelection {
  keys: readonly string[];
  bytes: number;
  t0: number;
  t1: number;
  rows?: readonly [number, number] | undefined;
}

/**
 * A status area's state, mirroring exactly the DOM writes the legacy page
 * performed on #browse-status / #raw-status: visibility (style.display),
 * kind (className "status" vs "status error"), text, and - for the
 * empty-result hint - the sample-key list rendered as <code> lines.
 * The text persists while hidden (the parity readouts read the hidden
 * element's text, e.g. the post-search "Searching…" leftover).
 */
export interface StatusState {
  visible: boolean;
  kind: "normal" | "error";
  text: string;
  /** Non-null renders `text` as a lead line followed by sample keys. */
  sampleKeys: readonly string[] | null;
}

export interface UiSlice {
  tab: "browse" | "raw";
  useLocalTz: boolean;
}

export interface ConfigSlice {
  /** Server runs demand-driven aggregation (#570; drives H3 mode + H6). */
  aggregationEnabled: boolean;
  /** Server declared a default prefix; Search waits for one (D9). */
  serverHasPrefix: boolean;
}

/** DOM-input mirror for renders that depend on typed values (see header). */
export interface FormSlice {
  prefix: string;
}

export interface SearchSlice {
  /** Active "Last Nhr" quick range in hours; null once manually edited. */
  quickRange: number | null;
  /** Discovered prefix chip labels (trailing "/" stripped). */
  suggestions: readonly string[];
  /** Chip currently marked active (explicit state: the legacy highlight
   * does NOT follow later prefix-input edits). */
  activeSuggestion: string | null;
  /** The prefix input's placeholder (D2's state cycling). */
  prefixPlaceholder: string;
}

export interface BrowseSlice {
  status: StatusState;
  /** Truncation warning banner text (F20); null = hidden. */
  warning: string | null;
  segments: readonly HeatmapSegment[];
  rows: readonly HeatmapRow[];
  /** Displayed time domain (may be zoomed); null before any results. */
  domain: TimeDomain | null;
  /** Full data extent; restored on zoom reset. */
  fullDomain: TimeDomain | null;
  heatmapVisible: boolean;
  selection: HeatmapSelection | null;
  /** Bumped by the debounced window-resize handler to force a repaint. */
  renderEpoch: number;
}

export interface RawSlice {
  status: StatusState;
  tableVisible: boolean;
  objects: readonly BrowseObject[];
  /** Keys of the checked row checkboxes (mirror of the DOM state; the
   * checkboxes themselves stay uncontrolled, as in the legacy page). */
  selected: ReadonlySet<string>;
  /** Bumped when the table body must rebuild (TZ toggle re-render). */
  renderEpoch: number;
}

export interface CredsSlice {
  /** Server supports BYO credentials; header button shown (B2). */
  enabled: boolean;
  /** Credentials stored (green button + check label). */
  active: boolean;
  panelOpen: boolean;
  status: { text: string; kind: "ok" | "error" | null };
  /** Last /api/buckets listing (legacy `lastBucketNames`). */
  buckets: readonly string[];
  bucketsRowVisible: boolean;
  /** "Show all" vs dial9-filtered picker view (C6). */
  showAll: boolean;
  /** Name of the picked bucket chip (renders `.selected`). */
  selectedBucket: string | null;
}

/** High-frequency overlay state (architecture 2.2 "transient channel"). */
export interface TransientSlice {
  /** In-progress heatmap drag (rubber band), in plot-local px. */
  drag: { x0: number; y0: number; x1: number; y1: number; zooming: boolean } | null;
  /** Footer drop-target highlight while a file drag is over the page. */
  footerDragActive: boolean;
}

export interface BrowserState {
  ui: UiSlice;
  config: ConfigSlice;
  form: FormSlice;
  search: SearchSlice;
  browse: BrowseSlice;
  raw: RawSlice;
  creds: CredsSlice;
  transient: TransientSlice;
}

export type BrowserStore = Store<BrowserState>;

/** Initial state, matching the static HTML the entry ships. */
export function initialBrowserState(): BrowserState {
  return {
    ui: { tab: "browse", useLocalTz: false },
    config: { aggregationEnabled: false, serverHasPrefix: false },
    form: { prefix: "" },
    search: {
      quickRange: null,
      suggestions: [],
      activeSuggestion: null,
      prefixPlaceholder: "detecting…",
    },
    browse: {
      status: {
        visible: true,
        kind: "normal",
        text: "Select a time range and click Search to find traces.",
        sampleKeys: null,
      },
      warning: null,
      segments: [],
      rows: [],
      domain: null,
      fullDomain: null,
      heatmapVisible: false,
      selection: null,
      renderEpoch: 0,
    },
    raw: {
      status: {
        visible: true,
        kind: "normal",
        text: "Enter a prefix and search.",
        sampleKeys: null,
      },
      tableVisible: false,
      objects: [],
      selected: new Set(),
      renderEpoch: 0,
    },
    creds: {
      enabled: false,
      active: false,
      panelOpen: false,
      status: { text: "", kind: null },
      buckets: [],
      bucketsRowVisible: false,
      showAll: false,
      selectedBucket: null,
    },
    transient: { drag: null, footerDragActive: false },
  };
}

export function createBrowserStore(): BrowserStore {
  return createStore(initialBrowserState());
}
