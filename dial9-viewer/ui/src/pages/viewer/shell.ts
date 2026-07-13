// src/pages/viewer/shell.ts - the viewer page shell (T21; DECIDED layout,
// docs/tickets/chunk-2-viewer.md header; mocks concept-1.html + concept-2.html).
//
// The whole viewer chrome as ONE declarative lit-html render driven by store
// state (N17: no DOM mutation outside a state-driven render). The layout is
// the concept-1 hybrid: a top toolbar row, an overview minimap slot, a body
// splitting a unified time-aligned TRACK COLUMN (left) from a persistent
// INSPECTOR sidebar (right), and a status bar slot along the bottom.
//
// T21 owns the SHELL and the app chrome (features/02 A shell/global, T help,
// U toasts, W reachability). Everything else is a labeled placeholder SLOT
// that a later ticket fills: tracks T22-T30, inspector T31, toolbar+rail
// T33, minimap+status T35. The slots carry ARIA landmarks and labels from
// the start (04 F1 axe fixes) so those tickets inherit an accessible frame.
//
// Landmark + tab order (DoD "toolbar -> minimap -> tracks -> inspector"):
// the regions appear in that DOM order, and the one focusable representative
// of each (toolbar help button, minimap region, track column, inspector)
// makes the Tab sequence follow the triage task flow (K6). No positive
// tabindex is used, so DOM order IS tab order.

import { html, render, nothing, type TemplateResult } from "lit-html";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState } from "../../types/state.js";
import { tracksTemplate, sizeTracks, type TracksViewModel } from "./tracks.js";
import { deriveAxisInputs } from "./axis.js";
import { deriveCpuInputs } from "./cpu.js";
import { createSpansTrack, type SpansTrackController } from "./spans-track.js";
import { createQueueTrack, type QueueTrackController } from "./queue-track.js";
import {
  createTaskDetailTrack,
  type TaskDetailTrackController,
} from "./task-detail-track.js";
import { createEventsTrack, type EventsTrackController } from "./events-track.js";
import { createToolbar, type ToolbarController, type ToolbarDeps } from "./toolbar.js";
import { createIssuesRail, type IssuesRailController } from "./issues-rail.js";
import {
  createTrackManageActions,
  type TrackManageActions,
} from "./track-management.js";
import type { KeyBinding } from "../../lib/interact/keyboard.js";

/** Callbacks the shell chrome needs from the page entry. */
export interface ShellDeps extends ToolbarDeps {
  /** Toggle the help overlay (bound to the `?` button and key). */
  toggleHelp(): void;
  /**
   * Human label for the loaded trace source (toolbar file info). A getter so
   * it tracks the load chrome's current source across drop/pick/demo/URL
   * loads (T34) rather than freezing at the boot value.
   */
  sourceLabel(): string;
  /**
   * Toolbar "New File" (features/02 B15): open the load chrome. The load
   * chrome runs the S3 confirm before discarding a loaded trace (T34). Absent
   * on mounts without load chrome (the button then does not render).
   */
  onNewFile?(): void;
}

/** Everything the shell template needs, derived from store state. The file-
 * info fields moved to the toolbar controller (T33 owns C1); the shell keeps
 * only the track inputs + the status-bar labels. */
interface ShellViewModel extends TracksViewModel {
  // File-info fields moved to the toolbar controller (T33 owns C1); the
  // selection/range/duration status fields moved to the mounted status-bar
  // component (T35 reads them from the store). The shell view model now carries
  // only the track inputs (TracksViewModel).
}

/** The persistent interaction hint chips (F5): always visible, never
 * auto-hidden (they replace the legacy load-time hint toasts, U3). */
const HINT_CHIPS: readonly string[] = [
  "Shift+drag = select region",
  "Option+drag = zoom",
  "/ search",
  "n / p points of interest",
  "f fit",
  "? help",
];

/** Build the view model for a render pass from the current store state. */
function viewModel(state: StoreState): ShellViewModel {
  const trace = state.trace.trace;
  const hasTrace = trace !== null;
  const taskSelected = state.selection.selectedTaskId !== null;
  const { viewStart, viewEnd } = state.viewport;

  return {
    hasTrace,
    taskSelected,
    viewStart,
    viewEnd,
    axis: deriveAxisInputs(state),
    cpu: deriveCpuInputs(state),
    // Track management (T36): the order + collapse map the track column reads.
    trackOrder: state.uiPrefs.trackOrder,
    collapsed: state.uiPrefs.collapsed,
  };
}

/** F4 empty state: teach the next steps instead of a bare drop target. */
function emptyStateTemplate(): TemplateResult {
  return html`
    <div class="d9-empty" role="note">
      <h2>No trace loaded</h2>
      <p>Open a trace to see worker lanes and time-aligned analysis tracks.</p>
      <ul class="d9-empty-steps">
        <li>Drop a <code>.bin</code> or <code>.bin.gz</code> trace file here</li>
        <li>Load the bundled demo trace</li>
        <li>Open a shared <code>?trace=</code> link</li>
      </ul>
      <p class="d9-empty-hint">
        Loading is wired by the file-load chrome (T34); this is the shell's
        teaching state.
      </p>
    </div>
  `;
}

function hintChipsTemplate(): TemplateResult {
  return html`
    <div class="d9-hint-chips" role="group" aria-label="Interaction hints">
      ${HINT_CHIPS.map((c) => html`<span class="d9-chip">${c}</span>`)}
    </div>
  `;
}

/**
 * The persistent inspector landmark (T31). Rendered EMPTY here (like the toast
 * region): the T31 inspector component (mountInspector) owns the tabs + body +
 * resize handle and renders them imperatively into this aside, so the shell's
 * declarative re-renders never clobber the inspector's interior (no child
 * bindings on this node = lit-html leaves imperatively-added children intact).
 * The landmark + role + tabindex live here so the A11y frame and tab order
 * (toolbar -> minimap -> tracks -> inspector) exist before the component mounts.
 */
function inspectorTemplate(): TemplateResult {
  return html`
    <aside
      class="d9-inspector"
      role="complementary"
      aria-label="Inspector"
      tabindex="0"
    ></aside>
  `;
}

/** The full shell template for one render pass. */
function shellTemplate(
  vm: ShellViewModel,
  state: StoreState,
  deps: ShellDeps,
  toolbar: ToolbarController,
  rail: IssuesRailController,
  trackActions: TrackManageActions,
  spansTrack: SpansTrackController,
  taskDetailTrack: TaskDetailTrackController,
  eventsTrack: EventsTrackController,
  queueTrack: QueueTrackController,
): TemplateResult {
  return html`
    <header class="d9-toolbar" role="banner">
      <h1 class="d9-app-title">dial9 trace viewer</h1>
      ${toolbar.fileInfoTemplate(state, deps.sourceLabel())}
      <span class="d9-toolbar-slot" role="group" aria-label="Analysis actions">
        ${toolbar.analysisTemplate(state, deps.sourceLabel())}
      </span>
      <span class="d9-toolbar-spacer"></span>
      <span class="d9-toolbar-slot" role="group" aria-label="Time display">
        ${toolbar.timeTemplate(state)}
      </span>
      ${vm.hasTrace && deps.onNewFile !== undefined
        ? html`<button
            type="button"
            class="d9-new-file-btn"
            @click=${deps.onNewFile}
          >
            New File
          </button>`
        : nothing}
      <button
        type="button"
        class="d9-help-btn"
        aria-label="Help"
        @click=${deps.toggleHelp}
      >
        ?
      </button>
    </header>

    <!-- Overview minimap (T35): an empty, focusable host that mountMinimap
         fills with its canvas + coverage badge. Kept empty in the template so
         the shell's declarative re-renders never orphan the component's
         imperative children (the toast/legend technique). -->
    <div
      class="d9-minimap"
      role="region"
      aria-label="Overview minimap"
      tabindex="0"
    ></div>

    <div class="d9-body">
      ${rail.template(state)}
      <main
        class="d9-track-column"
        aria-label="Trace timeline"
        tabindex="0"
      >
        ${hintChipsTemplate()}
        ${vm.hasTrace
          ? tracksTemplate(
              vm,
              trackActions,
              spansTrack,
              taskDetailTrack,
              eventsTrack,
              queueTrack,
            )
          : emptyStateTemplate()}
      </main>
      ${inspectorTemplate()}
    </div>

    <!-- Status bar (T35): an empty host that createStatusBar fills (selection
         line, view range, segment progress, copy-link button, key hints).
         Empty in the template for the same reason as the minimap host. -->
    <footer class="d9-status" role="contentinfo"></footer>

    <!-- Toast channel (features/02 U): imperative children, so no dynamic
         template content here - createToasts owns it. role=status makes
         errors/info audible (an axe gap the legacy container had). -->
    <div
      class="d9-toast-region"
      role="status"
      aria-live="polite"
      aria-label="Notifications"
    ></div>
  `;
}

export interface MountedShell {
  /** The toast container element (pass to createToasts). */
  toastRegion: HTMLElement;
  /** The track column element (canvas host for sizing). */
  trackColumn: HTMLElement;
  /**
   * Key bindings the toolbar + issues rail contribute to the unified router
   * (T20): the rail's `n`/`p` POI step and the toolbar's `g` goto-time. The
   * entry registers them alongside the lane-interaction bindings.
   */
  keyBindings: readonly KeyBinding[];
  /** The persistent inspector landmark (pass to mountInspector, T31). */
  inspectorRegion: HTMLElement;
  /** The overview-minimap host (pass to mountMinimap, T35). */
  minimapRegion: HTMLElement;
  /** The status-bar footer host (pass to createStatusBar, T35). */
  statusRegion: HTMLElement;
  /** Force one render+size pass (used after mount and on resize). */
  refresh(): void;
  /** Tear down the store subscription and resize listener. */
  dispose(): void;
}

/**
 * Mount the shell into `root`, wired to `store`. Subscribes to the slice
 * set that changes the chrome (trace/viewport/selection/uiPrefs) and
 * renders + sizes track canvases each frame INSIDE the store's notification
 * tick (the scheduler is the only place renders run and layout reads are
 * batched, F2/F3). Returns handles the entry needs (toast region, teardown).
 */
export function mountShell(
  root: HTMLElement,
  store: ViewerStore,
  deps: ShellDeps,
): MountedShell {
  root.classList.add("d9-viewer");

  // The spans track (T26) and custom-events track (T27) are store-wired
  // content components: created once so their derived caches + name->color
  // assignment live across renders. Other content tracks (T22/T28-T30) mount
  // the same way as they land.
  const spansTrack = createSpansTrack(store);
  // The queue track (T29): store-wired like spans, created once so its
  // trace-keyed series cache + drag state live across renders (F5).
  const queueTrack = createQueueTrack(store);
  // The task-detail track (T30): store-wired like spans, created once so its
  // selection-keyed derivation cache lives across renders (F5). Its row is
  // only rendered while a task is selected (selectionOnly, N1).
  const taskDetailTrack = createTaskDetailTrack(store);
  const eventsTrack = createEventsTrack(store);
  // Toolbar (file info / analysis / time) and the issues rail (T33): store-
  // wired controllers filling the toolbar slots + the body's left column.
  const toolbar = createToolbar(store, deps);
  const rail = createIssuesRail(store);
  // Track management (T36): collapse/reorder dispatchers the caret + grip in
  // the track column bind to. Persistence (hydrate on boot + save on change)
  // is wired at the page entry (main.ts) so the store itself stays pure.
  const trackActions = createTrackManageActions(store);

  function renderPass(): void {
    const state = store.getState() as StoreState;
    const vm = viewModel(state);
    render(
      shellTemplate(
        vm,
        state,
        deps,
        toolbar,
        rail,
        trackActions,
        spansTrack,
        taskDetailTrack,
        eventsTrack,
        queueTrack,
      ),
      root,
    );
    const column = root.querySelector<HTMLElement>(".d9-track-column");
    if (column && vm.hasTrace) {
      sizeTracks(column, vm, spansTrack, taskDetailTrack, eventsTrack, queueTrack);
    }
  }

  // Render the chrome whenever any chrome-affecting slice changes. The
  // shell is chrome, so it renders declaratively from state; track/inspector
  // CONTENT tickets add their own slice subscriptions against this store.
  const unsubscribe = store.subscribe(
    ["trace", "viewport", "selection", "poi", "uiPrefs"],
    () => renderPass(),
  );

  // Resize reflow (features/02 A15): re-render on window resize so the track
  // canvases refit. Dispatch a no-op store update so the render still runs
  // through the scheduler (never a direct out-of-tick render, N18).
  const onResize = (): void => {
    store.update("viewport", {});
  };
  window.addEventListener("resize", onResize);

  // Mount bootstrap: one synchronous render so the region skeleton exists
  // before the entry queries it. Every SUBSEQUENT render is store-driven
  // through the subscription above (the scheduler owns those); this single
  // build-the-DOM paint is the one-time mount path.
  renderPass();

  const toastRegion = ensureRegion(root, ".d9-toast-region");
  const trackColumn = ensureRegion(root, ".d9-track-column");
  const inspectorRegion = ensureRegion(root, ".d9-inspector");
  const minimapRegion = ensureRegion(root, ".d9-minimap");
  const statusRegion = ensureRegion(root, ".d9-status");

  return {
    toastRegion,
    trackColumn,
    keyBindings: [...rail.keyBindings, ...toolbar.keyBindings],
    inspectorRegion,
    minimapRegion,
    statusRegion,
    refresh: () => store.update("viewport", {}),
    dispose(): void {
      unsubscribe();
      window.removeEventListener("resize", onResize);
      spansTrack.dispose();
      queueTrack.dispose();
      taskDetailTrack.dispose();
      eventsTrack.dispose();
      toolbar.dispose();
      rail.dispose();
    },
  };
}

/** Query a shell region that the first render created; throw if missing. */
function ensureRegion(root: HTMLElement, selector: string): HTMLElement {
  const el = root.querySelector<HTMLElement>(selector);
  if (el === null) {
    throw new Error(`viewer shell: region ${selector} not rendered`);
  }
  return el;
}
