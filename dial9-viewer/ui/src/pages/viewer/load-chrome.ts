// The load-chrome DOM view + wiring: thin glue over the DOM-free load
// controller (load-controller.ts). Renders the drop zone / loading view / drag
// feedback as a declarative lit-html template, forwards window drag-drop and
// file-picker events to the controller, and registers the controller's
// escapable surface in the shell's Escape cascade.
//
// The layer is a fixed, full-cover container appended to the document body (a
// sibling of the shell's #app) so the shell's own declarative re-render never
// clobbers it. It is modal while the drop zone / loading view is up
// (pointer-events block the trace behind); when only the drag overlay shows it
// is click-through so the `drop` still reaches the document listener. Loading
// itself runs on the main thread (loadTraceOnMainThread) so the parsed trace
// is never structured-cloned across a worker boundary; this module only
// surfaces it.

import { html, render, nothing, type TemplateResult } from "lit-html";
import {
  loadTraceOnMainThread,
  isLoadPerfEnabled,
  Dial9Creds,
  Dial9Session,
} from "../../lib/trace/index.js";
import type { ReparseRange } from "../../lib/trace/index.js";
import type { ViewerStore } from "../../store/store.js";
import type { EscCascade } from "./esc-cascade.js";
import {
  createLoadController,
  initialUrlLabel,
  type LoadChromeState,
  type LoadController,
  type LoadControllerDeps,
} from "./load-controller.js";

export interface LoadChromeOptions {
  /** The page store (its trace slice is the load target + hasTrace source). */
  store: ViewerStore;
  /** The shell's Escape cascade; the load surface registers itself. */
  esc: EscCascade;
  /** Show a load failure to the user (wired to the toast channel). */
  onError(message: string): void;
  /** Boot `?trace=` components; when present the layer auto-loads them. */
  initialUrls?: readonly string[];
  /** Toolbar label for the boot source (shown once it loads). */
  initialLabel?: string;
  /** Test seams. */
  document?: Document;
  startLoad?: LoadControllerDeps["startLoad"];
  confirm?: (message: string) => boolean;
}

export interface LoadChrome {
  /** Toolbar "New File" click: the controller runs the confirm. */
  requestNewFile(): void;
  /** The current source label for the toolbar (updates on each load). */
  currentLabel(): string;
  /**
   * Set/Clear Range: re-parse the loaded trace to `range` (null = full trace).
   * Runs off the main thread; a no-op before the first load.
   */
  reparseToRange(range: ReparseRange | null): void;
  /**
   * Boot `s_*` scope: show the loading view immediately (`scopeLoading`) while
   * the entry re-lists the scope to its file URLs, then `loadUrls` the result
   * through the credentialed worker path — or `scopeFailed` to drop back to the
   * chooser when resolution errors or yields nothing. Returns a predicate that
   * remains true only while this scope load is current.
   */
  scopeLoading(label: string): () => boolean;
  loadUrls(urls: readonly string[], label: string): void;
  scopeFailed(): void;
  dispose(): void;
}

/**
 * Mount the load chrome. Returns the handle the entry threads into the shell
 * (New File) and the toolbar (source label). Boot behavior: with `initialUrls`
 * the drop zone shows the loading view and auto-loads; without, the drop zone
 * waits for a drop / pick / demo (the resting empty state).
 */
export function mountLoadChrome(options: LoadChromeOptions): LoadChrome {
  const doc = options.document ?? document;
  const store = options.store;

  const container = doc.createElement("div");
  container.className = "d9-load-layer";
  doc.body.appendChild(container);

  // Forward declarations so the event handlers below (defined before the
  // controller is built) can reference them. The label shown in the toolbar
  // is committed on a SUCCESSFUL load so a failed/aborted replace never
  // mislabels the still-resident old trace.
  let controller: LoadController;
  let committedLabel = options.initialLabel ?? "";
  let pendingLabel = options.initialLabel ?? "";

  // The file input and the lit-html render target are persistent siblings:
  // lit-html owns the render target's children, so the input lives OUTSIDE it
  // (a render pass would otherwise clobber a child input).
  const fileInput = doc.createElement("input");
  fileInput.type = "file";
  fileInput.accept = ".bin,.gz";
  fileInput.className = "d9-load-file-input";
  fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    // Reset so re-picking the SAME file still fires change.
    fileInput.value = "";
    if (file) {
      pendingLabel = file.name;
      controller.loadFile(file);
    }
  });
  container.appendChild(fileInput);
  const renderTarget = doc.createElement("div");
  renderTarget.className = "d9-load-render";
  container.appendChild(renderTarget);

  const deps: LoadControllerDeps = {
    hasTrace: () => store.getState().trace.trace !== null,
    startLoad:
      options.startLoad ??
      ((urls, opts) => loadTraceOnMainThread(store, urls, opts)),
    confirm: options.confirm ?? ((message) => window.confirm(message)),
    onError: options.onError,
    onChange: renderLayer,
    onLoaded: () => {
      committedLabel = pendingLabel;
    },
    onTiming: (timing) => {
      if (!isLoadPerfEnabled()) return;
      const fetchMs =
        timing.fetchDoneMs !== null ? timing.fetchDoneMs - timing.startMs : null;
      const parseMs = timing.parseDoneMs - (timing.fetchDoneMs ?? timing.startMs);
      console.info(
        `[dial9 loadPerf] loader-reported: ${fetchMs !== null ? `fetch ${fetchMs.toFixed(0)}ms  ` : ""}` +
          `parse ${parseMs.toFixed(0)}ms  total ${(timing.parseDoneMs - timing.startMs).toFixed(0)}ms  ` +
          `(${timing.mode}, ${timing.events.toLocaleString()} events, ${(timing.bytes / 1e6).toFixed(1)} MB)`,
      );
    },
    credsMissing: () => !Dial9Creds.has(),
    headers: () => {
      return Dial9Session.headers(Dial9Creds.headers());
    },
  };
  controller = createLoadController(deps);

  const unregisterEsc = options.esc.register(controller.escSurface);

  // ── Document-level drag-and-drop ──────────────────────────────────────────
  const isFileDrag = (e: DragEvent): boolean =>
    e.dataTransfer?.types.includes("Files") ?? false;

  const onDragEnter = (e: DragEvent): void => {
    if (!isFileDrag(e)) return;
    e.preventDefault();
    controller.dragEnter();
  };
  const onDragOver = (e: DragEvent): void => {
    if (isFileDrag(e)) e.preventDefault(); // required so `drop` fires
  };
  const onDragLeave = (e: DragEvent): void => {
    if (!isFileDrag(e)) return;
    controller.dragLeave();
  };
  const onDrop = (e: DragEvent): void => {
    if (!isFileDrag(e)) return;
    e.preventDefault();
    controller.endDrag();
    const file = e.dataTransfer?.files[0];
    if (file) {
      pendingLabel = file.name;
      controller.loadFile(file);
    }
  };
  doc.addEventListener("dragenter", onDragEnter);
  doc.addEventListener("dragover", onDragOver);
  doc.addEventListener("dragleave", onDragLeave);
  doc.addEventListener("drop", onDrop);

  // ── Template ──────────────────────────────────────────────────────────────
  function template(s: LoadChromeState): TemplateResult {
    return html`
      ${s.section === "loading" ? loadingView(s) : nothing}
      ${s.section === "chooser" ? chooser(s) : nothing}
      ${s.dragActive && s.hasTrace ? replaceOverlay() : nothing}
    `;
  }

  function chooser(s: LoadChromeState): TemplateResult {
    const dropClass = s.dragActive && !s.hasTrace ? "d9-drop-zone dragover" : "d9-drop-zone";
    return html`
      <div
        class=${dropClass}
        role="button"
        tabindex="0"
        aria-label="Load a trace file"
        @click=${(): void => fileInput.click()}
        @keydown=${onDropZoneKey}
      >
        ${s.canDismiss
          ? html`<button
              type="button"
              class="d9-load-close"
              aria-label="Close and return to the loaded trace"
              @click=${(e: Event): void => {
                e.stopPropagation();
                controller.dismiss();
              }}
            >
              &times;
            </button>`
          : nothing}
        <div class="d9-drop-title">
          Drop a <code>.bin</code> or <code>.bin.gz</code> trace file here or
          click to open
        </div>
        <div class="d9-drop-sub">Expects D9TF binary format</div>
        <a
          href="#"
          class="d9-load-demo"
          @click=${(e: Event): void => {
            e.preventDefault();
            e.stopPropagation();
            pendingLabel = "demo-trace.bin";
            controller.loadDemo();
          }}
          >or load demo trace</a
        >
        ${s.canDismiss
          ? html`<div class="d9-drop-sub">or press Esc to keep the current trace</div>`
          : nothing}
      </div>
    `;
  }

  function loadingView(s: LoadChromeState): TemplateResult {
    return html`
      <div class="d9-load-view" role="status" aria-live="polite">
        <div class="d9-load-spinner" aria-hidden="true"></div>
        <div class="d9-load-label">${s.progressLabel}${s.elapsedLabel}</div>
        <button
          type="button"
          class="d9-load-back"
          @click=${(): void => controller.cancel()}
        >
          Back
        </button>
        <div class="d9-drop-sub">or press Escape</div>
      </div>
    `;
  }

  function replaceOverlay(): TemplateResult {
    return html`
      <div class="d9-drag-overlay">
        <div class="d9-drag-overlay-msg">
          Drop trace file to load
          <small>Replaces the current trace</small>
        </div>
      </div>
    `;
  }

  function onDropZoneKey(e: KeyboardEvent): void {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      fileInput.click();
    }
  }

  function renderLayer(): void {
    const s = controller.getState();
    // Modal (blocks the trace) only while the drop zone / loading view is up;
    // a bare drag overlay stays click-through so `drop` reaches the document.
    const modal = s.section !== "closed";
    container.classList.toggle("open", modal || (s.dragActive && s.hasTrace));
    container.classList.toggle("modal", modal);
    render(template(s), renderTarget);
  }

  renderLayer();

  // Boot: auto-load `?trace=` components, else leave the drop zone waiting.
  if (options.initialUrls && options.initialUrls.length > 0) {
    controller.loadUrls(options.initialUrls, initialUrlLabel(options.initialUrls.length));
  }

  return {
    requestNewFile: () => {
      // A New-File click loads whatever the user then picks/drops; default the
      // pending label so a same-name file still reads sensibly until picked.
      pendingLabel = committedLabel;
      controller.requestNewFile();
    },
    currentLabel: () => committedLabel,
    reparseToRange: (range) => controller.reparse(range),
    scopeLoading: (label) => {
      const token = controller.showLoading(label);
      return () => controller.isCurrentLoad(token);
    },
    loadUrls: (urls, label) => {
      pendingLabel = label;
      controller.loadUrls(urls, label);
    },
    scopeFailed: () => controller.cancel(),
    dispose(): void {
      controller.dispose();
      unregisterEsc();
      doc.removeEventListener("dragenter", onDragEnter);
      doc.removeEventListener("dragover", onDragOver);
      doc.removeEventListener("dragleave", onDragLeave);
      doc.removeEventListener("drop", onDrop);
      container.remove();
    },
  };
}
