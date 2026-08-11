// The Span Explorer's URL contract: duration-band params, exemplar-only
// scoping, refinement depth, and raw-mode `trace` preservation.

import { describe, it, expect } from "vitest";
import { readFileSync } from "node:fs";
import {
  buildApiUrl,
  buildBrowserQuery,
  exemplarScopeKey,
  isAggregateMode,
  readScope,
  type PageScope,
  type ViewState,
} from "./scope.js";

const ORIGIN = "https://viewer.example";

const AGG_SCOPE: PageScope = {
  trace: null,
  dataDir: null,
  source: {
    bucket: "traces",
    region: "us-west-2",
    credentials: { kind: "ambient" },
  },
  prefix: "svc/",
  service: "metrics",
  hosts: ["host-a", "host-b"],
};

const ROLE = "arn:aws:iam::123456789012:role/Dial9TraceReader";

const EMPTY_VIEW: ViewState = {
  startNs: null,
  endNs: null,
  selectedUid: null,
  bandMinNs: null,
  bandMaxNs: null,
  attrFilters: [],
  maxFiles: null,
};

const q = (url: string): URLSearchParams => new URL(url).searchParams;

describe("userscript page marker", () => {
  it("retains #btn-copylink in static markup", () => {
    const html = readFileSync(
      new URL("../../../span_explorer.html", import.meta.url),
      "utf8",
    );
    expect(html).toContain('id="btn-copylink"');
  });
});

describe("readScope / isAggregateMode", () => {
  it("raw mode wins outright, even alongside aggregate selectors", () => {
    const params = new URLSearchParams("trace=demo-trace.bin&api=1&bucket=b");
    const scope = readScope(params);
    expect(scope.trace).toBe("demo-trace.bin");
    expect(isAggregateMode(params, scope)).toBe(false);
  });
  it("api=1, a bucket, or a data_dir each turn the stream on", () => {
    for (const qs of ["api=1", "bucket=b", "data_dir=/traces"]) {
      const params = new URLSearchParams(qs);
      expect(isAggregateMode(params, readScope(params)), qs).toBe(true);
    }
  });
  it("a bare URL addresses neither source", () => {
    const params = new URLSearchParams("");
    expect(isAggregateMode(params, readScope(params))).toBe(false);
  });
  it("repeated host params all survive", () => {
    expect(readScope(new URLSearchParams("host=a&host=b")).hosts).toEqual(["a", "b"]);
  });
});

describe("buildApiUrl", () => {
  it("carries the whole fixed scope", () => {
    const p = q(buildApiUrl("replace", AGG_SCOPE, EMPTY_VIEW, ORIGIN));
    expect(p.get("bucket")).toBe("traces");
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("prefix")).toBe("svc/");
    expect(p.get("service")).toBe("metrics");
    expect(p.getAll("host")).toEqual(["host-a", "host-b"]);
  });

  it("sends the selected duration band", () => {
    const p = q(
      buildApiUrl(
        "replace",
        AGG_SCOPE,
        { ...EMPTY_VIEW, bandMinNs: 1_000, bandMaxNs: 5_000 },
        ORIGIN,
      ),
    );
    expect(p.get("min_span_ns")).toBe("1000");
    expect(p.get("max_span_ns")).toBe("5000");
  });

  it("an open-ended band sends only the bound that is set", () => {
    const p = q(
      buildApiUrl("replace", AGG_SCOPE, { ...EMPTY_VIEW, bandMinNs: 1_000 }, ORIGIN),
    );
    expect(p.get("min_span_ns")).toBe("1000");
    expect(p.has("max_span_ns")).toBe(false);
  });

  // An exemplar refetch reads only already-folded spans parts. It must scope
  // itself to the selected type and never parse additional raw files.
  it("an exemplars stream is exemplars_only and scoped to the selected type", () => {
    const p = q(
      buildApiUrl("exemplars", AGG_SCOPE, { ...EMPTY_VIEW, selectedUid: "abc" }, ORIGIN),
    );
    expect(p.get("exemplars_only")).toBe("true");
    expect(p.get("span_type_uid")).toBe("abc");
  });

  it("catalog streams never send exemplars_only or a type filter", () => {
    for (const mode of ["replace", "refine"] as const) {
      const p = q(buildApiUrl(mode, AGG_SCOPE, { ...EMPTY_VIEW, selectedUid: "abc" }, ORIGIN));
      expect(p.has("exemplars_only"), mode).toBe(false);
      expect(p.has("span_type_uid"), mode).toBe(false);
    }
  });

  it("persists the refine depth, and drops it when cleared", () => {
    expect(q(buildApiUrl("refine", AGG_SCOPE, { ...EMPTY_VIEW, maxFiles: 400 }, ORIGIN)).get("max_files")).toBe("400");
    expect(q(buildApiUrl("refine", AGG_SCOPE, EMPTY_VIEW, ORIGIN)).has("max_files")).toBe(false);
  });

  it("attribute filters are repeated key=value params", () => {
    const p = q(
      buildApiUrl(
        "replace",
        AGG_SCOPE,
        {
          ...EMPTY_VIEW,
          attrFilters: [
            { key: "status_code", value: "500" },
            { key: "route", value: "/a=b" },
          ],
        },
        ORIGIN,
      ),
    );
    // The value may itself contain '='; only the FIRST one separates.
    expect(p.getAll("attr")).toEqual(["status_code=500", "route=/a=b"]);
  });

  // The single-transport rule: the role is header-only (restored via
  // applyToCreds at boot), so the /api/span-stats request URL must NOT carry
  // aws_role_arn — a role on both header and query is the server's
  // ConflictingCredentials 400. Region is safe on the request URL and stays.
  it("carries region but NEVER the role on the request URL", () => {
    const p = q(
      buildApiUrl("replace", {
        ...AGG_SCOPE,
        source: {
          ...AGG_SCOPE.source,
          credentials: { kind: "role", roleArn: ROLE },
        },
      }, EMPTY_VIEW, ORIGIN),
    );
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.has("aws_role_arn")).toBe(false);
  });
});

describe("buildBrowserQuery", () => {
  // The shareable link is where the role lives (aws_role_arn): the tab it opens
  // restores it at boot. Region rides here too.
  it("the shareable link carries BOTH region and the role", () => {
    const p = new URLSearchParams(
      buildBrowserQuery({
        ...AGG_SCOPE,
        source: {
          ...AGG_SCOPE.source,
          credentials: { kind: "role", roleArn: ROLE },
        },
      }, EMPTY_VIEW),
    );
    expect(p.get("aws_region")).toBe("us-west-2");
    expect(p.get("aws_role_arn")).toBe(ROLE);
  });

  it("aggregate mode pins api=1", () => {
    const p = new URLSearchParams(buildBrowserQuery(AGG_SCOPE, EMPTY_VIEW));
    expect(p.get("api")).toBe("1");
    expect(p.has("trace")).toBe(false);
  });

  it("raw mode keeps its trace and never gains api=1", () => {
    const raw: PageScope = {
      ...AGG_SCOPE,
      trace: "demo-trace.bin",
      source: { ...AGG_SCOPE.source, bucket: "" },
    };
    const p = new URLSearchParams(
      buildBrowserQuery(raw, { ...EMPTY_VIEW, selectedUid: "uid-1" }),
    );
    expect(p.get("trace")).toBe("demo-trace.bin");
    expect(p.has("api")).toBe(false);
    expect(p.get("span_type_uid")).toBe("uid-1");
  });

  it("round-trips the band and the selection", () => {
    const p = new URLSearchParams(
      buildBrowserQuery(AGG_SCOPE, {
        ...EMPTY_VIEW,
        selectedUid: "uid-1",
        bandMinNs: 12,
        bandMaxNs: 34,
        maxFiles: 64,
      }),
    );
    expect(p.get("span_type_uid")).toBe("uid-1");
    expect(p.get("min_span_ns")).toBe("12");
    expect(p.get("max_span_ns")).toBe("34");
    expect(p.get("max_files")).toBe("64");
  });
});

describe("exemplarScopeKey", () => {
  it("distinguishes every band, including the open-ended ones", () => {
    const key = (min: number | null, max: number | null): string =>
      exemplarScopeKey({ bandMinNs: min, bandMaxNs: max });
    const keys = [key(null, null), key(1, null), key(null, 1), key(1, 2)];
    expect(new Set(keys).size).toBe(4);
  });
  it("the same band is the same key", () => {
    expect(exemplarScopeKey({ bandMinNs: 1, bandMaxNs: 2 })).toBe(
      exemplarScopeKey({ bandMinNs: 1, bandMaxNs: 2 }),
    );
  });
});
