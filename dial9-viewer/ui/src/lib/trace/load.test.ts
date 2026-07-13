// load.ts tests: fetch + gunzip + concat and the stream/buffered parity, with
// in-memory gzip fixtures + a stubbed global fetch. These cover the typed
// wrapper's orchestration: option splitting, chunk capture, buffer reassembly,
// mode selection, and objectTraceUrls.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";
import { afterEach, beforeAll, describe, expect, it } from "vitest";
import {
  canStreamDecode,
  loadTrace,
  loadTraceBuffered,
  loadTraceStreamed,
  objectTraceUrls,
  parseTraceBuffer,
} from "./load.js";

// ── Fixtures: the demo trace, raw and gzipped, fully in memory ──────────

let rawTrace: Uint8Array;
let gzTrace: Uint8Array;
let singleEvents: number;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url))
  );
  rawTrace =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  gzTrace = new Uint8Array(gzipSync(rawTrace));
  singleEvents = (await parseTraceBuffer(rawTrace)).events.length;
  expect(singleEvents).toBeGreaterThan(0);
});

// ── fetch stub: URL -> bytes, Response-like with arrayBuffer() ───────────

interface RecordedCall {
  url: string;
  opts: { headers?: Record<string, string> } | undefined;
}

const originalFetch = globalThis.fetch;
let calls: RecordedCall[] = [];

function installFetchMock(urlToBytes: Record<string, Uint8Array>): void {
  calls = [];
  globalThis.fetch = (async (url: string, opts?: RecordedCall["opts"]) => {
    calls.push({ url, opts });
    const bytes = urlToBytes[url];
    if (!bytes) {
      return {
        ok: false,
        status: 404,
        async arrayBuffer() {
          return new ArrayBuffer(0);
        },
      };
    }
    return {
      ok: true,
      status: 200,
      async arrayBuffer() {
        return bytes.buffer.slice(
          bytes.byteOffset,
          bytes.byteOffset + bytes.byteLength
        );
      },
    };
  }) as unknown as typeof fetch;
}

afterEach(() => {
  globalThis.fetch = originalFetch;
});

const bytesOf = (buf: ArrayBuffer): Uint8Array => new Uint8Array(buf);

// Byte-equality via Buffer.equals (memcmp): vitest's toEqual deep-diffs
// typed arrays element-by-element, which times out on the ~11 MB trace.
function expectBytesEqual(actual: Uint8Array, expected: Uint8Array): void {
  expect(actual.length).toBe(expected.length);
  expect(Buffer.from(actual).equals(Buffer.from(expected))).toBe(true);
}

// ── Buffered path: fetch + gunzip + concat ───────────────────────────────

describe("loadTraceBuffered", () => {
  it("single raw component round-trips and parses", async () => {
    installFetchMock({ "/a": rawTrace });
    const { trace, buffer, mode } = await loadTraceBuffered("/a");
    expect(mode).toBe("buffered");
    expectBytesEqual(bytesOf(buffer), rawTrace);
    expect(trace.events.length).toBe(singleEvents);
  });

  it("gzipped component is gunzipped client-side", async () => {
    installFetchMock({ "/a.gz": gzTrace });
    const { buffer } = await loadTraceBuffered(["/a.gz"]);
    expectBytesEqual(bytesOf(buffer), rawTrace);
  });

  it("mixed gzip/raw components concatenate in order and parse as one trace", async () => {
    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const { trace, buffer } = await loadTraceBuffered(["/gz", "/raw"]);
    const out = bytesOf(buffer);
    expect(out.length).toBe(rawTrace.length * 2);
    expectBytesEqual(out.slice(0, rawTrace.length), rawTrace);
    expectBytesEqual(out.slice(rawTrace.length), rawTrace);
    // Decoder resets on the mid-stream TRC\0 header: double the events.
    expect(trace.events.length).toBe(singleEvents * 2);
  });

  it("forwards fetch options (headers) and keeps parse options separate", async () => {
    installFetchMock({ "/a": rawTrace, "/b": rawTrace });
    const headers = { "x-dial9-aws-access-key-id": "AKIA" };
    const { trace } = await loadTraceBuffered(["/a", "/b"], {
      headers,
      maxEvents: 5,
    });
    expect(calls.length).toBe(2);
    for (const call of calls) {
      expect(call.opts?.headers).toEqual(headers);
    }
    // maxEvents went to the parser, not the fetch.
    expect(trace.events.length).toBe(5);
    expect(trace.truncated).toBe(true);
  });

  it("a failed component rejects with the status in the message", async () => {
    installFetchMock({ "/ok": rawTrace });
    await expect(loadTraceBuffered(["/ok", "/missing"])).rejects.toThrow(/404/);
  });
});

// ── Streamed path: capture + reassembly parity with buffered ─────────────

describe("loadTraceStreamed", () => {
  it("runtime supports streaming (fixture precondition)", () => {
    expect(canStreamDecode()).toBe(true);
  });

  it("multi-URL stream parses to the same events and retains the full buffer", async () => {
    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const streamed = await loadTraceStreamed(["/gz", "/raw"]);
    expect(streamed.mode).toBe("stream");
    expect(streamed.trace.events.length).toBe(singleEvents * 2);

    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const buffered = await loadTraceBuffered(["/gz", "/raw"]);
    // The captured-chunk reassembly must be byte-identical to the
    // buffered concat, so Set/Clear-Range re-parses see the same trace.
    expectBytesEqual(bytesOf(streamed.buffer), bytesOf(buffered.buffer));
  });

  it("single-URL stream matches the raw bytes", async () => {
    installFetchMock({ "/a": rawTrace });
    const { trace, buffer } = await loadTraceStreamed("/a");
    expectBytesEqual(bytesOf(buffer), rawTrace);
    expect(trace.events.length).toBe(singleEvents);
  });
});

describe("loadTrace mode selection", () => {
  it("uses the streaming path when the runtime can stream-decode", async () => {
    installFetchMock({ "/a": rawTrace });
    const { mode } = await loadTrace("/a");
    expect(mode).toBe(canStreamDecode() ? "stream" : "buffered");
  });
});

// ── objectTraceUrls ─────────────────────────────────────

describe("objectTraceUrls", () => {
  it("builds one /api/object URL per key with encoded bucket and key", () => {
    const urls = objectTraceUrls("my-bucket", [
      "traces/2026-04-09/1900/svc/host/boot/1744224000-0.bin.gz",
      "a key/with spaces&stuff",
    ]);
    expect(urls).toEqual([
      "/api/object?bucket=my-bucket&key=traces%2F2026-04-09%2F1900%2Fsvc%2Fhost%2Fboot%2F1744224000-0.bin.gz",
      "/api/object?bucket=my-bucket&key=a+key%2Fwith+spaces%26stuff",
    ]);
  });

  it("returns an empty list for no keys", () => {
    expect(objectTraceUrls("b", [])).toEqual([]);
  });
});
