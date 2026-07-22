// Unit tests for the flamegraph page's pure URL/label logic. The api-mode
// expectations are transcribed from the legacy inline bootstrap
// (flamegraph.html buildApiUrl/updateBrowserUrl) - parameter ORDER included,
// since the browser URL is user-visible shared state.

import { describe, it, expect } from "vitest";
import {
  buildApiUrl,
  buildBrowserQuery,
  loadingLabel,
  resolveTraceUrls,
  seedFacetState,
  type ApiQueryState,
} from "./query.js";

const ORIGIN = "http://localhost:3051";

function state(over: Partial<ApiQueryState> = {}): ApiQueryState {
  return {
    dataDir: null,
    bucket: "demo-traces",
    prefix: "traces",
    service: null,
    hosts: [],
    facets: { source: "cpu", thread_class: "", spawn_location: "" },
    startNs: null,
    endNs: null,
    minPollNs: null,
    maxPollNs: null,
    maxFiles: null,
    spanTypeUid: null,
    minSpanNs: null,
    maxSpanNs: null,
    ...over,
  };
}

describe("resolveTraceUrls", () => {
  it("resolves relative values against the origin root (not the /new/ page path)", () => {
    expect(resolveTraceUrls(["demo-trace.bin"], ORIGIN)).toEqual([
      `${ORIGIN}/demo-trace.bin`,
    ]);
  });
  it("resolves root-relative /api/object components", () => {
    expect(
      resolveTraceUrls(["/api/object?bucket=b&key=traces%2Fseg-0.bin.gz"], ORIGIN),
    ).toEqual([`${ORIGIN}/api/object?bucket=b&key=traces%2Fseg-0.bin.gz`]);
  });
  it("passes absolute URLs through untouched (same-origin creds rule still applies downstream)", () => {
    expect(resolveTraceUrls(["https://other.example/t.bin.gz"], ORIGIN)).toEqual([
      "https://other.example/t.bin.gz",
    ]);
  });
  it("keeps component order (multi-trace loads are order-sensitive)", () => {
    expect(resolveTraceUrls(["a.bin", "b.bin"], ORIGIN)).toEqual([
      `${ORIGIN}/a.bin`,
      `${ORIGIN}/b.bin`,
    ]);
  });
});

describe("loadingLabel (phase text)", () => {
  it("streaming: Loading trace / Loading N traces, regardless of phase", () => {
    expect(loadingLabel("stream", "parsing", 1)).toBe("Loading trace\u2026");
    expect(loadingLabel("stream", "parsing", 3)).toBe("Loading 3 traces\u2026");
    expect(loadingLabel("stream", "fetching", 1)).toBe("Loading trace\u2026");
  });
  it("buffered: Fetching then Parsing", () => {
    expect(loadingLabel("buffered", "fetching", 1)).toBe("Fetching trace\u2026");
    expect(loadingLabel("buffered", "fetching", 2)).toBe("Fetching 2 traces\u2026");
    expect(loadingLabel("buffered", "parsing", 2)).toBe("Parsing trace\u2026");
  });
});

describe("seedFacetState", () => {
  it("defaults source to cpu, other facets empty", () => {
    expect(seedFacetState(new URLSearchParams(""))).toEqual({
      source: "cpu",
      thread_class: "",
      spawn_location: "",
    });
  });
  it("reads explicit facet params", () => {
    const s = seedFacetState(
      new URLSearchParams("?source=sched&thread_class=worker&spawn_location=src/main.rs:1"),
    );
    expect(s).toEqual({
      source: "sched",
      thread_class: "worker",
      spawn_location: "src/main.rs:1",
    });
  });
});

describe("buildApiUrl (legacy buildApiUrl parity)", () => {
  it("SSE stream URL: scope + non-empty facets, no refine flag", () => {
    expect(buildApiUrl(state(), ORIGIN)).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu`,
    );
  });
  it("hosts repeat, times and max_files serialize in legacy order", () => {
    const u = buildApiUrl(
      state({
        service: "svc-a",
        hosts: ["h1", "h2"],
        startNs: "1743000000000000000",
        endNs: "1743003600000000000",
        maxFiles: 64,
      }),
      ORIGIN,
    );
    expect(u).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&service=svc-a` +
        "&host=h1&host=h2&source=cpu&start_ns=1743000000000000000" +
        "&end_ns=1743003600000000000&max_files=64",
    );
  });
  it("poll-duration band rides after the time window (min_poll_ns/max_poll_ns)", () => {
    const u = buildApiUrl(
      state({ startNs: "1", endNs: "2", minPollNs: "500000", maxPollNs: "10000000" }),
      ORIGIN,
    );
    expect(u).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu` +
        "&start_ns=1&end_ns=2&min_poll_ns=500000&max_poll_ns=10000000",
    );
  });
  it("an open-ended band emits only the bound that is set", () => {
    expect(buildApiUrl(state({ minPollNs: "1000000" }), ORIGIN)).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu&min_poll_ns=1000000`,
    );
    expect(buildBrowserQuery(state({ maxPollNs: "2000000" }))).toBe(
      "api=1&bucket=demo-traces&prefix=traces&source=cpu&max_poll_ns=2000000",
    );
  });
  it("data_dir passthrough, empty facets skipped, later facet keys ride in insertion order", () => {
    const u = buildApiUrl(
      state({
        dataDir: "/var/traces",
        bucket: null,
        prefix: null,
        facets: { source: "cpu", thread_class: "", spawn_location: "", host_group: "blue" },
      }),
      ORIGIN,
    );
    expect(u).toBe(
      `${ORIGIN}/api/flamegraph?data_dir=%2Fvar%2Ftraces&source=cpu&host_group=blue`,
    );
  });
});

describe("buildBrowserQuery (legacy updateBrowserUrl parity)", () => {
  // max_files used to be omitted here (legacy parity). It now rides along, so a
  // link copied after "Refine more" reopens at the depth on screen instead of
  // silently resetting to the backend default.
  it("api=1 leads; max_files persists; no ui param", () => {
    expect(buildBrowserQuery(state({ maxFiles: 640 }))).toBe(
      "api=1&bucket=demo-traces&prefix=traces&source=cpu&max_files=640",
    );
  });
  it("an unset depth adds no max_files", () => {
    expect(buildBrowserQuery(state())).toBe(
      "api=1&bucket=demo-traces&prefix=traces&source=cpu",
    );
  });

  // Span Explorer deep links: the span filter is load scope, so it must survive
  // every URL rewrite the page does.
  it("carries the span-type filter and duration band", () => {
    expect(
      buildBrowserQuery(
        state({ spanTypeUid: "abc123", minSpanNs: "1000", maxSpanNs: "5000" }),
      ),
    ).toBe(
      "api=1&bucket=demo-traces&prefix=traces&source=cpu" +
        "&span_type_uid=abc123&min_span_ns=1000&max_span_ns=5000",
    );
  });
  it("full scope round-trips in legacy order", () => {
    expect(
      buildBrowserQuery(
        state({
          service: "svc-a",
          hosts: ["h1"],
          facets: { source: "sched", thread_class: "worker", spawn_location: "" },
          startNs: "1",
          endNs: "2",
        }),
      ),
    ).toBe(
      "api=1&bucket=demo-traces&prefix=traces&service=svc-a&host=h1" +
        "&source=sched&thread_class=worker&start_ns=1&end_ns=2",
    );
  });
});
