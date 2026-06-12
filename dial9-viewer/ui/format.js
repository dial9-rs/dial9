"use strict";

// Format a duration in nanoseconds as a human-friendly string with a sensible
// unit. Chosen to read naturally at any scale: "500ns", "1.5µs", "123.46ms",
// "30.00s", "5m 12.0s", "8h 0m 8s", "2d 4h 30m".
//
// Used in the viewer header and anywhere else we want a compact, readable
// duration regardless of magnitude.
function formatHumanDuration(ns) {
  if (!isFinite(ns) || ns < 0) return "0ns";
  if (ns < 1_000) return `${Math.round(ns)}ns`;
  if (ns < 1_000_000) return `${(ns / 1_000).toFixed(1)}µs`;
  if (ns < 1_000_000_000) return `${(ns / 1_000_000).toFixed(2)}ms`;

  const totalSec = ns / 1e9;
  if (totalSec < 60) return `${totalSec.toFixed(2)}s`;

  const totalMin = Math.floor(totalSec / 60);
  const sec = totalSec - totalMin * 60;
  if (totalMin < 60) return `${totalMin}m ${sec.toFixed(1)}s`;

  const totalHr = Math.floor(totalMin / 60);
  const min = totalMin - totalHr * 60;
  if (totalHr < 24) return `${totalHr}h ${min}m ${Math.floor(sec)}s`;

  const days = Math.floor(totalHr / 24);
  const hr = totalHr - days * 24;
  return `${days}d ${hr}h ${min}m`;
}

// Format a byte count as a human-friendly string using binary units
// (conventional for memory sizes like RSS): "512 B", "1.50 KiB", "12.00 GiB".
function formatHumanBytes(bytes) {
  if (!isFinite(bytes) || bytes < 0) return "0 B";
  if (bytes < 1024) return `${Math.round(bytes)} B`;

  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i++;
  }
  return `${value.toFixed(2)} ${units[i]}`;
}

// Format a field value according to its schema unit annotation.
// Unknown or missing units fall back to String(value),
// matching how unannotated fields have always rendered.
//
// The accepted set must stay in sync with SUPPORTED_UNITS in
// dial9-trace-format-derive (which validates `#[traceevent(unit = "...")]`
// at compile time).
function formatFieldValue(value, unit) {
  switch (unit) {
    case "ns":
      return formatHumanDuration(Number(value));
    case "us":
      return formatHumanDuration(Number(value) * 1e3);
    case "ms":
      return formatHumanDuration(Number(value) * 1e6);
    case "s":
      return formatHumanDuration(Number(value) * 1e9);
    case "bytes":
      return formatHumanBytes(Number(value));
    default:
      return String(value);
  }
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = { formatHumanDuration, formatHumanBytes, formatFieldValue };
}
