// The diff seam re-exports the shared flamegraph_diff.js tree, color, and
// scope-link helpers. These tests exercise that module through the same typed
// boundary used by the pages.
//
// Imports the seam module directly (not the lib/canvas barrel) so the node test
// environment does not pull in the DOM-mounting widget / diff view.

import { describe, it, expect } from "vitest";
import {
  DIFF_SHIFT_1H,
  DIFF_SHIFT_24H,
  DIFF_SHIFT_7D,
  addDiffCapture,
  chooseTarget,
  diffColor,
  fullScopeQuery,
  encodeScope,
  decodeScope,
  diffSearch,
  layoutSide,
  mergeTrees,
  nodeAtPath,
  parseDiff,
  pollBandLabel,
  removeDiffSide,
  scopeWithHost,
  shiftScopeTime,
  swapDiffCapture,
  type DiffMergedNode,
} from "./flamegraph_diff.js";

interface WireNode {
  name: string;
  count: number;
  self?: number;
  children?: WireNode[];
}

interface LayoutBox {
  name: string;
  x: number;
  w: number;
  a: number;
  b: number;
  path: string[];
}

function node(
  name: string,
  count: number,
  self = 0,
  children: WireNode[] = [],
): WireNode {
  return {
    name,
    count,
    ...(self === 0 ? {} : { self }),
    ...(children.length === 0 ? {} : { children }),
  };
}

function boxesByName(root: DiffMergedNode, side: "a" | "b", width: number) {
  const layout = layoutSide(root, ["(all)"], side, width);
  return {
    layout,
    boxes: Object.fromEntries(
      (layout.boxes as LayoutBox[]).map((box) => [box.name, box]),
    ) as Record<string, LayoutBox>,
  };
}

describe("mergeTrees", () => {
  const treeA = node("(all)", 100, 0, [
    node("foo", 100, 0, [node("bar", 60, 60), node("baz", 40, 40)]),
  ]);
  const treeB = node("(all)", 200, 0, [
    node("foo", 200, 0, [node("bar", 20, 20), node("qux", 180, 180)]),
  ]);

  it("unions frames and retains both sides' total and self counts", () => {
    const root = mergeTrees(treeA, treeB);
    expect(root).toMatchObject({ name: "(all)", a: 100, b: 200 });
    const foo = root.children.get("foo")!;
    expect([...foo.children.keys()].sort()).toEqual(["bar", "baz", "qux"]);
    expect(foo.children.get("bar")).toMatchObject({
      a: 60,
      b: 20,
      selfA: 60,
      selfB: 20,
    });
    expect(foo.children.get("baz")).toMatchObject({ a: 40, b: 0 });
    expect(foo.children.get("qux")).toMatchObject({ a: 0, b: 180 });
  });

  it("accepts absent sides and omitted wire fields", () => {
    expect(mergeTrees(null, node("(all)", 50, 0, [node("solo", 50, 50)])))
      .toMatchObject({ a: 0, b: 50 });
    expect(mergeTrees(null, null)).toMatchObject({
      name: "(all)",
      a: 0,
      b: 0,
      selfA: 0,
      selfB: 0,
    });
    expect(mergeTrees(node("(all)", 10), node("(all)", 10)).children.size)
      .toBe(0);
  });
});

describe("diffColor", () => {
  const channels = (color: string): [number, number, number] => {
    const match = /^rgb\((\d+),(\d+),(\d+)\)$/.exec(color);
    expect(match).not.toBeNull();
    return [Number(match![1]), Number(match![2]), Number(match![3])];
  };

  it("is neutral for equal fractions even when capture totals differ", () => {
    expect(diffColor(50, 50, 100, 100)).toBe("rgb(207,207,207)");
    expect(diffColor(10_000, 100_000, 100_000, 1_000_000))
      .toBe("rgb(207,207,207)");
  });

  it("colors B-heavy frames red and A-heavy frames blue", () => {
    const [hotBR, , hotBB] = channels(diffColor(10, 90, 100, 100));
    const [hotAR, , hotAB] = channels(diffColor(90, 10, 100, 100));
    expect(hotBR).toBeGreaterThan(hotBB);
    expect(hotAB).toBeGreaterThan(hotAR);

    const [tinyBR, , tinyBB] = channels(diffColor(0, 1, 1, 8_846));
    expect(tinyBR).toBeGreaterThan(tinyBB);
  });

  it("uses a finite epsilon floor and clamps extreme ratios", () => {
    expect(diffColor(0, 100, 100, 100)).toMatch(/^rgb\(\d+,\d+,\d+\)$/);
    expect(diffColor(1_000, 100_000, 1_000_000, 1_000_000))
      .toBe("rgb(255,40,16)");
    expect(diffColor(1_000, 500_000, 1_000_000, 1_000_000))
      .toBe("rgb(255,40,16)");
  });
});

describe("layoutSide / nodeAtPath", () => {
  const root = mergeTrees(
    node("(all)", 100, 0, [
      node("foo", 100, 0, [node("bar", 60, 60), node("baz", 40, 40)]),
    ]),
    node("(all)", 100, 0, [
      node("foo", 100, 0, [node("bar", 20, 20), node("baz", 80, 80)]),
    ]),
  );

  it("resolves zoom paths and normalizes widths independently per side", () => {
    expect(nodeAtPath(root, ["(all)"])).toBe(root);
    expect(nodeAtPath(root, ["(all)", "foo", "bar"])?.a).toBe(60);
    expect(nodeAtPath(root, ["(all)", "missing"])).toBeNull();

    const a = boxesByName(root, "a", 1_000);
    const b = boxesByName(root, "b", 1_000);
    expect(a.boxes["bar"]).toMatchObject({ w: 600, a: 60, b: 20 });
    expect(a.boxes["baz"]?.w).toBe(400);
    expect(b.boxes["bar"]?.w).toBe(200);
    expect(b.boxes["baz"]?.w).toBe(800);
    expect(a.boxes["bar"]!.x).toBeLessThan(a.boxes["baz"]!.x);
    expect(b.boxes["baz"]!.x).toBeLessThan(b.boxes["bar"]!.x);
    expect(a.boxes["bar"]?.path).toEqual(["(all)", "foo", "bar"]);
    expect(a.layout.maxDepth).toBe(2);
  });

  it("omits boxes narrower than the rendering threshold", () => {
    const narrow = layoutSide(
      mergeTrees(
        node("(all)", 10_000, 0, [
          node("big", 9_999, 9_999),
          node("sliver", 1, 1),
        ]),
        null,
      ),
      ["(all)"],
      "a",
      100,
      0.4,
    );
    expect((narrow.boxes as LayoutBox[]).map((box) => box.name))
      .toEqual(["(all)", "big"]);
  });
});

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
  it("preserves Span Explorer filters used to build scoped diffs", () => {
    const out = fullScopeQuery(
      new URLSearchParams(
        "api=1&bucket=b&span_type_uid=abc&min_span_ns=1000000&max_span_ns=50000000",
      ),
    );
    expect(out.get("span_type_uid")).toBe("abc");
    expect(out.get("min_span_ns")).toBe("1000000");
    expect(out.get("max_span_ns")).toBe("50000000");
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

  it("carries independent per-side time windows", () => {
    const a = fullScopeQuery(
      new URLSearchParams("bucket=b&start_ns=1000&end_ns=2000"),
    );
    const b = fullScopeQuery(
      new URLSearchParams("bucket=b&start_ns=8000&end_ns=9000"),
    );
    const parsed = parseDiff(diffSearch(a, b))!;
    expect(parsed.a.get("start_ns")).toBe("1000");
    expect(parsed.a.get("end_ns")).toBe("2000");
    expect(parsed.b.get("start_ns")).toBe("8000");
    expect(parsed.b.get("end_ns")).toBe("9000");
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

describe("chooseTarget", () => {
  const a = fullScopeQuery(new URLSearchParams("bucket=ba&host=h1"));
  const b = fullScopeQuery(new URLSearchParams("bucket=bb&host=h2"));

  it("routes captured scopes to the requested two-sided page", () => {
    const flamegraph = chooseTarget("flamegraph", {
      hasDiff: true,
      diffA: a,
      diffB: b,
    });
    expect(flamegraph.page).toBe("flamegraph.html");
    const flamegraphScopes = parseDiff(flamegraph.search)!;
    expect(flamegraphScopes.a.get("api")).toBe("1");
    expect(flamegraphScopes.b.get("api")).toBe("1");

    const tokio = chooseTarget("tokio", { hasDiff: true, diffA: a, diffB: b });
    expect(tokio.page).toBe("tokio_stats.html");
    expect(parseDiff(tokio.search)?.a.get("api")).toBeNull();
  });

  it("passes a single-scope query through unchanged", () => {
    expect(
      chooseTarget("flamegraph", {
        hasDiff: false,
        singleQuery: "api=1&bucket=b",
      }),
    ).toEqual({ page: "flamegraph.html", search: "api=1&bucket=b" });
  });
});

describe("diff capture state", () => {
  const a = new URLSearchParams("host=a");
  const b = new URLSearchParams("host=b");
  const c = new URLSearchParams("host=c");

  it("fills A then B, replacing only the most recent B", () => {
    const first = addDiffCapture(null, a);
    const second = addDiffCapture(first, b);
    const third = addDiffCapture(second, c);
    expect(second).toEqual({ a, b });
    expect(third).toEqual({ a, b: c });
    expect(first).toEqual({ a, b: null });
  });

  it("swaps complete captures and promotes B when A is removed", () => {
    expect(swapDiffCapture({ a, b })).toEqual({ a: b, b: a });
    expect(swapDiffCapture({ a, b: null })).toEqual({ a, b: null });
    expect(removeDiffSide({ a, b }, "a")).toEqual({ a: b, b: null });
    expect(removeDiffSide({ a, b }, "b")).toEqual({ a, b: null });
  });
});

describe("scope comparison presets", () => {
  const scope = fullScopeQuery(
    new URLSearchParams(
      "api=1&bucket=b&host=h1&host=h2" +
        "&start_ns=1782155999000000000&end_ns=1782159599000000000",
    ),
  );

  it("shifts nanosecond windows exactly with BigInt deltas", () => {
    const shifted = shiftScopeTime(scope, DIFF_SHIFT_24H);
    expect(shifted.get("start_ns")).toBe("1782069599000000000");
    expect(shifted.get("end_ns")).toBe("1782073199000000000");
    expect(
      shiftScopeTime(scope, DIFF_SHIFT_1H).get("start_ns"),
    ).toBe((BigInt(scope.get("start_ns")!) - DIFF_SHIFT_1H).toString());
    expect(
      shiftScopeTime(scope, DIFF_SHIFT_7D).get("start_ns"),
    ).toBe((BigInt(scope.get("start_ns")!) - DIFF_SHIFT_7D).toString());
    expect(scope.get("start_ns")).toBe("1782155999000000000");
  });

  it("replaces the host without mutating the original scope", () => {
    const other = scopeWithHost(scope, "h3");
    expect(other.getAll("host")).toEqual(["h3"]);
    expect(other.get("start_ns")).toBe(scope.get("start_ns"));
    expect(scope.getAll("host")).toEqual(["h1", "h2"]);
    expect(parseDiff(diffSearch(scope, other))?.b.get("host")).toBe("h3");
  });
});
