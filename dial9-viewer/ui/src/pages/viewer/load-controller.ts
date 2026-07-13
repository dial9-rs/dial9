// src/pages/viewer/load-controller.ts - the load-chrome STATE MACHINE (T34;
// features/02 section B). DOM-free so it is fully node-testable; the DOM
// view (load-chrome.ts) reflects getState() and forwards events to the
// actions here.
//
// It owns the load section's lifecycle - drop zone (chooser) -> loading ->
// closed - the drag-feedback counter (B3/B4/B5), the live elapsed timer
// (B9), the keep-exactly progress labels (B8), and the two amendments this
// ticket lands:
//
//   * S3 new-file-confirm: requestNewFile() runs deps.confirm() before it
//     opens the chooser over a loaded trace, so a stray "New File" click can
//     never silently discard the current analysis. Opening the chooser is
//     itself non-destructive - the T16 worker pipeline only replaces the
//     store's `trace` slice on SUCCESS, so a cancelled/failed replace keeps
//     the old trace resident (fixing S3's "no recovery in either direction").
//
//   * #281 dismissal: a chooser opened over a loaded trace is dismissible
//     (canDismiss), so Esc AND the visible close control return to the trace;
//     reopening is just another requestNewFile().
//
// The load itself is CONSUMED from T16/T17 (loadTraceInWorker), injected as
// `startLoad` so tests drive a scripted handle. File drops feed the worker as
// an object URL (the core sniffs the gzip magic on the fetched blob, so both
// .bin and .bin.gz decode - trace_parser.js maybeGunzip / fetchTraceStream).

import { ESC_PRIORITY } from "./esc-cascade.js";
import type { EscapableSurface } from "./esc-cascade.js";
import type { TraceWorkerProgress, ReparseRange } from "../../lib/trace/index.js";
import { isRangeActive } from "../../lib/trace/index.js";

/** The load section's discrete states (features/02 B1/B7). */
export type LoadSection = "closed" | "chooser" | "loading";

/** Everything the DOM view needs for one render pass. */
export interface LoadChromeState {
  section: LoadSection;
  /** A trace is currently resident in the store. */
  hasTrace: boolean;
  /** The chooser can be dismissed back to a loaded trace (#281). */
  canDismiss: boolean;
  /** A file drag is in progress over the window (B4/B5). */
  dragActive: boolean;
  /** Current phase/progress text while loading (B8, keep-exactly). */
  progressLabel: string;
  /** Live wall-clock elapsed suffix while loading (B9), e.g. " - 1.3s". */
  elapsedLabel: string;
}

/** A minimal handle over one in-flight load (satisfied by WorkerTraceLoad). */
export interface LoadHandle {
  done: Promise<unknown>;
  abort(): void;
}

export interface StartLoadOptions {
  onProgress(progress: TraceWorkerProgress): void;
  /** Same-origin credential headers (B17); omitted for local file loads. */
  headers?: Record<string, string>;
  /**
   * Set-Range reparse window (absolute ns, E3/E4): the worker filters events
   * to this range while re-parsing the retained buffer. Omitted for a normal
   * load and for Clear Range (full trace).
   */
  startTime?: number;
  endTime?: number;
}

/** Start a load over one or more URLs; returns its handle. Injected so tests
 * drive a scripted worker without a real Worker (see load.worker.test.ts). */
export type StartLoad = (
  urls: readonly string[],
  opts: StartLoadOptions,
) => LoadHandle;

export interface LoadControllerDeps {
  /** True when a parsed trace is resident (reads the store's trace slice). */
  hasTrace(): boolean;
  /** Kick off a load (default: loadTraceInWorker bound to the page store). */
  startLoad: StartLoad;
  /** Confirm gate for New-File-over-a-loaded-trace (S3); window.confirm. */
  confirm(message: string): boolean;
  /** Surface a load failure (the page shows it as an error toast, U4/B13). */
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
  /** Wall clock for the elapsed timer; default performance.now. */
  now?(): number;
  /** Interval scheduler for the 250ms elapsed tick; default setInterval. */
  setTimer?(fn: () => void, ms: number): unknown;
  clearTimer?(handle: unknown): void;
  /** Build a fetchable URL for a dropped/picked file; default createObjectURL. */
  createObjectUrl?(file: Blob): string;
  revokeObjectUrl?(url: string): void;
  /** True when a creds module is present but has no credentials (B13 hint). */
  credsMissing?(): boolean;
  /** Same-origin credential headers for URL loads (B17). */
  headers?(): Record<string, string> | undefined;
}

/** Message shown by the S3 New-File confirm gate. */
export const NEW_FILE_CONFIRM_MESSAGE =
  "Open a different trace? Your current view stays until you load a new file - " +
  "press Esc to return to it.";

/** The wall-clock elapsed suffix updates 4x/second (legacy B9 cadence). */
const ELAPSED_TICK_MS = 250;

/**
 * Format the load progress line from a worker progress message (B8),
 * keep-exactly with the legacy makeParseProgress / loadTraceFromUrl labels
 * (ASCII separators: legacy "·" -> " - ", "…" -> "..."). The phase
 * words and numeric content (percent, MB, k-events) are preserved verbatim.
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

/** The initial label for a URL load before any worker progress arrives (B8). */
export function initialUrlLabel(urlCount: number): string {
  return urlCount > 1 ? `Loading ${urlCount} traces...` : "Loading trace...";
}

/**
 * Build the user-facing error message for a failed load (B13): the HTTP-401
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
  /** The escapable-surface spec to register in the shell's esc cascade. */
  readonly escSurface: EscapableSurface;
  /** Toolbar "New File" (B15): confirm (S3), then open the chooser (#281). */
  requestNewFile(): void;
  /** Dismiss the chooser back to the loaded trace (#281). No-op if none. */
  dismiss(): void;
  /** Load a dropped/picked file via an object URL (B2/B3). */
  loadFile(file: Blob & { name?: string }): void;
  /** Load one or more URLs (boot `?trace=`, B12). `label` is the B8 initial. */
  loadUrls(urls: readonly string[], label: string): void;
  /**
   * Set/Clear Range (E3/E4): re-parse the retained buffer filtered to `range`
   * (null / open bounds = full trace). No-op before the first load completes.
   */
  reparse(range: ReparseRange | null): void;
  /** Load the bundled demo trace (B6). */
  loadDemo(): void;
  /** Cancel the in-flight load and return to the chooser (B10/B11). */
  cancel(): void;
  /** Drag bookkeeping (B3 dragCounter): a nested dragenter. */
  dragEnter(): void;
  /** A nested dragleave. */
  dragLeave(): void;
  /** A drop or drag-exit resets the counter. */
  endDrag(): void;
  dispose(): void;
}

/**
 * Create the load controller. The section starts "chooser" (the drop zone is
 * the resting empty state, B1); the page kicks a boot `?trace=` load via
 * loadUrls, or leaves the chooser up for the user.
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

  let section: LoadSection = "chooser";
  let dragCounter = 0;
  let progress = "";
  let loadStartMs: number | null = null;
  let timerHandle: unknown = null;
  // Monotonic token so a late-resolving handle from a superseded/cancelled
  // load never writes state (its done/catch callbacks capture the token).
  let loadToken = 0;
  let currentHandle: LoadHandle | null = null;
  // The decompressed trace bytes from the last successful load, retained for
  // Set/Clear Range reparse (B14). Null until the first load completes.
  let retainedBuffer: ArrayBuffer | null = null;

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
      /** Set-Range reparse window forwarded to the worker (E3/E4). */
      range?: ReparseRange | null;
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
      onProgress: (p) => {
        if (token !== loadToken) return;
        progress = progressLabel(p);
        notify();
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
        // Range can re-parse it with a time window (B14) without re-fetching.
        // Blob copies on reparse, so this reference stays valid across reparses.
        const buf = (result as { buffer?: unknown } | undefined)?.buffer;
        if (buf instanceof ArrayBuffer && buf.byteLength > 0) retainedBuffer = buf;
        cleanup();
        if (token !== loadToken) return;
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
        // Return to the drop zone (B13). The old trace, if any, is untouched
        // (worker updates the slice only on success), so canDismiss recovers.
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
    // Abort supersedes the handle; bump the token so its late done/catch is
    // ignored, then return to the drop zone (B10/B11).
    if (currentHandle !== null) {
      const h = currentHandle;
      currentHandle = null;
      loadToken += 1;
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
      });
    },
    loadUrls(urls, label): void {
      begin(urls, { label, withHeaders: true, objectUrl: null });
    },
    reparse(range): void {
      // Set/Clear Range (E3/E4): re-parse the retained buffer with a time
      // window, OFF the main thread (reuses the worker load path via an object
      // URL). The worker filters events to the window; initViewportFromTrace
      // refits to the new trace's extent. No-op before the first load.
      if (retainedBuffer === null) return;
      const objectUrl = createObjectUrl(new Blob([retainedBuffer]));
      begin([objectUrl], {
        label: isRangeActive(range ?? {})
          ? "Applying range..."
          : "Restoring full trace...",
        withHeaders: false,
        objectUrl,
        range,
      });
    },
    loadDemo(): void {
      begin(["/demo-trace.bin"], {
        label: "Loading demo trace...",
        withHeaders: true,
        objectUrl: null,
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
