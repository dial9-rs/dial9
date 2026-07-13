// src/pages/tokio-stats/exemplar.ts - the exemplar deep-link builder, a
// verbatim port of the legacy exemplarLink (tokio_stats.html:166-175;
// features/04 H1/H3).
//
// PRESERVED DEFECT (features/04 H4 / finding 1): the link still targets the
// `/api/trace` endpoint, which #582 removed - so every exemplar click opens
// a viewer tab whose trace fetch 404s. T41 is a behavior-preserving port
// (same treatment as T13/T14: defects are carried, not fixed), so the URL is
// built byte-identically to the legacy page; the census/behavioral differ
// therefore carries only the switch delta, and fixing the endpoint is a
// separate maintainer call (ledger). See HANDOFF.md.

import type { PollExemplar } from "../../lib/trace/index.js";

/**
 * Build the viewer deep link for one exemplar, or "" when the exemplar is
 * null or has no start_ns (H3 guard). `bucketFromData` is the response's
 * echoed source bucket (J13); `bucketParam` is the URL fallback (H1). All
 * values are URL-encoded.
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
