// Unit tests for the flamegraph page's pure URL/label logic. Parameter order is
// pinned because the browser URL is user-visible shared state.

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
    source: {
      bucket: "demo-traces",
      region: "",
      credentials: { kind: "ambient" },
    },
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
    inspect: null,
    ...over,
  };
}

describe("resolveTraceUrls", () => {
  it("resolves relative values against the origin root", () => {
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

describe("buildApiUrl", () => {
  it("SSE stream URL: scope + non-empty facets, no refine flag", () => {
    expect(buildApiUrl(state(), ORIGIN)).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu&format=flat-v1`,
    );
  });
  it("serializes repeated hosts, times and max_files in stable order", () => {
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
        "&end_ns=1743003600000000000&max_files=64&format=flat-v1",
    );
  });
  it("poll-duration band rides after the time window (min_poll_ns/max_poll_ns)", () => {
    const u = buildApiUrl(
      state({ startNs: "1", endNs: "2", minPollNs: "500000", maxPollNs: "10000000" }),
      ORIGIN,
    );
    expect(u).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu` +
        "&start_ns=1&end_ns=2&min_poll_ns=500000&max_poll_ns=10000000" +
        "&format=flat-v1",
    );
  });
  it("an open-ended band emits only the bound that is set", () => {
    expect(buildApiUrl(state({ minPollNs: "1000000" }), ORIGIN)).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu` +
        "&min_poll_ns=1000000&format=flat-v1",
    );
    expect(buildBrowserQuery(state({ maxPollNs: "2000000" }))).toBe(
      "api=1&bucket=demo-traces&credential_mode=ambient&prefix=traces&source=cpu&max_poll_ns=2000000",
    );
  });
  it("data_dir passthrough, empty facets skipped, later facet keys ride in insertion order", () => {
    const u = buildApiUrl(
      state({
        dataDir: "/var/traces",
        source: {
          bucket: "",
          region: "",
          credentials: { kind: "ambient" },
        },
        prefix: null,
        facets: { source: "cpu", thread_class: "", spawn_location: "", host_group: "blue" },
      }),
      ORIGIN,
    );
    expect(u).toBe(
      `${ORIGIN}/api/flamegraph?data_dir=%2Fvar%2Ftraces&source=cpu` +
        "&host_group=blue&format=flat-v1",
    );
  });
  it("forwards the current inspect frame for projection retention", () => {
    expect(buildApiUrl(state({ inspect: "core::task::poll" }), ORIGIN)).toBe(
      `${ORIGIN}/api/flamegraph?bucket=demo-traces&prefix=traces&source=cpu` +
        "&inspect=core%3A%3Atask%3A%3Apoll&format=flat-v1",
    );
  });
});

describe("buildBrowserQuery", () => {
  // max_files rides along so a link copied after "Refine more" reopens at the
  // depth on screen instead of silently resetting to the backend default.
  it("api=1 leads; max_files persists; no ui param", () => {
    expect(buildBrowserQuery(state({ maxFiles: 640 }))).toBe(
      "api=1&bucket=demo-traces&credential_mode=ambient&prefix=traces&source=cpu&max_files=640",
    );
  });
  it("an unset depth adds no max_files", () => {
    expect(buildBrowserQuery(state())).toBe(
      "api=1&bucket=demo-traces&credential_mode=ambient&prefix=traces&source=cpu",
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
      "api=1&bucket=demo-traces&credential_mode=ambient&prefix=traces&source=cpu" +
        "&span_type_uid=abc123&min_span_ns=1000&max_span_ns=5000",
    );
  });
  it("round-trips the full scope in stable order", () => {
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
      "api=1&bucket=demo-traces&credential_mode=ambient&prefix=traces&service=svc-a&host=h1" +
        "&source=sched&thread_class=worker&start_ns=1&end_ns=2",
    );
  });
});

// The unified SourceScope transport rule, and the region+role gap this refactor
// closes: flamegraph used to carry NEITHER region nor the role in its query
// state, so a fresh-session aggregate deep link into a cross-region / assume-
// role bucket 401'd. Now region+role ride the link; the role is header-only.
describe("SourceScope region+role transport (gap closed)", () => {
  const ROLE = "arn:aws:iam::123456789012:role/Dial9TraceReader";

  it("the shareable browser link carries BOTH region and the role", () => {
    const p = new URLSearchParams(
      buildBrowserQuery(state({
        source: {
          bucket: "demo-traces",
          region: "us-west-2",
          credentials: { kind: "role", roleArn: ROLE },
        },
      })),
    );
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("aws_role_arn")).toBe(ROLE);
  });

  it("the /api/flamegraph request URL carries region but NEVER the role", () => {
    // The role rides as a header (restored via applyToCreds at boot); emitting
    // aws_role_arn here too would be a role on both header and query — the
    // server's ConflictingCredentials 400. Region is safe on the request URL
    // and required for the ambient cross-region read.
    const p = new URL(
      buildApiUrl(state({
        source: {
          bucket: "demo-traces",
          region: "us-west-2",
          credentials: { kind: "role", roleArn: ROLE },
        },
      }), ORIGIN),
    ).searchParams;
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.has("aws_role_arn")).toBe(false);
  });
});
