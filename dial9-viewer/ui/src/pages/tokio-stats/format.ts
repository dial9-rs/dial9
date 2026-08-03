// The tokio-stats page's pure ns<->datetime + duration formatters. The UTC
// direction is an explicit parameter (not read from the DOM) so the conversions
// are Node-testable.

/**
 * Format an epoch-ns string as a `datetime-local` value (`YYYY-MM-DDTHH:MM:SS`).
 * Empty/absent bounds render blank. When `utc` is true the value is the UTC
 * wall-clock (ISO slice); otherwise it is shifted into local time so the native
 * picker shows local wall-clock.
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
 * precision.
 */
export function datetimeToNs(val: string, utc: boolean): string | null {
  if (!val) return null;
  const ms = utc ? new Date(val + "Z").getTime() : new Date(val).getTime();
  return String(Math.floor(ms * 1e6));
}

/**
 * Human duration with the unit ladder: s / ms / us (U+00B5 micro sign) / ns.
 * The threshold label and every P50/P99/Max cell render through this.
 */
export function formatDuration(ns: number): string {
  if (ns >= 1e9) return (ns / 1e9).toFixed(2) + "s";
  if (ns >= 1e6) return (ns / 1e6).toFixed(2) + "ms";
  if (ns >= 1e3) return (ns / 1e3).toFixed(0) + "µs";
  return ns + "ns";
}

/**
 * The threshold slider's log-scale mapping: value v in [-1, 3] -> 10^v ms in ns
 * (100us floor to 1s, default v=0 -> 1ms). The floor equals the server's 100us
 * duration floor, so the client can never ask below the wire.
 */
export function thresholdNs(sliderValue: string): number {
  return Math.pow(10, parseFloat(sliderValue)) * 1e6;
}

/**
 * Human-readable provenance for a runnable-to-poll scheduling measurement.
 * `undefined` (an unknown kind, e.g. from a future wire value) is presented as
 * "unknown evidence" rather than silently as a measured wake.
 */
export function schedulingDelayEvidenceLabel(kind: string | undefined): string {
  switch (kind) {
    case "spawn":
      return "spawn → first poll";
    case "wake":
      return "wake → poll";
    case "wake_during_poll":
      return "poll end → next poll";
    default:
      return "unknown evidence";
  }
}

