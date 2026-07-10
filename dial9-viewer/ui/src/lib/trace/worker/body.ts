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
import { streamTraceWithCapture } from "../stream.ts";
import type { ParsedTrace } from "../../../../trace_parser.js";
import type {
  TraceWorkerLoadMode,
  TraceWorkerLoadRequest,
  TraceWorkerPost,
  TraceWorkerRequest,
  TraceWorkerTiming,
} from "./protocol.js";

export interface TraceWorkerBody {
  handle(message: TraceWorkerRequest): void;
}

/**
 * Create the load-message handler over `post`. One body handles ONE load
 * (mirroring the one-worker-per-load orchestration in load.ts); a second
 * load request is a protocol error.
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
          urlCount: urls.length,
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

  return {
    handle(message: TraceWorkerRequest): void {
      switch (message.kind) {
        case "load": {
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
          runLoad(message).catch((err: unknown) => {
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
          return;
        }
        case "abort":
          controller.abort();
          return;
      }
    },
  };
}
