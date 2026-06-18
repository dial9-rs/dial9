#!/usr/bin/env node
// fold.js — collapse dial9 trace segment(s) into FOLDED stacks.
//
//   <root>;<caller>;...;<leaf> <count>
//
// "Folded" is the lingua franca of flamegraph tooling: flamegraph.pl, inferno
// (inferno-flamegraph / inferno-diff-folded), and speedscope all read it. So
// this one script unlocks SVG flamegraphs, differential flamegraphs, and
// programmatic stack analysis without any dial9-specific renderer.
//
// On-CPU samples only (cpuSample.source === 0). Inlined frames are expanded in
// place via the toolkit's symbolizeChain. The trace stores callchains leaf-first;
// folded format is root-first, so each chain is reversed.
//
// Usage:
//   node fold.js <file-or-dir> [more files/dirs...] > out.folded
//   node fold.js --max-depth N <...>   # keep only the N frames nearest the leaf
//   node fold.js --leaf <...>          # collapse to leaf symbol only (1-deep hotspots)
//   node fold.js --json <...>          # emit {stack: count} JSON instead of folded text
//
// The --json form is for caching: parsing a segment is the slow step (~10s),
// so fold each segment once to JSON, then merge cheaply (see merge_folded.js)
// to render any subset/union without re-parsing.

const fs = require("fs");
const path = require("path");

// Resolve toolkit files: sibling (unpacked skill / `dial9 agents toolkit`),
// the dial9-toolkit skill scripts, or the ui/ source tree.
function resolve(name) {
  const sibling = path.resolve(__dirname, name);
  if (fs.existsSync(sibling)) return sibling;
  const toolkit = path.resolve(__dirname, "..", "..", "dial9-toolkit", "scripts", name);
  if (fs.existsSync(toolkit)) return toolkit;
  return path.resolve(__dirname, "..", "..", "..", "ui", name);
}

const { parseTrace, symbolizeChain } = require(resolve("trace_parser.js"));

function clean(sym) {
  // inferno/flamegraph.pl split frames on ';' — neutralize any in symbol text.
  return String(sym).replace(/;/g, ":").replace(/\s+/g, " ").trim() || "(anon)";
}

async function fold(targets, { leafOnly = false, maxDepth = 0 } = {}) {
  const folded = new Map(); // "root;...;leaf" -> count
  for (const target of targets) {
    for await (const trace of parseTrace(target)) {
      for (const s of trace.cpuSamples || []) {
        if (s.source !== 0) continue; // on-CPU only
        let frames = symbolizeChain(s.callchain || [], trace.callframeSymbols).map(
          (f) => clean(f.symbol),
        );
        if (!frames.length) frames = ["(empty)"];
        frames.reverse(); // leaf-first -> root-first
        if (leafOnly) frames = [frames[frames.length - 1]];
        else if (maxDepth > 0 && frames.length > maxDepth)
          frames = frames.slice(frames.length - maxDepth);
        const key = frames.join(";");
        folded.set(key, (folded.get(key) || 0) + 1);
      }
    }
  }
  return folded;
}

async function main() {
  const argv = process.argv.slice(2);
  let leafOnly = false,
    maxDepth = 0,
    asJson = false;
  const targets = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--leaf") leafOnly = true;
    else if (argv[i] === "--json") asJson = true;
    else if (argv[i] === "--max-depth") maxDepth = parseInt(argv[++i], 10);
    else targets.push(argv[i]);
  }
  if (!targets.length) {
    console.error("usage: node fold.js [--leaf|--max-depth N|--json] <file-or-dir>...");
    process.exit(2);
  }
  const folded = await fold(targets, { leafOnly, maxDepth });
  if (asJson) {
    process.stdout.write(JSON.stringify(Object.fromEntries(folded)));
  } else {
    const out = [...folded.entries()]
      .sort((a, b) => b[1] - a[1])
      .map(([k, v]) => `${k} ${v}`)
      .join("\n");
    process.stdout.write(out + "\n");
  }
}

if (require.main === module) {
  main().catch((e) => {
    console.error("fold error:", e.stack || e.message);
    process.exit(1);
  });
}

module.exports = { fold };
