// Worker-path integration tests (T16 DoD): the REAL boundary. A
// node:worker_threads Worker runs node-worker-entry.mjs (which loads
// body.ts via Node's native type stripping), fetch runs INSIDE that
// worker against a local http server serving the demo trace, and every
// message crosses an actual postMessage structured-clone hop - the same
// algorithm browsers use. The orchestrator (loadTraceInWorker) drives it
// through a ~15-line Worker-API adapter and writes into a real T07 store.
//
// LAYER MAP (what each T16 suite exercises; for T12/T13 verification):
//   - worker/body.test.ts: pipeline logic in-process (no thread).
//   - load.worker.test.ts: orchestrator contract vs a scripted port.
//   - THIS FILE: real thread + clone boundary + in-worker fetch + store.
//   - Browser-only remainder: the Vite `new Worker(new URL(...))` entry
//     bundling in load.ts's default factory (node has no Web Worker;
//     verified at build time by the dist worker chunk, exercised live
//     once a page migrates - T13).

import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { Worker } from "node:worker_threads";
import { gunzipSync, gzipSync } from "node:zlib";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { createStore } from "../../../store/store.js";
import { loadTraceInWorker, parseTraceBuffer } from "../load.js";
import { parseSegmentInWorker } from "../segments.js";
import type { Server } from "node:http";
import type { Store } from "../../../store/store.js";
import type { TraceSlice } from "../../../types/state.js";
import type { ParsedTrace } from "../load.js";
import type {
  TraceWorkerFactory,
  TraceWorkerProgress,
  TraceWorkerResponse,
} from "./protocol.js";

// ── Local trace server: the worker's fetch target ────────────────────────

let server: Server;
let baseUrl: string;
let rawTrace: Uint8Array;
let gzTrace: Buffer;
let direct: ParsedTrace;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../../public/demo-trace.bin", import.meta.url))
  );
  rawTrace =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  gzTrace = gzipSync(rawTrace);
  direct = await parseTraceBuffer(rawTrace);

  server = createServer((req, res) => {
    if (req.url === "/demo.gz") {
      // Raw still-gzipped bytes, no Content-Encoding: the /api/object
      // serving shape - the client gunzips.
      res.writeHead(200, { "content-type": "application/octet-stream" });
      res.end(gzTrace);
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise<void>((resolve) => {
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (address === null || typeof address === "string") {
    throw new Error("unexpected server address");
  }
  baseUrl = `http://127.0.0.1:${address.port}`;
}, 60_000);

afterAll(async () => {
  await new Promise((resolve) => server.close(resolve));
});

// ── worker_threads adapter over the TraceWorkerPort surface ──────────────

function makeNodeWorker(): {
  factory: TraceWorkerFactory;
  /** Resolves with the exit code once the thread is gone (the DoD
   * "worker terminated, no live handle" observable). */
  exited: Promise<number>;
} {
  let resolveExited!: (code: number) => void;
  const exited = new Promise<number>((resolve) => {
    resolveExited = resolve;
  });
  const factory: TraceWorkerFactory = () => {
    // execArgv: [] - the worker must run PLAIN node (type stripping is
    // default-on), not inherit vitest's runner flags.
    const worker = new Worker(new URL("./node-worker-entry.mjs", import.meta.url), {
      execArgv: [],
    });
    worker.on("exit", (code) => {
      resolveExited(code);
    });
    return {
      postMessage: (message) => worker.postMessage(message),
      onMessage: (fn) => {
        worker.on("message", (message) => fn(message as TraceWorkerResponse));
      },
      onError: (fn) => {
        worker.on("error", fn);
      },
      terminate: () => {
        void worker.terminate();
      },
    };
  };
  return { factory, exited };
}

function makeTraceStore(): Store<{ trace: TraceSlice }> {
  return createStore<{ trace: TraceSlice }>(
    { trace: { trace: null } },
    { scheduler: (cb) => queueMicrotask(cb) }
  );
}

// Byte-equality via Buffer.equals (memcmp; see the T09 toEqual trap).
function expectBytesEqual(actual: Uint8Array, expected: Uint8Array): void {
  expect(actual.length).toBe(expected.length);
  expect(Buffer.from(actual).equals(Buffer.from(expected))).toBe(true);
}

// ── DoD check 1+2: parity through the worker path, live progress ─────────

describe("worker path (real thread + structured clone)", () => {
  it("parses the demo trace deep-equal to a direct parse and streams progress", async () => {
    const store = makeTraceStore();
    const progress: TraceWorkerProgress[] = [];
    const { factory, exited } = makeNodeWorker();
    const handle = loadTraceInWorker(store, `${baseUrl}/demo.gz`, {
      onProgress: (p) => progress.push(p),
      worker: factory,
    });
    const result = await handle.done;

    // Counts across every top-level collection that matters.
    expect(result.trace.events.length).toBe(direct.events.length);
    expect(result.trace.cpuSamples.length).toBe(direct.cpuSamples.length);
    expect(result.trace.customEvents.length).toBe(direct.customEvents.length);
    expect(result.trace.allocEvents.length).toBe(direct.allocEvents.length);
    expect(result.trace.blockInPlaceGaps.length).toBe(direct.blockInPlaceGaps.length);

    // Maps survive the clone AS Maps, sizes intact.
    expect(result.trace.callframeSymbols).toBeInstanceOf(Map);
    expect(result.trace.callframeSymbols.size).toBe(direct.callframeSymbols.size);
    expect(result.trace.runtimeWorkers).toBeInstanceOf(Map);
    expect(result.trace.runtimeWorkers.size).toBe(direct.runtimeWorkers.size);
    expect(result.trace.taskSpawnLocs.size).toBe(direct.taskSpawnLocs.size);
    expect(result.trace.threadNames.size).toBe(direct.threadNames.size);

    // Spot samples (per-element toEqual is fine; whole-array is not).
    const mid = Math.floor(direct.events.length / 2);
    expect(result.trace.events[0]).toEqual(direct.events[0]);
    expect(result.trace.events[mid]).toEqual(direct.events[mid]);
    expect(result.trace.events.at(-1)).toEqual(direct.events.at(-1));
    const midCustom = Math.floor(direct.customEvents.length / 2);
    expect(result.trace.customEvents[midCustom]).toEqual(direct.customEvents[midCustom]);
    const firstSymbolKey = direct.callframeSymbols.keys().next().value;
    expect(firstSymbolKey).toBeDefined();
    expect(result.trace.callframeSymbols.get(firstSymbolKey!)).toEqual(
      direct.callframeSymbols.get(firstSymbolKey!)
    );
    expect(result.trace.minTs).toBe(direct.minTs);
    expect(result.trace.maxTs).toBe(direct.maxTs);
    expect(result.trace.hasTaskTracking).toBe(direct.hasTaskTracking);

    // The transferred buffer round-trips byte-exact (re-parse source).
    expectBytesEqual(new Uint8Array(result.buffer), rawTrace);

    // The store's trace slice received the parsed trace.
    expect(store.getState().trace.trace).toBe(result.trace);

    // DoD: progress events with the load-timing fields non-empty.
    expect(progress.length).toBeGreaterThan(1);
    for (const p of progress) {
      expect(p.phase === "parsing" || p.phase === "fetching").toBe(true);
      expect(p.mode).toBe("stream"); // Node worker can stream-decode
      expect(p.urlCount).toBe(1);
      expect(p.bytesRead).toBeGreaterThanOrEqual(0);
      expect(p.eventCount).toBeGreaterThanOrEqual(0);
      expect(p.startMs).toBeGreaterThanOrEqual(0);
      expect(p.elapsedMs).toBeGreaterThanOrEqual(0);
    }
    const last = progress.at(-1)!;
    expect(last.bytesRead).toBeGreaterThan(0);
    expect(last.eventCount).toBeGreaterThan(0);

    // Timing record (B16 worker-measurable slice).
    expect(result.mode).toBe("stream");
    expect(result.timing.fetchDoneMs).toBeNull();
    expect(result.timing.parseDoneMs).toBeGreaterThanOrEqual(result.timing.startMs);
    expect(result.timing.events).toBe(direct.events.length);
    expect(result.timing.bytes).toBe(rawTrace.length);

    // Single-shot worker: the thread is gone after settle.
    await exited;
  }, 120_000);

  // ── DoD check 3: the three abort observables ──────────────────────────

  it("abort mid-parse: worker terminated, trace slice unchanged, no late progress", async () => {
    const store = makeTraceStore();
    let abortRequested = false;
    let progressAfterAbort = 0;
    const seen: TraceWorkerProgress[] = [];
    const { factory, exited } = makeNodeWorker();
    const handle = loadTraceInWorker(store, `${baseUrl}/demo.gz`, {
      onProgress: (p) => {
        if (abortRequested) progressAfterAbort += 1;
        seen.push(p);
      },
      worker: factory,
    });

    // Abort only once the parse is demonstrably mid-flight (decoded bytes
    // and events are flowing; the demo parse takes ~500 ms with ~100
    // progress ticks, so this lands well before completion).
    await vi.waitFor(
      () => {
        expect(seen.some((p) => p.phase === "parsing" && p.bytesRead > 0)).toBe(true);
      },
      { timeout: 60_000 }
    );
    abortRequested = true;
    handle.abort();

    // Observable 1: the worker is terminated - the thread actually exits
    // (no live handle would keep this promise pending and the test would
    // time out).
    await exited;

    // The handle settles as an abort.
    await expect(handle.done).rejects.toMatchObject({ name: "AbortError" });

    // Observable 2: the store's trace slice is untouched.
    expect(store.getState().trace.trace).toBeNull();

    // Observable 3: no pending progress callbacks fire after abort - give
    // any messages queued behind the abort a chance to (wrongly) deliver.
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(progressAfterAbort).toBe(0);
    expect(store.getState().trace.trace).toBeNull();
  }, 120_000);

  it("a worker-side fetch failure propagates across the thread with the status", async () => {
    const store = makeTraceStore();
    const { factory, exited } = makeNodeWorker();
    const handle = loadTraceInWorker(store, `${baseUrl}/missing.gz`, {
      worker: factory,
    });
    await expect(handle.done).rejects.toMatchObject({
      message: expect.stringMatching(/404/) as unknown,
    });
    expect(store.getState().trace.trace).toBeNull();
    await exited;
  }, 120_000);

  // ── T17: the parse-buffer request across the real clone boundary ──────

  it("parse-buffer: cached gzipped bytes parse in the thread with parity, no fetch involved", async () => {
    const { factory, exited } = makeNodeWorker();
    const progress: TraceWorkerProgress[] = [];
    const job = parseSegmentInWorker(new Uint8Array(gzTrace), {
      worker: factory,
      onProgress: (p) => progress.push(p),
    });
    const result = await job.done;

    // Parity: counts + spot samples (whole-object toEqual OOMs, T09 trap).
    expect(result.trace.events.length).toBe(direct.events.length);
    expect(result.trace.events[0]).toEqual(direct.events[0]);
    expect(result.trace.events.at(-1)).toEqual(direct.events.at(-1));
    expect(result.trace.minTs).toBe(direct.minTs);
    expect(result.trace.maxTs).toBe(direct.maxTs);
    expect(result.trace.callframeSymbols.size).toBe(direct.callframeSymbols.size);
    // The decompressed size is what the budget accountant records.
    expect(result.rawByteLength).toBe(rawTrace.length);
    // Progress flowed and never claimed a fetch phase.
    expect(progress.length).toBeGreaterThan(1);
    expect(progress.every((p) => p.phase === "parsing")).toBe(true);
    // Single-shot worker: the thread is gone after settle.
    await exited;
  }, 120_000);
});
