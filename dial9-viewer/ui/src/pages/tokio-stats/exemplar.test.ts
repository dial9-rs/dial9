// Unit tests for the exemplar deep-link builder.

import { describe, it, expect } from "vitest";
import type { PollExemplar } from "../../lib/trace/index.js";
import { exemplarLink } from "./exemplar.js";

const EX: PollExemplar = {
  start_ns: 1000,
  end_ns: 2000,
  duration_ns: 1000,
  host: "local",
  source_key: "traces/2026-04-09/1900/demo-service/local/host-0/abcd/1744224000-0.bin.gz",
};

describe("exemplarLink", () => {
  it("builds a viewer deep link that PRESERVES the /api/trace target (defect carried)", () => {
    const link = exemplarLink(EX, "demo-traces", null);
    expect(link.startsWith("viewer.html?")).toBe(true);
    const params = new URLSearchParams(link.slice("viewer.html?".length));
    // The trace param is the /api/trace endpoint, which no longer exists, so
    // this link 404s at HEAD; it is preserved byte-for-byte anyway. The
    // source_key is encodeURIComponent'd inside the trace URL, so the
    // once-decoded trace param carries the escaped key.
    expect(params.get("trace")).toBe(
      `/api/trace?bucket=demo-traces&keys=${encodeURIComponent(EX.source_key)}`,
    );
    expect(params.get("start_ns")).toBe("1000");
    expect(params.get("end_ns")).toBe("2000");
  });

  it("bucket precedence: response bucket wins, else the URL param, else empty", () => {
    const fromData = exemplarLink(EX, "resp-bucket", "url-bucket");
    expect(new URLSearchParams(fromData.split("?")[1]).get("trace")).toContain(
      "bucket=resp-bucket",
    );
    // Empty/absent response bucket falls back to the URL param.
    const fromParam = exemplarLink(EX, "", "url-bucket");
    expect(new URLSearchParams(fromParam.split("?")[1]).get("trace")).toContain(
      "bucket=url-bucket",
    );
    const neither = exemplarLink(EX, null, null);
    expect(new URLSearchParams(neither.split("?")[1]).get("trace")).toContain(
      "bucket=&keys=",
    );
  });

  it("returns '' for a null exemplar or one with no start_ns (guard)", () => {
    expect(exemplarLink(null, "b", null)).toBe("");
    expect(exemplarLink(undefined, "b", null)).toBe("");
    expect(exemplarLink({ ...EX, start_ns: 0 }, "b", null)).toBe("");
  });
});
