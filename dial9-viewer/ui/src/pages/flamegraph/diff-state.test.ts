// The diff view's diff_zoom/diff_search URL codec. Scenarios mirror the frozen
// node oracle (test_flamegraph_view_state.js, diff-view section): TAB-joined
// root-inclusive zoom path, delete-on-absence, disjoint from the single-graph
// namespace.

import { describe, it, expect } from "vitest";
import { readDiffState, writeDiffState } from "./diff-state.js";

const SEP = "\t";

describe("diff-state codec", () => {
  it("round-trips a root-inclusive zoom path + highlight", () => {
    const state = { zoom: ["(all)", "runtime", "poll"], search: "spawn" };
    const params = new URLSearchParams();
    writeDiffState(params, state);
    expect(params.get("diff_zoom")).toBe("(all)\truntime\tpoll");
    expect(params.get("diff_search")).toBe("spawn");
    expect(readDiffState(params)).toEqual(state);
  });

  it("deletes its keys on absence (URL stays clean at the top level)", () => {
    const params = new URLSearchParams();
    params.set("diff_zoom", "(all)" + SEP + "x");
    params.set("diff_search", "q");
    writeDiffState(params, {});
    expect(params.get("diff_zoom")).toBeNull();
    expect(params.get("diff_search")).toBeNull();
  });

  it("filters stray empty path segments on read", () => {
    const params = new URLSearchParams();
    params.set("diff_zoom", SEP + "(all)" + SEP + SEP + "poll" + SEP);
    expect(readDiffState(params).zoom).toEqual(["(all)", "poll"]);
  });

  it("reads a raw query string and leaves foreign keys untouched", () => {
    const params = new URLSearchParams("diff=1&a=xx&b=yy&diff_search=needle");
    expect(readDiffState(params).search).toBe("needle");
    writeDiffState(params, { search: "needle" });
    expect(params.get("diff")).toBe("1");
    expect(params.get("a")).toBe("xx");
  });
});
