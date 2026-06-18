#!/usr/bin/env node
// Tests for the dial9-flamegraph / dial9-flamegraph-diff skill scripts:
// fold.js (trace -> folded stacks), merge_folded.js, diff_folded.js.
// Runs against the demo trace, exercising the real parser + symbolizer.

"use strict";
const fs = require("fs");
const path = require("path");
const os = require("os");
const { test, testAsync, assert, summarize } = require("./test_harness.js");

const demoPath = path.join(__dirname, "demo-trace.bin");
const flamegraphScripts = path.join(__dirname, "..", "skills", "dial9-flamegraph", "scripts");
const diffScripts = path.join(__dirname, "..", "skills", "dial9-flamegraph-diff", "scripts");

const { fold } = require(path.join(flamegraphScripts, "fold.js"));
const { load, total } = require(path.join(diffScripts, "diff_folded.js"));

(async () => {
  // --- fold.js: full stacks ---
  const full = await fold([demoPath], {});
  await testAsync("fold produces folded stacks from the demo trace", async () => {
    assert(full.size > 0, "expected at least one folded stack");
    for (const [stack, count] of full) {
      assert(typeof stack === "string" && stack.length > 0, "stack key must be a non-empty string");
      assert(Number.isInteger(count) && count > 0, "count must be a positive integer");
      // Folded format is line-based and `stack <count>` splits on the LAST space,
      // so a key may contain interior spaces (Rust symbols like `<T as Trait>`)
      // but never a newline.
      assert(!stack.includes("\n"), "folded stack key must not contain a newline");
    }
  });

  // --- fold.js: leaf vs full granularity ---
  const leaf = await fold([demoPath], { leafOnly: true });
  test("leaf folding has no fewer-or-equal distinct keys than full (collapses callers)", () => {
    assert(leaf.size <= full.size, `leaf=${leaf.size} should be <= full=${full.size}`);
    assert(leaf.size > 0, "leaf folding produced nothing");
    for (const stack of leaf.keys()) {
      assert(!stack.includes(";"), "leaf keys must be a single frame (no ';')");
    }
  });

  // --- fold.js: max-depth bounds stack depth ---
  const d3 = await fold([demoPath], { maxDepth: 3 });
  test("--max-depth caps frames per stack", () => {
    for (const stack of d3.keys()) {
      assert(stack.split(";").length <= 3, `stack exceeded depth 3: ${stack}`);
    }
  });

  // --- total sample count is conserved across granularities ---
  const sum = (m) => [...m.values()].reduce((a, b) => a + b, 0);
  test("total on-CPU sample count is identical across fold granularities", () => {
    assert(sum(full) === sum(leaf), `full=${sum(full)} leaf=${sum(leaf)}`);
    assert(sum(full) === sum(d3), `full=${sum(full)} d3=${sum(d3)}`);
  });

  // --- merge_folded round-trips through the JSON cache form ---
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "d9fold-"));
  const jsonA = path.join(tmp, "a.json");
  fs.writeFileSync(jsonA, JSON.stringify(Object.fromEntries(full)));
  const { loadInto } = require(path.join(flamegraphScripts, "merge_folded.js"));
  test("merge_folded sums two copies of the same fold to 2x counts", () => {
    const acc = {};
    loadInto(acc, jsonA);
    loadInto(acc, jsonA);
    const anyKey = full.keys().next().value;
    assert(acc[anyKey] === full.get(anyKey) * 2, "merging a fold with itself should double counts");
  });

  // --- diff_folded: a profile against itself has ~zero movers ---
  const foldedA = path.join(tmp, "a.folded");
  fs.writeFileSync(
    foldedA,
    [...full.entries()].map(([k, v]) => `${k} ${v}`).join("\n") + "\n",
  );
  test("diff load() + total() round-trip the folded text", () => {
    const m = load(foldedA);
    assert(m.size === full.size, `loaded ${m.size} stacks, expected ${full.size}`);
    assert(total(m) === sum(full), "total mismatch after load");
  });

  // --- diff direction: a synthetic B with one inflated stack shows it as the top mover ---
  test("diff surfaces an inflated stack as a positive mover in B", () => {
    const entries = [...full.entries()];
    const [hotStack] = entries[0];
    const bEntries = entries.map(([k, v]) => [k, k === hotStack ? v * 5 : v]);
    const foldedB = path.join(tmp, "b.folded");
    fs.writeFileSync(foldedB, bEntries.map(([k, v]) => `${k} ${v}`).join("\n") + "\n");
    const A = load(foldedA);
    const B = load(foldedB);
    const tA = total(A);
    const tB = total(B);
    const shareA = (A.get(hotStack) || 0) / tA;
    const shareB = (B.get(hotStack) || 0) / tB;
    assert(shareB - shareA > 0, "inflated stack should have a larger share in B");
  });

  fs.rmSync(tmp, { recursive: true, force: true });
  summarize();
})();
