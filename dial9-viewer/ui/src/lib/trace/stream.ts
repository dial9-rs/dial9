// The streaming fetch + gunzip pipeline shared by the main-thread load path
// (load.ts loadTraceStreamed) and the Web Worker load body (worker/body.ts).
// It is a leaf module so the worker body can import it without pulling
// load.ts's worker orchestrator (and thus the worker entry) into the worker
// bundle graph.
//
// LEAF-MODULE RULE (plain-Node constraint): the worker body runs under plain
// Node via native type stripping (no bundler), which resolves import
// specifiers on disk as written. Every runtime import reachable from the body
// must therefore resolve on disk: this file may import the frozen core (real
// .js files at the ui root) and nothing else at runtime; type-only imports
// are erased and exempt.

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

/** A streamed parse and how many raw bytes it consumed. */
export interface StreamedParse {
  trace: ParsedTrace;
  /** Decompressed byte count (the bytes themselves are not retained). */
  bytes: number;
}

/**
 * Parse an async stream of raw (already-gunzipped) trace chunks. The chunk
 * source is the caller's concern: the URL path below feeds it fetch streams;
 * the worker's parse-buffer path feeds it a DecompressionStream over cached
 * gzip bytes.
 *
 * Chunks are handed to the parser and dropped. Retaining them would cost the
 * whole decompressed trace for the length of the parse, 1.28 GB at 30M events;
 * Set/Clear Range re-opens the source instead (viewer/load-controller.ts).
 */
export async function parseChunks(
  chunks: AsyncIterable<Uint8Array>,
  parseOpts: ParseOptions
): Promise<StreamedParse> {
  let bytes = 0;
  const counting: AsyncIterable<Uint8Array> = {
    async *[Symbol.asyncIterator]() {
      for await (const chunk of chunks) {
        bytes += chunk.length;
        yield chunk;
      }
    },
  };
  const trace = await parseTraceStream(counting, parseOpts);
  return { trace, bytes };
}

/**
 * Stream one OR MORE trace URLs: decode chunks as they download so parse
 * time overlaps the download (~max(download, parse) instead of their sum).
 * For multiple URLs a few fetches run at once and the components stream in
 * back-to-back, in order, as one logical trace - so parsing the first segment
 * overlaps the in-flight downloads of the rest.
 */
export async function streamTrace(
  urls: readonly string[],
  fetchOpts: FetchOptions,
  parseOpts: ParseOptions
): Promise<StreamedParse> {
  const stream =
    urls.length === 1
      ? await fetchTraceStream(urls[0]!, fetchOpts)
      : fetchTracesStream([...urls], fetchOpts);
  return parseChunks(stream, parseOpts);
}
