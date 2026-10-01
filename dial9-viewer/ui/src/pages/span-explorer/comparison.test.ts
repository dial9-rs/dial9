import { describe, expect, it } from "vitest";
import type { Coverage, SpanTypeStats } from "../../lib/trace/index.js";
import { retainMatchedHistogramPair } from "./comparison.js";

const coverage = (foldedSetId: string): Coverage => ({
  files_matched: 10,
  files_folded: 2,
  samples_folded: 200,
  total_bytes: 1000,
  hosts_matched: 1,
  hosts_folded: 1,
  folded_set_id: foldedSetId,
});

const spanType = (count: number): SpanTypeStats => ({
  span_type_uid: "record-metric",
  kind: "tracing",
  name: "RecordMetric",
  count,
  histogram: [{ lo_ns: 1_000, hi_ns: 2_000, count }],
  details_complete_count: count,
  partial_count: 0,
  exemplars: [],
  attribute_facets: [],
  attribute_keys_overflow: false,
});

describe("Span Explorer histogram pairs", () => {
  it("keeps the previous matched pair until both streams cover the new set", () => {
    const baselineA = spanType(10);
    const filteredA = spanType(4);
    const pairA = retainMatchedHistogramPair(
      null,
      baselineA,
      coverage("set-a"),
      filteredA,
      coverage("set-a"),
      true,
    );
    expect(pairA?.baseline).toEqual(baselineA.histogram);
    expect(pairA?.filtered).toEqual(filteredA.histogram);

    const baselineB = spanType(15);
    const stillA = retainMatchedHistogramPair(
      pairA,
      baselineB,
      coverage("set-b"),
      filteredA,
      coverage("set-a"),
      true,
    );
    expect(stillA).toBe(pairA);

    const filteredB = spanType(6);
    const pairB = retainMatchedHistogramPair(
      stillA,
      baselineB,
      coverage("set-b"),
      filteredB,
      coverage("set-b"),
      true,
    );
    expect(pairB?.baseline).toEqual(baselineB.histogram);
    expect(pairB?.filtered).toEqual(filteredB.histogram);
    expect(pairB?.foldedSetId).toBe("set-b");
  });

  it("requires equal set digests even when file counts agree", () => {
    const pair = retainMatchedHistogramPair(
      null,
      spanType(10),
      coverage("set-a"),
      spanType(4),
      coverage("set-b"),
      true,
    );
    expect(pair).toBeNull();
  });
});
