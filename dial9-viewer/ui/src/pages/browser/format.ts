// Pure display/format helpers for the browser page. They take the timezone
// mode as a parameter (rather than reading a global) so they stay pure -
// callers pass the live store value at render time.

/** Human byte size. */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  return (bytes / (1024 * 1024)).toFixed(1) + " MB";
}

/**
 * "YYYY-MM-DD HH:MM:SS" in the active TZ mode. Invalid input falls back to
 * the raw string; empty/missing input renders "".
 */
export function formatDate(dateStr: string | null | undefined, localTz: boolean): string {
  if (!dateStr) return "";
  try {
    const d = new Date(dateStr);
    if (localTz) {
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
  } catch {
    return dateStr;
  }
}

/** Epoch seconds -> formatDate; "" for 0/missing. */
export function formatEpochStr(epoch: number, localTz: boolean): string {
  if (!epoch) return "";
  return formatDate(new Date(epoch * 1000).toISOString(), localTz);
}

/**
 * Parse a browse object's `last_modified` into epoch seconds. The local-dir
 * backend sends numeric epoch seconds; S3 sends ISO-8601 strings. Returns 0
 * when absent/unparseable - the "no upload time" case, used as the mtime
 * fallback for local traces whose buffer-style key names carry no date/epoch.
 */
export function epochSeconds(lastModified: string | number | null | undefined): number {
  if (!lastModified) return 0;
  const n = Number(lastModified);
  if (!isNaN(n) && n > 0) return n;
  const ms = new Date(lastModified).getTime();
  return isNaN(ms) ? 0 : ms / 1000;
}

/** Axis/selection tick in the active TZ mode: HH:MM:SS by default; with
 * `withDate` the calendar date is prefixed ("YYYY-MM-DD HH:MM:SS") so ticks
 * on a day-crossing span stay unambiguous. The selection-count readout
 * keeps the time-only form.
 */
export function fmtTick(epoch: number, localTz: boolean, withDate = false): string {
  if (withDate) return formatEpochStr(epoch, localTz);
  const d = new Date(epoch * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return localTz
    ? pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds())
    : pad(d.getUTCHours()) + ":" + pad(d.getUTCMinutes()) + ":" + pad(d.getUTCSeconds());
}

/**
 * Whether [t0, t1] (epoch seconds) crosses a calendar-day boundary in the
 * active TZ mode - the trigger for date-carrying axis ticks. A span
 * strictly inside one calendar day keeps the compact HH:MM:SS ticks.
 */
export function crossesDayBoundary(t0: number, t1: number, localTz: boolean): boolean {
  const a = new Date(t0 * 1000);
  const b = new Date(t1 * 1000);
  if (localTz) {
    return (
      a.getFullYear() !== b.getFullYear() ||
      a.getMonth() !== b.getMonth() ||
      a.getDate() !== b.getDate()
    );
  }
  return (
    a.getUTCFullYear() !== b.getUTCFullYear() ||
    a.getUTCMonth() !== b.getUTCMonth() ||
    a.getUTCDate() !== b.getUTCDate()
  );
}

/** datetime-local picker string in the browser's local timezone. */
export function toLocalDatetime(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    d.getFullYear() +
    "-" +
    pad(d.getMonth() + 1) +
    "-" +
    pad(d.getDate()) +
    "T" +
    pad(d.getHours()) +
    ":" +
    pad(d.getMinutes())
  );
}

/** datetime-local picker string in UTC. */
export function toUTCDatetime(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    d.getUTCFullYear() +
    "-" +
    pad(d.getUTCMonth() + 1) +
    "-" +
    pad(d.getUTCDate()) +
    "T" +
    pad(d.getUTCHours()) +
    ":" +
    pad(d.getUTCMinutes())
  );
}

/** Format a Date for the datetime-local picker in the given TZ mode. */
export function dateToPickerStr(d: Date, localTz: boolean): string {
  return localTz ? toLocalDatetime(d) : toUTCDatetime(d);
}

/** Parse a datetime-local picker value into a Date using the TZ mode. */
export function pickerToDate(str: string, localTz: boolean): Date | null {
  if (!str) return null;
  if (localTz) return new Date(str);
  // Treat the picker value as UTC
  return new Date(str + "Z");
}

export function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}

/** Map an epoch-seconds time into pixel x over [tMin, tMax] and width W. */
export function timeToX(t: number, tMin: number, tMax: number, W: number): number {
  return ((t - tMin) / (tMax - tMin)) * W;
}

/** Inverse of timeToX. */
export function xToTime(x: number, tMin: number, tMax: number, W: number): number {
  return tMin + (x / W) * (tMax - tMin);
}
