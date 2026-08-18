// Unit tests for formatHumanDuration, formatHumanBytes, and formatFieldValue.
//
// formatHumanDuration takes a nanosecond value and returns a human-friendly
// string that picks a sensible unit (ns, µs, ms, s, m, h, d). This fixes the
// case where long traces show durations like "28808404.3ms" (8 hours) in the
// UI. formatFieldValue routes a field value through the right formatter based
// on its schema unit annotation.
//
// The frozen core is loaded through Node's native CJS loader because its
// guarded module.exports form cannot be statically analyzed by ESM named-import
// interop.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

const { formatHumanDuration, formatHumanBytes, formatFieldValue } =
  require("../../format.js") as {
    formatHumanDuration: (ns: number) => string;
    formatHumanBytes: (bytes: number) => string;
    formatFieldValue: (value: number | bigint | string, unit?: string) => string;
  };

describe("formatHumanDuration", () => {
  // Sub-microsecond -> ns
  it("zero", () => {
    expect(formatHumanDuration(0)).toBe("0ns");
  });
  it("500 ns", () => {
    expect(formatHumanDuration(500)).toBe("500ns");
  });
  it("999 ns", () => {
    expect(formatHumanDuration(999)).toBe("999ns");
  });

  // Microseconds
  it("1 µs", () => {
    expect(formatHumanDuration(1_000)).toBe("1.0µs");
  });
  it("1.5 µs", () => {
    expect(formatHumanDuration(1_500)).toBe("1.5µs");
  });
  it("just under 1 ms", () => {
    expect(formatHumanDuration(999_999)).toBe("1000.0µs");
  });

  // Milliseconds
  it("1 ms", () => {
    expect(formatHumanDuration(1_000_000)).toBe("1.00ms");
  });
  it("123 ms", () => {
    expect(formatHumanDuration(123_456_789)).toBe("123.46ms");
  });
  it("999 ms", () => {
    expect(formatHumanDuration(999_000_000)).toBe("999.00ms");
  });

  // Seconds
  it("1 s", () => {
    expect(formatHumanDuration(1_000_000_000)).toBe("1.00s");
  });
  it("59 s", () => {
    expect(formatHumanDuration(59_000_000_000)).toBe("59.00s");
  });

  // Minutes (>= 60s)
  it("60 s -> 1m 0.0s", () => {
    expect(formatHumanDuration(60_000_000_000)).toBe("1m 0.0s");
  });
  it("90 s -> 1m 30s", () => {
    expect(formatHumanDuration(90_000_000_000)).toBe("1m 30.0s");
  });
  it("just under 1 hour", () => {
    expect(formatHumanDuration(3_599_000_000_000)).toBe("59m 59.0s");
  });

  // Hours (>= 60 minutes)
  it("1 hour", () => {
    expect(formatHumanDuration(3_600_000_000_000)).toBe("1h 0m 0s");
  });
  // The bug report case: 28,808,404.3 ms ~= 8h 0m 8s
  it("8-hour trace from issue #200", () => {
    expect(formatHumanDuration(28_808_404_300_000)).toBe("8h 0m 8s");
  });

  // Days (>= 24 hours)
  it("1 day", () => {
    expect(formatHumanDuration(86_400_000_000_000)).toBe("1d 0h 0m");
  });
  it("1d 1h", () => {
    expect(formatHumanDuration(90_000_000_000_000)).toBe("1d 1h 0m");
  });
});

describe("formatHumanBytes", () => {
  it("zero bytes", () => {
    expect(formatHumanBytes(0)).toBe("0 B");
  });
  it("512 B", () => {
    expect(formatHumanBytes(512)).toBe("512 B");
  });
  it("just under 1 KiB", () => {
    expect(formatHumanBytes(1023)).toBe("1023 B");
  });
  it("1 KiB", () => {
    expect(formatHumanBytes(1024)).toBe("1.00 KiB");
  });
  it("1.5 KiB", () => {
    expect(formatHumanBytes(1536)).toBe("1.50 KiB");
  });
  it("1 MiB", () => {
    expect(formatHumanBytes(1_048_576)).toBe("1.00 MiB");
  });
  it("12 GiB RSS from issue #472", () => {
    expect(formatHumanBytes(12_884_901_888)).toBe("12.00 GiB");
  });
  it("1 TiB", () => {
    expect(formatHumanBytes(2 ** 40)).toBe("1.00 TiB");
  });
  it("caps at TiB", () => {
    expect(formatHumanBytes(2 ** 50)).toBe("1024.00 TiB");
  });
  it("negative clamps to 0 B", () => {
    expect(formatHumanBytes(-1)).toBe("0 B");
  });
});

describe("formatFieldValue", () => {
  it("ns unit", () => {
    expect(formatFieldValue(1_500_000, "ns")).toBe("1.50ms");
  });
  it("us unit", () => {
    expect(formatFieldValue(1_500, "us")).toBe("1.50ms");
  });
  it("ms unit", () => {
    expect(formatFieldValue(1.5, "ms")).toBe("1.50ms");
  });
  it("s unit", () => {
    expect(formatFieldValue(90, "s")).toBe("1m 30.0s");
  });
  it("bytes unit", () => {
    expect(formatFieldValue(12_884_901_888, "bytes")).toBe("12.00 GiB");
  });

  // Only the canonical short forms are accepted; aliases render raw.
  it("mu-char µs is not accepted", () => {
    expect(formatFieldValue(1_500, "µs")).toBe("1500");
  });
  it("b alias is not accepted", () => {
    expect(formatFieldValue(512, "b")).toBe("512");
  });

  // Decoded I64 fields arrive as BigInt and Varint fields as strings.
  it("BigInt value", () => {
    expect(formatFieldValue(1_500_000n, "ns")).toBe("1.50ms");
  });
  it("string value", () => {
    expect(formatFieldValue("1500000", "ns")).toBe("1.50ms");
  });
  it("BigInt bytes", () => {
    expect(formatFieldValue(12_884_901_888n, "bytes")).toBe("12.00 GiB");
  });

  // No or unknown unit falls back to String(value) - the pre-existing behavior.
  it("no unit", () => {
    expect(formatFieldValue(42)).toBe("42");
  });
  it("unknown unit", () => {
    expect(formatFieldValue(42, "furlongs")).toBe("42");
  });
  it("BigInt without unit", () => {
    expect(formatFieldValue(42n, undefined)).toBe("42");
  });
  it("string without unit", () => {
    expect(formatFieldValue("hello", undefined)).toBe("hello");
  });
});
