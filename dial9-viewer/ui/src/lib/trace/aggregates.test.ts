// aggregates.ts tests: both endpoints against RECORDED fixtures
// (no live server in vitest), a full cold -> refine -> warm(frozen) refine
// sequence, and the degradation matrix (404 / flag-false / partial
// coverage) - each yielding the documented signal without throwing.
//
// FIXTURE RECIPE (reproducible; captured 2026-07-10 against the stock
// dev-server, whose aggregate endpoints are functional against its seeded
// demo bucket - no dev_server.rs changes needed):
//
//   cd dial9-viewer/ui && npm run build
//   PORT=3041 cargo run -p dial9-viewer --bin dev-server --features dev-server
//   # flamegraph sequence (fresh server = fresh fold state):
//   curl 'http://localhost:3041/api/flamegraph?bucket=demo-traces&prefix=traces' \
//     -o tests/fixtures/aggregates/flamegraph-cold.json          # 638 B, folded 0/1
//   curl 'http://localhost:3041/api/flamegraph?bucket=demo-traces&prefix=traces&refine=true' \
//     -o tests/fixtures/aggregates/flamegraph-refine.json        # 124906 B, folded 1/1, 147 samples
//   curl 'http://localhost:3041/api/flamegraph?bucket=demo-traces&prefix=traces' \
//     -o tests/fixtures/aggregates/flamegraph-warm.json          # 124906 B, read-only, frozen
//   # restart the server (fold state is per-run), then the same three
//   # requests against /api/tokio-stats -> tokio-stats-{cold,refine,warm}.json
//   #   (199 B folded 0/1; 32490 B folded 1/1, 94212 polls; 32490 B frozen)
//   curl 'http://localhost:3041/api/flamegraph?bucket=demo-traces&prefix=no-such-prefix' \
//     -o tests/fixtures/aggregates/not-found-no-match.txt        # real 404, text/plain
//
// The OTHER 404 flavor (no agg context AND no BYO-creds allowance) cannot
// be produced from this dev-server (it always allows BYO creds), so that
// case is synthesized below with a stubbed fetch, shape-checked against
// the server handlers: plain-text body, status 404 (tokio_stats.rs:80-84,
// flamegraph.rs:203-208; the (StatusCode, String) rejection form).

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import type {
  AggregateResult,
  Coverage,
  FetchLike,
  FlamegraphResponse,
  TokioStatsResponse,
} from "./aggregates.js";
import {
  AggregatesRequestError,
  DEFAULT_MAX_REFINE_POLLS,
  coverageSignal,
  fetchFlamegraph,
  fetchTokioStats,
  flamegraphUrl,
  isCoverageFrozen,
  refineUntilFrozen,
  tokioStatsUrl,
} from "./aggregates.js";

function fixtureText(name: string): string {
  return readFileSync(
    new URL(`../../../tests/fixtures/aggregates/${name}`, import.meta.url),
    "utf8"
  );
}

// Fixtures are trusted recordings of the server's serialization; the cast
// mirrors the client's own trust in the wire shape.
function fixtureJson<T>(name: string): T {
  return JSON.parse(fixtureText(name)) as T;
}

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

function notFoundResponse(message: string): Response {
  // Shape of the handlers' (StatusCode::NOT_FOUND, String) rejection.
  return new Response(message, {
    status: 404,
    headers: { "content-type": "text/plain; charset=utf-8" },
  });
}

/** A FetchLike that serves a scripted response per call and records calls. */
function scriptedFetch(responses: Response[]): {
  fetch: FetchLike;
  calls: { url: string; headers?: Record<string, string> }[];
} {
  const calls: { url: string; headers?: Record<string, string> }[] = [];
  const fetch: FetchLike = (url, init) => {
    const call: { url: string; headers?: Record<string, string> } = { url };
    if (init?.headers !== undefined) call.headers = init.headers;
    calls.push(call);
    const resp = responses.shift();
    if (!resp) throw new Error("scriptedFetch: no response scripted for " + url);
    return Promise.resolve(resp);
  };
  return { fetch, calls };
}

function coverage(overrides: Partial<Coverage>): Coverage {
  return {
    files_matched: 1,
    files_folded: 1,
    samples_folded: 1,
    total_bytes: 1,
    hosts_matched: 1,
    hosts_folded: 1,
    ...overrides,
  };
}

// ─── URL building ────────────────────────────────────────────────────────

describe("flamegraphUrl", () => {
  it("serializes the full param set, hosts repeated, refine as literal true", () => {
    const url = flamegraphUrl({
      bucket: "demo-traces",
      prefix: "traces",
      service: "checkout-api",
      host: ["host-a", "host-b"],
      start_ns: "1744224000000000000", // string keeps ns precision > 2^53
      end_ns: 1744224060000000000,
      from: "2026-04-09T19:00",
      to: "2026-04-09T19:01",
      max_files: 50,
      thread_class: "worker",
      source: "cpu",
      spawn_location: "src/main.rs:311:24",
      refine: true,
    });
    const u = new URL("http://x" + url);
    expect(u.pathname).toBe("/api/flamegraph");
    expect(u.searchParams.getAll("host")).toEqual(["host-a", "host-b"]);
    // Serde bool: MUST be the literal "true" ("1" is rejected).
    expect(u.searchParams.get("refine")).toBe("true");
    expect(u.searchParams.get("start_ns")).toBe("1744224000000000000");
    expect(u.searchParams.get("max_files")).toBe("50");
    expect(u.searchParams.get("spawn_location")).toBe("src/main.rs:311:24");
  });

  it("omits absent params and refine=false entirely", () => {
    expect(flamegraphUrl({})).toBe("/api/flamegraph");
    expect(flamegraphUrl({ refine: false })).toBe("/api/flamegraph");
    const url = flamegraphUrl({ bucket: "b", prefix: "p" });
    expect(url).toBe("/api/flamegraph?bucket=b&prefix=p");
  });
});

describe("tokioStatsUrl", () => {
  it("serializes scope params and refine", () => {
    const url = tokioStatsUrl({
      bucket: "demo-traces",
      prefix: "traces",
      host: ["h1"],
      refine: true,
    });
    const u = new URL("http://x" + url);
    expect(u.pathname).toBe("/api/tokio-stats");
    expect(u.searchParams.get("bucket")).toBe("demo-traces");
    expect(u.searchParams.getAll("host")).toEqual(["h1"]);
    expect(u.searchParams.get("refine")).toBe("true");
  });

  it("omits refine when absent", () => {
    expect(tokioStatsUrl({ bucket: "b" })).toBe("/api/tokio-stats?bucket=b");
  });
});

// ─── coverageSignal / isCoverageFrozen units ─────────────────────────────

describe("coverageSignal", () => {
  it("absent coverage -> full (non-demand-driven single fetch)", () => {
    expect(coverageSignal(undefined)).toBe("full");
    expect(coverageSignal(null)).toBe("full");
  });

  it("nothing matched or nothing folded -> none", () => {
    expect(coverageSignal(coverage({ files_matched: 0, files_folded: 0 }))).toBe("none");
    expect(coverageSignal(coverage({ files_matched: 4, files_folded: 0 }))).toBe("none");
  });

  it("folded < matched on files -> partial", () => {
    expect(coverageSignal(coverage({ files_matched: 10, files_folded: 3 }))).toBe("partial");
  });

  it("folded < matched on hosts -> partial (fleet-breadth gap)", () => {
    expect(
      coverageSignal(coverage({ hosts_matched: 40, hosts_folded: 8 }))
    ).toBe("partial");
  });

  it("everything folded -> full", () => {
    expect(
      coverageSignal(
        coverage({ files_matched: 5, files_folded: 5, hosts_matched: 2, hosts_folded: 2 })
      )
    ).toBe("full");
  });
});

describe("isCoverageFrozen", () => {
  it("first poll (no prev) is never frozen", () => {
    expect(isCoverageFrozen(null, coverage({ files_folded: 0 }))).toBe(false);
    expect(isCoverageFrozen(undefined, coverage({ files_folded: 5 }))).toBe(false);
  });

  it("frozen when files_folded does not increase", () => {
    expect(
      isCoverageFrozen(coverage({ files_folded: 3 }), coverage({ files_folded: 3 }))
    ).toBe(true);
    expect(
      isCoverageFrozen(coverage({ files_folded: 3 }), coverage({ files_folded: 2 }))
    ).toBe(true);
  });

  it("not frozen while folding progresses", () => {
    expect(
      isCoverageFrozen(coverage({ files_folded: 0 }), coverage({ files_folded: 1 }))
    ).toBe(false);
  });
});

// ─── /api/flamegraph against recorded fixtures ───────────────────────────

describe("fetchFlamegraph (recorded fixtures)", () => {
  it("cold: empty tree, coverage 0/1 -> signal none", async () => {
    const { fetch, calls } = scriptedFetch([
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-cold.json")),
    ]);
    const r = await fetchFlamegraph({ bucket: "demo-traces", prefix: "traces" }, { fetch });
    expect(calls[0]?.url).toBe("/api/flamegraph?bucket=demo-traces&prefix=traces");
    expect(r.kind).toBe("data");
    if (r.kind !== "data") return;
    expect(r.coverage).toBe("none");
    expect(r.response.total_samples).toBe(0);
    expect(r.response.tree.count).toBe(0);
    // Empty tree: serde skips the empty children vec - absent, not [].
    expect(r.response.tree.children).toBeUndefined();
    expect(r.response.coverage?.files_folded).toBe(0);
    expect(r.response.coverage?.files_matched).toBe(1);
  });

  it("warm: folded tree, coverage 1/1 -> signal full, facets populated", async () => {
    const { fetch } = scriptedFetch([
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-warm.json")),
    ]);
    const r = await fetchFlamegraph({ bucket: "demo-traces", prefix: "traces" }, { fetch });
    expect(r.kind).toBe("data");
    if (r.kind !== "data") return;
    expect(r.coverage).toBe("full");
    expect(r.response.total_samples).toBe(147);
    expect(r.response.tree.count).toBe(147);
    expect(r.response.tree.children?.length).toBeGreaterThan(0);
    const facetNames = r.response.metadata.facets.map((f) => f.name);
    expect(facetNames).toEqual(["source", "thread_class", "host", "spawn_location"]);
    const source = r.response.metadata.facets.find((f) => f.name === "source");
    expect(source?.values).toEqual(["cpu", "sched"]);
    // Scope echo reflects what the server applied (source defaults to cpu).
    expect(r.response.metadata.scope.filters["source"]).toBe("cpu");
  });

  it("forwards BYO-credential headers to fetch", async () => {
    const { fetch, calls } = scriptedFetch([
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-cold.json")),
    ]);
    const headers = { "x-dial9-aws-access-key-id": "test" };
    await fetchFlamegraph({ bucket: "demo-traces" }, { fetch, headers });
    expect(calls[0]?.headers).toEqual(headers);
  });
});

// ─── /api/tokio-stats against recorded fixtures ──────────────────────────

describe("fetchTokioStats (recorded fixtures)", () => {
  it("cold: no spawn locs, coverage 0/1 -> signal none", async () => {
    const { fetch, calls } = scriptedFetch([
      jsonResponse(fixtureJson<TokioStatsResponse>("tokio-stats-cold.json")),
    ]);
    const r = await fetchTokioStats({ bucket: "demo-traces", prefix: "traces" }, { fetch });
    expect(calls[0]?.url).toBe("/api/tokio-stats?bucket=demo-traces&prefix=traces");
    expect(r.kind).toBe("data");
    if (r.kind !== "data") return;
    expect(r.coverage).toBe("none");
    expect(r.response.total_polls).toBe(0);
    expect(r.response.by_spawn_loc).toEqual([]);
    expect(r.response.bucket).toBe("demo-traces");
  });

  it("warm: folded polls, coverage 1/1 -> signal full, shapes intact", async () => {
    const { fetch } = scriptedFetch([
      jsonResponse(fixtureJson<TokioStatsResponse>("tokio-stats-warm.json")),
    ]);
    const r = await fetchTokioStats({ bucket: "demo-traces", prefix: "traces" }, { fetch });
    expect(r.kind).toBe("data");
    if (r.kind !== "data") return;
    expect(r.coverage).toBe("full");
    expect(r.response.total_polls).toBe(94212);
    expect(r.response.time_span_ns).toBeGreaterThan(1);
    expect(r.response.by_spawn_loc.length).toBeGreaterThan(0);
    const top = r.response.by_spawn_loc[0]!;
    // durations/classes are parallel arrays; exemplars a 4-slot array.
    expect(top.classes.length).toBe(top.durations_ns.length);
    expect(top.exemplars.length).toBe(4);
    expect(top.exemplars.some((e) => e !== null)).toBe(true);
    // Sorted descending by notable-poll count (server contract).
    const notable = r.response.by_spawn_loc.map((l) => l.durations_ns.length);
    expect([...notable].sort((a, b) => b - a)).toEqual(notable);
  });
});

// ─── Refine sequence (cold -> refine -> warm/frozen) ─────────────────────

describe("refineUntilFrozen", () => {
  it("drives the recorded flamegraph sequence to frozen in 3 polls", async () => {
    const { fetch, calls } = scriptedFetch([
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-cold.json")),
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-refine.json")),
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-warm.json")),
    ]);
    const seen: string[] = [];
    const final = await refineUntilFrozen(
      (refine) =>
        fetchFlamegraph({ bucket: "demo-traces", prefix: "traces", refine }, { fetch }),
      { onResult: (r) => seen.push(r.coverage) }
    );
    // Read-only first poll, then refine polls; frozen when the warm poll
    // repeats the refine poll's folded count (1 <= 1).
    expect(calls.map((c) => new URL("http://x" + c.url).searchParams.get("refine"))).toEqual(
      [null, "true", "true"]
    );
    // Progressive signals surface to the consumer: none -> full -> full.
    expect(seen).toEqual(["none", "full", "full"]);
    expect(final.kind).toBe("data");
    if (final.kind !== "data") return;
    expect(final.coverage).toBe("full");
    expect(final.response.total_samples).toBe(147);
  });

  it("drives the recorded tokio-stats sequence to frozen in 3 polls", async () => {
    const { fetch, calls } = scriptedFetch([
      jsonResponse(fixtureJson<TokioStatsResponse>("tokio-stats-cold.json")),
      jsonResponse(fixtureJson<TokioStatsResponse>("tokio-stats-refine.json")),
      jsonResponse(fixtureJson<TokioStatsResponse>("tokio-stats-warm.json")),
    ]);
    const final = await refineUntilFrozen((refine) =>
      fetchTokioStats({ bucket: "demo-traces", prefix: "traces", refine }, { fetch })
    );
    expect(calls.length).toBe(3);
    expect(final.kind).toBe("data");
    if (final.kind !== "data") return;
    expect(final.coverage).toBe("full");
    expect(final.response.total_polls).toBe(94212);
  });

  it("stops at the refine-poll ceiling when folding never freezes", async () => {
    // Synthetic ever-growing coverage: each refine poll folds one more of
    // 1000 files, so freezing never triggers - the ceiling must.
    let folded = 0;
    const poll = (refine: boolean): Promise<AggregateResult<TokioStatsResponse>> => {
      if (refine) folded += 1;
      const response: TokioStatsResponse = {
        time_span_ns: 1,
        total_polls: folded,
        bucket: "b",
        by_spawn_loc: [],
        coverage: coverage({ files_matched: 1000, files_folded: folded }),
      };
      return Promise.resolve({
        kind: "data",
        response,
        coverage: coverageSignal(response.coverage),
      });
    };
    const final = await refineUntilFrozen(poll, { maxRefinePolls: 5 });
    expect(folded).toBe(5); // 5 refine polls + the read-only baseline
    expect(final.kind).toBe("data");
    if (final.kind !== "data") return;
    expect(final.coverage).toBe("partial");
    expect(final.response.coverage?.files_folded).toBe(5);
  });

  it("default ceiling is documented and exported", () => {
    expect(DEFAULT_MAX_REFINE_POLLS).toBe(30);
  });

  it("returns immediately when a response carries no coverage", async () => {
    // Non-demand-driven single fetch: nothing to refine.
    const { fetch, calls } = scriptedFetch([
      jsonResponse({
        time_span_ns: 1,
        total_polls: 7,
        bucket: "b",
        by_spawn_loc: [],
      } satisfies TokioStatsResponse),
    ]);
    const final = await refineUntilFrozen((refine) =>
      fetchTokioStats({ bucket: "b", refine }, { fetch })
    );
    expect(calls.length).toBe(1);
    expect(final.kind).toBe("data");
    if (final.kind !== "data") return;
    expect(final.coverage).toBe("full");
  });

  it("propagates unavailable (404 mid-loop) without polling further", async () => {
    const { fetch, calls } = scriptedFetch([
      jsonResponse(fixtureJson<FlamegraphResponse>("flamegraph-cold.json")),
      notFoundResponse("no source files match this scope"),
    ]);
    const final = await refineUntilFrozen((refine) =>
      fetchFlamegraph({ bucket: "demo-traces", refine }, { fetch })
    );
    expect(calls.length).toBe(2);
    expect(final).toEqual({
      kind: "unavailable",
      reason: "not-found",
      coverage: "none",
      message: "no source files match this scope",
    });
  });
});

// ─── Degradation matrix: each signal, never a throw ────────────

describe("degradation: 404 -> none", () => {
  it("no-agg-context 404 (synthetic; not producible from the dev-server)", async () => {
    for (const [call, message] of [
      // Body strings shape-checked against the handlers' rejections.
      [
        (f: FetchLike) => fetchFlamegraph({}, { fetch: f }),
        "flamegraph requires demand-driven aggregation (start with --agg or supply a bucket)",
      ],
      [
        (f: FetchLike) => fetchTokioStats({}, { fetch: f }),
        "tokio-stats requires aggregation (start with --agg or supply a bucket)",
      ],
    ] as const) {
      const { fetch } = scriptedFetch([notFoundResponse(message)]);
      const r = await call(fetch);
      expect(r).toEqual({
        kind: "unavailable",
        reason: "not-found",
        coverage: "none",
        message,
      });
    }
  });

  it("no-files-match 404 (real recorded body)", async () => {
    const body = fixtureText("not-found-no-match.txt");
    const { fetch } = scriptedFetch([notFoundResponse(body)]);
    const r = await fetchFlamegraph(
      { bucket: "demo-traces", prefix: "no-such-prefix" },
      { fetch }
    );
    expect(r.kind).toBe("unavailable");
    if (r.kind !== "unavailable") return;
    expect(r.reason).toBe("not-found");
    expect(r.coverage).toBe("none");
    expect(r.message).toBe("no source files match this scope");
  });
});

describe("degradation: aggregation_enabled=false -> none, no request", () => {
  it("short-circuits both endpoints without touching fetch", async () => {
    const neverFetch: FetchLike = () => {
      throw new Error("fetch must not be called when aggregation is disabled");
    };
    for (const r of [
      await fetchFlamegraph({ bucket: "b" }, { aggregationEnabled: false, fetch: neverFetch }),
      await fetchTokioStats({ bucket: "b" }, { aggregationEnabled: false, fetch: neverFetch }),
    ]) {
      expect(r).toEqual({ kind: "unavailable", reason: "disabled", coverage: "none" });
    }
  });
});

describe("degradation: partial coverage counts -> partial", () => {
  it("folded < matched surfaces as a partial signal on the result", async () => {
    const partial = fixtureJson<FlamegraphResponse>("flamegraph-warm.json");
    // Recorded warm coverage is 1/1; widen the scope to simulate the
    // fleet case (8 of 40 hosts, 12 of 480 files folded).
    partial.coverage = coverage({
      files_matched: 480,
      files_folded: 12,
      samples_folded: 41203,
      hosts_matched: 40,
      hosts_folded: 8,
    });
    const { fetch } = scriptedFetch([jsonResponse(partial)]);
    const r = await fetchFlamegraph({ bucket: "demo-traces" }, { fetch });
    expect(r.kind).toBe("data");
    if (r.kind !== "data") return;
    expect(r.coverage).toBe("partial");
  });
});

describe("unexpected failures still throw", () => {
  it("500 -> AggregatesRequestError with status and body", async () => {
    const { fetch } = scriptedFetch([
      new Response("parquet read failed", { status: 500 }),
    ]);
    const err = await fetchFlamegraph({ bucket: "b" }, { fetch }).then(
      () => null,
      (e: unknown) => e
    );
    expect(err).toBeInstanceOf(AggregatesRequestError);
    if (!(err instanceof AggregatesRequestError)) return;
    expect(err.status).toBe(500);
    expect(err.message).toContain("/api/flamegraph");
    expect(err.message).toContain("parquet read failed");
  });
});
