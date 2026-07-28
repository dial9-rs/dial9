// The load-chrome STATE MACHINE: DOM-free so it is fully node-testable; the
// DOM view (load-chrome.ts) reflects getState() and forwards events to the
// actions here.
//
// It owns the load section's lifecycle - drop zone (chooser) -> loading ->
// closed - the drag-feedback counter, the live elapsed timer, the progress
// labels, and:
//
//   * new-file-confirm: requestNewFile() runs deps.confirm() before it opens
//     the chooser over a loaded trace, so a stray "New File" click can never
//     silently discard the current analysis. Opening the chooser is itself
//     non-destructive - the worker pipeline only replaces the store's `trace`
//     slice on SUCCESS, so a cancelled/failed replace keeps the old trace
//     resident.
//
//   * dismissal: a chooser opened over a loaded trace is dismissible
//     (canDismiss), so Esc AND the visible close control return to the trace;
//     reopening is just another requestNewFile().
//
// The load itself is injected as `startLoad` (default:
// loadTraceOnMainThread) so tests drive a scripted handle. File drops feed
// the loader an object URL (the core sniffs the gzip magic on the fetched
// blob, so both .bin and .bin.gz decode).

import { ESC_PRIORITY } from "./esc-cascade.js";
import type { EscapableSurface } from "./esc-cascade.js";
import type {
  TraceWorkerProgress,
  TraceWorkerTiming,
  ReparseRange,
} from "../../lib/trace/index.js";
import { isRangeActive } from "../../lib/trace/index.js";

/** The load section's discrete states. */
export type LoadSection = "closed" | "chooser" | "loading";

/** Everything the DOM view needs for one render pass. */
export interface LoadChromeState {
  section: LoadSection;
  /** A trace is currently resident in the store. */
  hasTrace: boolean;
  /** The chooser can be dismissed back to a loaded trace. */
  canDismiss: boolean;
  /** A file drag is in progress over the window. */
  dragActive: boolean;
  /** Current phase/progress text while loading. */
  progressLabel: string;
  /** Live wall-clock elapsed suffix while loading, e.g. " - 1.3s". */
  elapsedLabel: string;
}

/** A minimal handle over one in-flight load (satisfied by WorkerTraceLoad). */
export interface LoadHandle {
  done: Promise<unknown>;
  abort(): void;
}

export interface StartLoadOptions {
  onProgress(progress: TraceWorkerProgress): void;
  /** Whether this parse replaces the source or reparses the current source. */
  kind: "source" | "reparse";
  /** Same-origin credential headers; omitted for local file loads. */
  headers?: Record<string, string>;
  /**
   * Set-Range reparse window (absolute ns): the worker filters events to this
   * range while re-parsing the retained buffer. Omitted for a normal load and
   * for Clear Range (full trace).
   */
  startTime?: number;
  endTime?: number;
}

/** Start a load over one or more URLs; returns its handle. Injected so tests
 * drive a scripted worker without a real Worker. */
export type StartLoad = (
  urls: readonly string[],
  opts: StartLoadOptions,
) => LoadHandle;

export interface LoadControllerDeps {
  /** True when a parsed trace is resident (reads the store's trace slice). */
  hasTrace(): boolean;
  /** Kick off a load (default: loadTraceInWorker bound to the page store). */
  startLoad: StartLoad;
  /** Confirm gate for New-File-over-a-loaded-trace; window.confirm. */
  confirm(message: string): boolean;
  /** Surface a load failure (the page shows it as an error toast). */
  onError(message: string): void;
  /** Notify the view that getState() changed (re-render). */
  onChange(): void;
  /**
   * Fired once, synchronously, when a load SUCCEEDS - after the store's trace
   * slice is populated and before the section closes. The page commits the
   * loaded source's toolbar label here (order-safe: it runs in the load's
   * microtask, ahead of the store scheduler's next render frame).
   */
  onLoaded?(): void;
  /**
   * The completed load's phase timing, when the loader reported any. The
   * main-thread loader also emits its own User Timing marks (lib/trace/
   * load-perf.ts); this hook is what carries the numbers back from loaders that
   * time the parse somewhere this page cannot mark - notably the worker, whose
   * parse happens off-thread entirely.
   */
  onTiming?(timing: TraceWorkerTiming): void;
  /** Wall clock for the elapsed timer; default performance.now. */
  now?(): number;
  /** Interval scheduler for the 250ms elapsed tick; default setInterval. */
  setTimer?(fn: () => void, ms: number): unknown;
  clearTimer?(handle: unknown): void;
  /**
   * Frame scheduler for coalescing parse-progress renders; default
   * requestAnimationFrame. Injected in tests so progress renders are
   * deterministic rather than frame-timed.
   */
  scheduleFrame?(fn: () => void): void;
  /** Build a fetchable URL for a dropped/picked file; default createObjectURL. */
  createObjectUrl?(file: Blob): string;
  revokeObjectUrl?(url: string): void;
  /** True when a creds module is present but has no credentials. */
  credsMissing?(): boolean;
  /** Same-origin credential headers for URL loads. */
  headers?(): Record<string, string> | undefined;
}

/** Message shown by the New-File confirm gate. */
export const NEW_FILE_CONFIRM_MESSAGE =
  "Open a different trace? Your current view stays until you load a new file - " +
  "press Esc to return to it.";

/** The wall-clock elapsed suffix updates 4x/second. */
const ELAPSED_TICK_MS = 250;

/**
 * Format the load progress line from a worker progress message. Uses ASCII
 * separators (" - ", "..."); the phase words and numeric content (percent, MB,
 * k-events) are preserved.
 */
export function progressLabel(p: TraceWorkerProgress): string {
  if (p.phase === "fetching") {
    return p.urlCount > 1 ? `Fetching ${p.urlCount} traces...` : "Fetching...";
  }
  const events = `${Math.floor(p.eventCount / 1000)}k events`;
  if (p.totalBytes) {
    const pct = Math.floor((p.bytesRead / p.totalBytes) * 100);
    return `Parsing: ${pct}% - ${events}`;
  }
  const mb = (p.bytesRead / (1024 * 1024)).toFixed(1);
  return `Parsing: ${mb} MB - ${events}`;
}

/** The initial label for a URL load before any worker progress arrives. */
export function initialUrlLabel(urlCount: number): string {
  return urlCount > 1 ? `Loading ${urlCount} traces...` : "Loading trace...";
}

/**
 * Build the user-facing error message for a failed load: the HTTP-401
 * credentials hint when a creds module is present but empty, else a generic
 * line carrying the raw message.
 */
export function loadErrorMessage(
  rawMessage: string,
  credsMissing: boolean,
): string {
  if (/HTTP 401/.test(rawMessage) && credsMissing) {
    return (
      "This trace requires AWS credentials. Open it from the dial9 home page " +
      "after applying your credentials, or this tab will not have them."
    );
  }
  return `Could not load trace: ${rawMessage}`;
}

export interface LoadController {
  getState(): LoadChromeState;
  /** Whether another browser can reproduce the successfully loaded source. */
  isSourceShareable(): boolean;
  /** The escapable-surface spec to register in the shell's esc cascade. */
  readonly escSurface: EscapableSurface;
  /** Toolbar "New File": confirm, then open the chooser. */
  requestNewFile(): void;
  /** Dismiss the chooser back to the loaded trace. No-op if none. */
  dismiss(): void;
  /** Load a dropped/picked file via an object URL. */
  loadFile(file: Blob & { name?: string }): void;
  /** Load one or more URLs (boot `?trace=`). `label` is the initial label. */
  loadUrls(urls: readonly string[], label: string, range?: ReparseRange): void;
  /**
   * Show the loading view without starting a load, for the async gap while a
   * boot `s_*` scope is resolved to its URLs. The caller follows with loadUrls
   * (success) or cancel (failure/empty).
   */
  showLoading(label: string): number;
  /** Whether `token` still names the current loading operation. */
  isCurrentLoad(token: number): boolean;
  /**
   * Set/Clear Range: re-parse the retained buffer filtered to `range` (null /
   * open bounds = full trace). No-op before the first load completes.
   */
  reparse(range: ReparseRange | null): void;
  /** Load the bundled demo trace. */
  loadDemo(): void;
  /** Cancel the in-flight load and return to the chooser. */
  cancel(): void;
  /** Drag bookkeeping: a nested dragenter. */
  dragEnter(): void;
  /** A nested dragleave. */
  dragLeave(): void;
  /** A drop or drag-exit resets the counter. */
  endDrag(): void;
  dispose(): void;
}

/**
 * Create the load controller. The section starts "chooser" (the drop zone is
 * the resting empty state); the page kicks a boot `?trace=` load via loadUrls,
 * or leaves the chooser up for the user.
 */
export function createLoadController(deps: LoadControllerDeps): LoadController {
  const now = deps.now ?? (() => performance.now());
  const setTimer =
    deps.setTimer ?? ((fn: () => void, ms: number) => setInterval(fn, ms));
  const clearTimer =
    deps.clearTimer ?? ((h: unknown) => clearInterval(h as ReturnType<typeof setInterval>));
  const createObjectUrl =
    deps.createObjectUrl ?? ((file: Blob) => URL.createObjectURL(file));
  const revokeObjectUrl =
    deps.revokeObjectUrl ?? ((url: string) => URL.revokeObjectURL(url));
  const credsMissing = deps.credsMissing ?? (() => false);
  const headers = deps.headers ?? (() => undefined);
  const scheduleFrame =
    deps.scheduleFrame ??
    ((fn: () => void) => {
      if (typeof requestAnimationFrame === "function") requestAnimationFrame(fn);
      else setTimeout(fn, 0);
    });

  let section: LoadSection = "chooser";
  let dragCounter = 0;
  let progress = "";
  let loadStartMs: number | null = null;
  let timerHandle: unknown = null;
  // Monotonic token so a late-resolving handle from a superseded/cancelled
  // load never writes state (its done/catch callbacks capture the token).
  let loadToken = 0;
  let currentHandle: LoadHandle | null = null;
  // Source identity is committed only by a successful, current load. Pending,
  // aborted, failed, and superseded attempts cannot change Copy Link behavior.
  let sourceShareable = false;
  // The decompressed trace bytes from the last successful load, retained for
  // Set/Clear Range reparse. Null until the first load completes.
  let retainedBuffer: ArrayBuffer | null = null;

  // Parse progress arrives on every ~256 KB drain, and each notification drives
  // a full lit-html render of the loading layer. The main-thread parser runs
  // that render ON the parse critical path, so at a few hundred firings it is
  // the parse competing with its own progress bar. Coalescing to one render per
  // frame keeps the label live (the display cannot show more than that anyway)
  // and hands the rest of the budget back to the decoder.
  //
  // Only PROGRESS is coalesced. Section transitions still notify synchronously,
  // so opening/closing the loading view is never a frame late.
  let progressFramePending = false;
  function notifyProgress(): void {
    if (progressFramePending) return;
    progressFramePending = true;
    scheduleFrame(() => {
      progressFramePending = false;
      notify();
    });
  }

  function notify(): void {
    deps.onChange();
  }

  function elapsedLabel(): string {
    if (section !== "loading" || loadStartMs === null) return "";
    const secs = (now() - loadStartMs) / 1000;
    return ` - ${secs.toFixed(1)}s`;
  }

  function startTimer(): void {
    loadStartMs = now();
    if (timerHandle !== null) clearTimer(timerHandle);
    // The tick only re-renders; getState recomputes the elapsed suffix from
    // loadStartMs, so the display stays live even between worker messages.
    timerHandle = setTimer(() => notify(), ELAPSED_TICK_MS);
  }

  function stopTimer(): void {
    if (timerHandle !== null) {
      clearTimer(timerHandle);
      timerHandle = null;
    }
    loadStartMs = null;
  }

  function begin(
    urls: readonly string[],
    opts: {
      label: string;
      withHeaders: boolean;
      objectUrl: string | null;
      /** Set-Range reparse window forwarded to the worker. */
      range?: ReparseRange | null;
      /** Source replacement vs same-source Set/Clear Range reparse. */
      kind: "source" | "reparse";
      /** Commit on success; null preserves the current source (reparse). */
      shareableAfterSuccess: boolean | null;
    },
  ): void {
    // Supersede any in-flight load (its callbacks are already token-guarded).
    if (currentHandle !== null) currentHandle.abort();
    loadToken += 1;
    const token = loadToken;
    section = "loading";
    progress = opts.label;
    startTimer();

    const startOpts: StartLoadOptions = {
      kind: opts.kind,
      onProgress: (p) => {
        if (token !== loadToken) return;
        progress = progressLabel(p);
        notifyProgress();
      },
    };
    if (opts.withHeaders) {
      const h = headers();
      if (h !== undefined) startOpts.headers = h;
    }
    if (opts.range != null) {
      if (opts.range.startNs != null) startOpts.startTime = opts.range.startNs;
      if (opts.range.endNs != null) startOpts.endTime = opts.range.endNs;
    }

    const handle = deps.startLoad(urls, startOpts);
    currentHandle = handle;

    const cleanup = (): void => {
      if (opts.objectUrl !== null) revokeObjectUrl(opts.objectUrl);
      if (token === loadToken) {
        stopTimer();
        currentHandle = null;
      }
    };

    handle.done.then(
      (result: unknown) => {
        // Retain the decompressed buffer the worker transfers back so Set/Clear
        // Range can re-parse it with a time window without re-fetching. Blob
        // copies on reparse, so this reference stays valid across reparses.
        const settled = result as
          | { buffer?: unknown; timing?: TraceWorkerTiming }
          | undefined;
        const buf = settled?.buffer;
        if (buf instanceof ArrayBuffer && buf.byteLength > 0) retainedBuffer = buf;
        if (settled?.timing !== undefined) deps.onTiming?.(settled.timing);
        cleanup();
        if (token !== loadToken) return;
        if (opts.shareableAfterSuccess !== null) {
          sourceShareable = opts.shareableAfterSuccess;
        }
        // Success: the store's trace slice is now populated; commit the
        // toolbar label, then drop the load section so the tracks show through.
        deps.onLoaded?.();
        section = "closed";
        dragCounter = 0;
        notify();
      },
      (err: unknown) => {
        cleanup();
        if (token !== loadToken) return;
        if (isAbortError(err)) return; // cancel() already set the state
        const raw = err instanceof Error ? err.message : String(err);
        deps.onError(loadErrorMessage(raw, credsMissing()));
        // Return to the drop zone. The old trace, if any, is untouched (worker
        // updates the slice only on success), so canDismiss recovers.
        section = "chooser";
        notify();
      },
    );

    notify();
  }

  const escSurface: EscapableSurface = {
    name: "load",
    priority: ESC_PRIORITY.load,
    isOpen: () =>
      section === "loading" || (section === "chooser" && deps.hasTrace()),
    close: () => {
      if (section === "loading") cancelLoad();
      else if (section === "chooser") dismiss();
    },
  };

  function cancelLoad(): void {
    // Supersede both worker loads and the handle-less async scope resolution so
    // their late completions are ignored, then return to the drop zone.
    if (section === "loading") loadToken += 1;
    if (currentHandle !== null) {
      const h = currentHandle;
      currentHandle = null;
      h.abort();
    }
    stopTimer();
    section = "chooser";
    notify();
  }

  function dismiss(): void {
    if (section !== "chooser" || !deps.hasTrace()) return;
    section = "closed";
    notify();
  }

  return {
    getState(): LoadChromeState {
      return {
        section,
        hasTrace: deps.hasTrace(),
        canDismiss: section === "chooser" && deps.hasTrace(),
        dragActive: dragCounter > 0,
        progressLabel: progress,
        elapsedLabel: elapsedLabel(),
      };
    },
    isSourceShareable: () => sourceShareable,
    escSurface,
    requestNewFile(): void {
      if (section !== "closed") return; // already open; reopening is New File
      if (deps.hasTrace() && !deps.confirm(NEW_FILE_CONFIRM_MESSAGE)) return;
      section = "chooser";
      notify();
    },
    dismiss,
    loadFile(file): void {
      const objectUrl = createObjectUrl(file);
      const name = typeof file.name === "string" && file.name.length > 0
        ? file.name
        : "trace";
      begin([objectUrl], {
        label: `Loading ${name}...`,
        withHeaders: false,
        objectUrl,
        kind: "source",
        shareableAfterSuccess: false,
      });
    },
    loadUrls(urls, label, range): void {
      begin(urls, {
        label,
        withHeaders: true,
        objectUrl: null,
        kind: "source",
        shareableAfterSuccess: true,
        ...(range !== undefined ? { range } : {}),
      });
    },
    showLoading(label): number {
      // Show the loading view WITHOUT starting a worker load, for the async
      // gap while a boot `s_*` scope is re-listed to its file URLs (the caller
      // then calls loadUrls, or cancel() on failure). Return the token so a
      // superseded resolution's late loadUrls can be ignored by the caller.
      loadToken += 1;
      section = "loading";
      progress = label;
      startTimer();
      notify();
      return loadToken;
    },
    isCurrentLoad: (token) => token === loadToken && section === "loading",
    reparse(range): void {
      // Set/Clear Range: re-parse the retained buffer with a time window, OFF
      // the main thread (reuses the worker load path via an object URL). The
      // worker filters events to the window; initViewportFromTrace refits to
      // the new trace's extent. No-op before the first load.
      if (retainedBuffer === null) return;
      const objectUrl = createObjectUrl(new Blob([retainedBuffer]));
      begin([objectUrl], {
        label: isRangeActive(range ?? {})
          ? "Applying range..."
          : "Restoring full trace...",
        withHeaders: false,
        objectUrl,
        kind: "reparse",
        shareableAfterSuccess: null,
        range,
      });
    },
    loadDemo(): void {
      begin(["/demo-trace.bin"], {
        label: "Loading demo trace...",
        withHeaders: true,
        objectUrl: null,
        kind: "source",
        shareableAfterSuccess: false,
      });
    },
    cancel: cancelLoad,
    dragEnter(): void {
      dragCounter += 1;
      if (dragCounter === 1) notify();
    },
    dragLeave(): void {
      if (dragCounter > 0) dragCounter -= 1;
      if (dragCounter === 0) notify();
    },
    endDrag(): void {
      if (dragCounter === 0) return;
      dragCounter = 0;
      notify();
    },
    dispose(): void {
      loadToken += 1;
      if (currentHandle !== null) {
        currentHandle.abort();
        currentHandle = null;
      }
      stopTimer();
    },
  };
}

function isAbortError(err: unknown): boolean {
  return (
    typeof err === "object" &&
    err !== null &&
    (err as { name?: unknown }).name === "AbortError"
  );
}
