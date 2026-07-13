// bucket-filter.ts tests (T15, C6 amendment): the config-driven predicate
// that replaces the legacy hardcoded "dial9" bucket filter (Finding 2).

import { describe, expect, it } from "vitest";
import {
  DEFAULT_BUCKET_FILTER,
  bucketMatchesFilter,
  resolveBucketFilter,
} from "./bucket-filter.js";

describe("bucketMatchesFilter", () => {
  it("matches case-insensitive substrings (legacy predicate shape)", () => {
    expect(bucketMatchesFilter("acme-dial9-traces", "dial9")).toBe(true);
    expect(bucketMatchesFilter("ACME-DIAL9", "dial9")).toBe(true);
    expect(bucketMatchesFilter("acme-traces", "dial9")).toBe(false);
    expect(bucketMatchesFilter("demo-traces", "DEMO")).toBe(true);
  });

  it("an empty filter matches every bucket (filtering disabled)", () => {
    expect(bucketMatchesFilter("anything", "")).toBe(true);
    expect(bucketMatchesFilter("", "")).toBe(true);
  });
});

describe("resolveBucketFilter", () => {
  it("defaults to 'dial9' with no override param", () => {
    expect(resolveBucketFilter("")).toEqual({
      filter: DEFAULT_BUCKET_FILTER,
      override: null,
    });
    expect(resolveBucketFilter("?bucket=b&prefix=p")).toEqual({
      filter: "dial9",
      override: null,
    });
  });

  it("a bucket_filter= param overrides, including the empty string", () => {
    expect(resolveBucketFilter("?bucket_filter=demo")).toEqual({
      filter: "demo",
      override: "demo",
    });
    // Empty override = explicit "no filtering", distinct from absence.
    expect(resolveBucketFilter("?bucket_filter=")).toEqual({
      filter: "",
      override: "",
    });
  });

  it("decodes percent-encoded values", () => {
    expect(resolveBucketFilter("?bucket_filter=my%20team").filter).toBe("my team");
  });
});
