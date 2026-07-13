// The exemplar deep-link builder.
//
// KNOWN DEFECT (preserved): the link still targets the `/api/trace` endpoint,
// which #582 removed - so every exemplar click opens a viewer tab whose trace
// fetch 404s. Kept byte-identical; fixing the endpoint is a separate call.

import type { PollExemplar } from "../../lib/trace/index.js";

/**
 * Build the viewer deep link for one exemplar, or "" when the exemplar is null
 * or has no start_ns. `bucketFromData` is the response's echoed source bucket;
 * `bucketParam` is the URL fallback. All values are URL-encoded.
 */
export function exemplarLink(
  ex: PollExemplar | null | undefined,
  bucketFromData: string | null | undefined,
  bucketParam: string | null | undefined,
): string {
  if (!ex || !ex.start_ns) return "";
  const bucket = bucketFromData || bucketParam || "";
  const traceUrl = `/api/trace?bucket=${encodeURIComponent(bucket)}&keys=${encodeURIComponent(ex.source_key)}`;
  const p = new URLSearchParams();
  p.set("trace", traceUrl);
  p.set("start_ns", String(ex.start_ns));
  p.set("end_ns", String(ex.end_ns));
  return `viewer.html?${p.toString()}`;
}
