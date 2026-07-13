// Browser binding of the worker load body: the Vite worker entry, built via
// the statically-detected `new Worker(new URL("./worker/trace-worker.ts",
// import.meta.url), { type: "module" })` in load.ts. All logic lives in
// body.ts; this file only wires the DedicatedWorkerGlobalScope message
// plumbing. The Node binding (worker_threads, tests) is
// node-worker-entry.mjs.

import { TraceDecoder } from "../../../../decode.js";
import { createWorkerBody } from "./body.js";
import type { TraceWorkerRequest } from "./protocol.js";

// The bundled core resolves its decoder via `typeof require` (Node) or
// the TraceDecoder browser global that the legacy pages establish with
// <script src="decode.js"> ordering (trace_parser.js getTraceDecoder).
// Inside a bundled module worker there is neither: seed the global from
// the bundled decode.js before any parse message can arrive. The require
// branch is kept genuinely unreachable in bundles by
// commonjsOptions.ignoreDynamicRequires (vite.config.ts); the Node and
// vitest paths run the real CJS file and never need this.
(globalThis as { TraceDecoder?: unknown }).TraceDecoder = TraceDecoder;

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
