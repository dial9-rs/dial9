// raw-rows.ts tests:
// - display: known-layout keys keep their parsed Service/Host/Boot columns;
//   unknown-layout keys render RAW (parsedCols null - the view shows the full
//   key across those columns) while still carrying the layout-independent
//   filename epoch/segIndex.
// - sorting: all columns sortable, numeric for epoch/size/seg#, lexical
//   otherwise; repeated header clicks flip the direction.

import { describe, expect, it } from "vitest";
import { nextSort, sortRawRows, toRawRow, toRawRows } from "./raw-rows.js";

const KNOWN_KEY =
  "traces/2026-04-09/1910/checkout-api/us-east-1/abcd-123213/1744224000-3.bin.gz";
// The dev-server demo key has six components after the date, so its layout is
// unknown rather than positionally mislabelled.
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
  it("orders by trace-start epoch ascending by default", () => {
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

describe("sorting (amendment)", () => {
  const keyFor = (svc: string, host: string, epoch: number, seg: number) =>
    `traces/2026-04-09/1910/${svc}/${host}/boot-a/${epoch}-${seg}.bin.gz`;
  const objects = [
    {
      key: keyFor("billing", "host2", 1744224600, 2),
      size: 300,
      last_modified: "2026-04-09T19:20:00Z",
    },
    {
      key: keyFor("api", "host9", 1744224000, 10),
      size: 100,
      last_modified: "2026-04-09T19:00:00Z",
    },
    {
      key: keyFor("checkout", "host1", 1744225200, 3),
      size: 200,
      last_modified: "2026-04-09T19:10:00Z",
    },
  ];
  const rows = toRawRows(objects);
  const services = (sorted: ReturnType<typeof sortRawRows>) =>
    sorted.map((r) => r.parsedCols!.service);

  it("null sort keeps the default epoch-ascending order", () => {
    expect(services(sortRawRows(rows, null))).toEqual(["api", "billing", "checkout"]);
  });

  it("lexical columns sort by string (service)", () => {
    expect(services(sortRawRows(rows, { key: "service", dir: 1 }))).toEqual([
      "api",
      "billing",
      "checkout",
    ]);
    expect(services(sortRawRows(rows, { key: "service", dir: -1 }))).toEqual([
      "checkout",
      "billing",
      "api",
    ]);
  });

  it("size sorts numerically", () => {
    expect(sortRawRows(rows, { key: "size", dir: 1 }).map((r) => r.obj.size)).toEqual([
      100, 200, 300,
    ]);
  });

  it("seg # sorts numerically, not lexically (10 > 3)", () => {
    expect(
      sortRawRows(rows, { key: "segIndex", dir: 1 }).map((r) => r.segIndex),
    ).toEqual(["2", "3", "10"]);
  });

  it("trace start sorts by epoch; descending flips it", () => {
    expect(
      sortRawRows(rows, { key: "traceStart", dir: -1 }).map((r) => r.epoch),
    ).toEqual([1744225200, 1744224600, 1744224000]);
  });

  it("uploaded sorts chronologically via the ISO string", () => {
    expect(
      sortRawRows(rows, { key: "last_modified", dir: 1 }).map(
        (r) => r.obj.last_modified,
      ),
    ).toEqual([
      "2026-04-09T19:00:00Z",
      "2026-04-09T19:10:00Z",
      "2026-04-09T19:20:00Z",
    ]);
  });

  it("unknown-layout rows sort by their raw key in the Service/Host/Boot columns", () => {
    const mixed = toRawRows([
      objects[1]!, // service "api"
      {
        // unknown layout (six post-date components)
        key: "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744224000-0.bin.gz",
        size: 50,
      },
    ]);
    const sorted = sortRawRows(mixed, { key: "service", dir: 1 });
    // "api" < "traces/..." lexically: the parsed row leads ascending.
    expect(sorted[0]!.parsedCols?.service).toBe("api");
    expect(sorted[1]!.parsedCols).toBeNull();
  });

  it("nextSort: first click asc, same column flips, new column resets to asc", () => {
    expect(nextSort(null, "size")).toEqual({ key: "size", dir: 1 });
    expect(nextSort({ key: "size", dir: 1 }, "size")).toEqual({ key: "size", dir: -1 });
    expect(nextSort({ key: "size", dir: -1 }, "size")).toEqual({ key: "size", dir: 1 });
    expect(nextSort({ key: "size", dir: -1 }, "host")).toEqual({ key: "host", dir: 1 });
  });
});
