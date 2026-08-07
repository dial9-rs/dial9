// Type declarations for `tokio_stats_api.js` (pure helpers for the aggregated
// tokio-stats page). See src/types/decode.d.ts for the declaration-form
// rationale. Consumed by typed src/ through the lib/trace boundary
// (src/lib/trace/tokio_stats_api.ts). The exemplar deep-link builder and the
// latency heat scale are declared; the coverage/refine helpers are
// reimplemented in src/pages/tokio-stats/{format,stats}.ts.

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

  /**
   * Severity color for a poll/latency duration (ns): >=3ms red, >=1ms amber,
   * else green, in dial9's palette. Thresholds are pinned by
   * test_tokio_stats_api.js, so consumers must not re-derive them.
   */
  export function latencyHeat(ns: number): string;

  /**
   * Severity color for a worker busyness percentage, in dial9's palette.
   * Thresholds are pinned by test_tokio_stats_api.js, so consumers must not
   * re-derive them.
   */
  export function busynessHeat(busyPct: number): string;

  /**
   * A host's POOLED busyness percentage across its workers
   * (Σ busy_ns / Σ span_ns * 100), which weights each worker by its observed
   * time. Averaging per-worker percentages instead would let a sparsely-sampled
   * worker read as spuriously busy, so consumers must not re-derive this.
   */
  export function hostBusyPct(workers: readonly { busy_ns: number; span_ns: number }[]): number;

  /**
   * A host's `active` (observed) vs `total` (configured) worker counts. Tokio
   * numbers workers 0..N-1, so `total` is derived as max id + 1.
   */
  export function hostWorkerCounts(
    workers: readonly { worker_id: number }[],
  ): { active: number; total: number };
}
