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
  worker_id: 3,
  task_id: 42,
};

function viewerParams(link: string): URLSearchParams {
  return new URLSearchParams(link.slice("viewer.html?".length));
}

function traceBucket(link: string): string | null {
  const trace = viewerParams(link).get("trace") ?? "";
  return new URLSearchParams(trace.slice("/api/object?".length)).get("bucket");
}

describe("exemplarLink", () => {
  it("builds a non-destructive focus link to an /api/object trace component", () => {
    const link = exemplarLink(EX, "demo-traces", null);
    expect(link.startsWith("viewer.html?")).toBe(true);
    const p = viewerParams(link);

    const trace = p.get("trace") ?? "";
    expect(trace.startsWith("/api/object?")).toBe(true);
    const tq = new URLSearchParams(trace.slice("/api/object?".length));
    expect(tq.get("bucket")).toBe("demo-traces");
    expect(tq.get("key")).toBe(EX.source_key);

    expect(p.get("focus_start")).toBe("1000");
    expect(p.get("focus_end")).toBe("2000");
    expect(p.get("focus_worker")).toBe("3");
    expect(p.get("focus_task")).toBe("42");
    expect(p.get("host")).toBe("local");

    // Not the removed /api/trace endpoint or the destructive start/end filter.
    expect(trace.includes("/api/trace")).toBe(false);
    expect(p.get("start_ns")).toBeNull();
    expect(p.get("end_ns")).toBeNull();
  });

  it("omits worker/task focus for a spawn-loc exemplar", () => {
    const spawnLoc: PollExemplar = {
      start_ns: 500,
      end_ns: 900,
      duration_ns: 400,
      host: "h1",
      source_key: "traces/x/y.bin.gz",
    };
    const p = viewerParams(exemplarLink(spawnLoc, "b", null));
    expect(p.get("focus_start")).toBe("500");
    expect(p.get("focus_worker")).toBeNull();
    expect(p.get("focus_task")).toBeNull();
  });

  it("bucket precedence: response bucket wins, else the URL param, else empty", () => {
    expect(traceBucket(exemplarLink(EX, "resp-bucket", "url-bucket"))).toBe("resp-bucket");
    expect(traceBucket(exemplarLink(EX, "", "url-bucket"))).toBe("url-bucket");
    expect(traceBucket(exemplarLink(EX, null, null))).toBe("");
  });

  it("returns '' for a null exemplar or one with no start_ns (guard)", () => {
    expect(exemplarLink(null, "b", null)).toBe("");
    expect(exemplarLink(undefined, "b", null)).toBe("");
    expect(exemplarLink({ ...EX, start_ns: 0 }, "b", null)).toBe("");
  });
});
