// Type declarations for `tokio_stats_api.js` (pure helpers for the aggregated
// tokio-stats page). See src/types/decode.d.ts for the declaration-form
// rationale. Consumed by typed src/ through the lib/trace boundary
// (src/lib/trace/tokio_stats_api.ts). Only the exemplar deep-link builder is
// declared; the coverage/latency/refine helpers are reimplemented in
// src/pages/tokio-stats/{format,stats}.ts.

declare module "*/tokio_stats_api.js" {
  // The frozen builder guards each optional with `!= null`/truthiness, so
  // callers may pass `undefined` (e.g. a spawn-loc exemplar with no worker/task);
  // the `| undefined` unions keep that legal under exactOptionalPropertyTypes.
  export interface ExemplarViewerOpts {
    /** Source trace object key (the poll's file). Required. */
    sourceKey: string;
    bucket?: string | undefined;
    svc?: string | undefined;
    host?: string | undefined;
    /** Non-destructive viewer focus on the exact poll (focus_* params). */
    focusStartNs?: number | undefined;
    focusEndNs?: number | undefined;
    focusWorker?: number | undefined;
    focusTask?: number | undefined;
  }

  /**
   * Build the viewer deep link that focuses one poll: an `/api/object` trace
   * component plus non-destructive `focus_*` params. Empty string when
   * `sourceKey` is absent.
   */
  export function exemplarViewerUrl(opts: ExemplarViewerOpts): string;
}
