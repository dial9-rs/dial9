// lib/trace/worker/protocol.ts - the message contract between the worker
// load orchestrator (load.ts loadTraceInWorker) and the worker load body
// (worker/body.ts), plus the Worker-API port abstraction the orchestrator
// drives (T16; ADR-0004 section 6 "do now"; architecture 2.8 worker tier).
//
// TYPES ONLY - no runtime exports. body.ts must stay runnable under plain
// Node's native type stripping (see node-worker-entry.mjs), where
// `import type` of this module is erased instead of resolved on disk.
//
// CLONEABILITY (T16 first work item): every message crosses a
// structured-clone boundary (postMessage). Verified against the real parse
// output of public/demo-trace.bin under Node: ParsedTrace is plain data -
// plain objects/arrays, numbers, strings, nulls, and 11 Maps (Maps ARE
// structured-cloneable and round-trip as Maps); zero functions, accessors/
// getters, class instances, symbols, or typed-array leaves. Custom-event
// field values may be bigint (decode.js DecodedFieldValue), which the
// structured clone algorithm also supports. structuredClone() of the full
// 294k-event demo parse succeeds and round-trips counts + spot samples.
// No serializable snapshot shape is therefore needed; ParsedTrace crosses
// as-is. (The features/01 parseKey getters are page code, not core
// output.) `buffer` is an ArrayBuffer and is TRANSFERRED (zero-copy), not
// cloned.

import type { ParsedTrace } from "../../../../trace_parser.js";

/** How the load acquired bytes (features/02 B12/B16 `mode`). */
export type TraceWorkerLoadMode = "stream" | "buffered";

/**
 * Parse options that can cross the worker boundary. Deliberately NOT the
 * core's ParseOptions: callbacks (onParseProgress) cannot be cloned - the
 * body installs its own callback and forwards progress as messages.
 */
export interface TraceWorkerParseOptions {
  /** Cap event count (metadata/symbols always parsed). */
  maxEvents?: number;
  /** Filter events to a time range (absolute ns, inclusive). */
  startTime?: number;
  endTime?: number;
}

/**
 * Start a whole-trace load: fetch runs INSIDE the worker (T16 decision),
 * then gunzip + parse, then the result posts back.
 */
export interface TraceWorkerLoadRequest {
  kind: "load";
  /** Repeatable `trace=` components, fetched as one logical trace. */
  urls: readonly string[];
  /**
   * Same-origin credential headers (features/02 B17). The page resolves
   * Dial9Creds.headers() on the main thread and passes the plain record;
   * the core's same-origin withholding still applies inside the worker.
   */
  headers?: Record<string, string>;
  parse?: TraceWorkerParseOptions;
}

/**
 * Parse an already-fetched buffer (T17 segment windowing): the segment
 * orchestrator fetches raw gzipped segment bytes on the main thread (it
 * owns the raw-gzip byte cache, architecture 2.8 two-level cache) and
 * hands them here for off-main-thread gunzip + parse. The bytes may still
 * be gzipped (sniffed by magic, like the core's maybeGunzip) or raw.
 *
 * The buffer is CLONED, not transferred: the sender's copy is a live
 * cache entry that must survive the parse (a transfer would detach it).
 * The done message still transfers the decompressed buffer back.
 */
export interface TraceWorkerParseBufferRequest {
  kind: "parse-buffer";
  buffer: ArrayBuffer;
  parse?: TraceWorkerParseOptions;
}

/**
 * Abort the in-flight load: the body aborts its internal fetch
 * controller. Best-effort fetch-level cancellation - the orchestrator's
 * authoritative cancel is port.terminate(), which also kills a
 * compute-bound parse phase that no signal reaches. The message exists so
 * in-process transports (tests) and network cleanup get a cooperative
 * path.
 */
export interface TraceWorkerAbortRequest {
  kind: "abort";
}

export type TraceWorkerRequest =
  | TraceWorkerLoadRequest
  | TraceWorkerParseBufferRequest
  | TraceWorkerAbortRequest;

/**
 * One progress update. Carries the data behind the load-timing UX rows
 * (features/02 B8 progress text, B9 elapsed, B16 marks); the page owns
 * turning them into labels ("Fetching...", "Parsing: N% - Yk events",
 * "Loading N traces...").
 *
 * Clock note: startMs/elapsedMs are worker-clock performance.now() values
 * (a worker has its own timeOrigin). Consumers must use deltas, never mix
 * them with main-thread performance.now() marks.
 */
export interface TraceWorkerProgress {
  /**
   * "fetching" while the buffered-mode download runs; "parsing" once
   * decoded bytes flow. Stream mode is always "parsing" - fetch and parse
   * are fused (B12).
   */
  phase: "fetching" | "parsing";
  mode: TraceWorkerLoadMode;
  /** Number of trace= components in this load (B8 multi-trace labels). */
  urlCount: number;
  /** Decompressed bytes fed to the parser so far (B8 "X MB"). */
  bytesRead: number;
  /**
   * Total decompressed bytes; null while streaming (size unknown until
   * the stream ends - B8 shows MB instead of a percentage).
   */
  totalBytes: number | null;
  /** Events decoded so far (B8 "Yk events"). */
  eventCount: number;
  /** Worker-clock ms mark when the load began (B16 startMs). */
  startMs: number;
  /** Ms since startMs at post time (B9 elapsed). */
  elapsedMs: number;
}

/**
 * The worker-measurable slice of the legacy loadPerf record (features/02
 * B16). `totalMs` is deliberately absent: it is defined as "start ->
 * render-complete" and is finalized page-side via double-rAF after layout;
 * pages derive it from their own clock. Same worker-clock note as
 * TraceWorkerProgress.
 */
export interface TraceWorkerTiming {
  startMs: number;
  /**
   * Buffered mode only: when the fetch phase completed. null in stream
   * mode - streaming fuses fetch+parse, so there is no separate fetch
   * mark (legacy loadPerf parity).
   */
  fetchDoneMs: number | null;
  /** When parsing finished (all modes). */
  parseDoneMs: number;
  mode: TraceWorkerLoadMode;
  /** Decoded event count. */
  events: number;
  /** Decompressed buffer length. */
  bytes: number;
}

export interface TraceWorkerProgressMessage {
  kind: "progress";
  progress: TraceWorkerProgress;
}

export interface TraceWorkerDoneMessage {
  kind: "done";
  /** Structured-cloned as-is (see the cloneability note above). */
  trace: ParsedTrace;
  /**
   * The raw (gunzipped, concatenated) trace bytes, TRANSFERRED zero-copy
   * for Set/Clear-Range in-memory re-parse (features/02 B14).
   */
  buffer: ArrayBuffer;
  mode: TraceWorkerLoadMode;
  timing: TraceWorkerTiming;
}

/**
 * Any load failure, AbortError included (the body does not special-case
 * aborts; the orchestrator drops post-abort messages, and non-abort
 * consumers get `name` to preserve AbortError detection page-side).
 */
export interface TraceWorkerErrorMessage {
  kind: "error";
  message: string;
  name: string;
}

export type TraceWorkerResponse =
  | TraceWorkerProgressMessage
  | TraceWorkerDoneMessage
  | TraceWorkerErrorMessage;

/**
 * postMessage-shaped sink the body writes responses into. `transfer`
 * lists ArrayBuffers to move rather than clone (the done message's
 * buffer). Bindings map this onto their environment's postMessage.
 */
export type TraceWorkerPost = (
  message: TraceWorkerResponse,
  transfer?: ArrayBuffer[]
) => void;

/**
 * The Worker-API surface the orchestrator drives, so the transport is
 * substitutable: the default factory (load.ts) wraps a browser Worker on
 * the Vite-built trace-worker.ts entry; tests wrap node:worker_threads on
 * node-worker-entry.mjs, or drive the body in-process.
 */
export interface TraceWorkerPort {
  postMessage(message: TraceWorkerRequest): void;
  /** Register THE message consumer (single subscriber). */
  onMessage(fn: (message: TraceWorkerResponse) => void): void;
  /** Register THE transport-error consumer (worker script/thread crash). */
  onError(fn: (error: unknown) => void): void;
  /** Hard-kill the worker; must be idempotent. */
  terminate(): void;
}

/** Creates the port for one load (one worker per whole-trace load; T17
 * revisits lifetime for the per-segment tier). */
export type TraceWorkerFactory = () => TraceWorkerPort;
