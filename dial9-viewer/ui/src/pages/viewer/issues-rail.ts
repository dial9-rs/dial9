// The issues rail: a ranked, keyboard-navigable, sortable list of points of
// interest. A store-wired controller (createIssuesRail) created once so its
// sort memo lives across renders; it exposes a lit-html `template(state)` and
// `n`/`p` key bindings. Every handler dispatches store actions only - it never
// renders; the shell's store subscription repaints.
//
// The rail reads the derived POI model (poi.ts). Row click and `n`/`p` center
// the viewport on the POI and select its task; the filter dropdown and
// column-header sort re-scope/re-order the SAME detector rows, so the count is
// unchanged.

import { html, nothing, type TemplateResult } from "lit-html";
import { classMap } from "lit-html/directives/class-map.js";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import type { PoiSortKey, RailTab, TaskSortKey } from "../../types/state.js";
import type { PointOfInterestType } from "../../types/trace.js";
import type { KeyBinding } from "../../lib/interact/keyboard.js";
import {
  POI_FILTERS,
  derivePoiViewModel,
  filterLabel,
  parsePoiFilter,
  poiJump,
  stepIndex,
  type PoiViewModel,
} from "./poi.js";
import {
  deriveTaskViewModel,
  type TaskIndexRow,
  type TaskViewModel,
} from "./tasks-model.js";

/** A sortable column: its state key, header label, and default direction. */
interface Column {
  key: PoiSortKey;
  label: string;
  /** Column-scoped tooltip. */
  title: string;
  /** Direction applied on the FIRST click of this column. */
  defaultDir: "asc" | "desc";
}

const COLUMNS: readonly Column[] = [
  { key: "worker", label: "worker", title: "Sort by worker thread", defaultDir: "asc" },
  { key: "kind", label: "what", title: "Sort by issue kind", defaultDir: "asc" },
  { key: "time", label: "t", title: "Sort by time (chronological)", defaultDir: "asc" },
  { key: "duration", label: "dur", title: "Sort by severity (worst first)", defaultDir: "desc" },
];

/** A sortable Tasks-tab column. */
interface TaskColumn {
  key: TaskSortKey;
  label: string;
  title: string;
  defaultDir: "asc" | "desc";
}

const TASK_COLUMNS: readonly TaskColumn[] = [
  { key: "id", label: "task", title: "Sort by task id", defaultDir: "asc" },
  { key: "loc", label: "spawn", title: "Sort by spawn location", defaultDir: "asc" },
  { key: "polls", label: "polls", title: "Sort by poll count", defaultDir: "desc" },
  { key: "total", label: "total", title: "Sort by total poll time", defaultDir: "desc" },
  { key: "longest", label: "longest", title: "Sort by longest poll", defaultDir: "desc" },
  { key: "lifetime", label: "life", title: "Sort by lifetime", defaultDir: "desc" },
];

/** Clamp bounds for the rail's resize drag: never narrower than a usable
 *  table, never wider than 60% of the viewport (the track column must stay
 *  workable). */
const MIN_RAIL_WIDTH = 220;
const MAX_RAIL_VW = 0.6;

/**
 * Clamp a dragged rail width to [MIN_RAIL_WIDTH, MAX_RAIL_VW * viewport],
 * rounded to whole px. Exported for the bounds test; the drag handler calls
 * it with the live window.innerWidth.
 */
export function clampRailWidth(width: number, viewportWidth: number): number {
  const maxW = Number.isFinite(viewportWidth)
    ? viewportWidth * MAX_RAIL_VW
    : Infinity;
  return Math.round(Math.max(MIN_RAIL_WIDTH, Math.min(maxW, width)));
}

/** The narrowest a Tasks-table column can be dragged: enough for a short
 *  number and the sort caret to stay legible. No upper bound - the table
 *  scrolls horizontally inside the rail. */
const MIN_TASK_COL_WIDTH = 40;

/** Clamp a dragged Tasks-table column width, rounded to whole px. Exported
 *  for the bounds test. */
export function clampTaskColWidth(width: number): number {
  return Math.round(Math.max(MIN_TASK_COL_WIDTH, width));
}

/** The per-column widths map the Tasks table renders from (uiPrefs slice). */
type TaskColWidths = StoreState["uiPrefs"]["taskColWidths"];

export interface IssuesRailController {
  /** The rail template for one render pass (reads live store state). */
  template(state: StoreState): TemplateResult;
  /** `n`/`p` step bindings for the unified key router. */
  keyBindings: readonly KeyBinding[];
  /**
   * Supply the "scroll a worker's lane into view" action. Bound by the page
   * entry once the lanes are mounted, since those mount after the shell builds
   * this controller. Jumping to a POI calls it so the target lane is actually
   * visible, not merely time-aligned.
   */
  setRevealWorker(reveal: (workerId: number) => void): void;
  /** No live resources; present for symmetry with the track controllers. */
  dispose(): void;
}

/** Build the store-wired issues-rail controller. */
export function createIssuesRail(store: ViewerStore): IssuesRailController {
  // Sort memo: derivePoiViewModel re-sorts the retained list and formats the
  // visible window, so cache it on the (trace, filter, sortKey, sortDir, minTs,
  // index) key and skip that work on a pan/zoom/selection repaint (the common
  // frame). `index` is part of the key because the formatted window is anchored
  // on it - stepping past the window edge has to re-slice.
  let cacheKey = "";
  let cacheVm: PoiViewModel | null = null;

  function viewModel(state: StoreState): PoiViewModel {
    const trace = state.trace.trace;
    const { poi, viewport } = state;
    const key = trace
      ? `${idOf(trace)}|${poi.filter}|${poi.sortKey}|${poi.sortDir}|${viewport.minTs}|${poi.index}`
      : "none";
    if (cacheVm === null || key !== cacheKey) {
      cacheKey = key;
      cacheVm = derivePoiViewModel(trace, poi, viewport.minTs);
    }
    return cacheVm;
  }

  // Tasks-tab memo, keyed on its own sort state; the task index itself is
  // memoized on trace identity in tasks-model, so this only re-sorts + re-slices.
  let taskCacheKey = "";
  let taskCacheVm: TaskViewModel | null = null;

  function taskViewModel(state: StoreState): TaskViewModel {
    const trace = state.trace.trace;
    const { poi } = state;
    const key = trace
      ? `${idOf(trace)}|${poi.taskSort}|${poi.taskSortDir}|${poi.taskIndex}`
      : "none";
    if (taskCacheVm === null || key !== taskCacheKey) {
      taskCacheKey = key;
      taskCacheVm = deriveTaskViewModel(trace, poi);
    }
    return taskCacheVm;
  }

  /**
   * Bring a worker's lane row on screen. Bound late by the page entry: the
   * lanes are mounted after the shell (so their subscriber paints after the
   * chrome), which is after this controller is constructed. No-op until then,
   * and no-op in tests that never mount lanes.
   */
  let revealWorker: (workerId: number) => void = () => {};

  /** Center + select on the POI at absolute `index` of the retained list.
   *  Indexes `sorted`, not `rows`: `n`/`p` step the whole list while only a
   *  window around the cursor is ever formatted. */
  function jumpTo(index: number): void {
    const vm = viewModel(store.getState());
    const poi = vm.sorted[index];
    if (poi === undefined) return;
    const jump = poiJump(poi, store.getState().viewport);
    store.update("viewport", { viewStart: jump.viewStart, viewEnd: jump.viewEnd });
    store.update("selection", {
      selectedTaskId: jump.selectedTaskId,
      taskDump: null,
    });
    store.update("poi", { index });
    // Moving the time window is not enough: the lanes box scrolls
    // independently, so a POI on a lane below the fold would leave the user
    // looking at an unchanged screen.
    revealWorker(poi.worker);
  }

  /** Select a task and reveal it: frame its lifetime horizontally, scroll to
   *  its first-poll worker vertically. A task runs on many workers, so there is
   *  no single lane - the first poll's worker is the most defensible target. */
  function jumpToTask(index: number): void {
    const vm = taskViewModel(store.getState());
    const task = vm.sorted[index];
    if (task === undefined) return;
    const vp = store.getState().viewport;
    const window = revealTaskWindow(task, vp);
    if (window !== null) {
      store.update("viewport", { viewStart: window.start, viewEnd: window.end });
    }
    store.update("selection", {
      selectedTaskId: task.taskId,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
    });
    store.update("poi", { taskIndex: index });
    if (task.firstPollWorker >= 0) revealWorker(task.firstPollWorker);
  }

  /** `n`/`p`: step the current tab's cursor and jump. Steps across the whole
   *  retained/sorted list, which is what the index addresses. */
  function step(dir: 1 | -1): boolean {
    const state = store.getState();
    if (state.poi.railTab === "tasks") {
      const vm = taskViewModel(state);
      const next = stepIndex(vm.total, state.poi.taskIndex, dir);
      if (next < 0) return false;
      jumpToTask(next);
      return true;
    }
    const vm = viewModel(state);
    const next = stepIndex(vm.retained, state.poi.index, dir);
    if (next < 0) return false; // nothing to step to - decline the key
    jumpTo(next);
    return true;
  }

  function setTab(railTab: RailTab): void {
    store.update("poi", { railTab });
  }

  function setFilter(filter: PointOfInterestType): void {
    // A new filter rebuilds the list; the current index no longer maps.
    store.update("poi", { filter, index: -1 });
  }

  function sortTaskByColumn(col: TaskColumn): void {
    const { poi } = store.getState();
    const dir =
      poi.taskSort === col.key
        ? poi.taskSortDir === "asc"
          ? "desc"
          : "asc"
        : col.defaultDir;
    store.update("poi", { taskSort: col.key, taskSortDir: dir, taskIndex: -1 });
  }

  /** A column header click: toggle direction if already active, else its
   *  default direction. Index is preserved by re-clamping (a re-sort keeps
   *  the same rows), matching the "sort is count-independent" contract. */
  function sortByColumn(col: Column): void {
    const { poi } = store.getState();
    const dir =
      poi.sortKey === col.key
        ? poi.sortDir === "asc"
          ? "desc"
          : "asc"
        : col.defaultDir;
    store.update("poi", { sortKey: col.key, sortDir: dir, index: -1 });
  }

  // ── Resize drag (the rail's right-edge handle) ──────────────────────────
  // Width lives in uiPrefs.railWidth: the shell re-renders the rail (and
  // re-sizes the track canvases) on every uiPrefs change, and the trackPrefs
  // subscriber persists it. The imperative style write below only pre-empts
  // the store-driven render within the same frame so the drag never lags.

  let dragHost: HTMLElement | null = null;
  let dragLeft = 0;

  function applyWidth(width: number): void {
    const clamped = clampRailWidth(width, window.innerWidth);
    if (dragHost !== null) dragHost.style.width = `${clamped}px`;
    if (clamped !== store.getState().uiPrefs.railWidth) {
      store.update("uiPrefs", { railWidth: clamped });
      // Poke the viewport channel so the track-CONTENT controllers redraw at
      // the new column width in real time: lanes/spans/cpu/... subscribe to
      // `viewport`, not `uiPrefs`, so a bare railWidth change re-renders the
      // shell chrome + resizes the canvases but leaves their pixels stale.
      store.update("viewport", {});
    }
  }

  function onResizeDown(e: MouseEvent): void {
    const host = (e.currentTarget as HTMLElement).closest<HTMLElement>(".d9-rail");
    if (host === null) return;
    e.preventDefault();
    dragHost = host;
    dragLeft = host.getBoundingClientRect().left;
    document.body.style.userSelect = "none";
    document.body.style.cursor = "col-resize";
    window.addEventListener("mousemove", onResizeMove);
    window.addEventListener("mouseup", onResizeUp);
  }
  function onResizeMove(e: MouseEvent): void {
    if (dragHost === null) return;
    // The rail is on the left; dragging its right handle sets the width from
    // the aside's left edge to the pointer.
    applyWidth(e.clientX - dragLeft);
  }
  function onResizeUp(): void {
    if (dragHost === null) return;
    dragHost = null;
    document.body.style.userSelect = "";
    document.body.style.cursor = "";
    window.removeEventListener("mousemove", onResizeMove);
    window.removeEventListener("mouseup", onResizeUp);
  }

  // ── Per-column resize drag (the Tasks-table header dividers) ────────────
  // Store-driven only (no imperative style write): nothing outside the rail
  // depends on column widths, and the store's RAF tick re-renders the table
  // in the same frame the imperative write would land in.

  let colDragKey: TaskSortKey | null = null;
  let colDragStartX = 0;
  let colDragStartWidth = 0;

  function onColResizeDown(e: MouseEvent, key: TaskSortKey): void {
    const th = (e.currentTarget as HTMLElement).closest("th");
    const table = th?.closest("table");
    if (th == null || table == null) return;
    e.preventDefault();
    e.stopPropagation();
    // First drag: seed EVERY column from its rendered width, so switching to
    // fixed layout changes only the dragged column, not the whole table.
    let widths = store.getState().uiPrefs.taskColWidths;
    if (Object.keys(widths).length === 0) {
      const seeded: Record<string, number> = {};
      const ths = table.querySelectorAll("thead th");
      TASK_COLUMNS.forEach((col, i) => {
        const header = ths[i];
        if (header !== undefined) {
          seeded[col.key] = clampTaskColWidth(header.getBoundingClientRect().width);
        }
      });
      store.update("uiPrefs", { taskColWidths: seeded });
      widths = store.getState().uiPrefs.taskColWidths;
    }
    colDragKey = key;
    colDragStartX = e.clientX;
    colDragStartWidth =
      widths[key] ?? clampTaskColWidth(th.getBoundingClientRect().width);
    document.body.style.userSelect = "none";
    document.body.style.cursor = "col-resize";
    window.addEventListener("mousemove", onColResizeMove);
    window.addEventListener("mouseup", onColResizeUp);
  }
  function onColResizeMove(e: MouseEvent): void {
    if (colDragKey === null) return;
    const width = clampTaskColWidth(colDragStartWidth + (e.clientX - colDragStartX));
    const cur = store.getState().uiPrefs.taskColWidths;
    if (cur[colDragKey] !== width) {
      store.update("uiPrefs", {
        taskColWidths: { ...cur, [colDragKey]: width },
      });
    }
  }
  function onColResizeUp(): void {
    if (colDragKey === null) return;
    colDragKey = null;
    document.body.style.userSelect = "";
    document.body.style.cursor = "";
    window.removeEventListener("mousemove", onColResizeMove);
    window.removeEventListener("mouseup", onColResizeUp);
  }

  /** Double-click on any divider: back to automatic content-fit layout. */
  function onColResetAll(): void {
    store.update("uiPrefs", { taskColWidths: {} });
  }

  const keyBindings: readonly KeyBinding[] = [
    { key: "n", onKey: () => step(1) },
    { key: "p", onKey: () => step(-1) },
  ];

  return {
    template: (state) => {
      // Compute only the ACTIVE tab's view model. The inactive tab's body and
      // head are not rendered, so building its (whole-trace) model would be
      // pure waste on the load frame - the Tasks index in particular is a full
      // poll scan the Issues tab never shows. The inactive side gets a trivial
      // empty model (a literal, no derivation).
      const tab = state.poi.railTab;
      const poiVm =
        tab === "issues" ? viewModel(state) : derivePoiViewModel(null, state.poi, 0);
      const taskVm =
        tab === "tasks" ? taskViewModel(state) : deriveTaskViewModel(null, state.poi);
      return railTemplate(
        tab,
        poiVm,
        taskVm,
        state.uiPrefs.railWidth,
        state.uiPrefs.taskColWidths,
        {
          setTab,
          setFilter,
          sortByColumn,
          jumpTo,
          sortTaskByColumn,
          jumpToTask,
          onResizeDown,
          onColResizeDown,
          onColResetAll,
        },
      );
    },
    keyBindings,
    setRevealWorker(reveal: (workerId: number) => void): void {
      revealWorker = reveal;
    },
    dispose(): void {
      cacheVm = null;
      taskCacheVm = null;
      revealWorker = () => {};
      // A dispose mid-drag must not leave window listeners or the forced
      // cursor behind.
      onResizeUp();
      onColResizeUp();
    },
  };
}

/**
 * The viewport window to frame a task in: its [spawn, terminate] lifetime with
 * ~15% padding when both bounds are known, else a window around its first poll
 * at the current zoom, else null (nothing usable to frame). Clamped to the
 * trace extent.
 */
function revealTaskWindow(
  task: TaskIndexRow,
  vp: { minTs: number; maxTs: number; viewStart: number; viewEnd: number },
): { start: number; end: number } | null {
  const clamp = (start: number, end: number): { start: number; end: number } => ({
    start: Math.max(vp.minTs, start),
    end: Math.min(vp.maxTs, end),
  });
  if (task.spawnTs != null && task.terminateTs != null && task.terminateTs > task.spawnTs) {
    const pad = (task.terminateTs - task.spawnTs) * 0.15;
    return clamp(task.spawnTs - pad, task.terminateTs + pad);
  }
  // No lifetime: center the current window on the first poll, keeping zoom.
  if (task.firstPollStart >= 0) {
    const width = vp.viewEnd - vp.viewStart;
    if (width > 0) {
      return clamp(task.firstPollStart - width / 2, task.firstPollStart + width / 2);
    }
  }
  return null;
}

interface RailHandlers {
  setTab(tab: RailTab): void;
  setFilter(filter: PointOfInterestType): void;
  sortByColumn(col: Column): void;
  jumpTo(index: number): void;
  sortTaskByColumn(col: TaskColumn): void;
  jumpToTask(index: number): void;
  onResizeDown(e: MouseEvent): void;
  onColResizeDown(e: MouseEvent, key: TaskSortKey): void;
  onColResetAll(): void;
}

/** The tab strip switching Issues vs Tasks. */
function railTabs(active: RailTab, h: RailHandlers): TemplateResult {
  const tab = (id: RailTab, label: string): TemplateResult => html`<button
    type="button"
    role="tab"
    class=${classMap({ "d9-rail-tab": true, on: id === active })}
    aria-selected=${id === active ? "true" : "false"}
    @click=${() => h.setTab(id)}
  >
    ${label}
  </button>`;
  return html`<div class="d9-rail-tabs" role="tablist" aria-label="Rail view">
    ${tab("issues", "Issues")}${tab("tasks", "Tasks")}
  </div>`;
}

/** The rail's lit-html template for one render pass. */
function railTemplate(
  tab: RailTab,
  vm: PoiViewModel,
  taskVm: TaskViewModel,
  width: number,
  colWidths: TaskColWidths,
  h: RailHandlers,
): TemplateResult {
  return html`
    <aside
      class="d9-rail"
      role="region"
      aria-label="Issues and tasks"
      style="width:${width}px"
    >
      ${railTabs(tab, h)}
      ${tab === "tasks" ? tasksHead(taskVm) : issuesHead(vm, h)}
      ${tab === "tasks"
        ? taskVm.total === 0
          ? taskVm.hasFullTaskCoverage
            ? html`<p class="d9-rail-empty">No tasks in this trace.</p>`
            : html`<p class="d9-rail-empty">
                No tasks recorded. This trace was built without
                <code>--cfg tokio_unstable</code>, so only tasks spawned
                through dial9's own helpers are tracked, and this trace has
                none.
              </p>`
          : taskTable(taskVm, colWidths, h)
        : vm.total === 0
          ? html`<p class="d9-rail-empty">No issues match this filter.</p>`
          : railTable(vm, h)}
      <div
        class="d9-rail-resize"
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize issues rail"
        title="Drag to resize; widen to read full spawn locations"
        @mousedown=${h.onResizeDown}
      ></div>
    </aside>
  `;
}

function issuesHead(vm: PoiViewModel, h: RailHandlers): TemplateResult {
  const positionLabel =
    vm.total === 0
      ? "None found"
      : `${vm.index >= 0 ? vm.index + 1 : 0}/${vm.total.toLocaleString()}`;
  // Never let a capped list read as the whole story: say plainly that only the
  // worst `retained` of `total` are navigable.
  const truncated = vm.retained < vm.total;
  return html`
    <div class="d9-rail-head">
      <div class="d9-rail-title">
        <span class="d9-rail-heading">ISSUES</span>
        <span class="d9-rail-pos" data-poi-position>${positionLabel}</span>
      </div>
      ${truncated
        ? html`<div
            class="d9-rail-truncated"
            data-poi-truncated
            title="Too many matches to list. The worst ${vm.retained.toLocaleString()} are shown, ranked by severity."
          >
            showing worst ${vm.retained.toLocaleString()} of
            ${vm.total.toLocaleString()}
          </div>`
        : nothing}
      <div class="d9-rail-controls">
        <label class="d9-rail-filter-label">
          <span class="d9-sr-only">Point-of-interest filter</span>
          <select
            class="d9-rail-filter"
            data-poi-filter
            aria-label="Point-of-interest filter"
            title="Which class of issue to list"
            @change=${(e: Event) => {
              const filter = parsePoiFilter((e.target as HTMLSelectElement).value);
              if (filter !== null) h.setFilter(filter);
            }}
          >
            ${POI_FILTERS.map(
              (f) => html`<option value=${f} ?selected=${f === vm.filter}>
                ${filterLabel(f)}
              </option>`,
            )}
          </select>
        </label>
        <span class="d9-rail-hint" title="Step issues with the n / p keys"
          ><kbd>n</kbd>/<kbd>p</kbd> step</span
        >
      </div>
    </div>
  `;
}

function tasksHead(vm: TaskViewModel): TemplateResult {
  const positionLabel =
    vm.total === 0
      ? "No tasks"
      : `${vm.index >= 0 ? vm.index + 1 : 0}/${vm.total.toLocaleString()}`;
  return html`
    <div class="d9-rail-head">
      <div class="d9-rail-title">
        <span class="d9-rail-heading">TASKS</span>
        <span class="d9-rail-pos" data-task-position>${positionLabel}</span>
      </div>
      <div class="d9-rail-controls">
        <span class="d9-rail-hint" title="Step tasks with the n / p keys"
          ><kbd>n</kbd>/<kbd>p</kbd> step</span
        >
      </div>
    </div>
  `;
}

function railTable(vm: PoiViewModel, h: RailHandlers): TemplateResult {
  return html`
    <div class="d9-rail-list">
      <table
        class="d9-rail-table"
        role="listbox"
        aria-label="Points of interest"
        aria-activedescendant=${vm.index >= 0 ? `d9-poi-${vm.index}` : ""}
      >
        <thead>
          <tr>
            <th scope="col" aria-hidden="true"></th>
            ${COLUMNS.map(
              (col) => html`<th scope="col">
                <button
                  type="button"
                  class=${classMap({
                    "d9-rail-sort": true,
                    on: vm.sortKey === col.key,
                  })}
                  title=${col.title}
                  aria-label=${col.title}
                  @click=${() => h.sortByColumn(col)}
                >
                  ${col.label}${vm.sortKey === col.key
                    ? html`<span aria-hidden="true"
                        >${vm.sortDir === "asc" ? " ^" : " v"}</span
                      >`
                    : ""}
                </button>
              </th>`,
            )}
          </tr>
        </thead>
        <tbody>
          ${vm.rows.map((row, i) => {
            // Rows are a window into the retained list; every id / selection
            // check / jump target is the ABSOLUTE index, so navigation keeps
            // working when the window is not anchored at 0.
            const abs = vm.windowStart + i;
            return html`
              <tr
                id=${`d9-poi-${abs}`}
                class=${classMap({ "d9-rail-row": true, on: abs === vm.index })}
                role="option"
                aria-selected=${abs === vm.index ? "true" : "false"}
                title=${`${row.worker} · ${row.kind} · ${row.time} · ${row.duration}`}
                @click=${() => h.jumpTo(abs)}
              >
                <td class="d9-rail-dot-cell">
                  <span
                    class=${classMap({
                      "d9-rail-dot": true,
                      [`sev-${row.severity}`]: true,
                    })}
                    aria-hidden="true"
                  ></span>
                </td>
                <td class="d9-rail-worker">${row.worker}</td>
                <td>${row.kind}</td>
                <td class="d9-rail-num">${row.time}</td>
                <td class="d9-rail-num">${row.duration}</td>
              </tr>
            `;
          })}
        </tbody>
      </table>
    </div>
  `;
}

/** Give the hovered cell a native title tooltip carrying its own text, but
 *  only when that text is actually clipped (scrollWidth > clientWidth). Empty
 *  otherwise, so cells that fully fit show nothing. Reads `textContent` so the
 *  tooltip is scoped to the hovered cell, never the whole row. */
function titleWhenClipped(e: Event): void {
  const el = e.currentTarget as HTMLElement;
  el.title = el.scrollWidth > el.clientWidth ? (el.textContent ?? "") : "";
}

function taskTable(
  vm: TaskViewModel,
  colWidths: TaskColWidths,
  h: RailHandlers,
): TemplateResult {
  // Fixed layout only once every column has a width (the first divider drag
  // seeds all six). A partial map (e.g. a hand-edited URL) keeps the table at
  // 100% width so the unspecified columns absorb the remainder.
  const widths = TASK_COLUMNS.map((col) => colWidths[col.key]);
  const fixed = widths.some((w) => w !== undefined);
  const complete = widths.every((w) => w !== undefined);
  const tableStyle =
    fixed && complete
      ? `width:${widths.reduce((sum, w) => (sum ?? 0) + (w ?? 0), 0)}px`
      : "";
  return html`
    <div class="d9-rail-list">
      <table
        class=${classMap({
          "d9-rail-table": true,
          "d9-task-table": true,
          "d9-cols-fixed": fixed,
        })}
        style=${tableStyle}
        role="listbox"
        aria-label="Tasks"
        aria-activedescendant=${vm.index >= 0 ? `d9-task-${vm.index}` : ""}
      >
        ${fixed
          ? html`<colgroup>
              ${TASK_COLUMNS.map((col) => {
                const w = colWidths[col.key];
                return w !== undefined
                  ? html`<col style="width:${w}px" />`
                  : html`<col />`;
              })}
            </colgroup>`
          : nothing}
        <thead>
          <tr>
            ${TASK_COLUMNS.map((col) => {
              // A "-" lifetime cell reads as "instant task" unless the header
              // says it went unrecorded.
              const coverage =
                col.key === "lifetime" ? vm.taskLifetimeCoverage : "all";
              const unrecorded = coverage === "none";
              // Availability comes from whether spawn events are present, so
              // this is about what reached the view, not what the trace holds.
              const title = unrecorded
                ? "No task lifetimes in view (task tracking off, or nothing spawned in the window)"
                : coverage === "partial"
                  ? "Task lifetimes were not captured for every task (untracked runtime, or spawned before the capture window)"
                  : col.title;
              return html`<th scope="col">
                <button
                  type="button"
                  class=${classMap({
                    "d9-rail-sort": true,
                    on: vm.sortKey === col.key,
                    "d9-col-unrecorded": unrecorded,
                  })}
                  title=${title}
                  aria-label=${title}
                  @click=${() => h.sortTaskByColumn(col)}
                >
                  ${col.label}${vm.sortKey === col.key
                    ? html`<span aria-hidden="true"
                        >${vm.sortDir === "asc" ? " ^" : " v"}</span
                      >`
                    : ""}
                </button>
                <span
                  class="d9-col-resize"
                  role="separator"
                  aria-orientation="vertical"
                  aria-label="Resize ${col.label} column"
                  title="Drag to resize the ${col.label} column; double-click to reset all columns"
                  @mousedown=${(e: MouseEvent) => h.onColResizeDown(e, col.key)}
                  @dblclick=${h.onColResetAll}
                ></span>
              </th>`;
            })}
          </tr>
        </thead>
        <tbody>
          ${vm.rows.map((row, i) => {
            const abs = vm.windowStart + i;
            return html`
              <tr
                id=${`d9-task-${abs}`}
                class=${classMap({ "d9-rail-row": true, on: abs === vm.index })}
                role="option"
                aria-selected=${abs === vm.index ? "true" : "false"}
                @click=${() => h.jumpToTask(abs)}
              >
                <td class="d9-task-id" title="" @pointerenter=${titleWhenClipped}>${row.id}</td>
                <td class="d9-task-loc" title="" @pointerenter=${titleWhenClipped}>${row.loc}</td>
                <td class="d9-rail-num" title="" @pointerenter=${titleWhenClipped}>${row.polls}</td>
                <td class="d9-rail-num" title="" @pointerenter=${titleWhenClipped}>${row.total}</td>
                <td class="d9-rail-num" title="" @pointerenter=${titleWhenClipped}>${row.longest}</td>
                <td class="d9-rail-num" title="" @pointerenter=${titleWhenClipped}>${row.lifetime}</td>
              </tr>
            `;
          })}
        </tbody>
      </table>
    </div>
  `;
}

/** A stable per-trace identity string for the sort-memo key. */
let idSeq = 0;
const idMap = new WeakMap<object, number>();
function idOf(trace: object): number {
  let id = idMap.get(trace);
  if (id === undefined) {
    id = ++idSeq;
    idMap.set(trace, id);
  }
  return id;
}
