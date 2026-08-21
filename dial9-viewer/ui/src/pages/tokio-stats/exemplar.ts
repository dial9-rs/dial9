import {
  buildExemplarViewerUrl,
  type PollExemplar,
  type SourceScope,
} from "../../lib/trace/index.js";

/**
 * Build the viewer deep link for one exemplar, or "" when the exemplar is null
 * or has no start_ns. `bucketFromData` is the response's echoed source bucket;
 * `source.bucket` is the URL fallback. The outer viewer URL also carries the
 * source's safe credential identity so the new tab can fetch the trace.
 */
export function exemplarLink(
  ex: PollExemplar | null | undefined,
  bucketFromData: string | null,
  source: SourceScope,
): string {
  if (!ex || !ex.start_ns) return "";
  return buildExemplarViewerUrl({
    exemplar: ex,
    source,
    objectBucket: bucketFromData,
  });
}
