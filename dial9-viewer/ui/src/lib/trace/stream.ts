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
  /** Decompressed byte count (the decompressed bytes are not retained). */
  bytes: number;
  /**
   * The bytes as they arrived, before any gunzip, one entry per component.
   * Absent when nothing asked for them, or when the plain (non-gzip)
   * components outgrew the capture budget.
   *
   * Kept apart rather than concatenated: a gzip stream of several members
   * decodes on some runtimes and throws "trailing junk" on others, so each
   * component re-parses as its own source.
   */
  captured?: Uint8Array[];
}

/**
 * Parse an async stream of raw (already-gunzipped) trace chunks. The chunk
 * source is the caller's concern: the URL path below feeds it fetch streams;
 * the worker's parse-buffer path feeds it a DecompressionStream over cached
 * gzip bytes.
 *
 * Chunks are handed to the parser and dropped: retaining the decompressed form
 * costs the whole trace, 1.28 GB at 30M events. Set/Clear Range re-parses the
 * bytes as they arrived instead, which `streamTrace` captures: a third of that
 * for gzip (raw-byte-cache.ts makes the same trade for segments), and plain
 * bytes only up to a budget.
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
 * Plain (non-gzip) bytes `streamTrace` keeps for Set/Clear Range.
 */
export const PLAIN_CAPTURE_BUDGET_BYTES = 256 * 1024 * 1024;

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
  parseOpts: ParseOptions,
  capture = false,
  plainBudgetBytes = PLAIN_CAPTURE_BUDGET_BYTES
): Promise<StreamedParse> {
  // Chunks per component, in arrival order; index 0 for the single-URL path,
  // which never reports one. Sized up front: an empty component reports no
  // chunk, and the re-parse needs its entry all the same.
  let parts: Uint8Array[][] | null = capture ? Array.from(urls, () => []) : null;
  let plainBytes = 0;
  const opts: FetchOptions = capture
    ? {
        ...fetchOpts,
        onRawChunk: (chunk: Uint8Array, isGzip: boolean, component = 0): void => {
          if (parts === null) return;
          if (!isGzip) {
            plainBytes += chunk.length;
            if (plainBytes > plainBudgetBytes) {
              parts = null;
              return;
            }
          }
          parts[component]!.push(chunk);
        },
      }
    : fetchOpts;
  const stream =
    urls.length === 1
      ? await fetchTraceStream(urls[0]!, opts)
      : fetchTracesStream([...urls], opts);
  const parsed = await parseChunks(stream, parseOpts);
  if (parts === null) return parsed;
  const captured = parts.map((chunks) => {
    let total = 0;
    for (const c of chunks) total += c.length;
    const out = new Uint8Array(total);
    let off = 0;
    for (const c of chunks) {
      out.set(c, off);
      off += c.length;
    }
    return out;
  });
  return { ...parsed, captured };
}
