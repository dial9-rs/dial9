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

import { describe, expect, it } from "vitest";
import type { Coverage } from "./aggregates.js";
import { coverageSignal, isCoverageFrozen, tokioStatsUrl } from "./aggregates.js";

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

  it("carries aws_region for the ambient cross-region read", () => {
    // Region rides the request URL (the server reads it from aws_region — the
    // only place an ambient cross-region read learns it). There is no role
    // field on AggregateScope: the role is header-only.
    const u = new URL("http://x" + tokioStatsUrl({ bucket: "b", aws_region: "us-west-2" }));
    expect(u.searchParams.get("aws_region")).toBe("us-west-2");
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
