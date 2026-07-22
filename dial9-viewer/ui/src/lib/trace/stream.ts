// The streaming fetch + gunzip + capture mechanism shared by the main-thread
// load path (load.ts loadTraceStreamed) and the Web Worker load body
// (worker/body.ts). It is a leaf module so the worker body can import it
// without pulling load.ts's worker ORCHESTRATOR (and thus the worker entry)
// into the worker bundle graph.
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

/** A streamed parse plus the reassembled raw bytes it consumed. */
export interface StreamedParse {
  trace: ParsedTrace;
  /**
   * The raw (gunzipped, concatenated) trace bytes, captured while parsing
   * and reassembled, so Set/Clear Range can re-parse in memory without
   * re-fetching.
   */
  buffer: ArrayBuffer;
}

/**
 * Parse an async stream of raw (already-gunzipped) trace chunks while
 * capturing them, then reassemble the captured chunks into the full raw
 * buffer. The chunk source is the caller's concern: the URL path below
 * feeds it fetch streams; the worker's parse-buffer path feeds it a
 * DecompressionStream over cached gzip bytes.
 */
export async function parseChunksWithCapture(
  chunks: AsyncIterable<Uint8Array>,
  parseOpts: ParseOptions
): Promise<StreamedParse> {
  const captured: Uint8Array[] = [];
  const capturing: AsyncIterable<Uint8Array> = {
    async *[Symbol.asyncIterator]() {
      for await (const chunk of chunks) {
        captured.push(chunk);
        yield chunk;
      }
    },
  };
  const trace = await parseTraceStream(capturing, parseOpts);
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

/**
 * Stream one OR MORE trace URLs: decode chunks as they download so parse
 * time overlaps the download (~max(download, parse) instead of their sum).
 * For multiple URLs the fetches run concurrently and the components stream
 * in back-to-back, in order, as one logical trace - so parsing the first
 * segment overlaps the in-flight downloads of the rest. The gunzipped
 * chunks are captured while parsing so the full buffer is still
 * available afterwards for in-memory Set/Clear-Range re-parsing (which
 * never re-fetches).
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
  return parseChunksWithCapture(stream, parseOpts);
}
