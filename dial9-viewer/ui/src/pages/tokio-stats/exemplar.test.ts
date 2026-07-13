// Unit tests for the exemplar deep-link builder (features/04 H1/H3/H4).

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

describe("exemplarLink (H1)", () => {
  it("builds a viewer deep link that PRESERVES the /api/trace target (H4 defect carried)", () => {
    const link = exemplarLink(EX, "demo-traces", null);
    expect(link.startsWith("viewer.html?")).toBe(true);
    const params = new URLSearchParams(link.slice("viewer.html?".length));
    // The trace param is the /api/trace endpoint - which #582 removed, so
    // this link 404s at HEAD. Preserved byte-for-byte (behavior-preserving
    // port; the fix-vs-preserve is a ledgered maintainer call). The
    // source_key is encodeURIComponent'd inside the trace URL (legacy
    // exemplarLink), so the once-decoded trace param carries the escaped key.
    expect(params.get("trace")).toBe(
      `/api/trace?bucket=demo-traces&keys=${encodeURIComponent(EX.source_key)}`,
    );
    expect(params.get("start_ns")).toBe("1000");
    expect(params.get("end_ns")).toBe("2000");
  });

  it("bucket precedence: response bucket wins, else the URL param, else empty (H1)", () => {
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

  it("returns '' for a null exemplar or one with no start_ns (H3 guard)", () => {
    expect(exemplarLink(null, "b", null)).toBe("");
    expect(exemplarLink(undefined, "b", null)).toBe("");
    expect(exemplarLink({ ...EX, start_ns: 0 }, "b", null)).toBe("");
  });
});
