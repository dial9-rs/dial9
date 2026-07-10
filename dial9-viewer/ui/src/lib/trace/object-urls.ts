// lib/trace/object-urls.ts - /api/object URL building (features/01 I4).
//
// Moved out of load.ts (T14) so pages that only build links - the S3
// browser - can import it WITHOUT pulling the frozen parser into their
// bundle: load.ts imports trace_parser.js at module eval, which the
// browser page neither needs nor can evaluate (the core's browser branch
// expects <script>-established globals). load.ts re-exports this, so its
// import surface is unchanged.

/**
 * Build one viewer `trace=` component per selected key, each pointing at
 * /api/object (which serves that single file's raw, still-gzipped bytes).
 * The viewer and flamegraph read all repeated `trace=` params and download
 * them in parallel + gunzip client-side (see load.ts loadTrace). Ported
 * from index.html:1713-1720 (features/01 I4); this replaced a single
 * /api/trace URL that forced the backend to fetch, gunzip and merge every
 * file into one large uncompressed response.
 */
export function objectTraceUrls(bucket: string, keys: readonly string[]): string[] {
  return keys.map((key) => {
    const p = new URLSearchParams();
    p.set("bucket", bucket);
    p.set("key", key);
    return "/api/object?" + p.toString();
  });
}
