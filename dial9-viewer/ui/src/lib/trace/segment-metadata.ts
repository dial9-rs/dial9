// The trace-embedded service/host identity and its reconciliation with the
// key-derived svc/host the viewer also sees. Two sources:
//   1. TRACE-EMBEDDED metadata - `segmentMetadata.get("service"|"host")`,
//      written by the producer. Authoritative: it travels inside the trace.
//   2. KEY-DERIVED params - the `svc`/`host` URL query params the S3 browser
//      page appends when it opens the viewer (title.ts `traceTitleParams`,
//      from parsing the S3 object KEY). A hint from the storage path, not the
//      trace body.
//
// Rule: embedded metadata WINS when both are present; a disagreeing
// key-derived value is preserved as a tooltip. When only the key-derived
// value exists (e.g. an old trace with no embedded metadata), it is shown as
// a fallback.
//
// The key NAMES ("service", "host") are literal strings, confirmed against
// the writer side.

import type { ParsedTrace, SegmentIdentity } from "../../types/trace.js";

/** SegmentMetadata key the writer emits the service name under. */
export const SEGMENT_SERVICE_KEY = "service";
/** SegmentMetadata key the writer emits the host / instance path under. */
export const SEGMENT_HOST_KEY = "host";

/**
 * Read the service/host identity from a parsed trace's embedded
 * SegmentMetadata map. Empty-string values are treated as absent (a key
 * present with no value carries no identity). Returns `{}` for a null trace
 * or a trace whose metadata declares neither key.
 */
export function readSegmentIdentity(trace: ParsedTrace | null): SegmentIdentity {
  if (trace === null) return {};
  return pickIdentity(
    trace.segmentMetadata.get(SEGMENT_SERVICE_KEY),
    trace.segmentMetadata.get(SEGMENT_HOST_KEY),
  );
}

/** Return every segment-metadata entry, sorted by key for display. */
export function readSegmentMetadataEntries(
  trace: ParsedTrace | null,
): [string, string][] {
  if (trace === null) return [];
  return [...trace.segmentMetadata].sort(([a], [b]) => a.localeCompare(b));
}

/**
 * Parse the S3-key-derived `svc`/`host` identity the browser page appends to
 * the viewer URL (`traceTitleParams`). Accepts a `location.search` string.
 */
export function readKeyDerivedIdentity(search: string): SegmentIdentity {
  const params = new URLSearchParams(search);
  return pickIdentity(params.get("svc"), params.get("host"));
}

/** Build a SegmentIdentity, dropping null / undefined / empty values. */
function pickIdentity(
  service: string | null | undefined,
  host: string | null | undefined,
): SegmentIdentity {
  const identity: SegmentIdentity = {};
  if (service) identity.service = service;
  if (host) identity.host = host;
  return identity;
}

/** One reconciled identity field: the value to display plus provenance. */
export interface IdentityField {
  /** The value to display (embedded metadata wins over key-derived). */
  value: string;
  /** True when `value` came from trace-embedded metadata, false when it is
   *  the key-derived URL fallback (no embedded value existed). */
  fromMetadata: boolean;
  /** The key-derived value, set ONLY when it is present AND disagrees with
   *  the displayed embedded value (surfaced as a tooltip). */
  keyDerived?: string;
}

/** The reconciled service/host identity for display in the file-info area. */
export interface ReconciledIdentity {
  service?: IdentityField;
  host?: IdentityField;
}

/**
 * Reconcile the trace-EMBEDDED identity (source 1) against the KEY-derived
 * identity (source 2). Embedded metadata wins; a disagreeing key-derived
 * value is carried as `keyDerived` for a tooltip. A field present in only one
 * source is taken from that source. See the module header for the rule.
 */
export function reconcileIdentity(
  embedded: SegmentIdentity,
  keyDerived: SegmentIdentity,
): ReconciledIdentity {
  const reconciled: ReconciledIdentity = {};
  const service = reconcileField(embedded.service, keyDerived.service);
  if (service) reconciled.service = service;
  const host = reconcileField(embedded.host, keyDerived.host);
  if (host) reconciled.host = host;
  return reconciled;
}

/** Reconcile a single field per the embedded-wins rule. */
function reconcileField(
  embedded: string | undefined,
  keyDerived: string | undefined,
): IdentityField | undefined {
  if (embedded !== undefined) {
    const field: IdentityField = { value: embedded, fromMetadata: true };
    // Only tooltip the key-derived value when it genuinely disagrees.
    if (keyDerived !== undefined && keyDerived !== embedded) {
      field.keyDerived = keyDerived;
    }
    return field;
  }
  if (keyDerived !== undefined) {
    return { value: keyDerived, fromMetadata: false };
  }
  return undefined;
}
