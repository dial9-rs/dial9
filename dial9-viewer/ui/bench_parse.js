// bench_parse.js — diagnostic parse-speed benchmark for the trace decoder.
//
// Measures whole-buffer parse vs. streaming parse (parseTraceStream) at a range
// of chunk sizes, with and without the per-chunk progress yield, so we can see
// where streaming overhead comes from and iterate on it with real data.
//
// Usage:
//   node bench_parse.js [trace.bin|trace.bin.gz] [--iters N] [--quick]
//
// Defaults to demo-trace.bin in this directory. Prints a table of
// milliseconds + events/sec; the streaming rows show overhead vs the
// whole-buffer baseline.

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { parseTrace, parseTraceStream } = require("./trace_parser.js");

function parseArgs(argv) {
  const args = { file: null, iters: 3, quick: false };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--iters") args.iters = Number(argv[++i]);
    else if (a === "--quick") args.quick = true;
    else if (!a.startsWith("--")) args.file = a;
  }
  return args;
}

/** Decompress if gzipped, returning raw trace bytes as a Uint8Array. */
function loadRaw(file) {
  const buf = fs.readFileSync(file);
  if (buf.length >= 2 && buf[0] === 0x1f && buf[1] === 0x8b) {
    return new Uint8Array(zlib.gunzipSync(buf));
  }
  return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
}

/** Async iterable that yields `raw` in fixed-size chunks. Models the network /
 *  DecompressionStream feeding the decoder chunk-by-chunk. `delayMs` optionally
 *  simulates per-chunk arrival latency (0 = arrive as fast as the CPU drains). */
function chunkedSource(raw, chunkSize, delayMs = 0) {
  return {
    async *[Symbol.asyncIterator]() {
      for (let off = 0; off < raw.length; off += chunkSize) {
        const end = Math.min(off + chunkSize, raw.length);
        // Copy so each chunk is its own exactly-sized buffer (like a real
        // stream chunk), not a view into `raw`.
        const chunk = raw.slice(off, end);
        if (delayMs > 0) await new Promise((r) => setTimeout(r, delayMs));
        yield chunk;
      }
    },
  };
}

function fmtMs(ms) {
  return ms.toFixed(0).padStart(7) + " ms";
}
function fmtRate(events, ms) {
  const perSec = events / (ms / 1000);
  return (perSec / 1e6).toFixed(2).padStart(6) + " M ev/s";
}

async function timeIt(fn, iters) {
  const samples = [];
  let result = null;
  for (let i = 0; i < iters; i++) {
    const t0 = process.hrtime.bigint();
    result = await fn();
    const t1 = process.hrtime.bigint();
    samples.push(Number(t1 - t0) / 1e6);
  }
  samples.sort((a, b) => a - b);
  return { ms: samples[Math.floor(samples.length / 2)], result }; // median
}

async function main() {
  const args = parseArgs(process.argv);
  const file = args.file || path.join(__dirname, "demo-trace.bin");
  const raw = loadRaw(file);

  console.log(`\nTrace: ${file}`);
  console.log(`Raw size: ${(raw.length / 1048576).toFixed(2)} MB · iters: ${args.iters} (median reported)\n`);

  // Baseline: whole-buffer parse.
  const base = await timeIt(() => parseTrace(raw.slice()), args.iters);
  const events = base.result.events.length;
  console.log(`Events: ${events.toLocaleString()} · CPU samples: ${base.result.cpuSamples.length.toLocaleString()}\n`);

  const baseMs = base.ms;
  console.log("mode                              time         rate        overhead");
  console.log("─".repeat(72));
  console.log(
    `whole-buffer (baseline)        ${fmtMs(baseMs)}   ${fmtRate(events, baseMs)}        —`
  );

  const KB = 1024, MB = 1024 * 1024;
  const chunkSizes = args.quick
    ? [64 * KB, 1 * MB]
    : [16 * KB, 64 * KB, 256 * KB, 1 * MB, 4 * MB];

  for (const yield_ of [false, true]) {
    console.log(
      `\n  streaming · progress-yield ${yield_ ? "ON " : "OFF"} (onParseProgress set):`
    );
    for (const cs of chunkSizes) {
      // onParseProgress presence is what triggers the progress yield in
      // parseTraceStream, so toggle it to isolate the yield cost.
      const opts = yield_ ? { onParseProgress: () => {} } : {};
      const { ms, result } = await timeIt(
        () => parseTraceStream(chunkedSource(raw, cs), opts),
        args.iters
      );
      const ok = result.events.length === events;
      const label = `chunk ${(cs / KB).toString().padStart(5)}KB`;
      const ovh = `${(((ms - baseMs) / baseMs) * 100).toFixed(0)}%`.padStart(6);
      console.log(
        `    ${label}                ${fmtMs(ms)}   ${fmtRate(events, ms)}     ${ovh}${ok ? "" : "  ✗ EVENT COUNT MISMATCH"}`
      );
    }
  }

  // The browser's DecompressionStream("gzip") feeds ~16KB chunks with
  // onParseProgress set (the viewer always sets it). This is the real-world
  // case parseTraceStream must handle well: it feeds 16KB chunks through the
  // SAME parseTraceStream the viewer uses. With the internal MIN_DRAIN_BYTES
  // batching it should sit near the whole-buffer baseline, not ~1.7x it.
  console.log(
    "\n  ⮑ browser path: 16KB source (DecompressionStream-like) + progress-yield ON:"
  );
  {
    const { ms, result } = await timeIt(
      () => parseTraceStream(chunkedSource(raw, 16 * KB), { onParseProgress: () => {} }),
      args.iters
    );
    const ok = result.events.length === events;
    const ovh = `${(((ms - baseMs) / baseMs) * 100).toFixed(0)}%`.padStart(6);
    console.log(
      `    16KB chunks (batched)      ${fmtMs(ms)}   ${fmtRate(events, ms)}     ${ovh}${ok ? "" : "  ✗ EVENT COUNT MISMATCH"}`
    );
  }
  console.log("");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
