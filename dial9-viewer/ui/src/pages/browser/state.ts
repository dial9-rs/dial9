// Browser-page store state.
//
// One typed store for the page. Slices are replaced wholesale via
// store.update(); every DOM render subscribes to the slices it reads and
// runs through the store scheduler (src/store/store.ts).
//
// Text inputs (bucket/prefix/pickers/raw query/creds fields) stay DOM-owned
// (uncontrolled): user-typed values are not state the store renders. The
// single input mirrored into state is the prefix (FormSlice) because the
// Search button's disabled state renders from it.

import { createStore, type Store } from "../../store/store.js";
import type { HostRow } from "../../lib/canvas/heatmap.js";
import type { BucketInfo } from "../../lib/trace/creds.js";
import {
  EMPTY_SOURCE_SCOPE,
  type SourceScope,
} from "../../lib/trace/source-scope.js";
import { DEFAULT_BUCKET_FILTER } from "./bucket-filter.js";
import type { RawSort } from "./raw-rows.js";

/** One object row from GET /api/browse (`objects[]`). */
export interface BrowseObject {
  key: string;
  size: number;
  last_modified?: string | undefined;
}

/** Additive per-service metadata from GET /api/services. */
export interface ServiceMetadata {
  service: string;
  /** Omitted until the selected service has been browsed. */
  host_count?: number;
  /** Opaque backend discovery state; the UI only echoes it to `/api/browse`. */
  layout_hint?: string;
}

/** Normalized heatmap segment. */
export interface HeatmapSegment {
  key: string;
  size: number;
  /** Trace-start epoch seconds (from the key filename). */
  start: number;
  /** last_modified epoch seconds (upload time), or start when missing. */
  end: number;
  /**
   * Key-layout discriminant. For "unknown" segments service is "" and host
   * carries the key's raw directory path - the grouping key and the label
   * the browse view renders raw, instead of positionally shifted fields.
   */
  layout: "known" | "unknown";
  service: string;
  host: string;
  bootId: string;
}

/** A grouped host row plus the precomputed density inputs (tiled segments
 * + coverage gaps). */
export type HeatmapRow = HostRow<HeatmapSegment> & {
  tiled: readonly HeatmapSegment[];
  gaps: readonly { start: number; end: number }[];
};

export interface TimeDomain {
  tMin: number;
  tMax: number;
}

/** `rows` = [firstRow, lastRow] indices. */
export interface HeatmapSelection {
  keys: readonly string[];
  bytes: number;
  t0: number;
  t1: number;
  rows?: readonly [number, number] | undefined;
}

/**
 * A status area's state for #browse-status / #raw-status: visibility
 * (style.display), kind (className "status" vs "status error"), text, and
 * - for the empty-result hint - the sample-key list rendered as <code>
 * lines. The text persists while hidden.
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
  /** Server runs demand-driven aggregation. */
  aggregationEnabled: boolean;
  /** Server uses flat source keys: traces open directly by key rather than via
   * a scope, since buffer-style keys carry no service/host/date (#627). */
  localMode: boolean;
  /** Server declared a default prefix; Search waits for one. */
  serverHasPrefix: boolean;
  /** Bucket-picker filter substring in effect: URL override >
   * /api/config `bucket_filter` > "dial9". "" = no filter. */
  bucketFilter: string;
  /** The page-URL `bucket_filter=` override (null = absent). Non-null wins
   * over the server value and rides every URL sync (bucket-filter.ts). */
  bucketFilterOverride: string | null;
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
  /** Chip currently marked active (explicit state: the highlight does NOT
   * follow later prefix-input edits). */
  activeSuggestion: string | null;
  /** The prefix input's placeholder (state cycling). */
  prefixPlaceholder: string;
}

export interface BrowseSlice {
  /** Services found by the lightweight `/api/services` query. */
  services: readonly string[];
  /** Metadata for discovered services; absent on older servers. */
  serviceMetadata: readonly ServiceMetadata[];
  /** Focused service tab; null while multiple services await a click. */
  activeService: string | null;
  serviceDiscovery: "idle" | "loading" | "ready" | "error";
  status: StatusState;
  /** Truncation warning banner text; null = hidden. */
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
   * checkboxes themselves stay uncontrolled). */
  selected: ReadonlySet<string>;
  /** Active column sort; null = the default order (trace-start epoch
   * ascending). Unlike a search/TZ rebuild, a sort rebuild PRESERVES the
   * checkbox selection. */
  sort: RawSort | null;
  /** Bumped when the table body must rebuild (TZ toggle re-render). */
  renderEpoch: number;
}

export interface CredsSlice {
  /** Server supports BYO credentials; header button shown. */
  enabled: boolean;
  /** Credentials stored (green button + check label). */
  active: boolean;
  panelOpen: boolean;
  status: { text: string; kind: "ok" | "error" | null };
  /** Last /api/buckets listing. */
  buckets: readonly BucketInfo[];
  bucketsRowVisible: boolean;
  /** "Show all" vs filtered picker view. */
  showAll: boolean;
  /** Name of the picked bucket chip (renders `.selected`). */
  selectedBucket: string | null;
}

/**
 * Captured A/B differential-comparison scopes (#623). Each side is an
 * aggregate scope (a URLSearchParams from the shared scope codec) or null;
 * A fills before B. Held in the store so the diff tray re-renders on capture.
 */
export interface DiffSlice {
  a: URLSearchParams | null;
  b: URLSearchParams | null;
}

/** High-frequency overlay state (the "transient channel"). */
export interface TransientSlice {
  /** In-progress heatmap drag (rubber band), in plot-local px. */
  drag: { x0: number; y0: number; x1: number; y1: number; zooming: boolean } | null;
  /** Footer drop-target highlight while a file drag is over the page. */
  footerDragActive: boolean;
}

export interface BrowserState {
  source: SourceScope;
  ui: UiSlice;
  config: ConfigSlice;
  form: FormSlice;
  search: SearchSlice;
  browse: BrowseSlice;
  raw: RawSlice;
  creds: CredsSlice;
  diff: DiffSlice;
  transient: TransientSlice;
}

export type BrowserStore = Store<BrowserState>;

/** Initial state, matching the static HTML the entry ships. */
export function initialBrowserState(): BrowserState {
  return {
    source: {
      ...EMPTY_SOURCE_SCOPE,
      credentials: { ...EMPTY_SOURCE_SCOPE.credentials },
    },
    ui: { tab: "browse", useLocalTz: false },
    config: {
      aggregationEnabled: false,
      localMode: false,
      serverHasPrefix: false,
      bucketFilter: DEFAULT_BUCKET_FILTER,
      bucketFilterOverride: null,
    },
    form: { prefix: "" },
    search: {
      quickRange: null,
      suggestions: [],
      activeSuggestion: null,
      prefixPlaceholder: "detecting…",
    },
    browse: {
      services: [],
      serviceMetadata: [],
      activeService: null,
      serviceDiscovery: "idle",
      status: {
        visible: true,
        kind: "normal",
        text: "Select a bucket to find services.",
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
      sort: null,
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
    diff: { a: null, b: null },
    transient: { drag: null, footerDragActive: false },
  };
}

export function createBrowserStore(): BrowserStore {
  return createStore(initialBrowserState(), {
    // Microtask scheduler instead of the default requestAnimationFrame:
    // updates made in an event handler still coalesce (one flush per task),
    // but the flush lands before the event's turn ends. This page's renders
    // are cheap idempotent chrome writes plus an identity-memoized canvas
    // paint, so per-task flushing carries none of the per-frame-render
    // concerns the RAF default exists for; what it buys is that anything
    // observing the DOM right after an input event never sees a stale frame.
    scheduler: (cb) => queueMicrotask(cb),
  });
}
