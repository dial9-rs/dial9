// Regression tests for the flamegraph search stat math (#593 / T48).
//
// The bug: the toolbar stat divided matched SELF samples (leaf-attributed
// only) by the total, while the search highlight lights every matching
// frame's full INCLUSIVE bar. Mid-stack matches have self == 0, so the
// reported percentage could not track the highlighted area, and the
// `matchedSelf > 0` guard hid the figure entirely while half the canvas
// was lit.
//
// The fix (maintainer-confirmed option 1 for #593): the percentage is the
// highlighted-area share - the union of INCLUSIVE sample counts of the
// topmost matching frames (a match nested under another match adds no new
// highlighted extent) over the total samples in view. This is the same
// semantic as the exported SVG's embedded search ("Matched: X%",
// flamegraph_export.js) and flamegraph.pl.
//
// Written failing-first: on the pre-fix core, `countSearchMatches` is not
// exported and returned self-sample counts, so this whole suite fails.
//
// Frozen core loaded via createRequire (see format.test.ts for the
// rationale).

import { describe, it, expect, beforeAll } from "vitest";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface TreeNode {
  name: string;
  fullName?: string | undefined;
  children: Map<string, TreeNode>;
  count: number;
  self: number;
}

interface SearchMatches {
  matchedCount: number;
  frameCount: number;
}

const { countSearchMatches } = require("../../flamegraph.js") as {
  countSearchMatches: (root: TreeNode, queryLower: string) => SearchMatches;
};

/** Build a synthetic flamegraph tree node the way buildFlamegraphTree shapes them. */
function node(
  name: string,
  count: number,
  self: number,
  kids: TreeNode[] = [],
  fullName?: string,
): TreeNode {
  const children = new Map<string, TreeNode>();
  for (const k of kids) children.set(k.fullName ?? k.name, k);
  return { name, fullName, children, count, self };
}

/** The stat's percentage formatting (updateSearchStats): one decimal. */
function pct(matched: number, total: number): string {
  return ((matched / total) * 100).toFixed(1);
}

describe("countSearchMatches (highlighted-area semantics)", () => {
  it("is exported from the core module", () => {
    expect(typeof countSearchMatches).toBe("function");
  });

  it("mid-stack match with self=0 yields the full inclusive area, not zero (reporter's case)", () => {
    // root -> mid (self 0) -> leaf (all 10 samples attributed at the leaf).
    // Searching "mid" lights a full-width bar; the stat must say 100.0%,
    // and must not be hidden (the old matchedSelf>0 guard keyed on 0 here).
    const tree = node("(all)", 10, 0, [
      node("mid_stack_fn", 10, 0, [node("leaf_fn", 10, 10)]),
    ]);
    const m = countSearchMatches(tree, "mid_stack");
    expect(m.frameCount).toBe(1);
    expect(m.matchedCount).toBe(10);
    expect(m.matchedCount).toBeGreaterThan(0); // non-hidden
    expect(pct(m.matchedCount, tree.count)).toBe("100.0");
  });

  it("leaf-heavy match where self and inclusive area coincide stays consistent", () => {
    // hot leaf owns all of its subtree's samples: self == count, so the
    // old and new semantics agree - the fix must not change this case.
    const tree = node("(all)", 10, 0, [
      node("parent_fn", 10, 0, [node("hot_leaf", 4, 4), node("other_leaf", 6, 6)]),
    ]);
    const m = countSearchMatches(tree, "hot_leaf");
    expect(m.frameCount).toBe(1);
    expect(m.matchedCount).toBe(4);
    expect(pct(m.matchedCount, tree.count)).toBe("40.0");
  });

  it("a match nested under another match adds no new area (union of topmost extents)", () => {
    // Both frames match "alpha"; the inner one's extent lies inside the
    // outer one's, exactly as on the canvas. frameCount still counts both.
    const tree = node("(all)", 10, 0, [
      node("alpha_outer", 8, 0, [node("alpha_inner", 8, 8)]),
      node("beta", 2, 2),
    ]);
    const m = countSearchMatches(tree, "alpha");
    expect(m.frameCount).toBe(2);
    expect(m.matchedCount).toBe(8); // not 16
    expect(pct(m.matchedCount, tree.count)).toBe("80.0");
  });

  it("disjoint matches sum their extents", () => {
    const tree = node("(all)", 10, 0, [
      node("io_read", 4, 4),
      node("io_write", 6, 0, [node("kernel", 6, 6)]),
    ]);
    const m = countSearchMatches(tree, "io_");
    expect(m.frameCount).toBe(2);
    expect(m.matchedCount).toBe(10);
    expect(pct(m.matchedCount, tree.count)).toBe("100.0");
  });

  it("matches on fullName as well as display name (F34 preserved)", () => {
    const tree = node("(all)", 5, 0, [
      node("shortname", 5, 5, [], "std::vec::Vec::push"),
    ]);
    expect(countSearchMatches(tree, "std::vec").matchedCount).toBe(5);
    expect(countSearchMatches(tree, "shortname").matchedCount).toBe(5);
  });

  it("caller passes a lowercased query; node names match case-insensitively", () => {
    const tree = node("(all)", 5, 0, [node("FrameBuf::capture", 5, 0, [node("x", 5, 5)])]);
    const m = countSearchMatches(tree, "framebuf");
    expect(m.frameCount).toBe(1);
    expect(m.matchedCount).toBe(5);
  });

  it("no matches reports zero frames and zero area", () => {
    const tree = node("(all)", 5, 0, [node("a", 5, 5)]);
    const m = countSearchMatches(tree, "nomatch");
    expect(m.frameCount).toBe(0);
    expect(m.matchedCount).toBe(0);
  });
});

// ── Demo-trace anchors ────────────────────────────────────────────────────
// The concrete numbers from the #593 investigation (docs/tickets/
// issue-closures.md): reported stat vs canvas lit-area share, measured
// three independent ways (live draw-call interception, the export SVG's
// embedded search, and this exact tree math). The stat must now equal the
// lit-area share for all of them.

interface CpuSample {
  source: number;
  callchain: string[];
  workerId: number;
}

interface ParsedTraceLike {
  cpuSamples: CpuSample[];
  callframeSymbols: Map<string, unknown>;
}

const { parseTrace } = require("../../trace_parser.js") as {
  parseTrace: (buf: Buffer) => Promise<ParsedTraceLike>;
};
const { buildFlamegraphTree } = require("../../trace_analysis.js") as {
  buildFlamegraphTree: (samples: CpuSample[], syms: Map<string, unknown>) => TreeNode;
};

const DEMO = fileURLToPath(new URL("../../public/demo-trace.bin", import.meta.url));

describe("demo-trace anchors (#593 measurements)", () => {
  let workerTree: TreeNode;

  beforeAll(async () => {
    const trace = await parseTrace(readFileSync(DEMO));
    // Exactly the page's tree build: filterCpuSamples(cpuSamples, null,
    // null) then the workerId split (flamegraph.html:571 -> applyFilters).
    const samples = trace.cpuSamples.filter(
      (s) => s.callchain.length > 0 && s.source !== 1,
    );
    workerTree = buildFlamegraphTree(
      samples.filter((s) => s.workerId !== 255),
      trace.callframeSymbols,
    );
  });

  // [query, expected frames, expected pct of the in-view total]
  //
  // Exact measurements of the committed demo trace: a snapshot guarding
  // against drift in the parser/tree/search path between regens, NOT a
  // correctness oracle (the synthetic-tree tests above are). On a demo
  // regen, re-measure (rerun; received values are the new measurements)
  // but first rule out a code regression: point DEMO at the previous
  // trace bytes (git show <base>:dial9-viewer/ui/public/demo-trace.bin)
  // and confirm the old anchors still reproduce. Then sanity-check the
  // new values before copying: poll/tokio should dominate, spawn should
  // stay tiny, and shifts should be explainable by the capture.
  // Re-measured after the demo-trace regen. Read this before trusting the
  // numbers: the regen was captured on a host whose glibc lacks frame pointers
  // in most functions, so samples landing in libc (__malloc, __strlen_sse2) or
  // vendored assembly truncate at depth 1-2 instead of unwinding to a thread
  // root. The tree these anchors measure is CPU-profile samples only
  // (`source !== 1`, 66 of them — NOT the ~10k sched-wait samples), and only 36
  // of those reach depth >= 16. The "60.6%" values are therefore closer to the
  // unwind-success rate than to a property of the search path, and framebuf's
  // fall to 1.5% is FrameBuf::capture being absent from this capture's symbol
  // table, not a share shift.
  //
  // A code regression was ruled out — the previous anchors reproduce exactly
  // against the pre-regen bytes — so these are honest measurements of a thin
  // capture, not of broken code. But they guard less than the pre-regen set
  // did, and framebuf at 1 sample will flip on any regen. The fix is a heavier
  // or longer demo run: this regen collected 25.6k polls against the previous
  // 59.8k, which is why the fixed malloc/strlen noise grew from 9% to 29% of the
  // sample set. Re-measure and re-tighten these when that lands.
  const ANCHORS: Array<[string, number, string]> = [
    ["poll", 66, "60.6"],
    ["tokio", 83, "60.6"],
    ["axum", 23, "50.0"],
    ["dispatcher", 15, "51.5"],
    ["framebuf", 3, "1.5"],
    ["spawn", 2, "60.6"],
  ];

  for (const [query, frames, expected] of ANCHORS) {
    it(`"${query}" reports ${expected}% (= lit-area share), ${frames} frames`, () => {
      const m = countSearchMatches(workerTree, query);
      expect(m.frameCount).toBe(frames);
      expect(m.matchedCount).toBeGreaterThan(0); // never hidden when frames match
      expect(pct(m.matchedCount, workerTree.count)).toBe(expected);
    });
  }
});
