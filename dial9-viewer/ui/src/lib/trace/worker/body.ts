// lib/trace/worker/body.ts - the worker load pipeline as a PURE module: a
// message handler over a postMessage-shaped sink, with no Worker-global
// bindings (T16; ADR-0004 section 6 "do now": run the frozen core in a Web
// Worker unchanged - the 3.8-12.2 s load walls are main-thread parse time,
// 03-performance-findings.md). The thin environment bindings live next
// door: trace-worker.ts (browser, Vite worker build) and
// node-worker-entry.mjs (node:worker_threads, for the Vitest integration
// tests). Keeping the body pure lets Node tests drive the full pipeline
// in-process, while the worker_threads harness exercises the real thread
// + structured-clone boundary.
//
// PLAIN-NODE CONSTRAINT: node-worker-entry.mjs runs this file under Node's
// native type stripping, which resolves import specifiers on disk as
// written - no bundler. Every RUNTIME import reachable from here must
// resolve on disk: the frozen core's real .js files, or sibling TS via an
// explicit .ts specifier (hence allowImportingTsExtensions in tsconfig).
// Type-only imports are erased by the stripper and exempt.

import {
  canStreamDecode,
  fetchTraces,
  parseTrace,
} from "../../../../trace_parser.js";
import { parseChunksWithCapture, streamTraceWithCapture } from "../stream.ts";
import type { ParsedTrace } from "../../../../trace_parser.js";
import type {
  TraceWorkerLoadMode,
  TraceWorkerLoadRequest,
  TraceWorkerParseBufferRequest,
  TraceWorkerPost,
  TraceWorkerRequest,
  TraceWorkerTiming,
} from "./protocol.js";

export interface TraceWorkerBody {
  handle(message: TraceWorkerRequest): void;
}

/**
 * The progress/done reporting shared by both job kinds (url load and
 * buffer parse): posts progress messages carrying the features/02 B8/B9
 * load-timing fields, and the final done message with the B16 timing
 * record and the decompressed buffer transferred back zero-copy.
 */
function makeReporter(
  post: TraceWorkerPost,
  startMs: number,
  mode: TraceWorkerLoadMode,
  urlCount: number
) {
  let eventCount = 0;
  const progress = (
    phase: "fetching" | "parsing",
    bytesRead: number,
    totalBytes: number | null
  ): void => {
    post({
      kind: "progress",
      progress: {
        phase,
        mode,
        urlCount,
        bytesRead,
        totalBytes,
        eventCount,
        startMs,
        elapsedMs: performance.now() - startMs,
      },
    });
  };
  // The core fires this every 100 KB decoded (trace_parser.js); each
  // firing becomes one small progress message. Inside a worker the
  // parse loop's paint-yield macrotasks (B18) still run, which is what
  // lets the "abort" request be processed mid-parse.
  const onParseProgress = (p: {
    bytesRead: number;
    totalBytes: number | null;
    eventCount: number;
  }): void => {
    eventCount = p.eventCount;
    progress("parsing", p.bytesRead, p.totalBytes);
  };

  const finish = (
    trace: ParsedTrace,
    buffer: ArrayBuffer,
    fetchDoneMs: number | null
  ): void => {
    const timing: TraceWorkerTiming = {
      startMs,
      fetchDoneMs,
      parseDoneMs: performance.now(),
      mode,
      events: trace.events.length,
      bytes: buffer.byteLength,
    };
    // The buffer is TRANSFERRED (zero-copy); it is detached on this
    // side after post, so timing.bytes is computed above, before.
    post({ kind: "done", trace, buffer, mode, timing }, [buffer]);
  };

  return { progress, onParseProgress, finish };
}

const GZIP_MAGIC_0 = 0x1f;
const GZIP_MAGIC_1 = 0x8b;

/**
 * Gunzip `bytes` as an async chunk stream via DecompressionStream. Reads
 * through getReader() rather than async-iterating the ReadableStream
 * directly: Safari lacks ReadableStream[Symbol.asyncIterator] (the frozen
 * core's fetchTraceStream wraps a reader for the same reason).
 */
async function* gunzipChunks(bytes: Uint8Array): AsyncGenerator<Uint8Array> {
  const stream = new Blob([bytes as BlobPart])
    .stream()
    .pipeThrough(new DecompressionStream("gzip"));
  const reader = stream.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return;
    yield value;
  }
}

/**
 * Create the load-message handler over `post`. One body handles ONE job
 * (url load or buffer parse, mirroring the one-worker-per-job
 * orchestration in load.ts / segments.ts); a second job request is a
 * protocol error.
 */
export function createWorkerBody(post: TraceWorkerPost): TraceWorkerBody {
  // The body's fetch controller: an "abort" request aborts the in-flight
  // fetch (cooperative cancellation). The orchestrator's authoritative
  // cancel is terminating the worker, which also kills a compute-bound
  // parse phase that no signal reaches (see protocol.ts).
  const controller = new AbortController();
  let loading = false;

  async function runLoad(request: TraceWorkerLoadRequest): Promise<void> {
    const startMs = performance.now();
    const urls = request.urls;
    const parseOpts = request.parse ?? {};
    const mode: TraceWorkerLoadMode = canStreamDecode() ? "stream" : "buffered";
    const { progress, onParseProgress, finish } = makeReporter(
      post,
      startMs,
      mode,
      urls.length
    );

    const fetchOpts = { signal: controller.signal, headers: request.headers };
    if (mode === "stream") {
      // Streaming fuses fetch+parse (B12): no separate fetch mark
      // (fetchDoneMs stays null, legacy loadPerf parity), and the first
      // progress signals the parse phase directly.
      progress("parsing", 0, null);
      const { trace, buffer } = await streamTraceWithCapture(urls, fetchOpts, {
        ...parseOpts,
        onParseProgress,
      });
      finish(trace, buffer, null);
    } else {
      // Buffered fallback (no DecompressionStream): fetch every component
      // in parallel, gunzip + concatenate, then parse - separate phases.
      progress("fetching", 0, null);
      const buffer = await fetchTraces([...urls], fetchOpts);
      const fetchDoneMs = performance.now();
      progress("parsing", 0, buffer.byteLength);
      const trace = await parseTrace(buffer, { ...parseOpts, onParseProgress });
      finish(trace, buffer, fetchDoneMs);
    }
  }

  // Parse an already-fetched buffer (T17 segment windowing; see
  // protocol.ts). No fetch phase: the bytes arrived with the request.
  // Gzipped bytes stream through DecompressionStream so parse overlaps
  // gunzip (mode "stream", and the done buffer is the decompressed bytes
  // - its byteLength is what the segment budget accountant records); raw
  // bytes parse directly (mode "buffered", total size known up front).
  async function runParseBuffer(
    request: TraceWorkerParseBufferRequest
  ): Promise<void> {
    const startMs = performance.now();
    const parseOpts = request.parse ?? {};
    const bytes = new Uint8Array(request.buffer);
    const gzipped =
      bytes.length >= 2 &&
      bytes[0] === GZIP_MAGIC_0 &&
      bytes[1] === GZIP_MAGIC_1;

    if (gzipped) {
      // The core's maybeGunzip fallback for runtimes without
      // DecompressionStream is require("zlib"), which does not exist in
      // a browser worker - fail explicitly rather than let the bare
      // `require` throw a ReferenceError deep inside the core. (Every
      // runtime that reaches the segment tier has DecompressionStream;
      // Node >= 18 always does.)
      if (typeof DecompressionStream === "undefined") {
        throw new Error(
          "parse-buffer with gzipped bytes requires DecompressionStream"
        );
      }
      const { progress, onParseProgress, finish } = makeReporter(
        post,
        startMs,
        "stream",
        1
      );
      progress("parsing", 0, null);
      const { trace, buffer } = await parseChunksWithCapture(
        gunzipChunks(bytes),
        { ...parseOpts, onParseProgress }
      );
      finish(trace, buffer, null);
    } else {
      const { progress, onParseProgress, finish } = makeReporter(
        post,
        startMs,
        "buffered",
        1
      );
      progress("parsing", 0, bytes.byteLength);
      const trace = await parseTrace(request.buffer, {
        ...parseOpts,
        onParseProgress,
      });
      finish(trace, request.buffer, null);
    }
  }

  /** Shared one-job guard + error funnel for both job kinds. */
  function startJob(run: () => Promise<void>): void {
    if (loading) {
      post({
        kind: "error",
        name: "Error",
        message:
          "trace worker already has a load in flight (one load per worker)",
      });
      return;
    }
    loading = true;
    run().catch((err: unknown) => {
      // Every failure - AbortError included - surfaces as an error
      // message with the name preserved. The orchestrator drops
      // messages arriving after an abort, so pages never see the
      // abort echo; other errors keep name-based handling (the
      // HTTP-401 credentials hint etc. are page concerns).
      const e = err as { message?: unknown; name?: unknown } | null;
      post({
        kind: "error",
        message: typeof e?.message === "string" ? e.message : String(err),
        name: typeof e?.name === "string" ? e.name : "Error",
      });
    });
  }

  return {
    handle(message: TraceWorkerRequest): void {
      switch (message.kind) {
        case "load":
          startJob(() => runLoad(message));
          return;
        case "parse-buffer":
          startJob(() => runParseBuffer(message));
          return;
        case "abort":
          controller.abort();
          return;
      }
    },
  };
}
