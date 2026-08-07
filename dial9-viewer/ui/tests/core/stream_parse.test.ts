// Tests for the streaming trace decoder (parseTraceStream).
//
// The streaming parser must produce a ParsedTrace byte-for-byte identical to
// parseTrace() on the same concatenated bytes, no matter how the bytes are
// chunked. The risky part is the transactional frame decode in
// TraceDecoder.nextFrame(): a chunk boundary that falls mid-event (or mid
// TRC\0 header) must be detected as "need more bytes", rolled back, and
// re-attempted once the next chunk arrives — never silently dropped.
//
// We exercise adversarial chunk boundaries: size 1 (every byte boundary), a
// few fixed small sizes, prime sizes, and boundaries deliberately placed
// inside event timestamps and inside a mid-stream TRC\0 header.
//
// Migrated from test_stream_parse.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale). The trace bytes and
// header offsets are computed synchronously at module load so the test set
// (which depends on headerOffsets) is registered exactly as the original
// script would have run it.

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { deepStrictEqual } from "node:assert";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";

const require = createRequire(import.meta.url);

// The canonical() serializer touches many ParsedTrace fields; the frozen core
// is untyped, so keep the trace itself loose and let canonical() normalize.
interface ParsedTraceLoose {
  events: { timestamp: number }[];
  cpuSamples: unknown[];
  recordMinTs: number | null;
  recordMaxTs: number | null;
  [key: string]: unknown;
}

const { parseTrace, parseTraceStream, fetchTraceStream } =
  require("../../trace_parser.js") as {
    parseTrace: (
      buf: Uint8Array,
      opts?: { startTime?: number; endTime?: number },
    ) => Promise<ParsedTraceLoose>;
    parseTraceStream: (
      chunks: AsyncIterable<Uint8Array>,
      opts?: { startTime?: number; endTime?: number },
    ) => Promise<ParsedTraceLoose>;
    fetchTraceStream: (url: string) => Promise<AsyncIterable<Uint8Array>>;
  };

// Yield an async iterable of fixed-size Uint8Array chunks over `bytes`.
function chunked(bytes: Uint8Array, size: number): AsyncIterable<Uint8Array> {
  return {
    async *[Symbol.asyncIterator]() {
      for (let i = 0; i < bytes.length; i += size) {
        yield bytes.subarray(i, Math.min(i + size, bytes.length));
      }
    },
  };
}

// Yield an async iterable using an explicit list of boundary offsets. The
// boundaries are sorted and de-duplicated so the emitted chunks always cover
// `bytes` contiguously in order (a buggy unsorted list would silently reorder
// or drop bytes and mask real failures).
function chunkedAt(bytes: Uint8Array, boundaries: number[]): AsyncIterable<Uint8Array> {
  const inner = [...new Set(boundaries.filter((b) => b > 0 && b < bytes.length))].sort(
    (a, b) => a - b,
  );
  const offs = [0, ...inner, bytes.length];
  return {
    async *[Symbol.asyncIterator]() {
      for (let i = 1; i < offs.length; i++) {
        if (offs[i]! > offs[i - 1]!) yield bytes.subarray(offs[i - 1]!, offs[i]!);
      }
    },
  };
}

// Serialize a ParsedTrace into a stable, comparable plain object (compared
// with node:assert deepStrictEqual, as the original did — chai's deep-eql is
// far too slow on trace-sized structures). Maps are
// turned into sorted entry arrays so deep equality doesn't depend on Map
// insertion order (it shouldn't differ, but this makes failures legible and
// robust). BigInts are stringified.
function canonical(trace: ParsedTraceLoose): unknown {
  const mapEntries = (m: Map<unknown, unknown>) =>
    [...m.entries()].sort((a, b) => {
      const ka = String(a[0]),
        kb = String(b[0]);
      return ka < kb ? -1 : ka > kb ? 1 : 0;
    });
  const asMap = (v: unknown) => mapEntries(v as Map<unknown, unknown>);
  return JSON.parse(
    JSON.stringify(
      {
        magic: trace["magic"],
        version: trace["version"],
        events: trace.events,
        minTs: trace["minTs"],
        maxTs: trace["maxTs"],
        recordMinTs: trace.recordMinTs,
        recordMaxTs: trace.recordMaxTs,
        truncated: trace["truncated"],
        timeFiltered: trace["timeFiltered"],
        filterStartTime: trace["filterStartTime"],
        filterEndTime: trace["filterEndTime"],
        cpuSamples: trace.cpuSamples,
        allocEvents: trace["allocEvents"],
        freeEvents: trace["freeEvents"],
        memoryOverflows: trace["memoryOverflows"],
        customEvents: trace["customEvents"],
        clockSyncAnchors: trace["clockSyncAnchors"],
        clockOffsetNs: trace["clockOffsetNs"],
        blockInPlaceGaps: trace["blockInPlaceGaps"],
        spawnLocations: asMap(trace["spawnLocations"]),
        taskSpawnLocs: asMap(trace["taskSpawnLocs"]),
        taskSpawnTimes: asMap(trace["taskSpawnTimes"]),
        taskTerminateTimes: asMap(trace["taskTerminateTimes"]),
        taskInstrumented: asMap(trace["taskInstrumented"]),
        callframeSymbols: asMap(trace["callframeSymbols"]),
        threadNames: asMap(trace["threadNames"]),
        tidToWorker: asMap(trace["tidToWorker"]),
        stableTidToWorker: asMap(trace["stableTidToWorker"]),
        tidBindings: asMap(trace["tidBindings"]),
        runtimeWorkers: asMap(trace["runtimeWorkers"]),
        taskDumps: asMap(trace["taskDumps"]),
      },
      (_, v: unknown) => (typeof v === "bigint" ? v.toString() : v),
    ),
  );
}

const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

// Synchronous module-load setup (mirrors the original script's straight-line
// prelude) so header-dependent tests register conditionally, as before.
const fileBytes = readFileSync(tracePath);
const rawTrace =
  fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
    ? gunzipSync(fileBytes)
    : Buffer.from(fileBytes);
const raw = Uint8Array.from(rawTrace); // plain Uint8Array (subarray-safe)

// Find offsets of every TRC\0 mid-stream header. Skip the leading header at
// 0. (These byte patterns can also appear inside frame payloads; that's
// fine — splitting there still exercises the rollback path.)
function headerOffsetsIn(bytes: Uint8Array): number[] {
  const offs: number[] = [];
  for (let i = 1; i + 4 <= bytes.length; i++) {
    if (
      bytes[i] === 0x54 &&
      bytes[i + 1] === 0x52 &&
      bytes[i + 2] === 0x43 &&
      bytes[i + 3] === 0x00
    ) {
      offs.push(i);
    }
  }
  return offs;
}
const headerOffsets = headerOffsetsIn(raw);

// A prefix big enough to span several segments (TRC\0 resets), every event
// type, pools, and symbols — but small enough that byte-by-byte chunking is
// fast. We pick a prefix that ends just before a mid-stream header so the
// truncation is clean, falling back to ~1MB.
let prefixEnd = Math.min(1_000_000, raw.length);
for (const h of headerOffsets) {
  if (h >= 200_000 && h <= 1_200_000) {
    prefixEnd = h;
    break;
  }
}
const prefix = raw.subarray(0, prefixEnd);

let reference: ParsedTraceLoose;
let refCanon: unknown;

beforeAll(async () => {
  // Reference parse of the whole (gunzipped) buffer.
  reference = await parseTrace(raw);
  refCanon = canonical(reference);
  expect(reference.events.length, "reference has events").toBeGreaterThan(0);
});

async function streamCanon(
  iterable: AsyncIterable<Uint8Array>,
  opts?: { startTime?: number; endTime?: number },
): Promise<unknown> {
  const t = await parseTraceStream(iterable, opts);
  return canonical(t);
}

// Assert that streaming `bytes` (any prefix of the trace) with the given
// chunking yields the same ParsedTrace as parsing the whole `bytes` buffer.
// The buffered parser and the stream parser must handle a truncated tail
// identically, so this works for arbitrary byte slices.
async function assertStreamMatches(
  bytes: Uint8Array,
  iterable: AsyncIterable<Uint8Array>,
): Promise<void> {
  const ref = canonical(await parseTrace(bytes));
  const got = await streamCanon(iterable);
  deepStrictEqual(got, ref);
}

// Generous ceiling: the byte-by-byte chunking test alone takes tens of
// seconds, multiplied by parallel-worker contention on slow CI runners.
describe("parseTraceStream", { timeout: 300_000 }, () => {
  // ── chunk size 1 (every single-byte boundary, maximally adversarial)
  //    over the prefix — exercises rollback at every byte offset. ──
  it("chunk size 1 (byte-by-byte) over prefix matches", async () => {
    await assertStreamMatches(prefix, chunked(prefix, 1));
  });

  // ── tiny prime chunk sizes over the prefix ──
  for (const size of [2, 3, 5, 7, 13]) {
    it(`chunk size ${size} over prefix matches`, async () => {
      await assertStreamMatches(prefix, chunked(prefix, size));
    });
  }

  // ── a spread of fixed chunk sizes over the FULL buffer ──
  for (const size of [64, 256, 1024, 4096, 65536, 1 << 20]) {
    it(`chunk size ${size} (full trace) matches reference`, async () => {
      const got = await streamCanon(chunked(raw, size));
      deepStrictEqual(got, refCanon);
    });
  }

  // ── one giant chunk (whole buffer in a single read) ──
  it("single whole-buffer chunk matches reference", async () => {
    const got = await streamCanon(chunked(raw, raw.length));
    deepStrictEqual(got, refCanon);
  });

  // ── boundary placed inside a mid-stream TRC\0 header (partial header
  //    straddling the chunk boundary) on the full buffer. ──
  if (headerOffsets.length > 0) {
    const h = headerOffsets[0]!;
    for (const off of [h, h + 1, h + 2, h + 3, h + 4]) {
      it(`boundary inside/at TRC\\0 header at +${off - h}`, async () => {
        const got = await streamCanon(chunkedAt(raw, [off]));
        deepStrictEqual(got, refCanon);
      });
    }
    // Boundaries inside EVERY mid-stream header at once.
    it("boundary inside every TRC\\0 header at once", async () => {
      const got = await streamCanon(
        chunkedAt(raw, headerOffsets.flatMap((x) => [x + 1, x + 2, x + 3])),
      );
      deepStrictEqual(got, refCanon);
    });
  }

  // ── boundaries placed mid-event over the prefix. Event frames are
  //    TAG_EVENT(0x02) followed by a 2-byte type_id and (for timestamped
  //    events) a 3-byte delta. Split 1..4 bytes after each 0x02 byte to land
  //    inside the type_id / timestamp delta. Many of these land mid-event;
  //    any that land between frames are still valid splits. ──
  it("boundaries mid-event (after 0x02 tags) over prefix match", async () => {
    const boundaries: number[] = [];
    for (let i = 5; i < prefix.length; i++) {
      if (prefix[i] === 0x02) {
        for (const d of [1, 2, 3, 4]) {
          if (i + d < prefix.length) boundaries.push(i + d);
        }
      }
    }
    await assertStreamMatches(prefix, chunkedAt(prefix, boundaries));
  });

  // ── gzipped input, decoded after gunzip, fed chunked into the stream
  //    parser. (fetchTraceStream does the gunzip; here we gunzip then chunk the
  //    post-gunzip bytes, which is exactly what the stream parser consumes.) ──
  it("gzip round-trip: gunzip then chunked stream matches reference", async () => {
    const gz = gzipSync(Buffer.from(raw));
    const regunzipped = Uint8Array.from(gunzipSync(gz));
    const got = await streamCanon(chunked(regunzipped, 1000));
    deepStrictEqual(got, refCanon);
  });

  // ── empty / header-only streams error like the buffered parser ──
  it("empty stream throws Invalid trace header", async () => {
    let threw = false;
    try {
      await parseTraceStream(chunked(new Uint8Array(0), 1));
    } catch (e) {
      threw = /Invalid trace header/.test((e as Error).message);
    }
    expect(threw, "expected Invalid trace header").toBe(true);
  });

  // ── time-range filtering is honored identically when streaming ──
  it("time-range filtered stream matches filtered reference", async () => {
    const mid =
      reference.recordMinTs != null && reference.recordMaxTs != null
        ? Math.floor((reference.recordMinTs + reference.recordMaxTs) / 2)
        : null;
    if (mid == null) return; // no time bounds; skip
    const opts = { startTime: reference.recordMinTs!, endTime: mid };
    const refFiltered = canonical(await parseTrace(raw, opts));
    const streamFiltered = canonical(await parseTraceStream(chunked(raw, 333), opts));
    deepStrictEqual(streamFiltered, refFiltered);
  });

  // ── fetchTraceStream falls back gracefully when the ok response has
  //    no streamable `body` (e.g. cached/synthesized responses). It must still
  //    gunzip + decode to the same ParsedTrace instead of throwing on
  //    `null.getReader()`. We mock fetch to return a body-less response that
  //    only exposes arrayBuffer(), for both gzipped and raw bodies. ──
  for (const [label, body] of [
    ["gzipped", gzipSync(Buffer.from(raw))],
    ["raw", raw],
  ] as const) {
    it(`fetchTraceStream falls back when response has no body (${label})`, async () => {
      const u8 = Uint8Array.from(body);
      const globalAny = globalThis as { fetch: unknown };
      const original = globalAny.fetch;
      globalAny.fetch = async () => ({
        ok: true,
        status: 200,
        body: null, // no streamable body → exercise the arrayBuffer() fallback
        async arrayBuffer() {
          return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
        },
      });
      try {
        const stream = await fetchTraceStream("/no-body");
        const got = canonical(await parseTraceStream(stream));
        deepStrictEqual(got, refCanon);
      } finally {
        globalAny.fetch = original;
      }
    });
  }
});
