// Legacy-compatible key parsing + title metadata for the browser page (T14).
//
// ================================ T15 SEAM ================================
// T14 is a FAITHFUL port: the legacy page's `parseKey` silently falls back
// to positional parsing for keys whose layout matches no documented shape,
// SHIFTING columns (features/01 Live-validation Finding 1: the dev seed's
// 6-component demo key renders Service="host-0", Host="abcd"). The typed
// parser in lib/trace/keys.ts instead returns `{ layout: "unknown" }` for
// those keys (the ADR-0004 section 1 defect fix).
//
// This module bridges the two for T14: it consumes lib/trace `parseKey` and,
// for `unknown` layouts, reproduces the legacy positional fallback EXACTLY,
// so every rendered column, heatmap row, census entry and popup URL matches
// the legacy page byte-for-byte. T15 (index-page contract amendments)
// retires this compat layer: it switches rendering to the unknown-layout
// discriminant (raw-key display, no shifted columns) and re-points the
// title metadata at lib/trace/title.ts.
// ==========================================================================

import { parseKey, objectTraceUrls } from "../../lib/trace/index.js";
import { formatEpochStr } from "./format.js";

export { objectTraceUrls };

/** The flat shape the legacy inline `parseKey` produced (all-string fields,
 * "" where a component is absent; epoch 0 when the filename carries none).
 * The legacy object also carried a lazy `traceStart` getter reading the
 * page-global TZ flag; render sites call formatEpochStr(epoch, tz) instead. */
export interface LegacyParsedKey {
  service: string;
  host: string;
  bootId: string;
  epoch: number;
  segIndex: string;
}

const FILE_RE = /^(\d+)-(\d+)\.bin/;

/**
 * Parse an S3 key with the legacy page's exact semantics (see the T15 seam
 * note above). Known layouts come from the typed parser; unknown layouts
 * take the legacy positional fallback:
 *   parts.length >= 5 -> service/host from the trailing components (the
 *     Finding-1 mislabel), epoch/segIndex still read from the filename;
 *   shorter keys -> service "", host = the whole key, epoch 0.
 */
export function parseKeyCompat(key: string): LegacyParsedKey {
  const parsed = parseKey(key);
  if (parsed.layout === "known") {
    const { service, host, bootId, epoch, segIndex } = parsed;
    return { service, host, bootId, epoch, segIndex };
  }
  // T15 SEAM - legacy positional fallback for unknown layouts.
  const parts = key.split("/");
  if (parts.length >= 5) {
    const file = parts[parts.length - 1] ?? "";
    const match = FILE_RE.exec(file);
    return {
      service: parts[parts.length - 3]!,
      host: parts[parts.length - 2]!,
      bootId: "",
      epoch: match ? parseInt(match[1]!, 10) : 0,
      segIndex: match ? match[2]! : "",
    };
  }
  // Legacy last resort: the whole key in the Host column, epoch dropped
  // (the legacy code discarded even a parseable filename epoch here).
  return { service: "", host: key, bootId: "", epoch: 0, segIndex: "" };
}

/**
 * Everything before the first `YYYY-MM-DD` path segment of an S3 key - the
 * authoritative key prefix handed to the aggregation endpoints (features/01
 * I8, #570). "" when no date segment is found. Ported verbatim from the
 * legacy inline `extractPrefix`.
 */
export function extractPrefix(key: string): string {
  const parts = key.split("/");
  const dateRe = /^\d{4}-\d{2}-\d{2}$/;
  for (let i = 0; i < parts.length; i++) {
    if (dateRe.test(parts[i]!)) {
      return parts.slice(0, i).join("/");
    }
  }
  return "";
}

/**
 * Structured title metadata (svc / host / time window / segment count) as
 * URL params, with the LEGACY field semantics (features/01 I3): built over
 * parseKeyCompat, so unknown-layout keys contribute their positionally
 * shifted service/host exactly as the legacy page's links did (T15 seam;
 * lib/trace/title.ts is the amended replacement).
 */
export function titleParamsCompat(keys: readonly string[], localTz: boolean): URLSearchParams {
  const parsed = keys.map((k) => parseKeyCompat(k));
  const services = [...new Set(parsed.map((p) => p.service).filter(Boolean))];
  const hosts = [...new Set(parsed.map((p) => p.host).filter(Boolean))];
  const epochs = parsed
    .map((p) => p.epoch)
    .filter((e) => e > 0)
    .sort((a, b) => a - b);
  const params = new URLSearchParams();
  if (services.length) params.set("svc", services.join(", "));
  if (hosts.length === 1) params.set("host", hosts[0]!);
  if (epochs.length >= 2) {
    params.set("from", formatEpochStr(epochs[0]!, localTz));
    params.set("to", formatEpochStr(epochs[epochs.length - 1]!, localTz));
  } else if (epochs.length === 1) {
    params.set("from", formatEpochStr(epochs[0]!, localTz));
  }
  params.set("segs", String(keys.length));
  return params;
}
