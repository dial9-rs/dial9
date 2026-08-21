// S3 trace-key parsing. The parser returns an explicit
// `{ layout: "unknown", rawKey }` variant for keys whose component count
// matches no documented layout, rather than silently falling back to
// positional parsing and shifting columns (which mislabeled fields, e.g. a
// 6-segment demo key showing Service=host-0, Host=abcd). The positional
// fallback survives ONLY for keys with no date-shaped segment at all (custom
// prefix schemes), where it was genuinely best-effort rather than a mislabel
// of a documented layout.

/** A key that matched a documented layout (or the positional fallback). */
export interface KnownTraceKey {
  layout: "known";
  service: string;
  host: string;
  /**
   * Boot id from the versioned or historical layout; "" when absent.
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
 * A key whose directory layout is unrecognized. No DIRECTORY field is
 * guessed: positionally shifting columns for these keys is the defect this
 * variant fixes. Callers surface the raw key instead. The FILENAME
 * convention (`{epoch}-{index}.bin[.gz]`) is independent of the directory
 * layout, so epoch/segIndex are still parsed when the filename matches (they
 * drive time placement and sorting, never a Service/Host/Boot label).
 */
export interface UnknownTraceKey {
  layout: "unknown";
  rawKey: string;
  /**
   * Segment start (unix seconds) from the `{epoch}-{index}.bin[.gz]`
   * filename; 0 when the filename does not match that pattern.
   */
  epoch: number;
  /** Segment index from the filename; "" when the filename doesn't match. */
  segIndex: string;
}

export type ParsedTraceKey = KnownTraceKey | UnknownTraceKey;

const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
const FILE_RE = /^(\d+)-(\d+)\.bin(?:\.gz)?$/;

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
 * Default layout:
 *   {prefix}/version=1/date={YYYY-MM-DD}/service={service}/time={HHMM}/instance={instance}/boot={boot_id}/{epoch}-{index}.bin[.gz]
 * Historical layout (with boot id):
 *   {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{boot_id}/{epoch}-{index}.bin[.gz]
 * Older historical layout (no boot id):
 *   {prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{epoch}-{index}.bin[.gz]
 *
 * Date-like but unsupported positional layouts are `unknown`; dateless keys
 * retain the best-effort positional fallback.
 *
 * Parsing is pure: call `formatEpoch(key.epoch, { localTz })` at render time
 * rather than reading a page-global timezone toggle.
 */
export function parseKey(key: string): ParsedTraceKey {
  const parts = key.split("/");
  const file = parts[parts.length - 1] ?? "";
  const match = FILE_RE.exec(file);
  let epoch = 0;
  let segIndex = "";
  if (match) {
    epoch = parseInt(match[1]!, 10);
    segIndex = match[2]!;
  }
  const version1 = version1Layout(parts);
  if (version1) {
    if (
      typeof version1.date === "string" &&
      DATE_RE.test(version1.date) &&
      typeof version1.service === "string" &&
      typeof version1.time === "string" &&
      TIME_RE.test(version1.time) &&
      typeof version1.instance === "string" &&
      typeof version1.boot === "string"
    ) {
      return known(version1.service, version1.instance, version1.boot, epoch, segIndex);
    }
    return { layout: "unknown", rawKey: key, epoch, segIndex };
  }
  const historical = historicalLayout(parts);
  if (historical) {
    const { start, hasBoot } = historical;
    return known(
      parts[start + 2]!,
      parts[start + 3]!,
      hasBoot ? parts[start + 4]! : "",
      epoch,
      segIndex
    );
  }
  if (parts.some((part) => DATE_RE.test(part) || part === "version=1")) {
    return { layout: "unknown", rawKey: key, epoch, segIndex };
  }
  // No date-shaped segment anywhere: positional, best-effort (custom prefix
  // schemes).
  if (parts.length >= 5) {
    return known(
      parts[parts.length - 3]!,
      parts[parts.length - 2]!,
      "",
      epoch,
      segIndex
    );
  }
  return { layout: "unknown", rawKey: key, epoch, segIndex };
}

/**
 * Best-effort prefix inferred from the versioned layout or a historical
 * suffix. Returns "" when neither is found.
 */
export function extractPrefix(key: string): string {
  const parts = key.split("/");
  const version1 = version1Layout(parts);
  if (version1) return parts.slice(0, version1.start).join("/");
  const historical = historicalLayout(parts);
  if (historical) return parts.slice(0, historical.start).join("/");
  return "";
}

const TIME_RE = /^\d{4}$/;

interface HistoricalLayout {
  start: number;
  hasBoot: boolean;
}

function historicalLayout(parts: string[]): HistoricalLayout | null {
  for (const hasBoot of [true, false]) {
    const width = hasBoot ? 6 : 5;
    if (parts.length < width) continue;
    const start = parts.length - width;
    if (DATE_RE.test(parts[start]!) && TIME_RE.test(parts[start + 1]!)) {
      return { start, hasBoot };
    }
  }
  return null;
}

interface Version1Layout {
  start: number;
  date: string | null | undefined;
  time: string | null | undefined;
  service: string | null | undefined;
  instance: string | null | undefined;
  boot: string | null | undefined;
}

function version1Layout(parts: string[]): Version1Layout | null {
  if (parts.length < 7) return null;
  const start = parts.length - 7;
  if (parts[start] !== "version=1") return null;
  return {
    start,
    date: decodePartition(parts[start + 1]!, "date"),
    service: decodePartition(parts[start + 2]!, "service"),
    time: decodePartition(parts[start + 3]!, "time"),
    instance: decodePartition(parts[start + 4]!, "instance"),
    boot: decodePartition(parts[start + 5]!, "boot"),
  };
}

function decodePartition(segment: string, name: string): string | null {
  const prefix = `${name}=`;
  return segment.startsWith(prefix)
    ? decodePartitionValue(segment.slice(prefix.length))
    : null;
}

function decodePartitionValue(value: string): string | null {
  try {
    return decodeURIComponent(value);
  } catch {
    return null;
  }
}

/** Formatting options shared by `formatEpoch` / `traceTitleParams`. */
export interface EpochFormatOptions {
  /**
   * Render in the browser's local timezone instead of UTC (default false /
   * UTC). Pages pass their live preference here.
   */
  localTz?: boolean;
}

/**
 * Format a unix-seconds epoch as "YYYY-MM-DD HH:MM:SS" (UTC by default,
 * local time with `localTz`). Returns "" for 0/missing epochs - the
 * "filename didn't carry an epoch" case, not a hidden error.
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
