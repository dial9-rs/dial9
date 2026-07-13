// loadTraceInWorker orchestrator tests (T16): the message-level contract
// against a scripted in-process fake port and a REAL T07 store. This layer
// pins the single-AbortController semantics (the DoD abort observables:
// worker terminated, trace slice unchanged, no progress after abort)
// independent of transport; the true thread + structured-clone boundary is
// worker/integration.test.ts.

import { describe, expect, it, vi } from "vitest";
import { createStore } from "../../store/store.js";
import { loadTraceInWorker } from "./load.js";
import type { Store, ViewerStore } from "../../store/store.js";
import type { TraceSlice } from "../../types/state.js";
import type { ParsedTrace, TraceSliceStore } from "./load.js";
import type {
  TraceWorkerDoneMessage,
  TraceWorkerRequest,
  TraceWorkerResponse,
  TraceWorkerTiming,
} from "./worker/protocol.js";

// Compile-time: the full viewer store satisfies the orchestrator's store
// surface (no runtime assertion needed; this line fails tsc otherwise).
const _viewerStoreIsTraceSliceStore: (s: ViewerStore) => TraceSliceStore = (s) => s;
void _viewerStoreIsTraceSliceStore;

// ── Fixtures ─────────────────────────────────────────────────────────────

function makeTraceStore(): Store<{ trace: TraceSlice }> {
  // Node has no requestAnimationFrame: inject a microtask scheduler.
  return createStore<{ trace: TraceSlice }>(
    { trace: { trace: null } },
    { scheduler: (cb) => queueMicrotask(cb) }
  );
}

/** A scripted port: the test plays the worker side. */
function makeFakePort() {
  const sent: TraceWorkerRequest[] = [];
  let onMsg: ((message: TraceWorkerResponse) => void) | null = null;
  let onErr: ((error: unknown) => void) | null = null;
  let terminated = 0;
  return {
    port: {
      postMessage: (message: TraceWorkerRequest): void => {
        sent.push(message);
      },
      onMessage: (fn: (message: TraceWorkerResponse) => void): void => {
        onMsg = fn;
      },
      onError: (fn: (error: unknown) => void): void => {
        onErr = fn;
      },
      terminate: (): void => {
        terminated += 1;
      },
    },
    sent,
    emit: (message: TraceWorkerResponse): void => onMsg?.(message),
    emitError: (error: unknown): void => onErr?.(error),
    terminatedCount: () => terminated,
  };
}

const fakeTrace = { events: [{ timestamp: 1 }] } as unknown as ParsedTrace;
const timing: TraceWorkerTiming = {
  startMs: 1,
  fetchDoneMs: null,
  parseDoneMs: 2,
  mode: "stream",
  events: 1,
  bytes: 8,
};
const doneMessage: TraceWorkerDoneMessage = {
  kind: "done",
  trace: fakeTrace,
  buffer: new ArrayBuffer(8),
  mode: "stream",
  timing,
};
const progressMessage: TraceWorkerResponse = {
  kind: "progress",
  progress: {
    phase: "parsing",
    mode: "stream",
    urlCount: 1,
    bytesRead: 100,
    totalBytes: null,
    eventCount: 10,
    startMs: 1,
    elapsedMs: 5,
  },
};

describe("loadTraceInWorker", () => {
  it("posts one load request carrying urls, headers, and only defined parse options", () => {
    const fake = makeFakePort();
    loadTraceInWorker(makeTraceStore(), ["/a", "/b"], {
      headers: { "x-creds": "1" },
      maxEvents: 5,
      worker: () => fake.port,
    });
    expect(fake.sent).toEqual([
      {
        kind: "load",
        urls: ["/a", "/b"],
        headers: { "x-creds": "1" },
        parse: { maxEvents: 5 },
      },
    ]);
  });

  it("done: updates the trace slice, resolves with the result, terminates the worker", async () => {
    const fake = makeFakePort();
    const store = makeTraceStore();
    const onProgress = vi.fn();
    const handle = loadTraceInWorker(store, "/a", {
      onProgress,
      worker: () => fake.port,
    });
    fake.emit(progressMessage);
    expect(onProgress).toHaveBeenCalledTimes(1);
    fake.emit(doneMessage);
    const result = await handle.done;
    expect(result.trace).toBe(fakeTrace);
    expect(result.mode).toBe("stream");
    expect(result.timing).toEqual(timing);
    expect(store.getState().trace.trace).toBe(fakeTrace);
    expect(fake.terminatedCount()).toBe(1);
  });

  it("abort(): abort message + terminate + AbortError + store unchanged + no late progress", async () => {
    const fake = makeFakePort();
    const store = makeTraceStore();
    const onProgress = vi.fn();
    const handle = loadTraceInWorker(store, "/a", {
      onProgress,
      worker: () => fake.port,
    });
    fake.emit(progressMessage);
    expect(onProgress).toHaveBeenCalledTimes(1);

    handle.abort();
    // Observable 1: worker terminated (after the cooperative abort msg).
    expect(fake.sent.some((m) => m.kind === "abort")).toBe(true);
    expect(fake.terminatedCount()).toBe(1);
    await expect(handle.done).rejects.toMatchObject({ name: "AbortError" });

    // Observables 2+3: messages queued behind the abort are dropped - the
    // store stays untouched and no progress callback fires.
    fake.emit(progressMessage);
    fake.emit(doneMessage);
    expect(onProgress).toHaveBeenCalledTimes(1);
    expect(store.getState().trace.trace).toBeNull();

    // Idempotent.
    handle.abort();
    expect(fake.terminatedCount()).toBe(1);
  });

  it("an external signal funnels into the same single controller", async () => {
    const fake = makeFakePort();
    const store = makeTraceStore();
    const pageController = new AbortController();
    const handle = loadTraceInWorker(store, "/a", {
      signal: pageController.signal,
      worker: () => fake.port,
    });
    pageController.abort();
    expect(fake.terminatedCount()).toBe(1);
    await expect(handle.done).rejects.toMatchObject({ name: "AbortError" });
    expect(store.getState().trace.trace).toBeNull();
  });

  it("an already-aborted signal never posts the load request", async () => {
    const fake = makeFakePort();
    const pageController = new AbortController();
    pageController.abort();
    const handle = loadTraceInWorker(makeTraceStore(), "/a", {
      signal: pageController.signal,
      worker: () => fake.port,
    });
    await expect(handle.done).rejects.toMatchObject({ name: "AbortError" });
    // Only the cooperative abort message went out - no load.
    expect(fake.sent.filter((m) => m.kind === "load")).toEqual([]);
    expect(fake.terminatedCount()).toBe(1);
  });

  it("worker error message: rejects with the name preserved, store unchanged, terminated", async () => {
    const fake = makeFakePort();
    const store = makeTraceStore();
    const handle = loadTraceInWorker(store, "/a", { worker: () => fake.port });
    fake.emit({ kind: "error", message: "HTTP 401 fetching /a", name: "Error" });
    await expect(handle.done).rejects.toMatchObject({
      message: "HTTP 401 fetching /a",
      name: "Error",
    });
    expect(store.getState().trace.trace).toBeNull();
    expect(fake.terminatedCount()).toBe(1);
  });

  it("transport error (worker crash): rejects and terminates", async () => {
    const fake = makeFakePort();
    const handle = loadTraceInWorker(makeTraceStore(), "/a", {
      worker: () => fake.port,
    });
    fake.emitError(new Error("worker exploded"));
    await expect(handle.done).rejects.toMatchObject({ message: "worker exploded" });
    expect(fake.terminatedCount()).toBe(1);
  });

  it("fire-and-forget abort does not raise an unhandled rejection", async () => {
    const fake = makeFakePort();
    const handle = loadTraceInWorker(makeTraceStore(), "/a", {
      worker: () => fake.port,
    });
    handle.abort();
    // Give the microtask queue a turn; vitest fails the test run on any
    // unhandled rejection, so reaching the assertion is the check.
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(fake.terminatedCount()).toBe(1);
  });
});
