// src/pages/tokio-stats/format.ts - the tokio-stats page's pure
// ns<->datetime + duration formatters, ported verbatim from the legacy
// inline script (tokio_stats.html:75-91). The legacy versions read the UTC
// checkbox through the DOM (isUtc()); here the UTC direction is an explicit
// parameter so the conversions are Node-testable and free of DOM coupling
// (features/04 B2/B3/C3).

/**
 * Format an epoch-ns string as a `datetime-local` value (`YYYY-MM-DDTHH:MM:SS`).
 * Empty/absent bounds render blank. When `utc` is true the value is the UTC
 * wall-clock (ISO slice); otherwise it is shifted into local time so the
 * native picker shows local wall-clock (features/04 B3).
 */
export function nsToDatetime(ns: string | null, utc: boolean): string {
  if (!ns) return "";
  const d = new Date(Number(ns) / 1e6);
  if (utc) return d.toISOString().slice(0, 19);
  return new Date(d.getTime() - d.getTimezoneOffset() * 60000)
    .toISOString()
    .slice(0, 19);
}

/**
 * Parse a `datetime-local` value back to an epoch-ns string (ms-precision
 * floor). Empty -> null. `utc` appends `Z` (parse as UTC); otherwise the
 * string is parsed as local time. Kept as a string end-to-end for >2^53
 * precision (features/04 A4/B2).
 */
export function datetimeToNs(val: string, utc: boolean): string | null {
  if (!val) return null;
  const ms = utc ? new Date(val + "Z").getTime() : new Date(val).getTime();
  return String(Math.floor(ms * 1e6));
}

/**
 * Human duration with the legacy unit ladder: s / ms / us (U+00B5 micro
 * sign) / ns (features/04 C3). The threshold label and every P50/P99/Max
 * cell render through this.
 */
export function formatDuration(ns: number): string {
  if (ns >= 1e9) return (ns / 1e9).toFixed(2) + "s";
  if (ns >= 1e6) return (ns / 1e6).toFixed(2) + "ms";
  if (ns >= 1e3) return (ns / 1e3).toFixed(0) + "µs";
  return ns + "ns";
}

/**
 * The threshold slider's log-scale mapping: value v in [-1, 3] -> 10^v ms in
 * ns (100us floor to 1s, default v=0 -> 1ms). The floor equals the server's
 * 100us duration floor, so the client can never ask below the wire
 * (features/04 C3).
 */
export function thresholdNs(sliderValue: string): number {
  return Math.pow(10, parseFloat(sliderValue)) * 1e6;
}
