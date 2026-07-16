#!/usr/bin/env node
"use strict";

// Tests for the pure inspect/butterfly (#652) and search-results (#653) tree
// builders in flamegraph.js. These are DOM-free, so they run under plain node.

const { assert, test, summarize } = require("./test_harness.js");
const FG = require("./flamegraph.js");

// Build a flamegraph-tree node in the shape trace_analysis.buildFlamegraphTree
// produces: { name, fullName, count, self, children: Map }. Enforces the tree
// invariant count === self + Σ(children.count): if `count` is given, `self` is
// derived from it; otherwise `count` is derived from `self` (default 0) plus
// the children. This keeps fixtures internally consistent for free.
function node(name, spec) {
  spec = spec || {};
  const children = new Map();
  let childSum = 0;
  for (const c of spec.children || []) {
    children.set(c.fullName || c.name, c);
    childSum += c.count;
  }
  let count, self;
  if (spec.count != null) {
    count = spec.count;
    self = spec.self != null ? spec.self : count - childSum;
  } else {
    self = spec.self != null ? spec.self : 0;
    count = childSum + self;
  }
  return {
    name: name,
    fullName: spec.fullName || name,
    location: spec.location || null,
    docsUrl: spec.docsUrl || null,
    count: count,
    self: self,
    children: children,
  };
}

// Shared fixture:
//   (all) 100
//   ├── a 60 (self 10)
//   │   ├── b 30 (self 5)  → target 25
//   │   └── target 20
//   └── c 40
//       └── b 40 (self 10) → target 30
function fixture() {
  const targetA = node("target", { count: 25, self: 25 });
  const targetB = node("target", { count: 20, self: 20 });
  const targetC = node("target", { count: 30, self: 30 });
  const bUnderA = node("b", { self: 5, children: [targetA] });
  const bUnderC = node("b", { self: 10, children: [targetC] });
  const a = node("a", { self: 10, children: [bUnderA, targetB] });
  const c = node("c", { self: 0, children: [bUnderC] });
  return node("(all)", { children: [a, c] });
}

test("buildInspect: totals and self across all occurrences", () => {
  const root = fixture();
  const focus = root.children.get("a").children.get("target"); // any occurrence
  const ins = FG.buildInspect(root, focus);
  assert.strictEqual(ins.total, 75, "inclusive total = 25+20+30");
  assert.strictEqual(ins.self, 75, "self = 25+20+30 (all leaves)");
  assert.strictEqual(ins.occurrences, 3, "three call sites");
  assert.strictEqual(ins.focusName, "target");
});

test("buildInspect: callees tree merges subtrees (leaves → no children)", () => {
  const root = fixture();
  const focus = root.children.get("a").children.get("target");
  const ins = FG.buildInspect(root, focus);
  assert.strictEqual(ins.callees.count, 75, "callee root count is inclusive total");
  assert.strictEqual(ins.callees.self, 75, "callee root self is exact self");
  assert.strictEqual(ins.callees.children.size, 0, "target is a leaf everywhere");
});

test("buildInspect: callers tree is the inverted caller graph", () => {
  const root = fixture();
  const focus = root.children.get("a").children.get("target");
  const ins = FG.buildInspect(root, focus);
  const callers = ins.callers;
  assert.strictEqual(callers.count, 75, "caller root = focus inclusive total");
  // Immediate callers of target: b (25 via a→b, 30 via c→b = 55) and a (20).
  assert.strictEqual(callers.children.size, 2, "two immediate callers: b and a");
  const b = callers.children.get("b");
  const aDirect = callers.children.get("a");
  assert.ok(b && aDirect, "both b and a present as immediate callers");
  assert.strictEqual(b.count, 55, "b calls target with 25+30 samples");
  assert.strictEqual(aDirect.count, 20, "a directly calls target with 20 samples");
  // b's callers: a (25) and c (30).
  assert.strictEqual(b.children.size, 2, "b is called by a and c");
  assert.strictEqual(b.children.get("a").count, 25, "a→b→target = 25");
  assert.strictEqual(b.children.get("c").count, 30, "c→b→target = 30");
});

test("buildInspect: recursion is not double-counted", () => {
  // r → rec → rec → rec(self 50)
  const inner = node("rec", { count: 50, self: 50 });
  const mid = node("rec", { self: 0, children: [inner] });
  const outer = node("rec", { self: 0, children: [mid] });
  const root = node("(all)", { children: [node("r", { children: [outer] })] });
  const ins = FG.buildInspect(root, inner);
  assert.strictEqual(ins.total, 50, "inclusive counted once at top-most rec");
  assert.strictEqual(ins.self, 50, "self still summed across nested frames");
  assert.strictEqual(ins.occurrences, 1, "only the top-most rec is a call site");
  // Callers: only r (the caller of the top-most rec), not rec-under-rec.
  assert.strictEqual(ins.callers.children.size, 1, "single immediate caller");
  assert.ok(ins.callers.children.get("r"), "r is the caller");
  // Callees: the recursive self-calls remain visible below focus.
  assert.ok(ins.callees.children.has("rec"), "recursion visible in callees");
});

test("buildInspect: matches by fullName (aggregated trees may lack it)", () => {
  // Aggregated/API trees have no fullName; matching must fall back to name.
  const leaf1 = { name: "foo", count: 10, self: 10, children: new Map() };
  const leaf2 = { name: "foo", count: 5, self: 5, children: new Map() };
  const p = { name: "p", count: 15, self: 0, children: new Map([["foo", leaf1]]) };
  const q = { name: "q", count: 5, self: 0, children: new Map([["foo", leaf2]]) };
  const root = { name: "(all)", count: 20, self: 0, children: new Map([["p", p], ["q", q]]) };
  const ins = FG.buildInspect(root, leaf1);
  assert.strictEqual(ins.total, 15, "foo inclusive = 10+5");
  assert.strictEqual(ins.callers.children.size, 2, "p and q both call foo");
});

test("collectSearchResults: groups by function with sizes and sites", () => {
  const root = fixture();
  const results = FG.collectSearchResults(root, "target");
  assert.strictEqual(results.length, 1, "one matching function");
  const r = results[0];
  assert.strictEqual(r.name, "target");
  assert.strictEqual(r.total, 75, "inclusive across all sites");
  assert.strictEqual(r.self, 75, "self across all sites");
  assert.strictEqual(r.sites, 3, "three call sites");
});

test("collectSearchResults: substring match spans multiple functions", () => {
  const root = fixture();
  const results = FG.collectSearchResults(root, "b");
  const b = results.find((x) => x.name === "b");
  assert.ok(b, "b matched");
  assert.strictEqual(b.total, 70, "b inclusive = 30 (under a) + 40 (under c)");
  assert.strictEqual(b.self, 15, "b self = 5 + 10");
  assert.strictEqual(b.sites, 2, "two b call sites");
});

test("collectSearchResults: no matches → empty array", () => {
  const root = fixture();
  assert.deepStrictEqual(FG.collectSearchResults(root, "zzz"), []);
});

test("searchAggregate: inclusive coverage matches a frame's inspect total", () => {
  const root = fixture();
  // "target" appears in 3 places totalling 75 inclusive samples; rootTotal 100.
  const agg = FG.searchAggregate(root, "target");
  assert.strictEqual(agg.functions, 1, "one distinct matching function");
  assert.strictEqual(agg.covered, 75, "inclusive union = 25+20+30");
  assert.strictEqual(agg.rootTotal, 100);
  // Must equal what buildInspect reports for the same frame (no self/inclusive
  // mismatch between the summary line, the dropdown, and the focus band).
  const focus = root.children.get("a").children.get("target");
  const ins = FG.buildInspect(root, focus);
  assert.strictEqual(agg.covered, ins.total, "summary coverage == inspect total");
});

test("searchAggregate: nested matches are not double-counted", () => {
  // r → rec(inclusive 50) → rec → rec(self 50). Substring "rec" matches all
  // three nested frames, but coverage counts the top-most once = 50.
  const inner = node("rec", { count: 50, self: 50 });
  const mid = node("rec", { self: 0, children: [inner] });
  const outer = node("rec", { self: 0, children: [mid] });
  const root = node("(all)", { children: [node("r", { children: [outer] })] });
  const agg = FG.searchAggregate(root, "rec");
  assert.strictEqual(agg.functions, 1, "'rec' is one distinct function");
  assert.strictEqual(agg.covered, 50, "counted once at the top-most rec");
});

test("searchAggregate: distinct-function count matches dropdown", () => {
  const root = fixture();
  // "b" matches only function b; but a broad query can hit several functions.
  const aggB = FG.searchAggregate(root, "b");
  assert.strictEqual(aggB.functions, 1);
  const results = FG.collectSearchResults(root, "b");
  assert.strictEqual(aggB.functions, results.length,
    "summary 'N frames' equals dropdown row count");
});

test("buildInspect + search span multiple roots (worker + off-worker lanes)", () => {
  // Two independent lanes, both containing `target` under different callers.
  const workerTarget = node("target", { count: 25, self: 25 });
  const worker = node("(all)", {
    children: [node("w", { children: [workerTarget] })],
  });
  const offTarget = node("target", { count: 15, self: 15 });
  const offworker = node("(all)", {
    children: [node("o", { children: [offTarget] })],
  });
  const ins = FG.buildInspect([worker, offworker], workerTarget);
  assert.strictEqual(ins.total, 40, "target inclusive across both lanes = 25+15");
  assert.strictEqual(ins.rootTotal, 40, "rootTotal sums both lane roots");
  assert.strictEqual(ins.callers.children.size, 2, "callers w and o from both lanes");
  assert.ok(ins.callers.children.get("w") && ins.callers.children.get("o"));

  const results = FG.collectSearchResults([worker, offworker], "target");
  assert.strictEqual(results.length, 1);
  assert.strictEqual(results[0].total, 40, "search total spans both lanes");
  assert.strictEqual(results[0].sites, 2, "one site per lane");
});

summarize();
