// The trace_scope seam re-exports the frozen scope codec unchanged; these
// scenarios are transcribed from the Node oracle (test_trace_scope.js) so the
// seam is pinned to the same values the landing page's view* builders rely on:
// a selection maps to a scope whose deep link stays under CloudFront's
// 8192-byte cap (#589) and carries the bucket region (#621).
//
// Imports the seam directly (not the lib/trace barrel) so the node test env
// does not pull the barrel's parser-loading modules.

import { describe, it, expect } from "vitest";
import {
  encodeAggregationParams,
  encodeScope,
  readScope,
  scopeFromKeys,
  type TraceScope,
} from "./trace_scope.js";

const CLOUDFRONT_URI_LIMIT = 8192;

const key = (host: string, epoch: number, i: number): string =>
  `traces/2026-06-29/1915/shale/${host}/abcd-boot/${epoch}-${i}.bin.gz`;

describe("scopeFromKeys", () => {
  it("derives service/hosts/prefix/window and threads the region", () => {
    const keys = [key("h1", 1782760100, 1), key("h2", 1782760200, 1)];
    const s = scopeFromKeys("bkt", keys, 1782760000, 1782760800, "us-west-2");
    expect(s).not.toBeNull();
    expect(s!.service).toBe("shale");
    expect([...s!.hosts].sort()).toEqual(["h1", "h2"]);
    expect(s!.prefix).toBe("traces");
    expect(s!.region).toBe("us-west-2");
    expect(s!.from).toBe(1782760000);
    expect(s!.to).toBe(1782760800);
  });

  it("derives the window from key epochs when none is supplied (raw mode)", () => {
    const s = scopeFromKeys("bkt", [key("h1", 1782760100, 1), key("h1", 1782760300, 2)], null, null);
    expect(s!.from).toBe(1782760100);
    expect(s!.to).toBe(1782760301);
    expect(s!.region).toBe("");
  });

  it("returns null with no window and no parseable epoch (unrecognized layout)", () => {
    expect(scopeFromKeys("bkt", ["custom/layout/no-epoch.dat"], null, null)).toBeNull();
  });
});

describe("encodeScope large-selection compaction (#589)", () => {
  it("stays under the CloudFront cap for thousands of files across 60 hosts", () => {
    const keys: string[] = [];
    for (let h = 0; h < 60; h++) {
      const host = `ip-10-2-${100 + h}-50.us-west-2.compute.internal`;
      for (let f = 0; f < 84; f++) keys.push(key(host, 1782760000 + f, f));
    }
    const s = scopeFromKeys("cell1-prod-pdx-dial9-traces", keys, 1782760000, 1782760800)!;
    const { query, hostsDropped } = encodeScope(new URLSearchParams(), s);
    expect(query.length).toBeLessThanOrEqual(CLOUDFRONT_URI_LIMIT);
    expect(hostsDropped).toBe(false);
    expect(readScope(new URLSearchParams(query))!.hosts.length).toBe(60);
  });

  it("degrades a pathological host set to time-range-only but stays URI-safe", () => {
    const hosts: string[] = [];
    for (let h = 0; h < 5000; h++) hosts.push(`ip-10-2-${h}.us-west-2.compute.internal`);
    const s: TraceScope = {
      bucket: "b",
      region: "",
      roleArn: "",
      prefix: "traces",
      service: "shale",
      hosts,
      from: 1782760000,
      to: 1782760800,
    };
    const { query, hostsDropped } = encodeScope(new URLSearchParams(), s);
    expect(query.length).toBeLessThanOrEqual(CLOUDFRONT_URI_LIMIT);
    expect(hostsDropped).toBe(true);
    const got = readScope(new URLSearchParams(query))!;
    expect(got.hosts).toEqual([]);
    expect(got.from).toBe(1782760000);
  });
});

describe("encodeAggregationParams region in deep-links (#621)", () => {
  it("emits the un-namespaced names, ns window, and aws_region", () => {
    const s = scopeFromKeys(
      "bkt",
      [key("h1", 1782760100, 1), key("h1", 1782760200, 2)],
      1782760000,
      1782760800,
      "us-west-2",
    )!;
    const base = new URLSearchParams();
    base.set("api", "1");
    const { query, hostsDropped } = encodeAggregationParams(base, s);
    const p = new URLSearchParams(query);
    expect(hostsDropped).toBe(false);
    expect(p.get("api")).toBe("1");
    expect(p.get("bucket")).toBe("bkt");
    expect(p.get("prefix")).toBe("traces");
    expect(p.get("service")).toBe("shale");
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("start_ns")).toBe("1782760000000000000");
    expect(p.get("end_ns")).toBe("1782760800000000000");
    expect(p.getAll("host")).toEqual(["h1"]);
    // Aggregation params are NOT namespaced (they go straight to the server).
    expect(p.get("s_bucket")).toBeNull();
  });

  it("degrades a huge host set but keeps the window + region, staying URI-safe", () => {
    const hosts: string[] = [];
    for (let h = 0; h < 5000; h++) hosts.push(`ip-10-2-${h}.us-west-2.compute.internal`);
    const s: TraceScope = {
      bucket: "b",
      region: "us-west-2",
      roleArn: "",
      prefix: "traces",
      service: "shale",
      hosts,
      from: 1782760000,
      to: 1782760800,
    };
    const { query, hostsDropped } = encodeAggregationParams(new URLSearchParams(), s);
    expect(query.length).toBeLessThanOrEqual(CLOUDFRONT_URI_LIMIT);
    expect(hostsDropped).toBe(true);
    const p = new URLSearchParams(query);
    expect(p.getAll("host")).toEqual([]);
    expect(p.get("start_ns")).toBe("1782760000000000000");
    expect(p.get("aws_region")).toBe("us-west-2");
  });
});
