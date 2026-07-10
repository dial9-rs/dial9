// Pure display/format helpers for the browser page (T14), ported verbatim
// from the legacy index.html inline script (features/01 I1, D7, F10). The
// legacy versions read the page-global `useLocalTz` flag; these take the
// timezone mode as a parameter so they stay pure - callers pass the live
// store value at render time.

/** Human byte size, exactly as the legacy `formatSize` (index.html). */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return bytes + " B";
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + " KB";
  return (bytes / (1024 * 1024)).toFixed(1) + " MB";
}

/**
 * "YYYY-MM-DD HH:MM:SS" in the active TZ mode. Invalid input falls back to
 * the raw string (legacy `formatDate` catch branch); empty/missing input
 * renders "".
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

/** Epoch seconds -> formatDate; "" for 0/missing (legacy `formatEpoch`). */
export function formatEpochStr(epoch: number, localTz: boolean): string {
  if (!epoch) return "";
  return formatDate(new Date(epoch * 1000).toISOString(), localTz);
}

/** HH:MM:SS axis/selection tick in the active TZ mode (legacy `fmtTick`).
 * Time-of-day only - the day-crossing ambiguity is a recorded finding
 * (features/01 Finding 3) whose fix is T15's, not this port's. */
export function fmtTick(epoch: number, localTz: boolean): string {
  const d = new Date(epoch * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return localTz
    ? pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds())
    : pad(d.getUTCHours()) + ":" + pad(d.getUTCMinutes()) + ":" + pad(d.getUTCSeconds());
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
