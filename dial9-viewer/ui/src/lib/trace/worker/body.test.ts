// body.ts tests (T16): the worker load pipeline driven IN-PROCESS - the
// pure message handler with a captured post sink and a stubbed global
// fetch (the load.test.ts fixture pattern). This layer covers the full
// pipeline logic (mode selection, fetch-in-worker, gunzip, capture,
// progress fields, timing marks, abort wiring) without a thread; the real
// thread + structured-clone boundary is integration.test.ts's job, and
// the Vite `new Worker(new URL(...))` binding itself is browser-only
// (noted for T12/T13 verification).

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { canStreamDecode, parseTraceBuffer } from "../load.js";
import { createWorkerBody } from "./body.js";
import type { ParsedTrace } from "../load.js";
import type {
  TraceWorkerDoneMessage,
  TraceWorkerErrorMessage,
  TraceWorkerProgressMessage,
  TraceWorkerResponse,
} from "./protocol.js";

// ── Fixtures: the demo trace, raw and gzipped, fully in memory ──────────

let rawTrace: Uint8Array;
let gzTrace: Uint8Array;
let direct: ParsedTrace;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../../public/demo-trace.bin", import.meta.url))
  );
  rawTrace =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  gzTrace = new Uint8Array(gzipSync(rawTrace));
  direct = await parseTraceBuffer(rawTrace);
  expect(direct.events.length).toBeGreaterThan(0);
});

// ── fetch stub (Response-like with arrayBuffer(); streams fall back to ──
// the core's oneShotReader path when resp.body is absent) ────────────────

const originalFetch = globalThis.fetch;

function installFetchMock(urlToBytes: Record<string, Uint8Array>): void {
  globalThis.fetch = (async (url: string) => {
    const bytes = urlToBytes[url];
    if (!bytes) {
      return {
        ok: false,
        status: 404,
        async arrayBuffer() {
          return new ArrayBuffer(0);
        },
      };
    }
    return {
      ok: true,
      status: 200,
      async arrayBuffer() {
        return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
      },
    };
  }) as unknown as typeof fetch;
}

/** A fetch that never resolves but rejects with AbortError on its signal. */
function installHangingFetchMock(): void {
  globalThis.fetch = ((_url: string, opts?: { signal?: AbortSignal }) =>
    new Promise((_resolve, reject) => {
      opts?.signal?.addEventListener(
        "abort",
        () => {
          reject(new DOMException("The operation was aborted.", "AbortError"));
        },
        { once: true }
      );
    })) as unknown as typeof fetch;
}

afterEach(() => {
  globalThis.fetch = originalFetch;
  vi.unstubAllGlobals();
});

// ── Sink: collect posted responses ───────────────────────────────────────

interface Sink {
  messages: TraceWorkerResponse[];
  post: (message: TraceWorkerResponse, transfer?: ArrayBuffer[]) => void;
}

function makeSink(): Sink {
  const messages: TraceWorkerResponse[] = [];
  return { messages, post: (message) => messages.push(message) };
}

const progressOf = (sink: Sink): TraceWorkerProgressMessage[] =>
  sink.messages.filter((m): m is TraceWorkerProgressMessage => m.kind === "progress");
const doneOf = (sink: Sink): TraceWorkerDoneMessage[] =>
  sink.messages.filter((m): m is TraceWorkerDoneMessage => m.kind === "done");
const errorsOf = (sink: Sink): TraceWorkerErrorMessage[] =>
  sink.messages.filter((m): m is TraceWorkerErrorMessage => m.kind === "error");

async function waitForSettled(sink: Sink): Promise<void> {
  await vi.waitFor(
    () => {
      expect(doneOf(sink).length + errorsOf(sink).length).toBeGreaterThan(0);
    },
    { timeout: 30_000 }
  );
}

// Byte-equality via Buffer.equals (memcmp): vitest's toEqual deep-diffs
// typed arrays element-by-element, which times out on the ~11 MB trace.
function expectBytesEqual(actual: Uint8Array, expected: Uint8Array): void {
  expect(actual.length).toBe(expected.length);
  expect(Buffer.from(actual).equals(Buffer.from(expected))).toBe(true);
}

// ── Stream mode (Node supports DecompressionStream: default path) ───────

describe("worker body: stream load", () => {
  it("posts done with parity to a direct parse, plus full progress/timing fields", async () => {
    expect(canStreamDecode()).toBe(true);
    installFetchMock({ "/demo.gz": gzTrace });
    const sink = makeSink();
    const body = createWorkerBody(sink.post);
    body.handle({ kind: "load", urls: ["/demo.gz"] });
    await waitForSettled(sink);

    expect(errorsOf(sink)).toEqual([]);
    const done = doneOf(sink)[0]!;

    // Parity: counts + spot samples (NOT whole-object toEqual; the
    // ~300k-event arrays OOM vitest's differ - T09 HANDOFF trap).
    expect(done.trace.events.length).toBe(direct.events.length);
    expect(done.trace.cpuSamples.length).toBe(direct.cpuSamples.length);
    expect(done.trace.customEvents.length).toBe(direct.customEvents.length);
    expect(done.trace.events[0]).toEqual(direct.events[0]);
    const mid = Math.floor(direct.events.length / 2);
    expect(done.trace.events[mid]).toEqual(direct.events[mid]);
    expect(done.trace.events.at(-1)).toEqual(direct.events.at(-1));
    expect(done.trace.minTs).toBe(direct.minTs);
    expect(done.trace.maxTs).toBe(direct.maxTs);
    expect(done.trace.callframeSymbols.size).toBe(direct.callframeSymbols.size);

    // The raw buffer round-trips for Set/Clear-Range re-parse (B14).
    expect(done.mode).toBe("stream");
    expectBytesEqual(new Uint8Array(done.buffer), rawTrace);

    // Progress: first message opens the parse phase; later ones carry
    // live byte/event counters; every load-timing field is populated.
    const progress = progressOf(sink);
    expect(progress.length).toBeGreaterThan(1);
    const first = progress[0]!.progress;
    expect(first).toMatchObject({
      phase: "parsing",
      mode: "stream",
      urlCount: 1,
      bytesRead: 0,
      totalBytes: null,
      eventCount: 0,
    });
    expect(first.startMs).toBeGreaterThanOrEqual(0);
    expect(first.elapsedMs).toBeGreaterThanOrEqual(0);
    const live = progress.map((m) => m.progress).filter((p) => p.bytesRead > 0);
    expect(live.length).toBeGreaterThan(0);
    expect(live.at(-1)!.eventCount).toBeGreaterThan(0);

    // Timing (B16 minus page-side totalMs): stream mode has no separate
    // fetch mark.
    expect(done.timing.mode).toBe("stream");
    expect(done.timing.fetchDoneMs).toBeNull();
    expect(done.timing.parseDoneMs).toBeGreaterThanOrEqual(done.timing.startMs);
    expect(done.timing.events).toBe(direct.events.length);
    expect(done.timing.bytes).toBe(rawTrace.length);
  }, 60_000);

  it("forwards wire parse options (maxEvents) to the parser", async () => {
    installFetchMock({ "/demo.gz": gzTrace });
    const sink = makeSink();
    createWorkerBody(sink.post).handle({
      kind: "load",
      urls: ["/demo.gz"],
      parse: { maxEvents: 5 },
    });
    await waitForSettled(sink);
    expect(errorsOf(sink)).toEqual([]);
    const done = doneOf(sink)[0]!;
    expect(done.trace.events.length).toBe(5);
    expect(done.trace.truncated).toBe(true);
  }, 60_000);
});

// ── Buffered fallback (no DecompressionStream) ───────────────────────────

describe("worker body: buffered fallback", () => {
  it("fetch and parse are separate phases with a fetch-done mark", async () => {
    // canStreamDecode() keys off the DecompressionStream global; the
    // core's maybeGunzip has a zlib fallback, so gzipped fixtures still
    // decode on the buffered path.
    vi.stubGlobal("DecompressionStream", undefined);
    expect(canStreamDecode()).toBe(false);
    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const sink = makeSink();
    createWorkerBody(sink.post).handle({ kind: "load", urls: ["/gz", "/raw"] });
    await waitForSettled(sink);

    expect(errorsOf(sink)).toEqual([]);
    const done = doneOf(sink)[0]!;
    expect(done.mode).toBe("buffered");
    // Mid-stream TRC\0 header resets the decoder: double the events.
    expect(done.trace.events.length).toBe(direct.events.length * 2);
    expect(done.buffer.byteLength).toBe(rawTrace.length * 2);

    const progress = progressOf(sink).map((m) => m.progress);
    expect(progress[0]).toMatchObject({
      phase: "fetching",
      mode: "buffered",
      urlCount: 2,
      bytesRead: 0,
    });
    // Once fetched, the parse phase knows the total size (B8 percentage).
    const parsing = progress.filter((p) => p.phase === "parsing");
    expect(parsing.length).toBeGreaterThan(0);
    expect(parsing[0]!.totalBytes).toBe(rawTrace.length * 2);

    expect(done.timing.fetchDoneMs).not.toBeNull();
    expect(done.timing.fetchDoneMs!).toBeGreaterThanOrEqual(done.timing.startMs);
    expect(done.timing.parseDoneMs).toBeGreaterThanOrEqual(done.timing.fetchDoneMs!);
  }, 60_000);
});

// ── Abort + protocol errors ──────────────────────────────────────────────

describe("worker body: abort and protocol errors", () => {
  it("abort request aborts the in-flight fetch; failure posts with AbortError name", async () => {
    installHangingFetchMock();
    const sink = makeSink();
    const body = createWorkerBody(sink.post);
    body.handle({ kind: "load", urls: ["/hang"] });
    // The initial phase progress posts synchronously before the fetch.
    expect(progressOf(sink).length).toBe(1);
    body.handle({ kind: "abort" });
    await waitForSettled(sink);
    expect(doneOf(sink)).toEqual([]);
    const errors = errorsOf(sink);
    expect(errors.length).toBe(1);
    expect(errors[0]!.name).toBe("AbortError");
  });

  it("a fetch failure posts an error with the HTTP status in the message", async () => {
    installFetchMock({});
    const sink = makeSink();
    createWorkerBody(sink.post).handle({ kind: "load", urls: ["/missing"] });
    await waitForSettled(sink);
    expect(doneOf(sink)).toEqual([]);
    expect(errorsOf(sink)[0]!.message).toMatch(/404/);
  });

  it("a second load request on the same body is refused", async () => {
    installHangingFetchMock();
    const sink = makeSink();
    const body = createWorkerBody(sink.post);
    body.handle({ kind: "load", urls: ["/hang"] });
    body.handle({ kind: "load", urls: ["/hang"] });
    const errors = errorsOf(sink);
    expect(errors.length).toBe(1);
    expect(errors[0]!.message).toMatch(/one load per worker/);
    // Unblock the hanging first load so nothing leaks past the test.
    body.handle({ kind: "abort" });
    await waitForSettled(sink);
  });
});
