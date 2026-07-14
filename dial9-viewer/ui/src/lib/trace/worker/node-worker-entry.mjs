// lib/trace/worker/node-worker-entry.mjs - Node binding of the worker load
// body over node:worker_threads, used by the Vitest integration tests
// (integration.test.ts) to exercise the REAL thread + postMessage
// structured-clone boundary. Node has no Web Worker, and browser-mode
// testing is the Playwright suite's turf, so this is the chosen
// Node-shim path. The browser binding is trace-worker.ts.
//
// Plain .mjs on purpose: worker_threads needs an on-disk entry, and Node's
// native type stripping (>= 22.18; CI runs Node 24) loads body.ts directly
// through the explicit .ts specifier below - no bundler, no extra deps.
// body.ts keeps its runtime import chain plain-Node-resolvable for exactly
// this (see its header comment).

import { parentPort } from "node:worker_threads";

if (!parentPort) {
  throw new Error("node-worker-entry.mjs must run inside a worker_threads Worker");
}

const { createWorkerBody } = await import("./body.ts");

const body = createWorkerBody((message, transfer) => {
  parentPort.postMessage(message, transfer);
});

parentPort.on("message", (message) => {
  body.handle(message);
});
