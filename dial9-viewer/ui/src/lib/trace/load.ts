// lib/trace/load.ts - load orchestration (T09; architecture 2.7;
// features/02 B12/B14, features/01 I4). Typed wrapper over the frozen
// trace_parser.js load surface, plus the mechanism ported out of
// viewer.html's streamAndShowTrace/loadTraceFromUrl (viewer.html:1678-1712,
// 1855-1903): repeatable `trace=` components, parallel fetch + gunzip +
// concat, streaming parse with chunk capture so the full buffer stays
// available for in-memory re-parse (see ./reparse.ts).
//
// Mechanism only: loading-view labels, elapsed timers, loadPerf records,
// alerts/credential hints and drop-zone resets are page concerns (page
// tickets). The file-drop path is `parseTraceBuffer` (the page hands the
// FileReader result in); the demo path is `loadTrace("demo-trace.bin")`.
//
// This file also re-exports the rest of the trace_parser.js surface
// (constants, symbol/stack helpers, types) so nothing outside lib/trace
// ever imports the core module; ./analysis.ts does the same for
// trace_analysis.js.

import {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  canStreamDecode,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
  fetchTraces,
  formatFrame,
  parseTrace,
  symbolizeChain,
} from "../../../trace_parser.js";
import type {
  FetchOptions,
  ParseOptions,
  ParsedTrace,
} from "../../../trace_parser.js";
import { streamTraceWithCapture } from "./stream.js";
import type {
  TraceWorkerFactory,
  TraceWorkerLoadRequest,
  TraceWorkerParseOptions,
  TraceWorkerPort,
  TraceWorkerProgress,
  TraceWorkerRequest,
  TraceWorkerResponse,
  TraceWorkerTiming,
} from "./worker/protocol.js";

// Typed pass-throughs of the core load/parse/symbol surface.
export {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  canStreamDecode,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
  formatFrame,
  symbolizeChain,
};
export type {
  AllocEvent,
  BlockInPlaceGap,
  CallframeSymbols,
  ClockSyncAnchor,
  CpuSample,
  CustomTraceEvent,
  FetchOptions,
  FreeEvent,
  MemoryOverflowEvent,
  ParseOptions,
  ParseProgress,
  ParsedTrace,
  SampleGroup,
  SymbolFrame,
  TaskDump,
  TraceEvent,
} from "../../../trace_parser.js";
export type { DecodedFieldValue } from "../../../decode.js";

/** Options for a URL load: fetch options + parse options, flat. */
export interface LoadTraceOptions extends FetchOptions, ParseOptions {}

/** The result of loading one logical trace from URLs. */
export interface LoadedTrace {
  trace: ParsedTrace;
  /**
   * The raw (gunzipped, concatenated) trace bytes, retained so Set/Clear
   * Range can re-parse in memory without re-fetching (features/02 B14).
   */
  buffer: ArrayBuffer;
  /**
   * "stream" when download and decode overlapped (canStreamDecode
   * runtimes); "buffered" for the fetch-then-parse fallback. Pages use
   * this for their load-perf record (02 B16).
   */
  mode: "stream" | "buffered";
}

function splitOptions(opts: LoadTraceOptions): {
  fetchOpts: FetchOptions;
  parseOpts: ParseOptions;
} {
  const { signal, headers, ...parseOpts } = opts;
  const fetchOpts: FetchOptions = {};
  if (signal !== undefined) fetchOpts.signal = signal;
  if (headers !== undefined) fetchOpts.headers = headers;
  return { fetchOpts, parseOpts };
}

/**
 * Parse an already-fetched buffer (the file-drop path, features/02 B2/B3,
 * and the re-parse path). Thin typed alias over the core's parseTrace so
 * callers never import the core module.
 */
export function parseTraceBuffer(
  buffer: ArrayBuffer | Uint8Array,
  opts?: ParseOptions
): Promise<ParsedTrace> {
  return parseTrace(buffer, opts);
}

/**
 * Stream one OR MORE trace URLs: decode chunks as they download so parse
 * time overlaps the download (~max(download, parse) instead of their sum).
 * For multiple URLs the fetches run concurrently and the components stream
 * in back-to-back, in order, as one logical trace - so parsing the first
 * segment overlaps the in-flight downloads of the rest (issue #595). The
 * gunzipped chunks are captured while parsing so the full buffer is still
 * available afterwards for in-memory Set/Clear-Range re-parsing (which
 * never re-fetches). Mechanism lives in ./stream.ts (shared with the
 * worker body, which must not import this module - see stream.ts header).
 */
export async function loadTraceStreamed(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const { fetchOpts, parseOpts } = splitOptions(opts);
  const { trace, buffer } = await streamTraceWithCapture(list, fetchOpts, parseOpts);
  return { trace, buffer, mode: "stream" };
}

/**
 * Buffered fallback for runtimes without DecompressionStream: fetch every
 * component (in parallel), gunzip each independently, concatenate in
 * `urls` order, then parse the whole buffer (fetch and parse are separate
 * phases, unlike streaming).
 */
export async function loadTraceBuffered(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const { fetchOpts, parseOpts } = splitOptions(opts);
  const buffer = await fetchTraces([...list], fetchOpts);
  const trace = await parseTrace(buffer, parseOpts);
  return { trace, buffer, mode: "buffered" };
}

/**
 * Load one logical trace from one or more URLs (repeatable `trace=`
 * components): STREAM whenever the runtime supports it, buffered
 * fetch+gunzip+concat otherwise (features/02 B12). Errors propagate to the
 * caller - the page owns AbortError swallowing, the HTTP-401 credentials
 * hint, and reset-to-drop-zone (02 B10/B13).
 */
export function loadTrace(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  return canStreamDecode() ? loadTraceStreamed(urls, opts) : loadTraceBuffered(urls, opts);
}

// ── Worker load pipeline (T16; ADR-0004 section 6 "do now") ─────────────
//
// Runs fetch + gunzip + parse OFF the main thread: the 3.8-12.2 s load
// walls (03-performance-findings.md) are main-thread parse time, and the
// frozen core is environment-agnostic, so it runs in a Worker unchanged.
// The orchestrator spawns ONE worker per whole-trace load (T17 windows
// this per-segment), forwards progress messages (the B8/B9/B16
// load-timing fields; see worker/protocol.ts), writes the parsed trace
// into the store's `trace` slice on completion, and owns THE single
// AbortController for the load: abort => an "abort" message into the
// worker (cooperative fetch cancellation) followed by port.terminate()
// (authoritative - also kills a compute-bound parse phase that no signal
// reaches). Message and error payloads cross the boundary via structured
// clone; the raw buffer is transferred zero-copy.

/**
 * The store surface the worker pipeline writes into. Structurally
 * satisfied by the T07 store (Store<StoreState> / ViewerStore) without
 * lib/trace depending on src/store; the trace slice is replaced wholesale
 * per the architecture 2.2 contract.
 */
export interface TraceSliceStore {
  update(slice: "trace", patch: { trace: ParsedTrace }): void;
}

export interface WorkerLoadOptions {
  /**
   * Same-origin credential headers (features/02 B17): the page resolves
   * Dial9Creds.headers() on the main thread (creds are a page/global
   * concern) and passes the plain record for the worker-side fetch.
   */
  headers?: Record<string, string>;
  /** Cap event count (metadata/symbols always parsed). */
  maxEvents?: number;
  /** Filter events to a time range (absolute ns, inclusive). */
  startTime?: number;
  endTime?: number;
  /**
   * Progress stream: the B8 text fields, B9 elapsed, B16 marks. Never
   * invoked after abort or settle (late-arriving worker messages are
   * dropped).
   */
  onProgress?: (progress: TraceWorkerProgress) => void;
  /**
   * External abort hook (the page's Escape/Back cancel): forwarded into
   * the load's single internal controller; equivalent to calling
   * `abort()` on the returned handle.
   */
  signal?: AbortSignal;
  /**
   * Transport seam for tests (node:worker_threads adapter, in-process
   * fake). Defaults to the Vite-built browser worker entry.
   */
  worker?: TraceWorkerFactory;
}

/** Result of a worker load: LoadedTrace plus the worker's timing record
 * (worker-clock; pages derive totalMs themselves - see protocol.ts). */
export interface WorkerLoadResult extends LoadedTrace {
  timing: TraceWorkerTiming;
}

/** Handle over one in-flight worker load. */
export interface WorkerTraceLoad {
  /**
   * Resolves after the store's trace slice has been updated; rejects with
   * the worker's error (name preserved, e.g. for the page's HTTP-401
   * credentials hint) or a DOMException named AbortError on abort. A
   * no-op rejection handler is pre-attached so fire-and-forget callers
   * never produce an unhandled rejection; awaiting callers still receive
   * the rejection.
   */
  done: Promise<WorkerLoadResult>;
  /** Cancel the load: "abort" message + terminate. Idempotent. */
  abort(): void;
}

/**
 * The production transport: Vite's native worker build. The inline
 * `new Worker(new URL(...))` pattern is statically detected by Vite and
 * bundles trace-worker.ts (frozen core included) into a dedicated worker
 * chunk under dist/assets. Browser-only - Node tests substitute
 * WorkerLoadOptions.worker (node has no Web Worker; see
 * worker/node-worker-entry.mjs). Exported for the per-segment parse
 * driver (segments.ts), which spawns the same worker entry per job; not
 * re-exported through the barrel (transports are a lib/trace concern).
 */
export function defaultTraceWorkerFactory(): TraceWorkerPort {
  const worker = new Worker(new URL("./worker/trace-worker.ts", import.meta.url), {
    type: "module",
  });
  return {
    postMessage(message: TraceWorkerRequest): void {
      worker.postMessage(message);
    },
    onMessage(fn: (message: TraceWorkerResponse) => void): void {
      worker.onmessage = (event: MessageEvent): void => {
        fn(event.data as TraceWorkerResponse);
      };
    },
    onError(fn: (error: unknown) => void): void {
      worker.onerror = (event: ErrorEvent): void => {
        fn(event);
      };
    },
    terminate(): void {
      worker.terminate();
    },
  };
}

/**
 * Load one logical trace from one or more URLs entirely inside a Web
 * Worker (fetch + gunzip + parse off the main thread), writing the parsed
 * trace into `store`'s `trace` slice on completion. The worker picks
 * stream vs buffered mode itself (same B12 selection as loadTrace). The
 * worker is terminated on settle - success, error, or abort - so no live
 * handle outlasts the load.
 */
export function loadTraceInWorker(
  store: TraceSliceStore,
  urls: string | readonly string[],
  opts: WorkerLoadOptions = {}
): WorkerTraceLoad {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const port = (opts.worker ?? defaultTraceWorkerFactory)();
  // THE single AbortController for this load (T16 decision): the handle's
  // abort() and the optional external signal both funnel into it.
  const controller = new AbortController();
  let settled = false;

  let resolveDone!: (result: WorkerLoadResult) => void;
  let rejectDone!: (error: unknown) => void;
  const done = new Promise<WorkerLoadResult>((resolve, reject) => {
    resolveDone = resolve;
    rejectDone = reject;
  });
  // Fire-and-forget friendliness (progress/store-driven pages may never
  // await `done`): mark the rejection handled without consuming it.
  done.catch(() => {});

  /** Settle exactly once; always terminates (no live worker after). */
  const settle = (fn: () => void): void => {
    if (settled) return;
    settled = true;
    fn();
    port.terminate();
  };

  controller.signal.addEventListener(
    "abort",
    () => {
      settle(() => {
        // Cooperative fetch cancel first; settle's terminate is the
        // authoritative kill (covers compute-bound parse phases).
        port.postMessage({ kind: "abort" });
        rejectDone(new DOMException("trace load aborted", "AbortError"));
      });
    },
    { once: true }
  );

  if (opts.signal !== undefined) {
    if (opts.signal.aborted) {
      controller.abort();
    } else {
      opts.signal.addEventListener(
        "abort",
        () => {
          controller.abort();
        },
        { once: true }
      );
    }
  }

  port.onMessage((message) => {
    // Late messages (queued behind an abort, or a worker racing its own
    // termination) are dropped: no progress callback fires and the store
    // is never touched after settle.
    if (settled) return;
    switch (message.kind) {
      case "progress":
        opts.onProgress?.(message.progress);
        return;
      case "done":
        settle(() => {
          store.update("trace", { trace: message.trace });
          resolveDone({
            trace: message.trace,
            buffer: message.buffer,
            mode: message.mode,
            timing: message.timing,
          });
        });
        return;
      case "error":
        settle(() => {
          const error = new Error(message.message);
          // Preserve name-based handling (AbortError swallowing, the
          // HTTP-401 credentials hint) across the boundary.
          error.name = message.name;
          rejectDone(error);
        });
        return;
    }
  });
  port.onError((error) => {
    settle(() => {
      rejectDone(error);
    });
  });

  if (!settled) {
    // Only defined parse options cross the wire (the parser treats a
    // missing key and an undefined value the same, but the message stays
    // minimal and clone-friendly).
    const parse: TraceWorkerParseOptions = {};
    if (opts.maxEvents !== undefined) parse.maxEvents = opts.maxEvents;
    if (opts.startTime !== undefined) parse.startTime = opts.startTime;
    if (opts.endTime !== undefined) parse.endTime = opts.endTime;
    const request: TraceWorkerLoadRequest = { kind: "load", urls: list, parse };
    if (opts.headers !== undefined) request.headers = opts.headers;
    port.postMessage(request);
  }

  return {
    done,
    abort: (): void => {
      controller.abort();
    },
  };
}

// objectTraceUrls (features/01 I4) moved to ./object-urls.ts (T14) so the
// browser page can import it without pulling the parser into its bundle;
// re-exported here to keep this module's import surface unchanged.
export { objectTraceUrls } from "./object-urls.js";
