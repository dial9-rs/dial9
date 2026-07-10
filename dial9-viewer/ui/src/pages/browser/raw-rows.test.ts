// raw-rows.ts tests (T15, I2 amendment): known-layout keys keep their
// parsed Service/Host/Boot columns; unknown-layout keys render RAW
// (parsedCols null - the view shows the full key across those columns)
// while still carrying the layout-independent filename epoch/segIndex.

import { describe, expect, it } from "vitest";
import { toRawRow, toRawRows } from "./raw-rows.js";

const KNOWN_KEY =
  "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz";
// The dev-server demo key: six components after the date -> unknown layout
// (features/01 Finding 1; the legacy page mislabeled it Service=host-0).
const UNKNOWN_KEY =
  "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744224600-0.bin.gz";

describe("toRawRow", () => {
  it("known layout: parsed columns + filename epoch/segIndex", () => {
    const row = toRawRow({ key: KNOWN_KEY, size: 10 });
    expect(row.parsedCols).toEqual({
      service: "checkout-api",
      host: "us-east-1",
      bootId: "abcd-123213",
    });
    expect(row.epoch).toBe(1744224000);
    expect(row.segIndex).toBe("3");
  });

  it("unknown layout: no parsed columns (raw display), epoch/segIndex kept", () => {
    const row = toRawRow({ key: UNKNOWN_KEY, size: 10 });
    expect(row.parsedCols).toBeNull();
    expect(row.epoch).toBe(1744224600);
    expect(row.segIndex).toBe("0");
  });
});

describe("toRawRows", () => {
  it("orders by trace-start epoch ascending (legacy G3 default)", () => {
    const rows = toRawRows([
      { key: UNKNOWN_KEY, size: 1 }, // epoch 1744224600
      { key: KNOWN_KEY, size: 2 }, // epoch 1744224000
    ]);
    expect(rows.map((r) => r.obj.key)).toEqual([KNOWN_KEY, UNKNOWN_KEY]);
  });

  it("keys without a filename epoch sort first (epoch 0)", () => {
    const rows = toRawRows([
      { key: KNOWN_KEY, size: 2 },
      { key: "some/file.bin", size: 1 },
    ]);
    expect(rows[0]!.obj.key).toBe("some/file.bin");
  });
});
