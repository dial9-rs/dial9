// PROTOTYPE ONLY. Pure model for docs/design/viewer-ui-state-authority.md.
// Delete this directory after the design question is answered.

export type PageId =
  | "browser"
  | "viewer"
  | "flamegraph"
  | "tokio-stats"
  | "span-explorer";
export type RenderLane = "chrome" | "content" | "overlay";

export interface BrowserView {
  readonly bucket: string;
  readonly prefix: string;
  readonly range: readonly [string, string];
  readonly query: string;
  readonly timezone: "utc" | "local";
  readonly tab: "browse" | "raw";
  readonly selectedRows: readonly string[];
}

export interface ViewerView {
  readonly viewport: readonly [number, number] | null;
  readonly selectedTaskId: number | null;
  readonly filter: string;
  readonly inspector: "task" | "span" | "event";
  readonly helpOpen: boolean;
  readonly zoomHistory: readonly (readonly [number, number])[];
}

export interface FlamegraphView {
  readonly mode: "exact" | "api" | "diff";
  readonly service: string;
  readonly hosts: readonly string[];
  readonly maxFiles: number;
  readonly workerZoom: readonly string[];
  readonly offworkerZoom: readonly string[];
  readonly inspectFocus: string | null;
  readonly minimap: {
    readonly band: readonly [number, number] | null;
    readonly split: number;
    readonly collapsed: boolean;
  };
  readonly stream: { readonly sequence: number; readonly coverageEndNs: number | null };
}

export interface TokioStatsView {
  readonly range: readonly [number, number] | null;
  readonly timezone: "utc" | "local";
  readonly maxFiles: number;
  readonly thresholdMs: number;
  readonly activeTab: "tasks" | "workers" | "resources";
  readonly selectedPeriod: string | null;
  readonly periods: readonly string[];
}

export interface SpanExplorerView {
  readonly range: readonly [number, number] | null;
  readonly service: string;
  readonly durationBandMs: readonly [number, number] | null;
  readonly filters: readonly string[];
  readonly sort: readonly ["start" | "duration" | "name", "asc" | "desc"];
  readonly selectedSpanId: string | null;
  readonly hiddenColumns: readonly string[];
  readonly stream: { readonly sequence: number; readonly coverageEndNs: number | null };
}

export interface ViewByPage {
  readonly browser: BrowserView;
  readonly viewer: ViewerView;
  readonly flamegraph: FlamegraphView;
  readonly "tokio-stats": TokioStatsView;
  readonly "span-explorer": SpanExplorerView;
}

export type PageView = {
  readonly [P in PageId]: { readonly page: P; readonly value: ViewByPage[P] };
}[PageId];

export type Operation =
  | { readonly status: "idle" }
  | {
      readonly status: "running";
      readonly id: string;
      readonly sequence: number;
      readonly progress: string;
    }
  | { readonly status: "ready"; readonly id: string; readonly summary: string }
  | { readonly status: "failed"; readonly id: string; readonly message: string };

export interface Model {
  readonly revision: number;
  readonly nextRequest: number;
  readonly route: {
    readonly sourceUrl: string;
    readonly unknown: readonly (readonly [string, string])[];
    readonly projectedUrl: string;
  };
  readonly operation: Operation;
  readonly pointer: readonly [number, number] | null;
  readonly view: PageView;
}

export interface FieldPolicy {
  readonly role:
    | "source"
    | "durable"
    | "preference"
    | "session"
    | "interaction"
    | "operation";
  readonly url: "query" | "excluded";
  readonly storage: "preference" | "excluded";
  readonly lane: RenderLane | "none";
}

type Policy<T> = { readonly [K in keyof T]-?: FieldPolicy };
const field = (
  role: FieldPolicy["role"],
  url: FieldPolicy["url"],
  storage: FieldPolicy["storage"],
  lane: FieldPolicy["lane"],
): FieldPolicy => ({ role, url, storage, lane });
const durableChrome = field("durable", "query", "excluded", "chrome");
const durableContent = field("durable", "query", "excluded", "content");
const preferenceChrome = field("preference", "excluded", "preference", "chrome");
const sessionChrome = field("session", "excluded", "excluded", "chrome");
const sessionContent = field("session", "excluded", "excluded", "content");
const operationChrome = field("operation", "excluded", "excluded", "chrome");
const operationNone = field("operation", "excluded", "excluded", "none");
const sourceNone = field("source", "excluded", "excluded", "none");
const sourceContent = field("source", "excluded", "excluded", "content");
const interactionOverlay = field("interaction", "excluded", "excluded", "overlay");

// Adding a top-level model/page field fails TypeScript until it is classified.
export const COMMON_FIELD_POLICY = {
  revision: operationNone,
  nextRequest: operationNone,
  route: sourceNone,
  operation: operationChrome,
  pointer: interactionOverlay,
} satisfies Policy<Omit<Model, "view">>;

export const PAGE_FIELD_POLICY = {
  browser: {
    bucket: durableChrome,
    prefix: durableChrome,
    range: durableChrome,
    query: durableChrome,
    timezone: preferenceChrome,
    tab: durableChrome,
    selectedRows: sessionContent,
  },
  viewer: {
    viewport: durableContent,
    selectedTaskId: durableContent,
    filter: durableContent,
    inspector: durableChrome,
    helpOpen: sessionChrome,
    zoomHistory: sessionContent,
  },
  flamegraph: {
    mode: durableChrome,
    service: durableChrome,
    hosts: durableChrome,
    maxFiles: durableChrome,
    workerZoom: durableContent,
    offworkerZoom: durableContent,
    inspectFocus: durableContent,
    minimap: durableContent,
    stream: operationChrome,
  },
  "tokio-stats": {
    range: durableChrome,
    timezone: preferenceChrome,
    maxFiles: durableChrome,
    thresholdMs: durableContent,
    activeTab: durableChrome,
    selectedPeriod: durableContent,
    periods: sourceContent,
  },
  "span-explorer": {
    range: durableChrome,
    service: durableChrome,
    durationBandMs: durableContent,
    filters: durableContent,
    sort: durableContent,
    selectedSpanId: durableContent,
    hiddenColumns: preferenceChrome,
    stream: operationChrome,
  },
} satisfies { readonly [P in PageId]: Policy<ViewByPage[P]> };

// One representative semantic intent per page deliberately changes multiple
// related values atomically. This is the alternative to generic slice patches.
export type Intent =
  | {
      readonly type: "browser/search-configured";
      readonly bucket: string;
      readonly prefix: string;
      readonly range: readonly [string, string];
      readonly query: string;
      readonly timezone: BrowserView["timezone"];
      readonly tab: BrowserView["tab"];
      readonly selectedRows: readonly string[];
    }
  | {
      readonly type: "viewer/analysis-changed";
      readonly viewport: readonly [number, number];
      readonly taskId: number | null;
      readonly filter: string;
      readonly inspector: ViewerView["inspector"];
      readonly helpOpen: boolean;
    }
  | {
      readonly type: "flamegraph/analysis-configured";
      readonly mode: FlamegraphView["mode"];
      readonly service: string;
      readonly hosts: readonly string[];
      readonly maxFiles: number;
      readonly band: readonly [number, number];
      readonly split: number;
    }
  | {
      readonly type: "tokio/query-configured";
      readonly range: readonly [number, number];
      readonly timezone: TokioStatsView["timezone"];
      readonly maxFiles: number;
      readonly thresholdMs: number;
      readonly tab: TokioStatsView["activeTab"];
    }
  | {
      readonly type: "span/query-configured";
      readonly range: readonly [number, number];
      readonly service: string;
      readonly durationBandMs: readonly [number, number];
      readonly filters: readonly string[];
      readonly sort: SpanExplorerView["sort"];
      readonly selectedSpanId: string | null;
      readonly hiddenColumns: readonly string[];
    };

export type Event =
  | { readonly type: "route/observed"; readonly url: string }
  | { readonly type: "intent"; readonly intent: Intent }
  | { readonly type: "load/requested" }
  | {
      readonly type: "load/progressed";
      readonly requestId: string;
      readonly sequence: number;
      readonly progress: string;
    }
  | {
      readonly type: "load/completed";
      readonly requestId: string;
      readonly summary: string;
    }
  | {
      readonly type: "interaction/pointer-sampled";
      readonly x: number;
      readonly y: number;
    }
  | {
      readonly type: "flamegraph/widget-proposed";
      readonly baseRevision: number;
      readonly workerZoom: readonly string[];
      readonly offworkerZoom: readonly string[];
      readonly inspectFocus: string | null;
    };

export type Effect =
  | { readonly type: "history/replace"; readonly url: string }
  | {
      readonly type: "load/start";
      readonly requestId: string;
      readonly page: PageId;
      readonly scope: string;
    }
  | {
      readonly type: "flamegraph/present";
      readonly revision: number;
      readonly workerZoom: readonly string[];
      readonly offworkerZoom: readonly string[];
      readonly inspectFocus: string | null;
    };

export interface Step {
  readonly model: Model;
  readonly effects: readonly Effect[];
  readonly invalidated: ReadonlySet<RenderLane>;
  readonly outcome: "committed" | "ignored" | "rejected";
  readonly note: string;
}

const knownKeys: Record<PageId, ReadonlySet<string>> = {
  browser: new Set(["bucket", "prefix", "from", "to", "q", "tz", "tab", "row"]),
  viewer: new Set(["start", "end", "task", "filter", "inspector"]),
  flamegraph: new Set([
    "mode", "service", "host", "max_files", "worker-zoom", "offworker-zoom",
    "inspect", "mini_start", "mini_end", "split",
  ]),
  "tokio-stats": new Set([
    "start_ns", "end_ns", "tz", "max_files", "threshold_ms", "tab", "period",
  ]),
  "span-explorer": new Set([
    "start_ns", "end_ns", "service", "min_ms", "max_ms", "filter", "sort",
    "dir", "span", "hide",
  ]),
};

const number = (params: URLSearchParams, key: string): number | null => {
  const value = params.get(key);
  if (value === null || value === "") return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
};
const range = (
  params: URLSearchParams,
  start: string,
  end: string,
): readonly [number, number] | null => {
  const a = number(params, start);
  const b = number(params, end);
  return a !== null && b !== null && a < b ? [a, b] : null;
};
const path = (value: string | null): readonly string[] =>
  value === null || value === "" ? [] : value.split("\t").filter(Boolean);
const integer = (value: string | null, fallback: number): number => {
  const parsed = value === null ? NaN : Number(value);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : fallback;
};

function defaults(page: PageId): PageView {
  switch (page) {
    case "browser":
      return { page, value: {
        bucket: "", prefix: "", range: ["", ""], query: "", timezone: "utc",
        tab: "browse", selectedRows: [],
      } };
    case "viewer":
      return { page, value: {
        viewport: null, selectedTaskId: null, filter: "", inspector: "task",
        helpOpen: false, zoomHistory: [],
      } };
    case "flamegraph":
      return { page, value: {
        mode: "exact", service: "", hosts: [], maxFiles: 64, workerZoom: [],
        offworkerZoom: [], inspectFocus: null,
        minimap: { band: null, split: 0.5, collapsed: false },
        stream: { sequence: 0, coverageEndNs: null },
      } };
    case "tokio-stats":
      return { page, value: {
        range: null, timezone: "utc", maxFiles: 64, thresholdMs: 10,
        activeTab: "tasks", selectedPeriod: null, periods: [],
      } };
    case "span-explorer":
      return { page, value: {
        range: null, service: "", durationBandMs: null, filters: [],
        sort: ["start", "asc"], selectedSpanId: null, hiddenColumns: [],
        stream: { sequence: 0, coverageEndNs: null },
      } };
  }
}

function hydrate(page: PageId, url: URL): PageView {
  const p = url.searchParams;
  switch (page) {
    case "browser":
      return { page, value: {
        bucket: p.get("bucket") ?? "", prefix: p.get("prefix") ?? "",
        range: [p.get("from") ?? "", p.get("to") ?? ""], query: p.get("q") ?? "",
        timezone: p.get("tz") === "local" ? "local" : "utc",
        tab: p.get("tab") === "raw" ? "raw" : "browse",
        selectedRows: p.getAll("row"),
      } };
    case "viewer": {
      const task = number(p, "task");
      const inspector = p.get("inspector");
      return { page, value: {
        viewport: range(p, "start", "end"),
        selectedTaskId: task !== null && Number.isInteger(task) ? task : null,
        filter: p.get("filter") ?? "",
        inspector: inspector === "span" || inspector === "event" ? inspector : "task",
        helpOpen: false, zoomHistory: [],
      } };
    }
    case "flamegraph": {
      const mode = p.get("mode");
      const mini = range(p, "mini_start", "mini_end");
      const split = number(p, "split");
      return { page, value: {
        mode: mode === "api" || mode === "diff" ? mode : "exact",
        service: p.get("service") ?? "", hosts: p.getAll("host"),
        maxFiles: integer(p.get("max_files"), 64),
        workerZoom: path(p.get("worker-zoom")),
        offworkerZoom: path(p.get("offworker-zoom")),
        inspectFocus: p.get("inspect"),
        minimap: {
          band: mini,
          split: split !== null && split >= 0 && split <= 1 ? split : 0.5,
          collapsed: false,
        },
        stream: { sequence: 0, coverageEndNs: null },
      } };
    }
    case "tokio-stats": {
      const tab = p.get("tab");
      return { page, value: {
        range: range(p, "start_ns", "end_ns"),
        timezone: p.get("tz") === "local" ? "local" : "utc",
        maxFiles: integer(p.get("max_files"), 64),
        thresholdMs: number(p, "threshold_ms") ?? 10,
        activeTab: tab === "workers" || tab === "resources" ? tab : "tasks",
        selectedPeriod: p.get("period"), periods: [],
      } };
    }
    case "span-explorer": {
      const band = range(p, "min_ms", "max_ms");
      const sort = p.get("sort");
      return { page, value: {
        range: range(p, "start_ns", "end_ns"), service: p.get("service") ?? "",
        durationBandMs: band, filters: p.getAll("filter"),
        sort: [
          sort === "duration" || sort === "name" ? sort : "start",
          p.get("dir") === "desc" ? "desc" : "asc",
        ],
        selectedSpanId: p.get("span"), hiddenColumns: p.getAll("hide"),
        stream: { sequence: 0, coverageEndNs: null },
      } };
    }
  }
}

const set = (p: URLSearchParams, key: string, value: string): void => {
  if (value !== "") p.set(key, value);
};
const setRange = (
  p: URLSearchParams,
  keys: readonly [string, string],
  value: readonly [number, number] | null,
): void => {
  if (value !== null) {
    p.set(keys[0], String(value[0]));
    p.set(keys[1], String(value[1]));
  }
};

export function projectUrl(
  view: PageView,
  unknown: readonly (readonly [string, string])[],
): string {
  const p = new URLSearchParams();
  for (const [key, value] of unknown) p.append(key, value);
  switch (view.page) {
    case "browser":
      set(p, "bucket", view.value.bucket);
      set(p, "prefix", view.value.prefix);
      set(p, "from", view.value.range[0]);
      set(p, "to", view.value.range[1]);
      set(p, "q", view.value.query);
      if (view.value.timezone !== "utc") p.set("tz", view.value.timezone);
      if (view.value.tab !== "browse") p.set("tab", view.value.tab);
      for (const row of view.value.selectedRows) p.append("row", row);
      break;
    case "viewer":
      setRange(p, ["start", "end"], view.value.viewport);
      if (view.value.selectedTaskId !== null) p.set("task", String(view.value.selectedTaskId));
      set(p, "filter", view.value.filter);
      if (view.value.inspector !== "task") p.set("inspector", view.value.inspector);
      break;
    case "flamegraph":
      if (view.value.mode !== "exact") p.set("mode", view.value.mode);
      set(p, "service", view.value.service);
      for (const host of view.value.hosts) p.append("host", host);
      if (view.value.maxFiles !== 64) p.set("max_files", String(view.value.maxFiles));
      if (view.value.workerZoom.length > 0) p.set("worker-zoom", view.value.workerZoom.join("\t"));
      if (view.value.offworkerZoom.length > 0) p.set("offworker-zoom", view.value.offworkerZoom.join("\t"));
      if (view.value.inspectFocus !== null) p.set("inspect", view.value.inspectFocus);
      setRange(p, ["mini_start", "mini_end"], view.value.minimap.band);
      if (view.value.minimap.split !== 0.5) p.set("split", String(view.value.minimap.split));
      break;
    case "tokio-stats":
      setRange(p, ["start_ns", "end_ns"], view.value.range);
      if (view.value.timezone !== "utc") p.set("tz", view.value.timezone);
      if (view.value.maxFiles !== 64) p.set("max_files", String(view.value.maxFiles));
      if (view.value.thresholdMs !== 10) p.set("threshold_ms", String(view.value.thresholdMs));
      if (view.value.activeTab !== "tasks") p.set("tab", view.value.activeTab);
      if (view.value.selectedPeriod !== null) p.set("period", view.value.selectedPeriod);
      break;
    case "span-explorer":
      setRange(p, ["start_ns", "end_ns"], view.value.range);
      set(p, "service", view.value.service);
      setRange(p, ["min_ms", "max_ms"], view.value.durationBandMs);
      for (const filter of view.value.filters) p.append("filter", filter);
      if (view.value.sort[0] !== "start") p.set("sort", view.value.sort[0]);
      if (view.value.sort[1] !== "asc") p.set("dir", view.value.sort[1]);
      if (view.value.selectedSpanId !== null) p.set("span", view.value.selectedSpanId);
      for (const column of view.value.hiddenColumns) p.append("hide", column);
      break;
  }
  const query = p.toString();
  return query === "" ? `/${view.page}` : `/${view.page}?${query}`;
}

export function initialize(page: PageId, sourceUrl?: string): Model {
  const url = new URL(sourceUrl ?? `https://prototype.invalid/${page}`);
  const view = sourceUrl === undefined ? defaults(page) : hydrate(page, url);
  const unknown = sourceUrl === undefined
    ? []
    : [...url.searchParams.entries()].filter(([key]) => !knownKeys[page].has(key));
  return {
    revision: 0,
    nextRequest: 1,
    route: {
      sourceUrl: url.toString(),
      unknown,
      projectedUrl: projectUrl(view, unknown),
    },
    operation: { status: "idle" },
    pointer: null,
    view,
  };
}

function ignored(model: Model, note: string): Step {
  return {
    model, effects: [], invalidated: new Set(), outcome: "ignored", note,
  };
}

function rejected(model: Model, note: string): Step {
  return {
    model, effects: [], invalidated: new Set(), outcome: "rejected", note,
  };
}

function commit(
  previous: Model,
  patch: Omit<Partial<Model>, "revision">,
  invalidated: readonly RenderLane[],
  effects: readonly Effect[],
  note: string,
  writeUrl = false,
): Step {
  const provisional = {
    ...previous,
    ...patch,
    revision: previous.revision + 1,
  };
  const projectedUrl = projectUrl(provisional.view, provisional.route.unknown);
  const model: Model = {
    ...provisional,
    route: { ...provisional.route, projectedUrl },
  };
  const emitted = [...effects];
  if (writeUrl && projectedUrl !== previous.route.projectedUrl) {
    emitted.push({ type: "history/replace", url: projectedUrl });
  }
  return {
    model,
    effects: emitted,
    invalidated: new Set(invalidated),
    outcome: "committed",
    note,
  };
}

function reduceIntent(model: Model, intent: Intent): Step {
  switch (intent.type) {
    case "browser/search-configured": {
      if (model.view.page !== "browser") return rejected(model, "wrong page");
      const value: BrowserView = {
        bucket: intent.bucket,
        prefix: intent.prefix,
        range: intent.range,
        query: intent.query,
        timezone: intent.timezone,
        tab: intent.tab,
        selectedRows: [...intent.selectedRows],
      };
      return commit(model, { view: { page: "browser", value } }, ["chrome", "content"], [], "controlled browser state committed atomically", true);
    }
    case "viewer/analysis-changed": {
      if (model.view.page !== "viewer") return rejected(model, "wrong page");
      if (!(intent.viewport[0] < intent.viewport[1])) return rejected(model, "invalid viewport");
      const old = model.view.value.viewport;
      const value: ViewerView = {
        ...model.view.value,
        viewport: intent.viewport,
        selectedTaskId: intent.taskId,
        filter: intent.filter,
        inspector: intent.inspector,
        helpOpen: intent.helpOpen,
        zoomHistory: old === null ? model.view.value.zoomHistory : [...model.view.value.zoomHistory, old],
      };
      return commit(model, { view: { page: "viewer", value } }, ["chrome", "content", "overlay"], [], "viewer transition was one revision", true);
    }
    case "flamegraph/analysis-configured": {
      if (model.view.page !== "flamegraph") return rejected(model, "wrong page");
      if (!(intent.band[0] < intent.band[1]) || intent.split < 0 || intent.split > 1) {
        return rejected(model, "invalid minimap state");
      }
      const value: FlamegraphView = {
        ...model.view.value,
        mode: intent.mode,
        service: intent.service,
        hosts: [...new Set(intent.hosts)].sort(),
        maxFiles: intent.maxFiles,
        minimap: { ...model.view.value.minimap, band: intent.band, split: intent.split },
      };
      return commit(model, { view: { page: "flamegraph", value } }, ["chrome", "content"], [], "flamegraph scope and minimap share one authority", true);
    }
    case "tokio/query-configured": {
      if (model.view.page !== "tokio-stats") return rejected(model, "wrong page");
      if (!(intent.range[0] < intent.range[1]) || intent.thresholdMs < 0) {
        return rejected(model, "invalid Tokio Stats query");
      }
      const value: TokioStatsView = {
        ...model.view.value,
        range: intent.range,
        timezone: intent.timezone,
        maxFiles: intent.maxFiles,
        thresholdMs: intent.thresholdMs,
        activeTab: intent.tab,
      };
      return commit(model, { view: { page: "tokio-stats", value } }, ["chrome", "content"], [], "Tokio Stats DOM values moved through one intent", true);
    }
    case "span/query-configured": {
      if (model.view.page !== "span-explorer") return rejected(model, "wrong page");
      if (!(intent.range[0] < intent.range[1]) || intent.durationBandMs[0] > intent.durationBandMs[1]) {
        return rejected(model, "invalid Span Explorer query");
      }
      const value: SpanExplorerView = {
        ...model.view.value,
        range: intent.range,
        service: intent.service,
        durationBandMs: intent.durationBandMs,
        filters: [...new Set(intent.filters)],
        sort: intent.sort,
        selectedSpanId: intent.selectedSpanId,
        hiddenColumns: [...new Set(intent.hiddenColumns)].sort(),
      };
      return commit(model, { view: { page: "span-explorer", value } }, ["chrome", "content"], [], "Span Explorer query and selection committed atomically", true);
    }
  }
}

const scope = (model: Model): string => {
  switch (model.view.page) {
    case "browser":
      return `${model.view.value.bucket}/${model.view.value.prefix}`;
    case "viewer":
      return model.view.value.viewport?.join("..") ?? "full";
    case "flamegraph":
      return `${model.view.value.mode}:${model.view.value.service}:${model.view.value.hosts.join(",") || "*"}`;
    case "tokio-stats":
      return model.view.value.range?.join("..") ?? "unresolved";
    case "span-explorer":
      return `${model.view.value.service}:${model.view.value.range?.join("..") ?? "unresolved"}`;
  }
};

export function evolve(model: Model, event: Event): Step {
  switch (event.type) {
    case "route/observed": {
      const url = new URL(event.url);
      const view = hydrate(model.view.page, url);
      const unknown = [...url.searchParams.entries()]
        .filter(([key]) => !knownKeys[model.view.page].has(key));
      return commit(
        model,
        { view, route: { sourceUrl: url.toString(), unknown, projectedUrl: projectUrl(view, unknown) } },
        ["chrome", "content", "overlay"],
        [],
        "route hydrated without an echo write",
      );
    }
    case "intent":
      return reduceIntent(model, event.intent);
    case "load/requested": {
      const requestId = `request-${model.nextRequest}`;
      return commit(
        model,
        {
          nextRequest: model.nextRequest + 1,
          operation: { status: "running", id: requestId, sequence: 0, progress: "starting" },
        },
        ["chrome"],
        [{ type: "load/start", requestId, page: model.view.page, scope: scope(model) }],
        `started ${requestId}`,
      );
    }
    case "load/progressed": {
      const op = model.operation;
      if (op.status !== "running" || op.id !== event.requestId || event.sequence <= op.sequence) {
        return ignored(model, "stale or non-monotonic progress ignored");
      }
      return commit(
        model,
        { operation: { ...op, sequence: event.sequence, progress: event.progress } },
        ["chrome"],
        [],
        "accepted current monotonic progress",
      );
    }
    case "load/completed": {
      const op = model.operation;
      if (op.status !== "running" || op.id !== event.requestId) {
        return ignored(model, "stale completion ignored by request identity");
      }
      let view = model.view;
      if (view.page === "tokio-stats") {
        view = { page: "tokio-stats", value: {
          ...view.value,
          periods: ["09:00", "09:05", "09:10"],
          selectedPeriod: view.value.selectedPeriod ?? "09:05",
        } };
      } else if (view.page === "flamegraph") {
        view = { page: "flamegraph", value: {
          ...view.value,
          stream: { sequence: op.sequence, coverageEndNs: 9_000 },
        } };
      } else if (view.page === "span-explorer") {
        view = { page: "span-explorer", value: {
          ...view.value,
          stream: { sequence: op.sequence, coverageEndNs: 9_000 },
        } };
      }
      return commit(
        model,
        { operation: { status: "ready", id: event.requestId, summary: event.summary }, view },
        ["chrome", "content"],
        [],
        "accepted current completion",
      );
    }
    case "interaction/pointer-sampled":
      return commit(
        model,
        { pointer: [event.x, event.y] },
        ["overlay"],
        [],
        "latest pointer sample invalidated only the overlay",
      );
    case "flamegraph/widget-proposed": {
      if (model.view.page !== "flamegraph") return rejected(model, "wrong page");
      const current = model.view.value;
      if (event.baseRevision !== model.revision) {
        return {
          model,
          effects: [{
            type: "flamegraph/present",
            revision: model.revision,
            workerZoom: current.workerZoom,
            offworkerZoom: current.offworkerZoom,
            inspectFocus: current.inspectFocus,
          }],
          invalidated: new Set(),
          outcome: "ignored",
          note: "stale widget proposal rejected; canonical state reapplied",
        };
      }
      const value: FlamegraphView = {
        ...current,
        workerZoom: [...event.workerZoom],
        offworkerZoom: [...event.offworkerZoom],
        inspectFocus: event.inspectFocus,
      };
      return commit(
        model,
        { view: { page: "flamegraph", value } },
        ["chrome", "content"],
        [{
          type: "flamegraph/present",
          revision: model.revision + 1,
          workerZoom: value.workerZoom,
          offworkerZoom: value.offworkerZoom,
          inspectFocus: value.inspectFocus,
        }],
        "widget proposal committed; widget remains a projection",
        true,
      );
    }
  }
}
