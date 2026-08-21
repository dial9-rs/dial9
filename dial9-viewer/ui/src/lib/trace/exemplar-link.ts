import type { SourceScope } from "./source-scope.js";
import { objectTraceUrls } from "./object-urls.js";

/** The trace instance and optional viewer focus carried by an exemplar. */
export interface ViewerExemplar {
  source_key?: string | null | undefined;
  host?: string | null | undefined;
  start_ns?: number | string | null | undefined;
  end_ns?: number | string | null | undefined;
  worker_id?: number | string | null | undefined;
  task_id?: number | string | null | undefined;
  /** Selects an exact named span when the focus window overlaps other spans. */
  span_name?: string | null | undefined;
}

/** Everything needed to open one exemplar in the trace viewer. */
export interface ExemplarViewerLink {
  exemplar: ViewerExemplar;
  source: SourceScope;
  /** Existing raw trace URL. When absent, source_key becomes an /api/object URL. */
  trace?: string | null | undefined;
  /** Response-echoed bucket override for a bucket-relative source_key. */
  objectBucket?: string | null;
  service?: string | null | undefined;
}

function objectTraceUrl(sourceKey: string, fallbackBucket: string): string {
  let bucket = fallbackBucket;
  let key = sourceKey;
  // Span aggregates store fully-qualified S3 source keys, while /api/object
  // requires a bucket-relative key. Poll aggregates already use relative keys.
  const qualified = /^s3:\/\/([^/]+)\/(.+)$/.exec(sourceKey);
  if (qualified) {
    bucket = qualified[1] ?? "";
    key = qualified[2] ?? "";
  }

  return objectTraceUrls(bucket, [key])[0] ?? "";
}

/**
 * Build a viewer URL for a poll, scheduling delay, worker outlier, or span.
 *
 * Aggregate exemplars become one /api/object trace component; raw exemplars
 * reuse their existing trace URL. The viewer fetches that component with the
 * tab's credential headers and decompresses trace objects client-side.
 *
 * The module owns object-key normalization, safe source identity, and focus.
 * `focus_*` pans, highlights, and frames after parsing; unlike `start`/`end`,
 * it never destructively filters surrounding events out of the trace.
 *
 * Literal credential values cannot enter the URL because SourceScope exposes
 * them only behind the `literal` discriminant and this projection writes only
 * that discriminant.
 */
export function buildExemplarViewerUrl(options: ExemplarViewerLink): string {
  const { exemplar, source } = options;
  const traceUrl =
    options.trace ||
    (exemplar.source_key
      ? objectTraceUrl(
          exemplar.source_key,
          options.objectBucket ?? source.bucket,
        )
      : "");
  if (!traceUrl) return "";

  const p = new URLSearchParams();
  p.set("trace", traceUrl);
  if (options.service) p.set("svc", options.service);
  // An exemplar's host identifies its own source file and wins over page scope.
  if (exemplar.host) p.set("host", exemplar.host);
  if (source.region) p.set("aws_region", source.region);
  p.set("credential_mode", source.credentials.kind);
  if (source.credentials.kind === "role") {
    p.set("aws_role_arn", source.credentials.roleArn);
  }

  if (exemplar.start_ns != null) {
    p.set("focus_start", String(exemplar.start_ns));
    if (exemplar.end_ns != null) {
      p.set("focus_end", String(exemplar.end_ns));
    }
    if (exemplar.worker_id != null) {
      p.set("focus_worker", String(exemplar.worker_id));
    }
    if (exemplar.task_id != null) {
      p.set("focus_task", String(exemplar.task_id));
    }
    if (exemplar.span_name) {
      p.set("focus_span_name", exemplar.span_name);
    }
  }

  return "viewer.html?" + p.toString();
}
