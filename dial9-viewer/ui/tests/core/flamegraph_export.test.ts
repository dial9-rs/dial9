// Tests for flamegraph_export.js — the folded-stacks serializer and the
// interactive-SVG generator. These run against both a hand-built tree (for
// exact-output assertions) and a tree built from demo-trace.bin via
// buildFlamegraphTree (for the real node shape), so the export stays in
// lockstep with the analysis layer.
//
// Migrated from test_flamegraph_export.js (T11); frozen core loaded via
// createRequire (see format.test.ts for the rationale).

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);

interface TreeNode {
  name: string;
  fullName: string;
  location: string | null;
  count: number;
  self: number;
  children: Map<string, TreeNode>;
}

interface Panel {
  label: string;
  tree: TreeNode | null;
}

interface LayoutFrame {
  depth: number;
  sTime: number;
  eTime: number;
  node: TreeNode;
}

const FE = require("../../flamegraph_export.js") as {
  treeToFolded: (tree: TreeNode | null) => string;
  buildExportRoot: (panels: Panel[]) => TreeNode | null;
  layoutTree: (
    tree: TreeNode,
    depth: number,
  ) => { frames: LayoutFrame[]; maxDepth: number };
  treeToInteractiveSvg: (
    panels: Panel[],
    opts: { title?: string; formatValue?: (count: number) => string },
  ) => string;
  filenameStem: (label: string) => string;
};
const { parseTrace } = require("../../trace_parser.js") as {
  parseTrace: (buf: Buffer) => Promise<{
    cpuSamples: { callchain: string[]; source: number }[];
    callframeSymbols: Map<unknown, unknown>;
  }>;
};
const TraceAnalysis = require("../../trace_analysis.js") as {
  buildFlamegraphTree: (
    samples: { callchain: string[] }[],
    symbols: Map<unknown, unknown>,
  ) => TreeNode;
};

// Build a small tree matching the buildFlamegraphTree output shape.
function node(
  name: string,
  count: number,
  self: number,
  children: TreeNode[],
): TreeNode {
  const m = new Map<string, TreeNode>();
  for (const c of children || []) m.set(c.fullName || c.name, c);
  return { name, fullName: name, location: null, count, self: self, children: m };
}

function makeTree(): TreeNode {
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
describe("treeToFolded", () => {
  it("emits one line per self-bearing leaf path", () => {
    const folded = FE.treeToFolded(makeTree());
    const lines = folded.trim().split("\n").sort();
    expect(lines).toEqual([
      "main;idle 40",
      "main;work 40",
      "main;work;inner 20",
    ]);
  });

  it("omits the synthetic (all) root from paths", () => {
    expect(FE.treeToFolded(makeTree()).includes("(all)")).toBe(false);
  });

  it("self-weights sum to the root count", () => {
    const sum = FE.treeToFolded(makeTree())
      .trim()
      .split("\n")
      .reduce((acc, l) => acc + Number(l.slice(l.lastIndexOf(" ") + 1)), 0);
    expect(sum).toBe(100);
  });

  it("on empty/null tree is a safe empty string", () => {
    expect(FE.treeToFolded(null)).toBe("");
    expect(FE.treeToFolded(node("(all)", 0, 0, []))).toBe("");
  });

  it("on an all-internal tree (no self weight) is empty", () => {
    // The folded-export concat in flamegraph.js skips panels whose treeToFolded
    // is "" so it never emits a dangling "# label" header; this is that precondition.
    const internal = node("(all)", 50, 0, [node("a", 50, 0, [node("b", 0, 0, [])])]);
    expect(FE.treeToFolded(internal)).toBe("");
  });
});

// ── buildExportRoot (panel merge) ────────────────────────────────────────
describe("buildExportRoot", () => {
  it("returns the single panel's tree unchanged", () => {
    const t = makeTree();
    const root = FE.buildExportRoot([{ label: "Worker threads", tree: t }]);
    expect(root, "single panel should pass through by identity").toBe(t);
  });

  it("synthesizes a combined root for multiple panels", () => {
    const a = makeTree();
    const b = node("(all)", 50, 0, [node("other", 50, 50, [])]);
    const root = FE.buildExportRoot([
      { label: "Worker threads", tree: a },
      { label: "Off-worker", tree: b },
    ])!;
    expect(root.count, "root count = sum of panels").toBe(150);
    expect(root.children.size, "one child frame per panel").toBe(2);
    expect(root.children.has("[Worker threads]")).toBe(true);
    expect(root.children.has("[Off-worker]")).toBe(true);
    // Must NOT mutate inputs.
    expect(a.count).toBe(100);
  });

  it("returns null when there is no data", () => {
    expect(FE.buildExportRoot([])).toBeNull();
    expect(
      FE.buildExportRoot([{ label: "x", tree: node("(all)", 0, 0, []) }]),
    ).toBeNull();
  });
});

// ── layout ──────────────────────────────────────────────────────────────
describe("layoutTree", () => {
  it("packs children left-to-right by descending count", () => {
    const { frames, maxDepth } = FE.layoutTree(makeTree(), 0);
    expect(maxDepth, "(all)=0, main=1, work/idle=2, inner=3").toBe(3);
    // The root frame must exist at depth 0 spanning the full count range.
    const root = frames.find((f) => f.depth === 0)!;
    expect(root.sTime).toBe(0);
    expect(root.eTime).toBe(100);
    // 'work' (60) is wider so it must be placed before 'idle' (40): work starts at 0.
    const depth2 = frames
      .filter((f) => f.depth === 2)
      .sort((a, b) => a.sTime - b.sTime);
    expect(depth2[0]!.node.name).toBe("work");
  });
});

// ── Interactive SVG ──────────────────────────────────────────────────────
function svgOf(
  panels: Panel[],
  opts: { title?: string; formatValue?: (count: number) => string },
): string {
  return FE.treeToInteractiveSvg(panels, opts);
}

describe("treeToInteractiveSvg", () => {
  it("produces a well-formed standalone svg", () => {
    const svg = svgOf([{ label: "Worker threads", tree: makeTree() }], { title: "T" });
    expect(svg.includes("<svg "), "has <svg>").toBe(true);
    expect(svg.trim().endsWith("</svg>"), "ends with </svg>").toBe(true);
    expect(
      svg.includes('xmlns="http://www.w3.org/2000/svg"'),
      "svg namespace",
    ).toBe(true);
    expect(svg.includes('onload="init(evt)"'), "wires the init() entrypoint").toBe(
      true,
    );
  });

  it("embeds the interactive script + chrome", () => {
    const svg = svgOf([{ label: "W", tree: makeTree() }], {});
    // The interactive behaviors must all be present in the embedded script.
    for (const fn of [
      "function zoom(",
      "function search(",
      "function unzoom(",
      "function toggle_ignorecase(",
      "function init(",
      "function update_text(",
    ]) {
      expect(svg.includes(fn), `embedded script defines ${fn}`).toBe(true);
    }
    // Clickable chrome elements the script binds to by id.
    for (const id of [
      'id="unzoom"',
      'id="search"',
      'id="ignorecase"',
      'id="details"',
      'id="matched"',
      'id="frames"',
    ]) {
      expect(svg.includes(id), `has element ${id}`).toBe(true);
    }
    // CDATA wrapping so the JS survives XML parsing.
    expect(
      svg.includes("<![CDATA[") && svg.includes("]]>"),
      "script is CDATA-wrapped",
    ).toBe(true);
  });

  it("frames carry title+rect+text in a <g> (zoom contract)", () => {
    const svg = svgOf([{ label: "W", tree: makeTree() }], {});
    // The embedded JS relies on each frame being <g><title/><rect/><text/></g>.
    expect(
      /<g>\s*<title>[^<]*<\/title>\s*<rect /.test(svg),
      "g>title>rect order",
    ).toBe(true);
    expect(svg.includes("work ("), "frame title shows func + samples").toBe(true);
    expect(
      svg.includes("all (100 samples, 100%)"),
      "root frame labeled 'all ... 100%'",
    ).toBe(true);
  });

  it("escapes XML metacharacters in frame names", () => {
    const t = node("(all)", 10, 0, [node('a<b>&"x', 10, 10, [])]);
    const svg = svgOf([{ label: "L", tree: t }], {});
    expect(svg.includes("a<b>"), "raw < must be escaped").toBe(false);
    expect(svg.includes("&lt;") && svg.includes("&amp;"), "uses entities").toBe(true);
  });

  it("defaults frame weights to 'samples'", () => {
    // Frame weight is the node's total count (work=60), not its self weight.
    const svg = svgOf([{ label: "W", tree: makeTree() }], { title: "T" });
    expect(svg.includes("work (60 samples,"), "leaf labeled in samples").toBe(true);
    expect(
      svg.includes("all (100 samples, 100%)"),
      "root labeled in samples",
    ).toBe(true);
  });

  it("uses formatValue for frame weights (heap units)", () => {
    // Heap exports pass a formatter that renders bytes/allocs instead of samples.
    const fmt = (count: number) => `~${count} B`;
    const svg = svgOf([{ label: "W", tree: makeTree() }], {
      title: "T",
      formatValue: fmt,
    });
    expect(svg.includes("all (~100 B, 100%)"), "root uses formatValue").toBe(true);
    expect(svg.includes("work (~60 B,"), "leaf uses formatValue").toBe(true);
    expect(
      /\d samples/.test(svg),
      "no hardcoded 'samples' when formatValue is supplied",
    ).toBe(false);
  });

  it("with no usable panels renders a placeholder, not a crash", () => {
    const svg = svgOf([{ label: "L", tree: node("(all)", 0, 0, []) }], {});
    expect(svg.includes("<svg ") && svg.includes("No data to export")).toBe(true);
  });

  it("merges multiple panels into one searchable graph", () => {
    const a = makeTree();
    const b = node("(all)", 50, 0, [node("other", 50, 50, [])]);
    const svg = svgOf(
      [
        { label: "Worker threads", tree: a },
        { label: "Off-worker", tree: b },
      ],
      { title: "combined" },
    );
    expect(svg.includes("[Worker threads]"), "worker panel frame present").toBe(true);
    expect(svg.includes("[Off-worker]"), "off-worker panel frame present").toBe(true);
    expect(
      svg.includes("all (150 samples, 100%)"),
      "combined root = 150 samples",
    ).toBe(true);
  });

  // Regression: a last child that exactly covers its parent's right edge must
  // emit an identical right edge (x+width) after rounding, or the embedded zoom's
  // ancestor test (fudge=0.0001) fails and the ancestor row blanks out on zoom.
  // We use sample counts that force fractional pixel positions.
  it("coinciding parent/child right edges round identically", () => {
    // parent=37 split into first=20, last=17 (last covers parent's right edge).
    // 37 over a 1200px-wide graph yields non-grid pixel positions.
    const last = node("last", 17, 17, []);
    const first = node("first", 20, 20, []);
    const parent = node("parent", 37, 0, [first, last]);
    const root = node("(all)", 37, 0, [parent]);
    const svg = svgOf([{ label: "W", tree: root }], {});
    // Pull every FRAME rect's x and width (frame rects carry rx="2"; the
    // background rect does not, so this excludes it).
    const rects = [
      ...svg.matchAll(/<rect x="([\d.]+)" y="[\d.]+" width="([\d.]+)"[^/]*rx="2"/g),
    ].map((m) => ({
      x: Number(m[1]),
      w: Number(m[2]),
      right: Number(m[1]) + Number(m[2]),
    }));
    expect(rects.length, `expected >=4 rects, got ${rects.length}`).toBeGreaterThanOrEqual(4);
    // The widest rects (root, parent) and the last child should share a right edge.
    const maxRight = Math.max(...rects.map((r) => r.right));
    const sharing = rects.filter((r) => Math.abs(r.right - maxRight) < 1e-9);
    // root + parent + last child = at least 3 frames share the max right edge.
    expect(
      sharing.length,
      `expected >=3 frames to share the right edge exactly, got ${sharing.length} (rights: ${rects
        .map((r) => r.right.toFixed(1))
        .join(",")})`,
    ).toBeGreaterThanOrEqual(3);
  });
});

// ── filename ──────────────────────────────────────────────────────────────
describe("filenameStem", () => {
  it("sanitizes labels into safe stems", () => {
    expect(FE.filenameStem("Flamegraph — Magnus @ host-1")).toBe("Magnus_host-1");
    expect(FE.filenameStem("")).toBe("flamegraph");
    expect(FE.filenameStem("a/b\\c d")).toBe("a_b_c_d");
  });

  it("never produces a dotfile or punctuation-only name", () => {
    // Leading/trailing dots must be trimmed so the download is not a hidden file
    // and dot-only inputs fall back to the default stem.
    expect(FE.filenameStem("..")).toBe("flamegraph");
    expect(FE.filenameStem("...")).toBe("flamegraph");
    expect(FE.filenameStem(".cache")).toBe("cache");
    expect(FE.filenameStem("a...")).toBe("a");
    expect(FE.filenameStem("Flamegraph — ..")).toBe("flamegraph");
  });
});

// ── Real tree from the demo trace ─────────────────────────────────────────
const tracePath = fileURLToPath(
  new URL("../../public/demo-trace.bin", import.meta.url),
);

describe("demo-trace export", { timeout: 60_000 }, () => {
  it.skipIf(!existsSync(tracePath))(
    "exports a real tree built from demo-trace.bin",
    async () => {
      const trace = await parseTrace(readFileSync(tracePath));
      const samples = trace.cpuSamples.filter(
        (s) => s.callchain.length > 0 && s.source !== 1,
      );
      expect(samples.length, "demo trace has CPU samples").toBeGreaterThan(0);
      const tree = TraceAnalysis.buildFlamegraphTree(samples, trace.callframeSymbols);

      const folded = FE.treeToFolded(tree);
      expect(folded.length, "folded output is non-empty").toBeGreaterThan(0);
      for (const line of folded.trim().split("\n")) {
        expect(/ \d+$/.test(line), `folded line well-formed: ${line}`).toBe(true);
      }

      const svg = FE.treeToInteractiveSvg(
        [{ label: "Worker threads", tree }],
        { title: "demo" },
      );
      expect(svg.includes("<svg ") && svg.trim().endsWith("</svg>")).toBe(true);
      expect(svg.includes('onload="init(evt)"') && svg.includes("function zoom(")).toBe(
        true,
      );
      // Sanity: many frames rendered from a real trace.
      const frameCount = (svg.match(/<g>\s*<title>/g) || []).length;
      expect(frameCount, `expected many frames, got ${frameCount}`).toBeGreaterThan(10);
    },
  );
});
