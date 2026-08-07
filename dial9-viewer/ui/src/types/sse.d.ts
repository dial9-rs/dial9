// Type declarations for `sse.js` - the minimal Server-Sent Events client over
// `fetch` used by the aggregated (`?api=1`) flamegraph/tokio-stats streams.
// See src/types/decode.d.ts for the declaration-form rationale.
//
// Not frozen core, but loaded as a browser global (CommonJS-guard form) and
// consumed by typed src/ through the lib/trace boundary (src/lib/trace/sse.ts).
// Native `EventSource` can't be used because the aggregate endpoints
// authenticate via `x-dial9-aws-*` request headers, so this reads the
// text/event-stream body off a `fetch` reader instead.

declare module "*/sse.js" {
  export interface SseOptions {
    /** Request headers (e.g. Dial9Creds.headers()). */
    headers?: Record<string, string>;
    /** AbortSignal to cancel the stream (quiet: no onClose/onError on abort). */
    signal?: AbortSignal;
    /** Called with the parsed JSON object per event. */
    onEvent?: (obj: unknown) => void;
    /** Called on network/HTTP error (not on abort); carries `status` on HTTP errors. */
    onError?: (err: Error & { status?: number }) => void;
    /** Called when the server closes the stream cleanly. */
    onClose?: () => void;
  }

  /**
   * Open an SSE stream over `fetch` and drive the callbacks. The returned
   * promise resolves when the stream ends for any reason (clean close, abort,
   * or error) and never rejects.
   */
  export function openSse(url: string, opts?: SseOptions): Promise<void>;

  /** Incremental SSE frame decoder (pure; feed text chunks, get event payloads). */
  export class SseDecoder {
    push(chunk: string): string[];
    flush(): string[];
  }
}
