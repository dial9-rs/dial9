// The persistent status bar: selection state, the visible time range, segment
// fetch/parse progress, a copy-link button, and the key hints. It fills the
// shell's `.d9-status` footer.
//
// A pure view model (statusViewModel, Node-testable over store state) plus an
// imperative mount. The text content is a lit-html render into a host the
// component owns; the copy-link button is appended once as a sibling so its
// "Copied" flash survives the text re-renders.

import { html, render, nothing, type TemplateResult } from "lit-html";
import type { ViewerStore } from "../../store/store.js";
import type { StoreState, SegmentsSlice } from "../../types/state.js";
import { formatHumanDuration } from "../../lib/trace/index.js";
import { mountCopyLink } from "../../lib/url/index.js";
import type { CopyLinkHandle } from "../../lib/url/index.js";

/** Selection descriptor for the status line. */
export interface StatusSelection {
  hasSelection: boolean;
  label: string;
}

/** Segment fetch/parse progress descriptor. */
export interface StatusProgress {
  /** null when no segment windowing is active (empty segments slice). */
  label: string | null;
  /** True while any segment is fetching/parsing (drives the spinner). */
  active: boolean;
}

/** Everything the status-bar template needs, derived from store state. */
export interface StatusViewModel {
  selection: StatusSelection;
  viewRangeLabel: string;
  progress: StatusProgress;
}

const KEY_HINTS = "/ search · n/p POI · f fit · z undo zoom · g goto · ? help";

/** Relative offset from the trace start, e.g. `+1.23s`. */
function relOffset(ns: number, minTs: number): string {
  return `+${((ns - minTs) / 1e9).toFixed(2)}s`;
}

/** Selection line: the primary highlighted entity, or "No selection". */
export function selectionState(sel: StoreState["selection"]): StatusSelection {
  if (sel.selectedTaskId !== null) {
    return {
      hasSelection: true,
      label: `Task 0x${sel.selectedTaskId.toString(16)} selected`,
    };
  }
  if (sel.spanFocus !== null) {
    return { hasSelection: true, label: `Span ${sel.spanFocus.spanId} focused` };
  }
  if (sel.pinnedEvent !== null) {
    return { hasSelection: true, label: `Event "${sel.pinnedEvent.name}" pinned` };
  }
  return { hasSelection: false, label: "No selection" };
}

/**
 * Segment fetch/parse progress from the segments slice: how many segments are
 * resident (parsed) of the listing, plus any in-flight fetch and any oversized
 * (can-never-fit) segments. null when segment windowing is not active
 * (whole-trace path).
 */
export function segmentProgress(segments: SegmentsSlice): StatusProgress {
  const total = segments.segments.size;
  if (total === 0) return { label: null, active: false };
  let parsed = 0;
  let fetching = 0;
  let oversized = 0;
  for (const entry of segments.segments.values()) {
    if (entry.state === "parsed") parsed += 1;
    else if (entry.state === "fetching") fetching += 1;
    else if (entry.state === "oversized") oversized += 1;
  }
  let label = `Segments ${parsed}/${total} loaded`;
  if (fetching > 0) label += ` · fetching ${fetching}`;
  if (oversized > 0) label += ` · ${oversized} oversized`;
  return { label, active: fetching > 0 };
}

/** Build the status view model from store state (pure). */
export function statusViewModel(state: StoreState): StatusViewModel {
  const hasTrace = state.trace.trace !== null;
  const { viewStart, viewEnd, minTs } = state.viewport;
  const viewRangeLabel = hasTrace
    ? `view ${relOffset(viewStart, minTs)} - ${relOffset(viewEnd, minTs)} ` +
      `(${formatHumanDuration(Math.max(0, viewEnd - viewStart))})`
    : "no trace loaded";
  return {
    selection: selectionState(state.selection),
    viewRangeLabel,
    progress: segmentProgress(state.segments),
  };
}

/** The status-bar text template (everything except the imperative copy button). */
function statusTemplate(vm: StatusViewModel, onClear: () => void): TemplateResult {
  return html`
    <span class="d9-status-sel" data-status-selection>
      ${vm.selection.label}
      ${vm.selection.hasSelection
        ? html`<button
            type="button"
            class="d9-status-clear"
            title="Clear selection (Esc)"
            aria-label="Clear selection"
            @click=${onClear}
          >
            x
          </button>`
        : nothing}
    </span>
    <span class="d9-status-view" data-status-view>${vm.viewRangeLabel}</span>
    ${vm.progress.label !== null
      ? html`<span
          class="d9-status-progress ${vm.progress.active ? "active" : ""}"
          data-status-progress
          role="status"
          >${vm.progress.label}</span
        >`
      : nothing}
    <span class="d9-status-hints">${KEY_HINTS}</span>
  `;
}

/** Callbacks the status bar needs from the page entry. */
export interface StatusBarDeps {
  /** Clear affordance: clear the selection highlight state. */
  clearSelection(): void;
  /** Run before copy-link reads the URL (sync-binding flush seam). */
  beforeCopyLink?(): void;
  /** URL text supplier for copy-link; defaults to the live location href. */
  copyLinkText?(): string;
  /** Clipboard writer; injectable for tests. */
  copy?(text: string): Promise<void>;
}

export interface MountedStatusBar {
  dispose(): void;
}

/**
 * Mount the status bar into `region` (the shell's `.d9-status` footer). Owns a
 * text host it re-renders on store changes and a copy-link button appended
 * once. Returns a teardown handle.
 */
export function createStatusBar(
  region: HTMLElement,
  store: ViewerStore,
  deps: StatusBarDeps,
): MountedStatusBar {
  const doc = region.ownerDocument;
  const textHost = doc.createElement("div");
  textHost.className = "d9-status-main";
  region.appendChild(textHost);

  const onClear = (): void => deps.clearSelection();

  function renderPass(): void {
    const state = store.getState();
    render(statusTemplate(statusViewModel(state), onClear), textHost);
  }

  // The copy-link button: appended as a sibling so its flash state is never
  // reset by the text re-render. beforeCopy flushes any pending view-state write
  // (wired by the page when the URL sync binding exists).
  const copyOptions: Parameters<typeof mountCopyLink>[1] = {};
  if (deps.beforeCopyLink !== undefined) copyOptions.beforeCopy = deps.beforeCopyLink;
  if (deps.copyLinkText !== undefined) copyOptions.getText = deps.copyLinkText;
  if (deps.copy !== undefined) copyOptions.copy = deps.copy;
  const copyLink: CopyLinkHandle = mountCopyLink(region, copyOptions);

  const unsubscribe = store.subscribe(
    ["trace", "viewport", "selection", "segments"],
    () => renderPass(),
  );
  renderPass();

  return {
    dispose(): void {
      unsubscribe();
      copyLink.dispose();
      textHost.remove();
    },
  };
}
