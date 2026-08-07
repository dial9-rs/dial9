// Typed seam over the frozen SSE client (sse.js): the aggregated (`?api=1`)
// flamegraph/tokio-stats endpoints are text/event-stream, and the server owns
// the refine/stop loop. Pages consume the stream through this re-export instead
// of importing the core module directly, matching api_format.ts.
//
// sse.js is self-contained (only fetch + TextDecoder), so it needs no
// global-seed chain like the flamegraph widget does.

export { openSse } from "../../../sse.js";
export type { SseOptions } from "../../../sse.js";
