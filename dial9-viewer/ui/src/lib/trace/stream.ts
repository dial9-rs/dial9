// lib/trace/stream.ts - the streaming fetch + gunzip + capture mechanism
// shared by the main-thread load path (load.ts loadTraceStreamed) and the
// Web Worker load body (worker/body.ts). Extracted from load.ts in T16:
// load.ts also hosts the worker ORCHESTRATOR (the `new Worker(new URL(...))`
// factory), so the worker body importing load.ts would pull the worker
// entry's own reference into the worker bundle graph. This leaf module
// breaks that cycle.
//
// LEAF-MODULE RULE (plain-Node constraint): the worker body runs under
// plain Node via native type stripping (worker/node-worker-entry.mjs),
// which resolves import specifiers on disk as written - no bundler. Every
// runtime import reachable from the body must therefore resolve on disk:
// this file may import the frozen core (real .js files at the ui root)
// and nothing else at runtime; type-only imports are erased and exempt.

import {
  fetchTraceStream,
  fetchTracesStream,
  parseTraceStream,
} from "../../../trace_parser.js";
import type {
  FetchOptions,
  ParseOptions,
  ParsedTrace,
} from "../../../trace_parser.js";

/** A streamed parse plus the reassembled raw bytes it consumed. */
export interface StreamedParse {
  trace: ParsedTrace;
  /**
   * The raw (gunzipped, concatenated) trace bytes, captured while parsing
   * and reassembled, so Set/Clear Range can re-parse in memory without
   * re-fetching (features/02 B14).
   */
  buffer: ArrayBuffer;
}

/**
 * Stream one OR MORE trace URLs: decode chunks as they download so parse
 * time overlaps the download (~max(download, parse) instead of their sum).
 * For multiple URLs the fetches run concurrently and the components stream
 * in back-to-back, in order, as one logical trace - so parsing the first
 * segment overlaps the in-flight downloads of the rest (issue #595). The
 * gunzipped chunks are captured while parsing so the full buffer is still
 * available afterwards for in-memory Set/Clear-Range re-parsing (which
 * never re-fetches). This is the streamAndShowTrace mechanism
 * (viewer.html:1678-1712) without the page chrome.
 */
export async function streamTraceWithCapture(
  urls: readonly string[],
  fetchOpts: FetchOptions,
  parseOpts: ParseOptions
): Promise<StreamedParse> {
  const stream =
    urls.length === 1
      ? await fetchTraceStream(urls[0]!, fetchOpts)
      : fetchTracesStream([...urls], fetchOpts);
  const captured: Uint8Array[] = [];
  const capturing: AsyncIterable<Uint8Array> = {
    async *[Symbol.asyncIterator]() {
      for await (const chunk of stream) {
        captured.push(chunk);
        yield chunk;
      }
    },
  };
  const trace = await parseTraceStream(capturing, parseOpts);
  // Reassemble the full buffer from the captured chunks.
  let total = 0;
  for (const c of captured) total += c.length;
  const buffer = new Uint8Array(total);
  let off = 0;
  for (const c of captured) {
    buffer.set(c, off);
    off += c.length;
  }
  return { trace, buffer: buffer.buffer };
}
