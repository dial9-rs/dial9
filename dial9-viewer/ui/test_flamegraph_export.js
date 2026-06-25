#!/usr/bin/env node
"use strict";

// Tests for flamegraph_export.js — the folded-stacks serializer and the
// interactive-SVG generator. These run against both a hand-built tree (for
// exact-output assertions) and a tree built from demo-trace.bin via
// buildFlamegraphTree (for the real node shape), so the export stays in
// lockstep with the analysis layer.

const fs = require("fs");
const path = require("path");
const { assert, test, testAsync, summarize } = require("./test_harness.js");
const FE = require("./flamegraph_export.js");
const { parseTrace } = require("./trace_parser.js");
const TraceAnalysis = require("./trace_analysis.js");

// Build a small tree matching the buildFlamegraphTree output shape.
function node(name, count, self, children) {
  const m = new Map();
  for (const c of children || []) m.set(c.fullName || c.name, c);
  return { name, fullName: name, location: null, count, self: self, children: m };
}

function makeTree() {
  // (all) 100
  //  └─ main 100 (self 0)
  //      ├─ work 60 (self 40)
  //      │   └─ inner 20 (self 20)
  //      └─ idle 40 (self 40)
  const inner = node("inner", 20, 20, []);
  const work = node("work", 60, 40, [inner]);
  const idle = node("idle", 40, 40, []);
  const main = node("main", 100, 0, [work, idle]);
  return node("(all)", 100, 0, [main]);
}

// ── Folded stacks ──────────────────────────────────────────────────────
test("treeToFolded emits one line per self-bearing leaf path", () => {
  const folded = FE.treeToFolded(makeTree());
  const lines = folded.trim().split("\n").sort();
  assert.deepStrictEqual(lines, [
    "main;idle 40",
    "main;work 40",
    "main;work;inner 20",
  ]);
});

test("treeToFolded omits the synthetic (all) root from paths", () => {
  assert.ok(!FE.treeToFolded(makeTree()).includes("(all)"));
});

test("treeToFolded self-weights sum to the root count", () => {
  const sum = FE.treeToFolded(makeTree())
    .trim()
    .split("\n")
    .reduce((acc, l) => acc + Number(l.slice(l.lastIndexOf(" ") + 1)), 0);
  assert.strictEqual(sum, 100);
});

test("treeToFolded on empty/null tree is a safe empty string", () => {
  assert.strictEqual(FE.treeToFolded(null), "");
  assert.strictEqual(FE.treeToFolded(node("(all)", 0, 0, [])), "");
});

test("treeToFolded on an all-internal tree (no self weight) is empty", () => {
  // The folded-export concat in flamegraph.js skips panels whose treeToFolded
  // is "" so it never emits a dangling "# label" header; this is that precondition.
  const internal = node("(all)", 50, 0, [node("a", 50, 0, [node("b", 0, 0, [])])]);
  assert.strictEqual(FE.treeToFolded(internal), "");
});

// ── buildExportRoot (panel merge) ────────────────────────────────────────
test("buildExportRoot returns the single panel's tree unchanged", () => {
  const t = makeTree();
  const root = FE.buildExportRoot([{ label: "Worker threads", tree: t }]);
  assert.strictEqual(root, t, "single panel should pass through by identity");
});

test("buildExportRoot synthesizes a combined root for multiple panels", () => {
  const a = makeTree();
  const b = node("(all)", 50, 0, [node("other", 50, 50, [])]);
  const root = FE.buildExportRoot([
    { label: "Worker threads", tree: a },
    { label: "Off-worker", tree: b },
  ]);
  assert.strictEqual(root.count, 150, "root count = sum of panels");
  assert.strictEqual(root.children.size, 2, "one child frame per panel");
  assert.ok(root.children.has("[Worker threads]"));
  assert.ok(root.children.has("[Off-worker]"));
  // Must NOT mutate inputs.
  assert.strictEqual(a.count, 100);
});

test("buildExportRoot returns null when there is no data", () => {
  assert.strictEqual(FE.buildExportRoot([]), null);
  assert.strictEqual(FE.buildExportRoot([{ label: "x", tree: node("(all)", 0, 0, []) }]), null);
});

// ── layout ──────────────────────────────────────────────────────────────
test("layoutTree packs children left-to-right by descending count", () => {
  const { frames, maxDepth } = FE.layoutTree(makeTree(), 0);
  assert.strictEqual(maxDepth, 3, "(all)=0, main=1, work/idle=2, inner=3");
  // The root frame must exist at depth 0 spanning the full count range.
  const root = frames.find((f) => f.depth === 0);
  assert.strictEqual(root.sTime, 0);
  assert.strictEqual(root.eTime, 100);
  // 'work' (60) is wider so it must be placed before 'idle' (40): work starts at 0.
  const depth2 = frames.filter((f) => f.depth === 2).sort((a, b) => a.sTime - b.sTime);
  assert.strictEqual(depth2[0].node.name, "work");
});

// ── Interactive SVG ──────────────────────────────────────────────────────
function svgOf(panels, opts) {
  return FE.treeToInteractiveSvg(panels, opts);
}

test("treeToInteractiveSvg produces a well-formed standalone svg", () => {
  const svg = svgOf([{ label: "Worker threads", tree: makeTree() }], { title: "T" });
  assert.ok(svg.includes("<svg "), "has <svg>");
  assert.ok(svg.trim().endsWith("</svg>"), "ends with </svg>");
  assert.ok(svg.includes('xmlns="http://www.w3.org/2000/svg"'), "svg namespace");
  assert.ok(svg.includes('onload="init(evt)"'), "wires the init() entrypoint");
});

test("treeToInteractiveSvg embeds the interactive script + chrome", () => {
  const svg = svgOf([{ label: "W", tree: makeTree() }], {});
  // The interactive behaviors must all be present in the embedded script.
  for (const fn of ["function zoom(", "function search(", "function unzoom(",
    "function toggle_ignorecase(", "function init(", "function update_text("]) {
    assert.ok(svg.includes(fn), `embedded script defines ${fn}`);
  }
  // Clickable chrome elements the script binds to by id.
  for (const id of ['id="unzoom"', 'id="search"', 'id="ignorecase"', 'id="details"', 'id="matched"', 'id="frames"']) {
    assert.ok(svg.includes(id), `has element ${id}`);
  }
  // CDATA wrapping so the JS survives XML parsing.
  assert.ok(svg.includes("<![CDATA[") && svg.includes("]]>"), "script is CDATA-wrapped");
});

test("treeToInteractiveSvg frames carry title+rect+text in a <g> (zoom contract)", () => {
  const svg = svgOf([{ label: "W", tree: makeTree() }], {});
  // The embedded JS relies on each frame being <g><title/><rect/><text/></g>.
  assert.ok(/<g>\s*<title>[^<]*<\/title>\s*<rect /.test(svg), "g>title>rect order");
  assert.ok(svg.includes("work ("), "frame title shows func + samples");
  assert.ok(svg.includes("all (100 samples, 100%)"), "root frame labeled 'all ... 100%'");
});

test("treeToInteractiveSvg escapes XML metacharacters in frame names", () => {
  const t = node("(all)", 10, 0, [node('a<b>&"x', 10, 10, [])]);
  const svg = svgOf([{ label: "L", tree: t }], {});
  assert.ok(!svg.includes("a<b>"), "raw < must be escaped");
  assert.ok(svg.includes("&lt;") && svg.includes("&amp;"), "uses entities");
});

test("treeToInteractiveSvg defaults frame weights to 'samples'", () => {
  // Frame weight is the node's total count (work=60), not its self weight.
  const svg = svgOf([{ label: "W", tree: makeTree() }], { title: "T" });
  assert.ok(svg.includes("work (60 samples,"), "leaf labeled in samples");
  assert.ok(svg.includes("all (100 samples, 100%)"), "root labeled in samples");
});

test("treeToInteractiveSvg uses formatValue for frame weights (heap units)", () => {
  // Heap exports pass a formatter that renders bytes/allocs instead of samples.
  const fmt = (count) => `~${count} B`;
  const svg = svgOf([{ label: "W", tree: makeTree() }], { title: "T", formatValue: fmt });
  assert.ok(svg.includes("all (~100 B, 100%)"), "root uses formatValue");
  assert.ok(svg.includes("work (~60 B,"), "leaf uses formatValue");
  assert.ok(!/\d samples/.test(svg), "no hardcoded 'samples' when formatValue is supplied");
});

test("treeToInteractiveSvg with no usable panels renders a placeholder, not a crash", () => {
  const svg = svgOf([{ label: "L", tree: node("(all)", 0, 0, []) }], {});
  assert.ok(svg.includes("<svg ") && svg.includes("No data to export"));
});

test("treeToInteractiveSvg merges multiple panels into one searchable graph", () => {
  const a = makeTree();
  const b = node("(all)", 50, 0, [node("other", 50, 50, [])]);
  const svg = svgOf([
    { label: "Worker threads", tree: a },
    { label: "Off-worker", tree: b },
  ], { title: "combined" });
  assert.ok(svg.includes("[Worker threads]"), "worker panel frame present");
  assert.ok(svg.includes("[Off-worker]"), "off-worker panel frame present");
  assert.ok(svg.includes("all (150 samples, 100%)"), "combined root = 150 samples");
});

// Regression: a last child that exactly covers its parent's right edge must
// emit an identical right edge (x+width) after rounding, or the embedded zoom's
// ancestor test (fudge=0.0001) fails and the ancestor row blanks out on zoom.
// We use sample counts that force fractional pixel positions.
test("coinciding parent/child right edges round identically", () => {
  // parent=37 split into first=20, last=17 (last covers parent's right edge).
  // 37 over a 1200px-wide graph yields non-grid pixel positions.
  const last = node("last", 17, 17, []);
  const first = node("first", 20, 20, []);
  const parent = node("parent", 37, 0, [first, last]);
  const root = node("(all)", 37, 0, [parent]);
  const svg = svgOf([{ label: "W", tree: root }], {});
  // Pull every FRAME rect's x and width (frame rects carry rx="2"; the
  // background rect does not, so this excludes it).
  const rects = [...svg.matchAll(/<rect x="([\d.]+)" y="[\d.]+" width="([\d.]+)"[^/]*rx="2"/g)]
    .map((m) => ({ x: Number(m[1]), w: Number(m[2]), right: Number(m[1]) + Number(m[2]) }));
  assert.ok(rects.length >= 4, `expected >=4 rects, got ${rects.length}`);
  // The widest rects (root, parent) and the last child should share a right edge.
  const maxRight = Math.max(...rects.map((r) => r.right));
  const sharing = rects.filter((r) => Math.abs(r.right - maxRight) < 1e-9);
  // root + parent + last child = at least 3 frames share the max right edge.
  assert.ok(sharing.length >= 3,
    `expected >=3 frames to share the right edge exactly, got ${sharing.length} (rights: ${rects.map((r) => r.right.toFixed(1)).join(",")})`);
});

// ── filename ──────────────────────────────────────────────────────────────
test("filenameStem sanitizes labels into safe stems", () => {
  assert.strictEqual(FE.filenameStem("Flamegraph — Magnus @ host-1"), "Magnus_host-1");
  assert.strictEqual(FE.filenameStem(""), "flamegraph");
  assert.strictEqual(FE.filenameStem("a/b\\c d"), "a_b_c_d");
});

test("filenameStem never produces a dotfile or punctuation-only name", () => {
  // Leading/trailing dots must be trimmed so the download is not a hidden file
  // and dot-only inputs fall back to the default stem.
  assert.strictEqual(FE.filenameStem(".."), "flamegraph");
  assert.strictEqual(FE.filenameStem("..."), "flamegraph");
  assert.strictEqual(FE.filenameStem(".cache"), "cache");
  assert.strictEqual(FE.filenameStem("a..."), "a");
  assert.strictEqual(FE.filenameStem("Flamegraph — .."), "flamegraph");
});

async function main() {
  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (fs.existsSync(tracePath)) {
    await testAsync("exports a real tree built from demo-trace.bin", async () => {
      const trace = await parseTrace(fs.readFileSync(tracePath));
      const samples = trace.cpuSamples.filter((s) => s.callchain.length > 0 && s.source !== 1);
      assert.ok(samples.length > 0, "demo trace has CPU samples");
      const tree = TraceAnalysis.buildFlamegraphTree(samples, trace.callframeSymbols);

      const folded = FE.treeToFolded(tree);
      assert.ok(folded.length > 0, "folded output is non-empty");
      for (const line of folded.trim().split("\n")) {
        assert.ok(/ \d+$/.test(line), `folded line well-formed: ${line}`);
      }

      const svg = FE.treeToInteractiveSvg([{ label: "Worker threads", tree }], { title: "demo" });
      assert.ok(svg.includes("<svg ") && svg.trim().endsWith("</svg>"));
      assert.ok(svg.includes('onload="init(evt)"') && svg.includes("function zoom("));
      // Sanity: many frames rendered from a real trace.
      const frameCount = (svg.match(/<g>\s*<title>/g) || []).length;
      assert.ok(frameCount > 10, `expected many frames, got ${frameCount}`);
    });
  } else {
    console.log("(skipping demo-trace.bin test — file not present)");
  }
  summarize();
}

main();
