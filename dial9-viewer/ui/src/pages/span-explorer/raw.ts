import type { SpanStatsResponse } from "../../lib/trace/index.js";

type FetchFn = typeof fetch;

/** Fetch one trace component without decoding or decompressing it in the browser. */
export async function fetchRawTraceBytes(
  traceUrl: string,
  origin: string,
  headers: Record<string, string>,
  request: FetchFn = globalThis.fetch,
): Promise<ArrayBuffer> {
  const url = new URL(traceUrl, origin);
  const sameOrigin = url.origin === new URL(origin).origin;
  const response = await request(url, {
    headers: sameOrigin ? headers : undefined,
  });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} fetching raw trace`);
  }
  return response.arrayBuffer();
}

/** Send already-fetched trace bytes to the Rust span decoder and aggregator. */
export async function requestRawSpanStats(
  traceBytes: ArrayBuffer,
  origin: string,
  request: FetchFn = globalThis.fetch,
): Promise<SpanStatsResponse> {
  const response = await request(new URL("/api/span-stats", origin), {
    method: "POST",
    headers: { "content-type": "application/octet-stream" },
    body: traceBytes,
  });
  if (!response.ok) {
    const detail = (await response.text()).trim();
    throw new Error(
      `HTTP ${response.status} computing raw span stats${detail ? `: ${detail}` : ""}`,
    );
  }

  const value: unknown = await response.json();
  if (
    value == null ||
    typeof value !== "object" ||
    !Array.isArray((value as { span_types?: unknown }).span_types)
  ) {
    throw new Error("raw span stats response is missing span_types");
  }
  return value as SpanStatsResponse;
}

/** Describe response truncation explicitly so a partial catalog cannot look complete. */
export function rawStatsSummary(response: SpanStatsResponse): string {
  const returnedTypes = response.span_types.length;
  const trackedTypes = response.total_span_types_tracked;
  const instances = response.span_types.reduce((sum, type) => sum + type.count, 0);
  const typeSummary = response.types_truncated
    ? `${returnedTypes} of ${trackedTypes} tracked span types`
    : `${returnedTypes} span types`;
  const instanceSummary =
    response.types_truncated || response.types_overflow_instances > 0
      ? `${instances.toLocaleString()} shown instances`
      : `${instances.toLocaleString()} instances`;
  const overflow =
    response.types_overflow_instances > 0
      ? ` · ${response.types_overflow_instances.toLocaleString()} instances omitted by type cap`
      : "";
  return `${typeSummary} · ${instanceSummary}${overflow}`;
}
