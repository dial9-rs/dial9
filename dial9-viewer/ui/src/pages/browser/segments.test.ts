// segments.ts tests: the pure S3-object -> heatmap model pipeline.
// - toSegments: key layout drives the row grouping fields (known keys keep
//   parsed service/host/boot; unknown keys carry "" service and their raw
//   directory as host), while time placement stays layout-independent and
//   falls back to the upload mtime for keys with no filename epoch (#627).
// - toRows: upload-lag overlaps are tiled away rather than double-counted,
//   and only genuine missing coverage becomes a gap.
// - computeExtent: the union of every segment's normalized span.

import { describe, expect, it } from "vitest";
import {
  computeExtent,
  toRows,
  toSegments,
  unknownGroupPath,
} from "./segments.js";

// Default Hive-style layout.
const knownKey = (
  epoch: number,
  { service = "api", host = "host-1", boot = "boot-a", seg = 0 } = {},
) => `traces/version=1/date=2026-04-09/service=${service}/time=1910/instance=${host}/boot=${boot}/${epoch}-${seg}.bin.gz`;

// Six post-date components match no documented layout -> layout "unknown".
const unknownKey = (epoch: number, dir = "traces/2026-04-09/1900/demo/local/host-0/abcd") =>
  `${dir}/${epoch}-0.bin.gz`;

describe("unknownGroupPath", () => {
  it("groups by the key's raw directory path", () => {
    expect(unknownGroupPath("a/b/c/1744224000-0.bin.gz")).toBe("a/b/c");
  });

  it("falls back to the whole key when there is no directory", () => {
    expect(unknownGroupPath("buffer-0.bin")).toBe("buffer-0.bin");
  });
});

describe("toSegments", () => {
  it("empty input -> empty output", () => {
    expect(toSegments([])).toEqual([]);
  });

  it("known layout keeps the parsed service/host/boot labels", () => {
    const [s] = toSegments([
      { key: knownKey(1000, { service: "checkout", host: "us-east-1" }), size: 42, last_modified: "1060" },
    ]);
    expect(s).toMatchObject({
      layout: "known",
      service: "checkout",
      host: "us-east-1",
      bootId: "boot-a",
      start: 1000,
      end: 1060,
      size: 42,
    });
  });

  it("unknown layout carries the raw directory as host, never a guessed service", () => {
    const [s] = toSegments([{ key: unknownKey(1000), size: 1, last_modified: "1060" }]);
    expect(s).toMatchObject({
      layout: "unknown",
      service: "",
      host: "traces/2026-04-09/1900/demo/local/host-0/abcd",
      bootId: "",
      // The filename epoch is layout-independent: time placement is unchanged.
      start: 1000,
      end: 1060,
    });
  });

  it("a missing last_modified collapses end onto start", () => {
    const [s] = toSegments([{ key: knownKey(1000), size: 1 }]);
    expect(s).toMatchObject({ start: 1000, end: 1000 });
  });

  it("no filename epoch -> the upload mtime places the segment (#627)", () => {
    const [s] = toSegments([{ key: "buffer-0.bin", size: 1, last_modified: "1500" }]);
    expect(s).toMatchObject({ start: 1500, end: 1500 });
  });

  it("drops objects with no derivable start (no epoch, no mtime)", () => {
    expect(toSegments([{ key: "buffer-0.bin", size: 1 }])).toEqual([]);
  });
});

describe("toRows", () => {
  it("empty input -> no rows", () => {
    expect(toRows([])).toEqual([]);
  });

  it("unknown-layout keys group by directory, one row per directory", () => {
    const dirA = "traces/2026-04-09/1900/svc/local/host-0/abcd";
    const dirB = "traces/2026-04-09/1900/svc/local/host-1/abcd";
    const rows = toRows(
      toSegments([
        { key: unknownKey(1000, dirA), size: 1, last_modified: "1010" },
        { key: unknownKey(2000, dirA), size: 1, last_modified: "2010" },
        { key: unknownKey(1000, dirB), size: 1, last_modified: "1010" },
      ]),
    );
    expect(rows).toHaveLength(2);
    expect(rows.map((r) => r.host)).toEqual([dirA, dirB]);
    expect(rows[0]!.segments).toHaveLength(2);
    // No service is guessed for an unknown layout, so the label is bare.
    expect(rows[0]!.service).toBe("");
  });

  it("known-layout segments group by service/host, not by boot id", () => {
    const rows = toRows(
      toSegments([
        { key: knownKey(1000, { boot: "boot-a" }), size: 1, last_modified: "1010" },
        { key: knownKey(2000, { boot: "boot-b" }), size: 1, last_modified: "2010" },
      ]),
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]!.segments.map((s) => s.bootId)).toEqual(["boot-a", "boot-b"]);
  });

  it("upload-lag overlap is tiled away instead of double-counted at the seam", () => {
    // A's last_modified (1062) runs past B's start (1060) because of upload
    // lag; left as-is those 2s of bytes would be counted in both segments.
    const rows = toRows(
      toSegments([
        { key: knownKey(1000), size: 100, last_modified: "1062" },
        { key: knownKey(1060, { seg: 1 }), size: 100, last_modified: "1120" },
      ]),
    );
    const [row] = rows;
    expect(row!.tiled.map((s) => [s.start, s.end])).toEqual([
      [1000, 1060], // clamped to the next start
      [1060, 1120],
    ]);
    // Tiling is density-only: the untiled segments keep the true file
    // extent that selection and gap detection read.
    expect(row!.segments.map((s) => s.end)).toEqual([1062, 1120]);
    // An upload-lag overlap is not missing coverage.
    expect(row!.gaps).toEqual([]);
  });

  it("only genuine missing coverage becomes a gap", () => {
    const rows = toRows(
      toSegments([
        { key: knownKey(1000), size: 1, last_modified: "1010" },
        { key: knownKey(2000, { seg: 1 }), size: 1, last_modified: "2010" },
      ]),
    );
    expect(rows[0]!.gaps).toEqual([{ start: 1010, end: 2000 }]);
  });
});

describe("computeExtent", () => {
  it("spans the union of every segment's normalized span", () => {
    const extent = computeExtent(
      toSegments([
        { key: knownKey(2000, { host: "h2" }), size: 1, last_modified: "2500" },
        { key: knownKey(1000, { host: "h1" }), size: 1, last_modified: "1500" },
      ]),
    );
    expect(extent).toEqual({ tMin: 1000, tMax: 2500 });
  });

  it("a zero-width segment still gets a non-empty domain", () => {
    // segmentSpan floors the span at MIN_SEGMENT_SECONDS, so end > start holds
    // before computeExtent's own tMax<=tMin guard is ever consulted.
    const extent = computeExtent(toSegments([{ key: knownKey(1000), size: 1 }]));
    expect(extent).toEqual({ tMin: 1000, tMax: 1001 });
  });

  it("empty input degenerates rather than inverting the domain", () => {
    // The tMax<=tMin guard's only reachable input: with any segment present
    // segmentSpan already guarantees tMax > tMin. Callers gate on rows.length
    // before asking for an extent, so this shape never reaches a render.
    expect(computeExtent([])).toEqual({ tMin: Infinity, tMax: Infinity });
  });
});
