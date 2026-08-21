import { describe, expect, it } from "vitest";
import {
  TIME_CATEGORIES,
  addAttrFilter,
  classifyExemplarSnapshot,
  collectExemplarAttributeKeys,
  columnIsDegenerate,
  completeExemplarRefresh,
  computeTimeComposition,
  countInBand,
  durationAtPercentile,
  exemplarAttrValue,
  exemplarRequestMatches,
  exemplarsInBand,
  flamegraphUrl,
  fmtNs,
  fmtPercentile,
  formatAttrFilterParams,
  hasAttrFilter,
  mergeSelectedExemplarSnapshot,
  normalizeSpanHistogram,
  parseAttrFilterParams,
  percentileForDuration,
  removeAttrFilter,
  setMaxFilesParam,
  shouldAdoptCatalogSnapshot,
  sortSpanTypes,
  spanBrushToBand,
  spanHistogramLayout,
  spanNsToPx,
  spanTypeLabel,
  spanTypeQuality,
  type HistogramBarLike,
  type SpanExplorerState,
} from "./span_explorer.js";

const HISTOGRAM: HistogramBarLike[] = [
  { lo_ns: 1_000_000, hi_ns: 2_000_000, count: 10 },
  { lo_ns: 2_000_000, hi_ns: 4_000_000, count: 10 },
  { lo_ns: 4_000_000, hi_ns: 8_000_000, count: 10 },
];

function pickParams(params: URLSearchParams, keys: readonly string[]) {
  return Object.fromEntries(keys.map((key) => [key, params.get(key)]));
}

describe("scope and catalog helpers", () => {
  it("sets or clears the requested refinement depth", () => {
    const params = new URLSearchParams("api=1");
    expect(setMaxFilesParam(params, 80)).toBe(params);
    expect(params.get("max_files")).toBe("80");
    setMaxFilesParam(params, null);
    expect(params.has("max_files")).toBe(false);
  });

  it("protects the catalog from partial refinement and exemplar streams", () => {
    expect(shouldAdoptCatalogSnapshot("replace", 80, 0)).toBe(true);
    expect(shouldAdoptCatalogSnapshot("exemplars", 80, 100)).toBe(false);
    expect(shouldAdoptCatalogSnapshot("refine", 80, 79)).toBe(false);
    expect(shouldAdoptCatalogSnapshot("refine", 80, 80)).toBe(true);
  });

  it("formats, labels, sorts, and reports detail coverage", () => {
    expect([fmtNs(500), fmtNs(1_500_000), fmtNs(2_000_000_000)])
      .toEqual(["500ns", "1.5ms", "2s"]);
    expect([fmtNs(null), fmtNs(-1), fmtNs(0)]).toEqual(["—", "—", "0"]);
    expect(
      spanTypeLabel({
        name: "handle_request",
        callsite_file: "src/handler.rs",
        callsite_line: 42,
      }),
    ).toBe("handle_request (handler.rs:42)");
    expect(spanTypeLabel({})).toBe("(unnamed)");

    const rows = [
      { name: "a", count: 10 },
      { name: "b", count: 50 },
      { name: "c", count: 5 },
    ];
    expect(sortSpanTypes(rows, "count").map((row) => row.name))
      .toEqual(["b", "a", "c"]);
    expect(sortSpanTypes(rows, "name", true).map((row) => row.name))
      .toEqual(["a", "b", "c"]);
    expect(spanTypeQuality({ details_complete_count: 80, partial_count: 20 }))
      .toBe(0.8);
    expect(spanTypeQuality({ details_complete_count: 0, partial_count: 0 }))
      .toBeNull();
  });
});

describe("histogram geometry and percentiles", () => {
  it("normalizes the wire bars and lays them out by count", () => {
    const bars = normalizeSpanHistogram([
      { lo_ns: "2000000", hi_ns: "4000000", count: "10" },
      { lo_ns: 1_000_000, hi_ns: 2_000_000, count: 20 },
      { lo_ns: 100, hi_ns: 50, count: 1 },
    ]);
    expect(bars).toHaveLength(2);
    expect(bars.map((bar) => bar.lo_ns)).toEqual([1_000_000, 2_000_000]);
    const layout = spanHistogramLayout(bars, 200, 2);
    expect(layout.maxCount).toBe(20);
    expect(layout.cols).toMatchObject([
      { x: 0, w: 98, hFrac: 1 },
      { x: 100, w: 98, hFrac: 0.5 },
    ]);
  });

  it("maps a real drag to a duration band and ignores clicks", () => {
    const band = spanBrushToBand(HISTOGRAM, 300, 25, 250);
    expect(band).not.toBeNull();
    expect(band!.min_ns).toBeGreaterThanOrEqual(1_000_000);
    expect(band!.max_ns).toBeLessThanOrEqual(8_000_000);
    expect(band!.min_ns).toBeLessThan(band!.max_ns!);
    expect(spanBrushToBand(HISTOGRAM, 300, 50, 51)).toBeNull();
  });

  it("keeps percentile and pixel projections mutually consistent", () => {
    expect(durationAtPercentile(HISTOGRAM, 0)).toBe(1_000_000);
    expect(durationAtPercentile(HISTOGRAM, 100)).toBe(8_000_000);
    const p50 = durationAtPercentile(HISTOGRAM, 50)!;
    expect(Math.abs(p50 - 2_000_000 * Math.SQRT2)).toBeLessThan(5);
    const p90 = durationAtPercentile(HISTOGRAM, 90)!;
    expect(percentileForDuration(HISTOGRAM, p90)).toBeCloseTo(90, 5);
    expect(spanNsToPx(HISTOGRAM, 300, p50)).toBeCloseTo(150, 4);
    expect(spanNsToPx(HISTOGRAM, 300, 500_000)).toBe(0);
    expect(spanNsToPx(HISTOGRAM, 300, 16_000_000)).toBe(300);
    expect(fmtPercentile(99.99)).toBe("p99.99");
  });

  it("counts every histogram bucket intersecting the selected band", () => {
    expect(countInBand(HISTOGRAM, null, null)).toBe(30);
    expect(countInBand(HISTOGRAM, 2_000_000, 4_000_000)).toBe(10);
    expect(countInBand(HISTOGRAM, 1_500_000, 3_000_000)).toBe(20);
    expect(countInBand(HISTOGRAM, 10_000_000, 20_000_000)).toBe(0);
  });
});

describe("time composition", () => {
  it("falls back to one unknown category without backend composition", () => {
    const composition = computeTimeComposition({ histogram: HISTOGRAM });
    expect(composition.total_ns).toBeGreaterThan(0);
    expect(composition.categories).toMatchObject([
      { key: "unknown", frac: 1 },
    ]);
  });

  it("uses equal instance weighting when fraction sums are available", () => {
    const composition = computeTimeComposition({
      composition: {
        on_cpu_ns: 15_000,
        blocked_ns: 0,
        async_wait_ns: 200_000_000,
        scheduler_delay_ns: 0,
        unknown_ns: 0,
        instance_count: 11,
        on_cpu_frac_sum: 10,
        blocked_frac_sum: 0,
        async_wait_frac_sum: 1,
        scheduler_delay_frac_sum: 0,
        unknown_frac_sum: 0,
      },
    });
    expect(composition.weighting).toBe("equal");
    expect(composition.categories.find((cat) => cat.key === "on_cpu")?.frac)
      .toBeCloseTo(10 / 11);
    expect(
      composition.categories.find((cat) => cat.key === "async_wait")?.frac,
    ).toBeCloseTo(1 / 11);
  });

  it("uses nanosecond weighting when fraction sums are unavailable", () => {
    const composition = computeTimeComposition({
      composition: {
        on_cpu_ns: 5_000,
        blocked_ns: 2_000,
        async_wait_ns: 1_000,
        scheduler_delay_ns: 500,
        unknown_ns: 1_500,
      },
    });
    expect(composition.weighting).toBe("time");
    expect(composition.total_ns).toBe(10_000);
    expect(composition.categories.find((cat) => cat.key === "on_cpu"))
      .toMatchObject({ ns: 5_000, frac: 0.5 });
    expect(composition.categories.find((cat) => cat.key === "unknown"))
      .toMatchObject({ ns: 1_500, frac: 0.15 });
  });

  it("scopes composition to overlapping duration buckets", () => {
    const spanType = {
      composition: {
        on_cpu_ns: 300,
        blocked_ns: 0,
        async_wait_ns: 1_000_000,
        scheduler_delay_ns: 0,
        unknown_ns: 0,
      },
      composition_histogram: [
        {
          lo_ns: 1_000,
          hi_ns: 2_000,
          on_cpu_ns: 300,
          blocked_ns: 0,
          async_wait_ns: 0,
          scheduler_delay_ns: 0,
          unknown_ns: 0,
        },
        {
          lo_ns: 1_000_000,
          hi_ns: 2_000_000,
          on_cpu_ns: 0,
          blocked_ns: 0,
          async_wait_ns: 1_000_000,
          scheduler_delay_ns: 0,
          unknown_ns: 0,
        },
      ],
    };
    const long = computeTimeComposition(spanType, {
      min_ns: 500_000,
      max_ns: 3_000_000,
    });
    expect(long.total_ns).toBe(1_000_000);
    expect(long.categories.find((cat) => cat.key === "async_wait")?.frac)
      .toBe(1);
    expect(TIME_CATEGORIES.map((cat) => cat.key)).toEqual([
      "on_cpu",
      "blocked",
      "async_wait",
      "sched_delay",
      "unknown",
    ]);
  });
});

describe("exemplar table helpers", () => {
  const exemplars = [
    {
      elapsed_ns: 50,
      attributes: [
        { key: "request_id", value: "r1" },
        { key: "status_code", value: "500" },
      ],
    },
    {
      elapsed_ns: 30,
      attributes: [
        { key: "status_code", value: "200" },
        { key: "region", value: "pdx" },
      ],
    },
    { elapsed_ns: 10, attributes: [] },
  ];

  it("collects stable attribute columns and reads their values", () => {
    expect(collectExemplarAttributeKeys(exemplars))
      .toEqual(["request_id", "status_code", "region"]);
    expect(exemplarAttrValue(exemplars[0], "status_code")).toBe("500");
    expect(exemplarAttrValue(exemplars[0], "region")).toBeNull();
  });

  it("detects uniform columns without hiding one-row tables", () => {
    expect(columnIsDegenerate([{ h: "a" }, { h: "a" }], (row) => row.h))
      .toBe(true);
    expect(columnIsDegenerate([{ h: "a" }, { h: "b" }], (row) => row.h))
      .toBe(false);
    expect(columnIsDegenerate([{ h: "a" }], (row) => row.h)).toBe(false);
  });

  it("filters exemplars with inclusive, open-ended bounds", () => {
    expect(exemplarsInBand(exemplars, 10, 30).map((ex) => ex.elapsed_ns))
      .toEqual([30, 10]);
    expect(exemplarsInBand(exemplars, 31, null).map((ex) => ex.elapsed_ns))
      .toEqual([50]);
  });

  it("round-trips repeated key=value filters without mutating the input", () => {
    const parsed = parseAttrFilterParams([
      "status_code=500",
      "url=/a?b=c",
      "malformed",
      "=empty-key",
    ]);
    expect(parsed).toEqual([
      { key: "status_code", value: "500" },
      { key: "url", value: "/a?b=c" },
    ]);
    expect(formatAttrFilterParams(parsed))
      .toEqual(["status_code=500", "url=/a?b=c"]);
    expect(hasAttrFilter(parsed, "status_code", "500")).toBe(true);
    expect(addAttrFilter(parsed, "host", "h1")).toHaveLength(3);
    expect(addAttrFilter(parsed, "status_code", "500")).toHaveLength(2);
    expect(removeAttrFilter(parsed, "status_code", "500"))
      .toEqual([{ key: "url", value: "/a?b=c" }]);
    expect(parsed).toHaveLength(2);
  });
});

describe("exemplar refresh state", () => {
  interface CatalogRow {
    span_type_uid: string;
    count: number;
    exemplars: { elapsed_ns: number }[];
    selected_duration_count?: number;
  }

  const catalog: CatalogRow[] = [
    { span_type_uid: "selected", count: 100, exemplars: [{ elapsed_ns: 900 }] },
    { span_type_uid: "other", count: 50, exemplars: [{ elapsed_ns: 500 }] },
  ];

  it("patches only the selected row's query-scoped fields", () => {
    const merged = mergeSelectedExemplarSnapshot(
      catalog,
      [{
        span_type_uid: "selected",
        count: 3,
        exemplars: [{ elapsed_ns: 25 }],
        selected_duration_count: 3,
      }],
      "selected",
    );
    expect(merged.matched).toBe(true);
    expect(merged.spanTypes[0]).toMatchObject({
      count: 100,
      exemplars: [{ elapsed_ns: 25 }],
      selected_duration_count: 3,
    });
    expect(merged.spanTypes[1]).toBe(catalog[1]);

    const absent = mergeSelectedExemplarSnapshot(catalog, [], "missing");
    expect(absent.matched).toBe(false);
    expect(absent.spanTypes).toBe(catalog);
  });

  it("distinguishes preview snapshots from cache-complete snapshots", () => {
    expect(classifyExemplarSnapshot("catalog", "partial", "catalog"))
      .toEqual({ preview: true, complete: false });
    expect(classifyExemplarSnapshot("catalog", "catalog", "catalog"))
      .toEqual({ preview: true, complete: true });
    expect(classifyExemplarSnapshot("catalog", "other", "other-target"))
      .toEqual({ preview: false, complete: false });
  });

  it("keeps catalog data while tracking completion and stale responses", () => {
    const coverage = { files_folded: 100 };
    expect(completeExemplarRefresh(catalog, coverage, true))
      .toEqual({ spanTypes: catalog, coverage, pending: false });
    expect(completeExemplarRefresh(catalog, coverage, false).pending).toBe(true);
    expect(exemplarRequestMatches("A", "10:20", "A", "10:20")).toBe(true);
    expect(exemplarRequestMatches("A", "10:20", "A", ":")).toBe(false);
  });
});

describe("visualization deep links", () => {
  const state: SpanExplorerState = {
    data_dir: "/tmp/dial9-traces",
    max_files: 80,
    bucket: "bkt",
    region: "us-west-2",
    credentialMode: "role",
    roleArn: "arn:aws:iam::123:role/Dial9",
    prefix: "traces",
    service: "svc",
    hosts: ["h1"],
    start_ns: "100",
    end_ns: "200",
    span_type_uid: "aabb",
    min_span_ns: 1_000,
    max_span_ns: 5_000,
  };

  it("builds CPU and blocked flamegraphs with the full selected scope", () => {
    const cpu = new URL(flamegraphUrl(state, "cpu"), "https://viewer.test");
    expect(cpu.pathname).toBe("/flamegraph.html");
    expect(pickParams(
      cpu.searchParams,
      [
        "data_dir", "max_files", "bucket", "aws_region", "credential_mode",
        "aws_role_arn", "prefix", "service", "start_ns", "end_ns",
        "span_type_uid", "min_span_ns", "max_span_ns", "source",
      ],
    )).toEqual({
      data_dir: "/tmp/dial9-traces",
      max_files: "80",
      bucket: "bkt",
      aws_region: "us-west-2",
      credential_mode: "role",
      aws_role_arn: "arn:aws:iam::123:role/Dial9",
      prefix: "traces",
      service: "svc",
      start_ns: "100",
      end_ns: "200",
      span_type_uid: "aabb",
      min_span_ns: "1000",
      max_span_ns: "5000",
      source: "cpu",
    });
    expect(cpu.searchParams.getAll("host")).toEqual(["h1"]);
    const blocked = new URL(flamegraphUrl(state, "blocking"), cpu);
    expect(blocked.searchParams.get("source")).toBe("sched");
  });

});
