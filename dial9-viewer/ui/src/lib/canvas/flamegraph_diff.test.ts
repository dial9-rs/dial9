// The diff seam re-exports the frozen flamegraph_diff.js scope-link codec and
// helpers unchanged; these scenarios are transcribed from the frozen node
// oracle (test_flamegraph_diff.js) so the seam is pinned to the same values the
// diff dispatch (main.ts) and fast/slow diff (minimap) rely on.
//
// Imports the seam module directly (not the lib/canvas barrel) so the node test
// environment does not pull in the DOM-mounting widget / diff view.

import { describe, it, expect } from "vitest";
import {
  fullScopeQuery,
  encodeScope,
  decodeScope,
  diffSearch,
  parseDiff,
  pollBandLabel,
} from "./flamegraph_diff.js";

describe("fullScopeQuery", () => {
  it("keeps the full scope, drops transient/credential params", () => {
    const src = new URLSearchParams(
      "api=1&bucket=my-bucket&aws_region=us-west-2&prefix=traces/svc&service=svc" +
        "&host=h1&host=h2&thread_class=worker&source=cpu&start_ns=1&end_ns=2&max_files=256" +
        "&worker-zoom=foo%09bar&x-dial9-aws-access-key-id=AKIASECRET",
    );
    const out = fullScopeQuery(src);
    expect(out.get("bucket")).toBe("my-bucket");
    expect(out.get("aws_region")).toBe("us-west-2");
    expect(out.get("max_files")).toBe("256");
    expect(out.getAll("host")).toEqual(["h1", "h2"]);
    expect(out.get("worker-zoom")).toBeNull();
    expect(out.get("x-dial9-aws-access-key-id")).toBeNull();
  });
  it("omits empty-string values", () => {
    const out = fullScopeQuery(new URLSearchParams("api=1&service=&bucket=b"));
    expect(out.get("service")).toBeNull();
    expect(out.get("bucket")).toBe("b");
    expect(out.has("host")).toBe(false);
  });
});

describe("encodeScope / decodeScope", () => {
  it("round-trips scope params through the base64url blob", () => {
    const scope = fullScopeQuery(
      new URLSearchParams("api=1&bucket=b&prefix=traces/svc&host=h1&host=h2&source=cpu"),
    );
    const back = decodeScope(encodeScope(scope));
    expect(back.get("bucket")).toBe("b");
    expect(back.get("prefix")).toBe("traces/svc");
    expect(back.getAll("host")).toEqual(["h1", "h2"]);
    expect(back.get("source")).toBe("cpu");
  });
});

describe("diffSearch / parseDiff (dispatch decision)", () => {
  it("round-trips two independent scopes and never leaks raw keys", () => {
    const a = fullScopeQuery(new URLSearchParams("api=1&bucket=ba&prefix=pa&service=svc&host=h1"));
    const b = fullScopeQuery(new URLSearchParams("api=1&bucket=bb&prefix=pb&service=svc&host=h2"));
    const search = diffSearch(a, b);
    expect(search.startsWith("diff=1")).toBe(true);
    expect(search.includes("bucket=")).toBe(false);
    const parsed = parseDiff(search)!;
    expect(parsed).not.toBeNull();
    expect(parsed.a.get("bucket")).toBe("ba");
    expect(parsed.b.get("bucket")).toBe("bb");
    expect(parsed.a.get("host")).toBe("h1");
    expect(parsed.b.get("host")).toBe("h2");
  });

  it("carries independent per-side poll-duration bands (fast vs slow)", () => {
    const a = fullScopeQuery(new URLSearchParams("bucket=b&service=svc&host=h1&max_poll_ns=1000000"));
    const b = fullScopeQuery(new URLSearchParams("bucket=b&service=svc&host=h1&min_poll_ns=10000000"));
    const parsed = parseDiff(diffSearch(a, b))!;
    expect(parsed.a.get("max_poll_ns")).toBe("1000000");
    expect(parsed.a.get("min_poll_ns")).toBeNull();
    expect(parsed.b.get("min_poll_ns")).toBe("10000000");
    expect(parsed.b.get("max_poll_ns")).toBeNull();
  });

  it("rejects non-diff and malformed links (dispatch falls through)", () => {
    expect(parseDiff("api=1&bucket=b")).toBeNull();
    expect(parseDiff("diff=1&a=abc")).toBeNull(); // missing b
    expect(parseDiff("diff=1")).toBeNull();
    expect(parseDiff("?trace=t.bin")).toBeNull();
    expect(
      parseDiff(new URLSearchParams("diff=1&a=" + encodeScope("bucket=x") + "&b=" + encodeScope("bucket=y"))),
    ).not.toBeNull();
  });
});

describe("pollBandLabel", () => {
  it("summarizes the ns band as human ms", () => {
    expect(pollBandLabel(null, null)).toBe("");
    expect(pollBandLabel("", "")).toBe("");
    expect(pollBandLabel("10000000", null)).toBe("poll ≥ 10ms");
    expect(pollBandLabel(null, "1000000")).toBe("poll ≤ 1ms");
    expect(pollBandLabel("1000000", "10000000")).toBe("poll 1–10ms");
    expect(pollBandLabel(1000000, 10000000)).toBe("poll 1–10ms");
  });
});
