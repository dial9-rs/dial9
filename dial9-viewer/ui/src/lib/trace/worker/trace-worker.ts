// lib/trace/worker/trace-worker.ts - browser binding of the worker load
// body: the Vite worker entry, built via the statically-detected
// `new Worker(new URL("./worker/trace-worker.ts", import.meta.url),
// { type: "module" })` in load.ts. All logic lives in body.ts (pure
// module); this file only wires the DedicatedWorkerGlobalScope message
// plumbing. The Node binding (worker_threads, tests) is
// node-worker-entry.mjs.

import { createWorkerBody } from "./body.js";
import type { TraceWorkerRequest } from "./protocol.js";

// The project tsconfig lib is DOM (window-shaped globals); type the two
// worker globals this entry touches minimally instead of pulling
// lib.webworker into the whole project.
interface DedicatedWorkerScope {
  postMessage(message: unknown, transfer?: ArrayBuffer[]): void;
  onmessage: ((event: MessageEvent) => void) | null;
}

const scope = self as unknown as DedicatedWorkerScope;

const body = createWorkerBody((message, transfer) => {
  scope.postMessage(message, transfer);
});

scope.onmessage = (event: MessageEvent): void => {
  body.handle(event.data as TraceWorkerRequest);
};
