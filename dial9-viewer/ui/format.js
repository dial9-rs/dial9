"use strict";

// The sub-minute unit ladder, ascending. The unit is picked so the mantissa
// lands in [1, 1000), which is what makes the unit itself carry the magnitude:
// 100ns reads "100ns", never "0.1µs". Picoseconds are the floor - divided
// durations (per-poll averages, rates) can land below a nanosecond.
const HUMAN_DURATION_UNITS = [
  { div: 1e-3, suffix: "ps" },
  { div: 1, suffix: "ns" },
  { div: 1e3, suffix: "µs" },
  { div: 1e6, suffix: "ms" },
  { div: 1e9, suffix: "s" },
];

// 3 significant digits, at most 2 decimals: 2 below 10, 1 below 100, none
// above. `Number(...)` drops trailing zeros so a round value reads "1µs", not
// "1.00µs".
function humanSignificand(mantissa) {
  const decimals = mantissa >= 100 ? 0 : mantissa >= 10 ? 1 : 2;
  return String(Number(mantissa.toFixed(decimals)));
}

// The sub-minute reading, or null when it belongs to the composite branch:
// either the value is already a minute or more, or rounding carried the
// seconds reading onto the boundary (59.96s must not print "60s" right next to
// a real minute printing "1m 0.0s").
function subMinuteDuration(ns) {
  if (ns >= 60e9) return null;
  let i = 0;
  while (i < HUMAN_DURATION_UNITS.length - 1 && ns >= HUMAN_DURATION_UNITS[i + 1].div) i++;
  let mantissa = humanSignificand(ns / HUMAN_DURATION_UNITS[i].div);
  // Rounding can carry the mantissa up to 1000 (999.6ns): promote a unit
  // rather than print a 4-digit reading.
  if (Number(mantissa) >= 1000 && i < HUMAN_DURATION_UNITS.length - 1) {
    i++;
    mantissa = humanSignificand(ns / HUMAN_DURATION_UNITS[i].div);
  }
  const unit = HUMAN_DURATION_UNITS[i];
  if (unit.suffix === "s" && Number(mantissa) >= 60) return null;
  return mantissa + unit.suffix;
}

// Format a duration in nanoseconds as a human-friendly string with a sensible
// unit: "100ps", "500ns", "1.5µs", "123ms", "30s", "5m 12.0s", "8h 0m 8s",
// "2d 4h 30m".
//
// Sub-minute values carry 3 significant digits (at most 2 decimals) in the unit
// that keeps the mantissa under 1000, so the reading is always scannable and
// the unit tells you the scale at a glance. At a minute and above the composite
// m/h/d form takes over, where the leading unit already reads that way.
//
// This is the viewer's ONE duration format: the time-lane ruler, tooltips,
// inspector rows, flamegraph axes and the tokio-stats tables all route here.
function formatHumanDuration(ns) {
  if (!(ns > 0) || !isFinite(ns)) return "0ns";

  const reading = subMinuteDuration(ns);
  if (reading !== null) return reading;

  // A value whose seconds reading rounded up to a minute is formatted as the
  // minute it rounded to; using the raw 59.96s here would read "0m 60.0s".
  const totalSec = Math.max(ns, 60e9) / 1e9;
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
