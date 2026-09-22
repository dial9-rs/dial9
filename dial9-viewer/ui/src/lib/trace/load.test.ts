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
  loadTraceOnMainThread,
  loadTraceStreamed,
  objectTraceUrls,
  parseTraceBuffer,
} from "./load.js";
import { parseChunks, streamTrace } from "./stream.js";
import type { ParsedTrace } from "./load.js";

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
    const { trace, bytes, mode } = await loadTraceBuffered("/a");
    expect(mode).toBe("buffered");
    expect(bytes).toBe(rawTrace.length);
    expect(trace.events.length).toBe(singleEvents);
  });

  it("gzipped component is gunzipped client-side", async () => {
    installFetchMock({ "/a.gz": gzTrace });
    const { bytes } = await loadTraceBuffered(["/a.gz"]);
    expect(bytes).toBe(rawTrace.length);
  });

  it("mixed gzip/raw components concatenate in order and parse as one trace", async () => {
    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const { trace, bytes } = await loadTraceBuffered(["/gz", "/raw"]);
    expect(bytes).toBe(rawTrace.length * 2);
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

// ── Streamed path: parity with buffered ──────────────────────────────────

describe("loadTraceStreamed", () => {
  it("runtime supports streaming (fixture precondition)", () => {
    expect(canStreamDecode()).toBe(true);
  });

  it("multi-URL stream sees the same events and bytes as the buffered concat", async () => {
    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const streamed = await loadTraceStreamed(["/gz", "/raw"]);
    expect(streamed.mode).toBe("stream");
    expect(streamed.trace.events.length).toBe(singleEvents * 2);

    installFetchMock({ "/gz": gzTrace, "/raw": rawTrace });
    const buffered = await loadTraceBuffered(["/gz", "/raw"]);
    expect(streamed.bytes).toBe(buffered.bytes);
  });

  // Set/Clear Range re-parses these rather than re-fetching, so they have to be
  // the compressed form and they have to decode back to the same trace.
  it("captures the compressed bytes, not the decompressed ones", async () => {
    installFetchMock({ "/a.gz": gzTrace, "/b.gz": gzTrace });
    const { trace, bytes, compressed } = await streamTrace(
      ["/a.gz", "/b.gz"],
      {},
      {},
      true,
    );
    expect(compressed).toBeDefined();
    expect(compressed!.length).toBe(gzTrace.length * 2);
    expect(compressed!.length).toBeLessThan(bytes);

    // Re-parsed the way the page does it: hand the capture back as one source.
    installFetchMock({ "/again": compressed! });
    const again = await streamTrace(["/again"], {}, {});
    expect(again.trace.events.length).toBe(trace.events.length);
    expect(again.bytes).toBe(bytes);
  });

  // The reader sniffs one gzip magic for the whole capture, so a plain
  // component would leave a tail that cannot be decoded.
  it("drops the capture when any component arrives uncompressed", async () => {
    installFetchMock({ "/a.gz": gzTrace, "/raw": rawTrace });
    const { compressed } = await streamTrace(["/a.gz", "/raw"], {}, {}, true);
    expect(compressed).toBeUndefined();
  });

  it("skips the capture unless asked", async () => {
    installFetchMock({ "/a": rawTrace });
    const { compressed } = await streamTrace(["/a"], {}, {}, false);
    expect(compressed).toBeUndefined();
  });

  it("single-URL stream counts the raw bytes", async () => {
    installFetchMock({ "/a": rawTrace });
    const { trace, bytes } = await loadTraceStreamed("/a");
    expect(bytes).toBe(rawTrace.length);
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

// ── Main-thread loader (no worker clone) ─────────────────────────────────

describe("loadTraceOnMainThread", () => {
  function fakeStore(): {
    updates: { trace: ParsedTrace }[];
    update(slice: "trace", patch: { trace: ParsedTrace }): void;
  } {
    const updates: { trace: ParsedTrace }[] = [];
    return {
      updates,
      update(_slice, patch): void {
        updates.push(patch);
      },
    };
  }

  it("parses on the caller thread, writes the store slice, resolves with timing", async () => {
    installFetchMock({ "/t.bin": gzTrace });
    const store = fakeStore();
    const result = await loadTraceOnMainThread(store, ["/t.bin"], {}).done;
    expect(store.updates).toHaveLength(1);
    expect(store.updates[0]!.trace.events.length).toBe(singleEvents);
    // The resolved trace IS the one written to the store (same identity, no clone).
    expect(result.trace).toBe(store.updates[0]!.trace);
    expect(result.mode).toBe(canStreamDecode() ? "stream" : "buffered");
    expect(result.timing.events).toBe(singleEvents);
    expect(result.bytes).toBe(rawTrace.length);
  });

  // The page can only report analysis progress while nothing else is rendering
  // the new trace, so the store write has to wait for the last slice.
  it("finishes analyzing before it writes the store", async () => {
    installFetchMock({ "/t.bin": gzTrace });
    const log: string[] = [];
    const store = {
      updates: [] as { trace: ParsedTrace }[],
      update(_slice: "trace", patch: { trace: ParsedTrace }): void {
        log.push("store");
        store.updates.push(patch);
      },
    };
    const fractions: number[] = [];
    const result = await loadTraceOnMainThread(store, ["/t.bin"], {
      onProgress: (p): void => {
        if (p.phase !== "analyzing") return;
        fractions.push(p.bytesRead);
        log.push("analyze");
      },
    }).done;

    expect(log.at(-1)).toBe("store");
    expect(log.filter((e) => e === "store")).toHaveLength(1);
    expect(fractions.at(0)).toBe(0);
    expect(fractions.at(-1)).toBe(1);
    // Non-decreasing, so the label never walks backwards.
    for (let i = 1; i < fractions.length; i++) {
      expect(fractions[i]!).toBeGreaterThanOrEqual(fractions[i - 1]!);
    }
    expect(result.trace).toBe(store.updates[0]!.trace);
  });

  it("forwards parse progress with a growing event count", async () => {
    installFetchMock({ "/t.bin": gzTrace });
    const store = fakeStore();
    let sawParsing = false;
    let maxEvents = 0;
    await loadTraceOnMainThread(store, ["/t.bin"], {
      onProgress: (p): void => {
        if (p.phase === "parsing") sawParsing = true;
        maxEvents = Math.max(maxEvents, p.eventCount);
      },
    }).done;
    expect(sawParsing).toBe(true);
    expect(maxEvents).toBeGreaterThan(0);
  });

  it("abort() rejects with AbortError and never touches the store", async () => {
    installFetchMock({ "/t.bin": gzTrace });
    const store = fakeStore();
    const load = loadTraceOnMainThread(store, ["/t.bin"], {});
    load.abort();
    await expect(load.done).rejects.toMatchObject({ name: "AbortError" });
    expect(store.updates).toHaveLength(0);
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
