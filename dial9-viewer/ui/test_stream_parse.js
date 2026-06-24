#!/usr/bin/env node
"use strict";

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

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { assert, testAsync, summarize } = require("./test_harness.js");
const { parseTrace, parseTraceStream, fetchTraceStream } = require("./trace_parser.js");

// Yield an async iterable of fixed-size Uint8Array chunks over `bytes`.
function chunked(bytes, size) {
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
function chunkedAt(bytes, boundaries) {
  const inner = [...new Set(boundaries.filter((b) => b > 0 && b < bytes.length))]
    .sort((a, b) => a - b);
  const offs = [0, ...inner, bytes.length];
  return {
    async *[Symbol.asyncIterator]() {
      for (let i = 1; i < offs.length; i++) {
        if (offs[i] > offs[i - 1]) yield bytes.subarray(offs[i - 1], offs[i]);
      }
    },
  };
}

// Serialize a ParsedTrace into a stable, comparable plain object. Maps are
// turned into sorted entry arrays so deepStrictEqual doesn't depend on Map
// insertion order (it shouldn't differ, but this makes failures legible and
// robust). BigInts are stringified.
function canonical(trace) {
  const mapEntries = (m) => [...m.entries()].sort((a, b) => {
    const ka = String(a[0]), kb = String(b[0]);
    return ka < kb ? -1 : ka > kb ? 1 : 0;
  });
  return JSON.parse(JSON.stringify({
    magic: trace.magic,
    version: trace.version,
    events: trace.events,
    minTs: trace.minTs,
    maxTs: trace.maxTs,
    recordMinTs: trace.recordMinTs,
    recordMaxTs: trace.recordMaxTs,
    truncated: trace.truncated,
    timeFiltered: trace.timeFiltered,
    filterStartTime: trace.filterStartTime,
    filterEndTime: trace.filterEndTime,
    cpuSamples: trace.cpuSamples,
    allocEvents: trace.allocEvents,
    freeEvents: trace.freeEvents,
    memoryOverflows: trace.memoryOverflows,
    customEvents: trace.customEvents,
    clockSyncAnchors: trace.clockSyncAnchors,
    clockOffsetNs: trace.clockOffsetNs,
    blockInPlaceGaps: trace.blockInPlaceGaps,
    spawnLocations: mapEntries(trace.spawnLocations),
    taskSpawnLocs: mapEntries(trace.taskSpawnLocs),
    taskSpawnTimes: mapEntries(trace.taskSpawnTimes),
    taskTerminateTimes: mapEntries(trace.taskTerminateTimes),
    taskInstrumented: mapEntries(trace.taskInstrumented),
    callframeSymbols: mapEntries(trace.callframeSymbols),
    threadNames: mapEntries(trace.threadNames),
    tidToWorker: mapEntries(trace.tidToWorker),
    runtimeWorkers: mapEntries(trace.runtimeWorkers),
    taskDumps: mapEntries(trace.taskDumps),
  }, (_, v) => (typeof v === "bigint" ? v.toString() : v)));
}

async function main() {
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }

  const fileBytes = fs.readFileSync(tracePath);
  const rawTrace =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? zlib.gunzipSync(fileBytes)
      : Buffer.from(fileBytes);
  const raw = Uint8Array.from(rawTrace); // plain Uint8Array (subarray-safe)

  // Reference parse of the whole (gunzipped) buffer.
  const reference = await parseTrace(raw);
  const refCanon = canonical(reference);
  console.log(
    `Reference: ${reference.events.length} events, ` +
      `${reference.cpuSamples.length} cpu samples, ` +
      `${raw.length} raw bytes`
  );
  assert.ok(reference.events.length > 0, "reference has events");

  // Find offsets of every TRC\0 mid-stream header. Skip the leading header at
  // 0. (These byte patterns can also appear inside frame payloads; that's
  // fine — splitting there still exercises the rollback path.)
  function headerOffsetsIn(bytes) {
    const offs = [];
    for (let i = 1; i + 4 <= bytes.length; i++) {
      if (bytes[i] === 0x54 && bytes[i + 1] === 0x52 && bytes[i + 2] === 0x43 && bytes[i + 3] === 0x00) {
        offs.push(i);
      }
    }
    return offs;
  }
  const headerOffsets = headerOffsetsIn(raw);
  console.log(`Found ${headerOffsets.length} mid-stream TRC\\0 header pattern(s)`);

  async function streamCanon(iterable) {
    const t = await parseTraceStream(iterable);
    return canonical(t);
  }

  // Assert that streaming `bytes` (any prefix of the trace) with the given
  // chunking yields the same ParsedTrace as parsing the whole `bytes` buffer.
  // The buffered parser and the stream parser must handle a truncated tail
  // identically, so this works for arbitrary byte slices.
  async function assertStreamMatches(bytes, iterable) {
    const ref = canonical(await parseTrace(bytes));
    const got = await streamCanon(iterable);
    assert.deepStrictEqual(got, ref);
  }

  // A prefix big enough to span several segments (TRC\0 resets), every event
  // type, pools, and symbols — but small enough that byte-by-byte chunking is
  // fast. We pick a prefix that ends just before a mid-stream header so the
  // truncation is clean, falling back to ~1MB.
  let prefixEnd = Math.min(1_000_000, raw.length);
  for (const h of headerOffsets) {
    if (h >= 200_000 && h <= 1_200_000) { prefixEnd = h; break; }
  }
  const prefix = raw.subarray(0, prefixEnd);
  const prefixHeaders = headerOffsetsIn(prefix);
  console.log(
    `Adversarial prefix: ${prefix.length} bytes, ${prefixHeaders.length} segment header(s)`
  );

  // ── Test: chunk size 1 (every single-byte boundary, maximally adversarial)
  //    over the prefix — exercises rollback at every byte offset. ──
  await testAsync("chunk size 1 (byte-by-byte) over prefix matches", async () => {
    await assertStreamMatches(prefix, chunked(prefix, 1));
  });

  // ── Test: tiny prime chunk sizes over the prefix ──
  for (const size of [2, 3, 5, 7, 13]) {
    await testAsync(`chunk size ${size} over prefix matches`, async () => {
      await assertStreamMatches(prefix, chunked(prefix, size));
    });
  }

  // ── Test: a spread of fixed chunk sizes over the FULL buffer ──
  for (const size of [64, 256, 1024, 4096, 65536, 1 << 20]) {
    await testAsync(`chunk size ${size} (full trace) matches reference`, async () => {
      const got = await streamCanon(chunked(raw, size));
      assert.deepStrictEqual(got, refCanon);
    });
  }

  // ── Test: one giant chunk (whole buffer in a single read) ──
  await testAsync("single whole-buffer chunk matches reference", async () => {
    const got = await streamCanon(chunked(raw, raw.length));
    assert.deepStrictEqual(got, refCanon);
  });

  // ── Test: boundary placed inside a mid-stream TRC\0 header (partial header
  //    straddling the chunk boundary) on the full buffer. ──
  if (headerOffsets.length > 0) {
    const h = headerOffsets[0];
    for (const off of [h, h + 1, h + 2, h + 3, h + 4]) {
      await testAsync(`boundary inside/at TRC\\0 header at +${off - h}`, async () => {
        const got = await streamCanon(chunkedAt(raw, [off]));
        assert.deepStrictEqual(got, refCanon);
      });
    }
    // Boundaries inside EVERY mid-stream header at once.
    await testAsync("boundary inside every TRC\\0 header at once", async () => {
      const got = await streamCanon(chunkedAt(raw, headerOffsets.flatMap((x) => [x + 1, x + 2, x + 3])));
      assert.deepStrictEqual(got, refCanon);
    });
  }

  // ── Test: boundaries placed mid-event over the prefix. Event frames are
  //    TAG_EVENT(0x02) followed by a 2-byte type_id and (for timestamped
  //    events) a 3-byte delta. Split 1..4 bytes after each 0x02 byte to land
  //    inside the type_id / timestamp delta. Many of these land mid-event;
  //    any that land between frames are still valid splits. ──
  await testAsync("boundaries mid-event (after 0x02 tags) over prefix match", async () => {
    const boundaries = [];
    for (let i = 5; i < prefix.length; i++) {
      if (prefix[i] === 0x02) {
        for (const d of [1, 2, 3, 4]) {
          if (i + d < prefix.length) boundaries.push(i + d);
        }
      }
    }
    await assertStreamMatches(prefix, chunkedAt(prefix, boundaries));
  });

  // ── Test: gzipped input, decoded after gunzip, fed chunked into the stream
  //    parser. (fetchTraceStream does the gunzip; here we gunzip then chunk the
  //    post-gunzip bytes, which is exactly what the stream parser consumes.) ──
  await testAsync("gzip round-trip: gunzip then chunked stream matches reference", async () => {
    const gz = zlib.gzipSync(Buffer.from(raw));
    const regunzipped = Uint8Array.from(zlib.gunzipSync(gz));
    const got = await streamCanon(chunked(regunzipped, 1000));
    assert.deepStrictEqual(got, refCanon);
  });

  // ── Test: empty / header-only streams error like the buffered parser ──
  await testAsync("empty stream throws Invalid trace header", async () => {
    let threw = false;
    try {
      await parseTraceStream(chunked(new Uint8Array(0), 1));
    } catch (e) {
      threw = /Invalid trace header/.test(e.message);
    }
    assert.ok(threw, "expected Invalid trace header");
  });

  // ── Test: time-range filtering is honored identically when streaming ──
  await testAsync("time-range filtered stream matches filtered reference", async () => {
    const mid = reference.recordMinTs != null && reference.recordMaxTs != null
      ? Math.floor((reference.recordMinTs + reference.recordMaxTs) / 2)
      : null;
    if (mid == null) return; // no time bounds; skip
    const opts = { startTime: reference.recordMinTs, endTime: mid };
    const refFiltered = canonical(await parseTrace(raw, opts));
    const streamFiltered = canonical(await parseTraceStream(chunked(raw, 333), opts));
    assert.deepStrictEqual(streamFiltered, refFiltered);
  });

  // ── Test: fetchTraceStream falls back gracefully when the ok response has
  //    no streamable `body` (e.g. cached/synthesized responses). It must still
  //    gunzip + decode to the same ParsedTrace instead of throwing on
  //    `null.getReader()`. We mock fetch to return a body-less response that
  //    only exposes arrayBuffer(), for both gzipped and raw bodies. ──
  for (const [label, body] of [["gzipped", zlib.gzipSync(Buffer.from(raw))], ["raw", raw]]) {
    await testAsync(`fetchTraceStream falls back when response has no body (${label})`, async () => {
      const u8 = Uint8Array.from(body);
      const original = global.fetch;
      global.fetch = async () => ({
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
        assert.deepStrictEqual(got, refCanon);
      } finally {
        global.fetch = original;
      }
    });
  }

  summarize();
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
