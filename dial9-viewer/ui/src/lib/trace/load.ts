// lib/trace/load.ts - load orchestration (T09; architecture 2.7;
// features/02 B12/B14, features/01 I4). Typed wrapper over the frozen
// trace_parser.js load surface, plus the mechanism ported out of
// viewer.html's streamAndShowTrace/loadTraceFromUrl (viewer.html:1678-1712,
// 1855-1903): repeatable `trace=` components, parallel fetch + gunzip +
// concat, streaming parse with chunk capture so the full buffer stays
// available for in-memory re-parse (see ./reparse.ts).
//
// Mechanism only: loading-view labels, elapsed timers, loadPerf records,
// alerts/credential hints and drop-zone resets are page concerns (page
// tickets). The file-drop path is `parseTraceBuffer` (the page hands the
// FileReader result in); the demo path is `loadTrace("demo-trace.bin")`.
//
// This file also re-exports the rest of the trace_parser.js surface
// (constants, symbol/stack helpers, types) so nothing outside lib/trace
// ever imports the core module; ./analysis.ts does the same for
// trace_analysis.js.

import {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  canStreamDecode,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
  fetchTraceStream,
  fetchTraces,
  fetchTracesStream,
  formatFrame,
  parseTrace,
  parseTraceStream,
  symbolizeChain,
} from "../../../trace_parser.js";
import type {
  FetchOptions,
  ParseOptions,
  ParsedTrace,
} from "../../../trace_parser.js";

// Typed pass-throughs of the core load/parse/symbol surface.
export {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  canStreamDecode,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
  formatFrame,
  symbolizeChain,
};
export type {
  AllocEvent,
  BlockInPlaceGap,
  CallframeSymbols,
  ClockSyncAnchor,
  CpuSample,
  CustomTraceEvent,
  FetchOptions,
  FreeEvent,
  MemoryOverflowEvent,
  ParseOptions,
  ParseProgress,
  ParsedTrace,
  SampleGroup,
  SymbolFrame,
  TaskDump,
  TraceEvent,
} from "../../../trace_parser.js";
export type { DecodedFieldValue } from "../../../decode.js";

/** Options for a URL load: fetch options + parse options, flat. */
export interface LoadTraceOptions extends FetchOptions, ParseOptions {}

/** The result of loading one logical trace from URLs. */
export interface LoadedTrace {
  trace: ParsedTrace;
  /**
   * The raw (gunzipped, concatenated) trace bytes, retained so Set/Clear
   * Range can re-parse in memory without re-fetching (features/02 B14).
   */
  buffer: ArrayBuffer;
  /**
   * "stream" when download and decode overlapped (canStreamDecode
   * runtimes); "buffered" for the fetch-then-parse fallback. Pages use
   * this for their load-perf record (02 B16).
   */
  mode: "stream" | "buffered";
}

function splitOptions(opts: LoadTraceOptions): {
  fetchOpts: FetchOptions;
  parseOpts: ParseOptions;
} {
  const { signal, headers, ...parseOpts } = opts;
  const fetchOpts: FetchOptions = {};
  if (signal !== undefined) fetchOpts.signal = signal;
  if (headers !== undefined) fetchOpts.headers = headers;
  return { fetchOpts, parseOpts };
}

/**
 * Parse an already-fetched buffer (the file-drop path, features/02 B2/B3,
 * and the re-parse path). Thin typed alias over the core's parseTrace so
 * callers never import the core module.
 */
export function parseTraceBuffer(
  buffer: ArrayBuffer | Uint8Array,
  opts?: ParseOptions
): Promise<ParsedTrace> {
  return parseTrace(buffer, opts);
}

/**
 * Stream one OR MORE trace URLs: decode chunks as they download so parse
 * time overlaps the download (~max(download, parse) instead of their sum).
 * For multiple URLs the fetches run concurrently and the components stream
 * in back-to-back, in order, as one logical trace - so parsing the first
 * segment overlaps the in-flight downloads of the rest (issue #595). The
 * gunzipped chunks are captured while parsing so the full buffer is still
 * available afterwards for in-memory Set/Clear-Range re-parsing (which
 * never re-fetches).
 */
export async function loadTraceStreamed(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const { fetchOpts, parseOpts } = splitOptions(opts);
  const stream =
    list.length === 1
      ? await fetchTraceStream(list[0]!, fetchOpts)
      : fetchTracesStream([...list], fetchOpts);
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
  return { trace, buffer: buffer.buffer, mode: "stream" };
}

/**
 * Buffered fallback for runtimes without DecompressionStream: fetch every
 * component (in parallel), gunzip each independently, concatenate in
 * `urls` order, then parse the whole buffer (fetch and parse are separate
 * phases, unlike streaming).
 */
export async function loadTraceBuffered(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  const list = Array.isArray(urls) ? (urls as readonly string[]) : [urls as string];
  const { fetchOpts, parseOpts } = splitOptions(opts);
  const buffer = await fetchTraces([...list], fetchOpts);
  const trace = await parseTrace(buffer, parseOpts);
  return { trace, buffer, mode: "buffered" };
}

/**
 * Load one logical trace from one or more URLs (repeatable `trace=`
 * components): STREAM whenever the runtime supports it, buffered
 * fetch+gunzip+concat otherwise (features/02 B12). Errors propagate to the
 * caller - the page owns AbortError swallowing, the HTTP-401 credentials
 * hint, and reset-to-drop-zone (02 B10/B13).
 */
export function loadTrace(
  urls: string | readonly string[],
  opts: LoadTraceOptions = {}
): Promise<LoadedTrace> {
  return canStreamDecode() ? loadTraceStreamed(urls, opts) : loadTraceBuffered(urls, opts);
}

/**
 * Build one viewer `trace=` component per selected key, each pointing at
 * /api/object (which serves that single file's raw, still-gzipped bytes).
 * The viewer and flamegraph read all repeated `trace=` params and download
 * them in parallel + gunzip client-side (see loadTrace above). Ported from
 * index.html:1713-1720 (features/01 I4); this replaced a single /api/trace
 * URL that forced the backend to fetch, gunzip and merge every file into
 * one large uncompressed response.
 */
export function objectTraceUrls(bucket: string, keys: readonly string[]): string[] {
  return keys.map((key) => {
    const p = new URLSearchParams();
    p.set("bucket", bucket);
    p.set("key", key);
    return "/api/object?" + p.toString();
  });
}
