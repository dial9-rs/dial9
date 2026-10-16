import { createRequire } from "node:module";
import { describe, expect, it } from "vitest";
import { diffSearch } from "../../src/lib/canvas/flamegraph_diff.js";
import { encodeAggregationParams, encodeScope, readScope } from "../../src/lib/trace/trace_scope.js";
import type { TraceScope } from "../../src/lib/trace/trace_scope.js";
import { browserHref } from "../../src/lib/url/browser-link.js";

const require = createRequire(import.meta.url);
const UrlState = require("../../url_state.js") as {
  parse(search: string): Record<string, unknown>;
};

const SCOPE: TraceScope = {
  bucket: "traces-bucket",
  region: "us-west-2",
  roleArn: "arn:aws:iam::123456789012:role/reader",
  credentialMode: "role",
  prefix: "dial9",
  service: "checkout",
  hosts: ["host-a", "host-b"],
  from: 1_700_000_000,
  to: 1_700_000_600,
};

function restored(href: string): {
  path: string;
  state: Record<string, unknown>;
  selection: TraceScope | null;
} {
  const [path, query = ""] = href.split("?");
  return {
    path: path!,
    state: UrlState.parse(query),
    selection: readScope(new URLSearchParams(query)),
  };
}

const EXPECTED_STATE = {
  bucket: "traces-bucket",
  region: "us-west-2",
  credentialMode: "role",
  roleArn: "arn:aws:iam::123456789012:role/reader",
  prefix: "dial9",
  service: "checkout",
  from: 1_700_000_000,
  to: 1_700_000_600,
};

describe("browserHref", () => {
  it("restores the viewer's s_* scope as the browser source, window, and selection", () => {
    const { query } = encodeScope(new URLSearchParams({ host: "title-only" }), SCOPE);
    const out = restored(browserHref("?" + query));
    expect(out.path).toBe("/");
    expect(out.state).toEqual(EXPECTED_STATE);
    expect(out.selection).toMatchObject({
      hosts: ["host-a", "host-b"],
      from: 1_700_000_000,
      to: 1_700_000_600,
    });
  });

  it("restores the aggregation vocabulary used by flamegraph, tokio stats, and span explorer", () => {
    const base = new URLSearchParams({ api: "1" });
    const { query } = encodeAggregationParams(base, SCOPE);
    const out = restored(browserHref(query));
    expect(out.state).toEqual(EXPECTED_STATE);
    expect(out.selection?.hosts).toEqual(["host-a", "host-b"]);
  });

  it("widens a sub-second aggregation window to whole seconds", () => {
    const out = restored(
      browserHref("?bucket=b&start_ns=1700000000500000000&end_ns=1700000600500000000"),
    );
    expect(out.state).toMatchObject({ from: 1_700_000_000, to: 1_700_000_601 });
  });

  it("returns to side A of a diff link", () => {
    const a = encodeAggregationParams(new URLSearchParams(), SCOPE).query;
    const b = encodeAggregationParams(new URLSearchParams(), {
      ...SCOPE,
      service: "payments",
      from: SCOPE.from - 3600,
      to: SCOPE.to - 3600,
    }).query;
    const out = restored(browserHref("?" + diffSearch(a, b)));
    expect(out.state).toEqual(EXPECTED_STATE);
  });

  it("keeps the source without inventing a window when the scope has none", () => {
    const out = restored(browserHref("?bucket=b&prefix=p&service=s"));
    expect(out.state).toEqual({ bucket: "b", prefix: "p", service: "s" });
    expect(out.selection).toBeNull();
  });

  it("links to the browser root for sources without a bucket scope", () => {
    expect(browserHref("")).toBe("/");
    expect(browserHref("?trace=/api/object%3Fkey%3Da.bin")).toBe("/");
  });
});
