// Load orchestration: a typed wrapper over the frozen trace_parser.js load
// surface, plus repeatable `trace=` components, parallel fetch + gunzip +
// concat, and streaming parse with chunk capture so the full buffer stays
// available for in-memory re-parse (see ./reparse.ts). Loading-view labels,
// elapsed timers, loadPerf records, alerts/credential hints and drop-zone
// resets are page concerns. The file-drop path is `parseTraceBuffer`; the
// demo path is `loadTrace("demo-trace.bin")`.
//
// This file also re-exports the rest of the trace_parser.js surface so
// nothing outside lib/trace ever imports the core module; ./analysis.ts does
// the same for trace_analysis.js.

// FIRST import: seeds the `TraceDecoder`/`TraceParser` browser globals the
// frozen core resolves at runtime. loadTraceOnMainThread runs the core parser
// on the page's main thread, where (unlike Node or the worker bundle) nothing
// else establishes `globalThis.TraceDecoder`; without this the first parse
// throws "TraceDecoder not found".
import "./core-globals.js";
import { ColumnarEvents, capacityForBytes } from "./columnar-events.js";
import { ColumnarCpuSamples } from "./columnar-cpu-samples.js";
import { ColumnarSpanEvents } from "./columnar-span-events.js";
import { startLoadPerf } from "./load-perf.js";
import type { LoadPerfRecorder } from "./load-perf.js";
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
  SingleEventSpan,
  SymbolFrame,
  TaskDump,
  TidWorkerBinding,
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
   * Range can re-parse in memory without re-fetching.
   */
  buffer: ArrayBuffer;
  /**
   * "stream" when download and decode overlapped (canStreamDecode
   * runtimes); "buffered" for the fetch-then-parse fallback. Pages use this
   * for their load-perf record.
   */
  mode: "stream" | "buffered";
}

/** Fetch and gunzip one logical trace without running the JavaScript decoder. */
export function fetchTraceBytes(
  urls: string | readonly string[],
  opts: FetchOptions = {},
): Promise<ArrayBuffer> {
  const list = Array.isArray(urls) ? [...urls] : [urls as string];
  return fetchTraces(list, opts);
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
 * Parse an already-fetched buffer (the file-drop and re-parse paths). Thin
 * typed alias over the core's parseTrace so callers never import the core
 * module.
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
 * segment overlaps the in-flight downloads of the rest. The gunzipped
 * chunks are captured while parsing so the full buffer is still available
 * afterwards for in-memory Set/Clear-Range re-parsing (which never
 * re-fetches). Mechanism lives in ./stream.ts (shared with the worker body,
 * which must not import this module - see stream.ts header).
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
 * fetch+gunzip+concat otherwise. Errors propagate to the caller - the page
 * owns AbortError swallowing, the HTTP-401 credentials hint, and
 * reset-to-drop-zone.
 */
export function loadTrace(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  return canStreamDecode() ? loadTraceStreamed(urls, opts) : loadTraceBuffered(urls, opts);
}

// ── Worker load pipeline ─────────────────────────────────────────────────
//
// Runs fetch + gunzip + parse OFF the main thread (the frozen core is
// environment-agnostic, so it runs in a Worker unchanged). The orchestrator
// spawns ONE worker per whole-trace load, forwards progress messages, writes
// the parsed trace into the store's `trace` slice on completion, and owns
// THE single AbortController for the load: abort => an "abort" message into
// the worker (cooperative fetch cancellation) followed by port.terminate()
// (authoritative - also kills a compute-bound parse phase that no signal
// reaches). Message and error payloads cross the boundary via structured
// clone; the raw buffer is transferred zero-copy.

/**
 * The store surface the worker pipeline writes into. Structurally satisfied
 * by the viewer store (Store<StoreState> / ViewerStore) without lib/trace
 * depending on src/store; the trace slice is replaced wholesale.
 */
export interface TraceSliceStore {
  update(slice: "trace", patch: { trace: ParsedTrace }): void;
}

export interface WorkerLoadOptions {
  /**
   * Same-origin credential headers: the page resolves Dial9Creds.headers()
   * on the main thread (creds are a page/global concern) and passes the
   * plain record for the worker-side fetch.
   */
  headers?: Record<string, string>;
  /** Cap event count (metadata/symbols always parsed). */
  maxEvents?: number;
  /** Filter events to a time range (absolute ns, inclusive). */
  startTime?: number;
  endTime?: number;
  /**
   * Progress stream. Never invoked after abort or settle (late-arriving
   * worker messages are dropped).
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
 * stream vs buffered mode itself (same selection as loadTrace). The worker
 * is terminated on settle - success, error, or abort - so no live handle
 * outlasts the load.
 */
export function loadTraceInWorker(
  store: TraceSliceStore,
  urls: string | readonly string[],
  opts: WorkerLoadOptions = {}
): WorkerTraceLoad {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const port = (opts.worker ?? defaultTraceWorkerFactory)();
  // THE single AbortController for this load: the handle's abort() and the
  // optional external signal both funnel into it.
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

/**
 * Main-thread equivalent of loadTraceInWorker: fetch + gunzip + parse on the
 * CALLING thread and write the parsed trace into `store`'s `trace` slice,
 * exposing the SAME WorkerTraceLoad handle (done/abort) so it is a drop-in for
 * the worker loader at the page's startLoad seam.
 *
 * Why this exists: loadTraceInWorker parses off-thread but must
 * structured-CLONE the entire ParsedTrace (13M+ events, several Maps) back
 * across the worker boundary on completion (worker/body.ts posts `trace` by
 * clone; only the raw buffer transfers zero-copy). For a large trace that
 * clone is a second full copy of the object graph and blows the worker's
 * memory budget ("Data cannot be cloned, out of memory"). Parsing here builds
 * the graph in place with ZERO clone - the legacy (pre-worker) behavior - so
 * peak memory is one copy, not two.
 *
 * Responsiveness: the core parser yields to the event loop on a wall-clock
 * paint throttle (~5x/s) while firing onParseProgress, so the progress line
 * still updates and the tab is not fully frozen during the parse. There is no
 * off-thread parallelism, which is the accepted trade for not cloning; the
 * separate columnar-representation work is what would cut the parse cost
 * itself.
 *
 * Abort: cooperative. The fetch is cancelled via the shared AbortController;
 * a compute-bound parse cannot be force-killed (no worker to terminate), so
 * onParseProgress throws AbortError at the next yield, unwinding the parse.
 * The store is never touched once aborted.
 */
export function loadTraceOnMainThread(
  store: TraceSliceStore,
  urls: string | readonly string[],
  opts: WorkerLoadOptions = {}
): WorkerTraceLoad {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const controller = new AbortController();
  let settled = false;

  let resolveDone!: (result: WorkerLoadResult) => void;
  let rejectDone!: (error: unknown) => void;
  const done = new Promise<WorkerLoadResult>((resolve, reject) => {
    resolveDone = resolve;
    rejectDone = reject;
  });
  done.catch(() => {});

  const settle = (fn: () => void): void => {
    if (settled) return;
    settled = true;
    fn();
  };

  controller.signal.addEventListener(
    "abort",
    () => {
      settle(() => {
        rejectDone(new DOMException("trace load aborted", "AbortError"));
      });
    },
    { once: true }
  );

  if (opts.signal !== undefined) {
    if (opts.signal.aborted) {
      controller.abort();
    } else {
      opts.signal.addEventListener("abort", () => controller.abort(), {
        once: true,
      });
    }
  }

  const startMs = performance.now();
  const mode: "stream" | "buffered" = canStreamDecode() ? "stream" : "buffered";
  let eventCount = 0;
  let fetchDoneMs: number | null = null;
  const perf: LoadPerfRecorder = startLoadPerf();
  perf.mark("start");

  const emit = (
    phase: "fetching" | "parsing",
    bytesRead: number,
    totalBytes: number | null
  ): void => {
    opts.onProgress?.({
      phase,
      mode,
      urlCount: list.length,
      bytesRead,
      totalBytes,
      eventCount,
      startMs,
      elapsedMs: performance.now() - startMs,
    });
  };

  // The parser calls this synchronously at each 100KB/paint-throttle gate;
  // throwing here is the only lever to unwind a compute-bound parse on abort.
  const onParseProgress = (p: {
    bytesRead: number;
    totalBytes: number | null;
    eventCount: number;
  }): void => {
    if (controller.signal.aborted) {
      throw new DOMException("trace load aborted", "AbortError");
    }
    eventCount = p.eventCount;
    emit("parsing", p.bytesRead, p.totalBytes);
  };

  const fetchOpts: FetchOptions = { signal: controller.signal };
  if (opts.headers !== undefined) fetchOpts.headers = opts.headers;
  // Columnar event store: the parser writes events into typed-array columns
  // instead of ~13M fat objects, so peak memory drops ~7x and the parse no
  // longer GC-thrashes. A fresh store per load (incl. Set/Clear-Range reparse).
  // Columnar span-event store: SpanEnter/Exit/Close and annotated single-event
  // spans (the ~2.3 GB of fat span objects that stall a 13M-event parse) route
  // into typed columns; other custom events stay fat.
  // buildSpanDataColumnar reads the columns.
  const spanEventSink = new ColumnarSpanEvents();
  const parseOpts: ParseOptions = {
    onParseProgress,
    eventSink: new ColumnarEvents(),
    cpuSampleSink: new ColumnarCpuSamples(),
    spanEventSink,
  };
  if (opts.maxEvents !== undefined) parseOpts.maxEvents = opts.maxEvents;
  if (opts.startTime !== undefined) parseOpts.startTime = opts.startTime;
  if (opts.endTime !== undefined) parseOpts.endTime = opts.endTime;

  const run = async (): Promise<{ trace: ParsedTrace; buffer: ArrayBuffer }> => {
    if (mode === "stream") {
      emit("parsing", 0, null);
      return streamTraceWithCapture(list, fetchOpts, parseOpts);
    }
    emit("fetching", 0, null);
    const buffer = await fetchTraces([...list], fetchOpts);
    fetchDoneMs = performance.now();
    perf.mark("fetch-done");
    emit("parsing", 0, buffer.byteLength);
    // Buffered mode is the only path that knows the decoded size before
    // parsing, so it is the only one that can size the columns to land in a
    // single allocation instead of doubling-and-copying all 12 of them up to
    // the final length.
    parseOpts.eventSink = new ColumnarEvents(capacityForBytes(buffer.byteLength));
    const trace = await parseTrace(buffer, parseOpts);
    return { trace, buffer };
  };

  run()
    .then(({ trace, buffer }) => {
      perf.mark("parse-done");
      // Attach the columnar span-event store; buildSpanDataColumnar reads it
      // instead of the (now non-span-only) fat customEvents array.
      trace.spanEvents = spanEventSink;
      settle(() => {
        store.update("trace", { trace });
        perf.mark("store-updated");
        // The store arms its flush rAF inside update() above, so this callback
        // is registered AFTER it and runs once the first render pass over the
        // new trace has completed. That pass is where every consumer's
        // post-parse derivation lands, which is what "derive" measures.
        const report = (): void =>
          perf.finish({
            mode,
            urlCount: list.length,
            events: trace.events.length,
            bytes: buffer.byteLength,
          });
        if (typeof requestAnimationFrame === "function") {
          requestAnimationFrame(() => {
            perf.mark("first-paint");
            report();
          });
        } else {
          report(); // No rAF (tests / headless): report without the derive span.
        }
        const timing: TraceWorkerTiming = {
          startMs,
          fetchDoneMs,
          parseDoneMs: performance.now(),
          mode,
          events: trace.events.length,
          bytes: buffer.byteLength,
        };
        resolveDone({ trace, buffer, mode, timing });
      });
    })
    .catch((err: unknown) => {
      // Release this run's marks; an aborted/failed load has no useful spans
      // but would otherwise leak entries into the User Timing buffer.
      perf.finish({ mode, urlCount: list.length });
      settle(() => {
        const e = err as { message?: unknown; name?: unknown } | null;
        const error = new Error(
          typeof e?.message === "string" ? e.message : String(err)
        );
        error.name = typeof e?.name === "string" ? e.name : "Error";
        rejectDone(error);
      });
    });

  return {
    done,
    abort: (): void => {
      controller.abort();
    },
  };
}

// objectTraceUrls lives in ./object-urls.ts so the browser page can import
// it without pulling the parser into its bundle; re-exported here to keep
// this module's import surface unchanged.
export { objectTraceUrls } from "./object-urls.js";
