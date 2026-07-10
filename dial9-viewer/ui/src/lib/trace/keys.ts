// lib/trace/keys.ts - S3 trace-key parsing (T09; architecture 2.7,
// features/01 I2). Ported out of the inline `parseKey` in index.html
// (index.html:1006-1059); the legacy inline copy stays until T15 re-points
// its callers and retires tests/core/parse_key.test.ts.
//
// DEFECT FIX (ADR-0004 section 1; features/01 "Live validation" Finding 1):
// the legacy parser silently fell back to positional parsing for keys whose
// component count matched no documented layout, shifting columns (the
// dev-server's 6-segment demo key showed Service=host-0, Host=abcd). The
// typed parser returns an explicit `{ layout: "unknown", rawKey }` variant
// instead of silently mislabeled fields. The positional fallback survives
// ONLY for keys with no date-shaped segment at all (custom prefix schemes),
// where it was genuinely best-effort rather than a mislabel of a documented
// layout.

/** A key that matched a documented layout (or the positional fallback). */
export interface KnownTraceKey {
  layout: "known";
  service: string;
  host: string;
  /**
   * Boot id from the #225 layout; "" for the legacy (pre-#225) layout and
   * the positional fallback, which carry no boot id on the path.
   */
  bootId: string;
  /**
   * Segment start (unix seconds) from the `{epoch}-{index}.bin[.gz]`
   * filename; 0 when the filename does not match that pattern.
   */
  epoch: number;
  /** Segment index from the filename; "" when the filename doesn't match. */
  segIndex: string;
}

/**
 * A key whose directory layout is unrecognized. No field is guessed: the
 * legacy behavior of positionally shifting columns for these keys is the
 * defect this variant fixes. Callers surface the raw key instead.
 */
export interface UnknownTraceKey {
  layout: "unknown";
  rawKey: string;
}

export type ParsedTraceKey = KnownTraceKey | UnknownTraceKey;

const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
const FILE_RE = /^(\d+)-(\d+)\.bin/;

function known(
  service: string,
  host: string,
  bootId: string,
  epoch: number,
  segIndex: string
): KnownTraceKey {
  return { layout: "known", service, host, bootId, epoch, segIndex };
}

/**
 * Parse an S3 trace key into service / host / boot / segment metadata.
 *
 * Default layout (as of issue #225):
 *   {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{boot_id}/{epoch}-{index}.bin[.gz]
 * Legacy layout (pre-#225):
 *   {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{epoch}-{index}.bin[.gz]
 *
 * We find the date-shaped segment and count components between it and the
 * filename to distinguish. Keys with a date segment but a component count
 * matching neither layout are `layout: "unknown"` (see the defect-fix note
 * above). Keys with NO date-shaped segment fall back to best-effort
 * positional parsing when they have enough components, and are otherwise
 * `unknown` too.
 *
 * The legacy parse result exposed a lazy `traceStart` getter that read the
 * page-global timezone toggle; the typed boundary keeps parsing pure - call
 * `formatEpoch(key.epoch, { localTz })` at render time instead.
 */
export function parseKey(key: string): ParsedTraceKey {
  const parts = key.split("/");
  let dateIdx = -1;
  for (let i = parts.length - 1; i >= 0; i--) {
    // split() never yields holes; the index is in range.
    if (DATE_RE.test(parts[i]!)) {
      dateIdx = i;
      break;
    }
  }
  const file = parts[parts.length - 1] ?? "";
  const match = FILE_RE.exec(file);
  let epoch = 0;
  let segIndex = "";
  if (match) {
    epoch = parseInt(match[1]!, 10);
    segIndex = match[2]!;
  }
  if (dateIdx >= 0) {
    // Components after the date, not including the date itself.
    const below = parts.length - 1 - dateIdx;
    if (below === 5) {
      return known(
        parts[dateIdx + 2]!,
        parts[dateIdx + 3]!,
        parts[dateIdx + 4]!,
        epoch,
        segIndex
      );
    }
    if (below === 4) {
      return known(parts[dateIdx + 2]!, parts[dateIdx + 3]!, "", epoch, segIndex);
    }
    // A date-shaped segment with an undocumented component count: the
    // legacy code shifted columns positionally here (Finding 1's demo-key
    // mislabel). Flag it instead.
    return { layout: "unknown", rawKey: key };
  }
  // No date-shaped segment anywhere: positional, best-effort (preserved
  // legacy behavior for custom prefix schemes).
  if (parts.length >= 5) {
    return known(
      parts[parts.length - 3]!,
      parts[parts.length - 2]!,
      "",
      epoch,
      segIndex
    );
  }
  return { layout: "unknown", rawKey: key };
}

/** Formatting options shared by `formatEpoch` / `traceTitleParams`. */
export interface EpochFormatOptions {
  /**
   * Render in the browser's local timezone instead of UTC. The legacy
   * pages read a page-global `useLocalTz` toggle (default false / UTC);
   * pages pass their live preference here.
   */
  localTz?: boolean;
}

/**
 * Format a unix-seconds epoch as "YYYY-MM-DD HH:MM:SS" (UTC by default,
 * local time with `localTz`). Returns "" for 0/missing epochs - the
 * "filename didn't carry an epoch" case, not a hidden error.
 * Ported from index.html's `formatEpoch`/`formatDate` (index.html:988-1004).
 */
export function formatEpoch(epoch: number, opts: EpochFormatOptions = {}): string {
  if (!epoch) return "";
  const d = new Date(epoch * 1000);
  if (opts.localTz) {
    const pad = (n: number) => String(n).padStart(2, "0");
    return (
      d.getFullYear() +
      "-" +
      pad(d.getMonth() + 1) +
      "-" +
      pad(d.getDate()) +
      " " +
      pad(d.getHours()) +
      ":" +
      pad(d.getMinutes()) +
      ":" +
      pad(d.getSeconds())
    );
  }
  return d.toISOString().replace("T", " ").slice(0, 19);
}
