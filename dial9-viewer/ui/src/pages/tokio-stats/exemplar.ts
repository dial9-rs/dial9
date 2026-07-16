// The exemplar deep-link builder: delegates to the frozen exemplarViewerUrl so
// the viewer link stays identical between the two UIs. The poll's window is
// forwarded as a non-destructive focus (focus_*), targeting an /api/object
// trace component.

import type { PollExemplar } from "../../lib/trace/index.js";
import { exemplarViewerUrl } from "../../lib/trace/tokio_stats_api.js";

/**
 * Build the viewer deep link for one exemplar, or "" when the exemplar is null
 * or has no start_ns. `bucketFromData` is the response's echoed source bucket;
 * `bucketParam` is the URL fallback.
 */
export function exemplarLink(
  ex: PollExemplar | null | undefined,
  bucketFromData: string | null | undefined,
  bucketParam: string | null | undefined,
): string {
  if (!ex || !ex.start_ns) return "";
  return exemplarViewerUrl({
    sourceKey: ex.source_key,
    bucket: bucketFromData || bucketParam || "",
    host: ex.host || undefined,
    focusStartNs: ex.start_ns,
    focusEndNs: ex.end_ns,
    focusWorker: ex.worker_id,
    focusTask: ex.task_id,
  });
}
