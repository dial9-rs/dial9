#!/usr/bin/env node
"use strict";

// Unit tests for formatHumanDuration, formatHumanBytes, and formatFieldValue.
//
// formatHumanDuration takes a nanosecond value and returns a human-friendly
// string that picks a sensible unit (ns, µs, ms, s, m, h, d). This fixes the
// case where long traces show durations like "28808404.3ms" (8 hours) in the
// UI. formatFieldValue routes a field value through the right formatter based
// on its schema unit annotation.

const {
  formatHumanDuration,
  formatHumanBytes,
  formatFieldValue,
} = require("./format.js");

let failed = 0;
let passed = 0;

function assertEq(actual, expected, desc) {
  if (actual === expected) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

// Sub-microsecond → ns
assertEq(formatHumanDuration(0), "0ns", "zero");
assertEq(formatHumanDuration(500), "500ns", "500 ns");
assertEq(formatHumanDuration(999), "999ns", "999 ns");

// Microseconds
assertEq(formatHumanDuration(1_000), "1.0µs", "1 µs");
assertEq(formatHumanDuration(1_500), "1.5µs", "1.5 µs");
assertEq(formatHumanDuration(999_999), "1000.0µs", "just under 1 ms");

// Milliseconds
assertEq(formatHumanDuration(1_000_000), "1.00ms", "1 ms");
assertEq(formatHumanDuration(123_456_789), "123.46ms", "123 ms");
assertEq(formatHumanDuration(999_000_000), "999.00ms", "999 ms");

// Seconds
assertEq(formatHumanDuration(1_000_000_000), "1.00s", "1 s");
assertEq(formatHumanDuration(59_000_000_000), "59.00s", "59 s");

// Minutes (>= 60s)
assertEq(formatHumanDuration(60_000_000_000), "1m 0.0s", "60 s → 1m 0.0s");
assertEq(formatHumanDuration(90_000_000_000), "1m 30.0s", "90 s → 1m 30s");
assertEq(
  formatHumanDuration(3_599_000_000_000),
  "59m 59.0s",
  "just under 1 hour",
);

// Hours (>= 60 minutes)
assertEq(formatHumanDuration(3_600_000_000_000), "1h 0m 0s", "1 hour");
// The bug report case: 28,808,404.3 ms ≈ 8h 0m 8s
assertEq(
  formatHumanDuration(28_808_404_300_000),
  "8h 0m 8s",
  "8-hour trace from issue #200",
);

// Days (>= 24 hours)
assertEq(formatHumanDuration(86_400_000_000_000), "1d 0h 0m", "1 day");
assertEq(formatHumanDuration(90_000_000_000_000), "1d 1h 0m", "1d 1h");

// ── formatHumanBytes ──
assertEq(formatHumanBytes(0), "0 B", "zero bytes");
assertEq(formatHumanBytes(512), "512 B", "512 B");
assertEq(formatHumanBytes(1023), "1023 B", "just under 1 KiB");
assertEq(formatHumanBytes(1024), "1.00 KiB", "1 KiB");
assertEq(formatHumanBytes(1536), "1.50 KiB", "1.5 KiB");
assertEq(formatHumanBytes(1_048_576), "1.00 MiB", "1 MiB");
assertEq(
  formatHumanBytes(12_884_901_888),
  "12.00 GiB",
  "12 GiB RSS from issue #472",
);
assertEq(formatHumanBytes(2 ** 40), "1.00 TiB", "1 TiB");
assertEq(formatHumanBytes(2 ** 50), "1024.00 TiB", "caps at TiB");
assertEq(formatHumanBytes(-1), "0 B", "negative clamps to 0 B");

// ── formatFieldValue ──
assertEq(formatFieldValue(1_500_000, "ns"), "1.50ms", "ns unit");
assertEq(formatFieldValue(1_500, "us"), "1.50ms", "us unit");
assertEq(formatFieldValue(1.5, "ms"), "1.50ms", "ms unit");
assertEq(formatFieldValue(90, "s"), "1m 30.0s", "s unit");
assertEq(formatFieldValue(12_884_901_888, "bytes"), "12.00 GiB", "bytes unit");
// Only the canonical short forms are accepted; aliases render raw.
assertEq(formatFieldValue(1_500, "µs"), "1500", "mu-char µs is not accepted");
assertEq(formatFieldValue(512, "b"), "512", "b alias is not accepted");
// Decoded I64 fields arrive as BigInt and Varint fields as strings.
assertEq(formatFieldValue(1_500_000n, "ns"), "1.50ms", "BigInt value");
assertEq(formatFieldValue("1500000", "ns"), "1.50ms", "string value");
assertEq(
  formatFieldValue(12_884_901_888n, "bytes"),
  "12.00 GiB",
  "BigInt bytes",
);
// No or unknown unit falls back to String(value) — the pre-existing behavior.
assertEq(formatFieldValue(42), "42", "no unit");
assertEq(formatFieldValue(42, "furlongs"), "42", "unknown unit");
assertEq(formatFieldValue(42n, undefined), "42", "BigInt without unit");
assertEq(formatFieldValue("hello", undefined), "hello", "string without unit");

// ── Summary ──
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
