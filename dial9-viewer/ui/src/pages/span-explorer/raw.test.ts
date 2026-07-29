import { describe, expect, it, vi } from "vitest";

import { fetchRawTraceBytes, rawStatsSummary, requestRawSpanStats } from "./raw";

describe("fetchRawTraceBytes", () => {
  it("forwards compressed bytes unchanged and keeps credentials same-origin", async () => {
    const gzip = new Uint8Array([0x1f, 0x8b, 0x01, 0x02]);
    const request = vi.fn(async () => new Response(gzip));

    const result = await fetchRawTraceBytes(
      "/api/object?key=trace.bin.gz",
      "http://viewer.test",
      { "x-dial9-aws-access-key-id": "key" },
      request,
    );

    expect(new Uint8Array(result)).toEqual(gzip);
    expect(request.mock.calls[0]![1]?.headers).toEqual({
      "x-dial9-aws-access-key-id": "key",
    });
  });

  it("does not send Dial9 credentials cross-origin", async () => {
    const request = vi.fn(async () => new Response(new Uint8Array([1])));
    await fetchRawTraceBytes(
      "https://example.test/trace.bin",
      "http://viewer.test",
      { "x-dial9-aws-access-key-id": "secret" },
      request,
    );
    expect(request.mock.calls[0]![1]?.headers).toBeUndefined();
  });
});

describe("requestRawSpanStats", () => {
  it("posts trace bytes to the Rust span-stats endpoint", async () => {
    const traceBytes = new Uint8Array([0x54, 0x52, 0x43, 0]).buffer;
    const payload = {
      span_types: [{ kind: "metrique", name: "RecordMetric", p50_ns: 100 }],
      coverage: null,
      types_truncated: false,
      total_span_types_tracked: 1,
      types_overflow_instances: 0,
    };
    const request = vi.fn(
      async (_input: RequestInfo | URL, _init?: RequestInit): Promise<Response> =>
        new Response(JSON.stringify(payload), {
          headers: { "content-type": "application/json" },
        }),
    );

    await expect(
      requestRawSpanStats(traceBytes, "http://127.0.0.1:3003", request),
    ).resolves.toEqual(payload);
    expect(request).toHaveBeenCalledOnce();
    const [url, init] = request.mock.calls[0]!;
    expect(String(url)).toBe("http://127.0.0.1:3003/api/span-stats");
    expect(init).toMatchObject({
      method: "POST",
      headers: { "content-type": "application/octet-stream" },
      body: traceBytes,
    });
  });

  it("surfaces backend decode errors", async () => {
    const request = vi.fn(
      async (_input: RequestInfo | URL, _init?: RequestInit): Promise<Response> =>
        new Response("invalid trace", { status: 400 }),
    );

    await expect(
      requestRawSpanStats(new ArrayBuffer(0), "http://viewer.test", request),
    ).rejects.toThrow("HTTP 400 computing raw span stats: invalid trace");
  });
});

describe("rawStatsSummary", () => {
  it("reports response and tracking truncation", () => {
    expect(
      rawStatsSummary({
        span_types: [{ count: 7 }, { count: 3 }] as never,
        coverage: undefined,
        types_truncated: true,
        total_span_types_tracked: 1_200,
        types_overflow_instances: 42,
      }),
    ).toBe(
      "2 of 1200 tracked span types · 10 shown instances · 42 instances omitted by type cap",
    );
  });
});
