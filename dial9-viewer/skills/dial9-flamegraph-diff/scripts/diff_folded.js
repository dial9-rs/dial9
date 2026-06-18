#!/usr/bin/env node
// diff_folded.js — compare two folded-stack profiles (A = baseline, B = candidate).
//
// Produces, by default, a ranked "top movers" report: which stacks got hotter
// (red, more on-CPU time in B) or colder (blue, more in A), after normalizing
// the two profiles to equal total weight so you compare SHAPE, not volume.
// This is the form an agent reasons over — no renderer required.
//
// Usage:
//   node diff_folded.js A.folded B.folded                 # ranked top-movers report
//   node diff_folded.js --json A.folded B.folded           # same data as JSON
//   node diff_folded.js --inferno A.folded B.folded > d.folded
//        # emit inferno differential-folded (`stack baseline candidate`) for
//        # `inferno-flamegraph` to render as a red/blue differential SVG
//   node diff_folded.js --top 40 A.folded B.folded         # show N movers (default 25)
//   node diff_folded.js --raw A.folded B.folded            # keep unsymbolized 0x… leaf frames
//
// Folded inputs come from fold.js / merge_folded.js. Both sides MUST be folded
// at the same granularity (both full, or both --max-depth N) or stacks won't align.
//
// NOTE: by default the ranked report drops stacks whose LEAF is an unsymbolized
// raw address (0x…). Those are per-process stack/JIT addresses that never match
// across two different captures, so they always show up as spurious top movers.
// They are NOT dropped from --inferno output (rendering should show everything);
// pass --raw to keep them in the ranked report too.

const fs = require("fs");

function load(file) {
  const map = new Map();
  const txt = fs.readFileSync(file, "utf8");
  for (const line of txt.split("\n")) {
    if (!line.trim()) continue;
    const sp = line.lastIndexOf(" ");
    if (sp < 0) continue;
    const stack = line.slice(0, sp);
    const n = parseInt(line.slice(sp + 1), 10);
    if (!Number.isFinite(n)) continue;
    map.set(stack, (map.get(stack) || 0) + n);
  }
  return map;
}

function total(map) {
  let t = 0;
  for (const v of map.values()) t += v;
  return t;
}

function main() {
  const argv = process.argv.slice(2);
  let asJson = false,
    inferno = false,
    raw = false,
    top = 25;
  const files = [];
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--json") asJson = true;
    else if (argv[i] === "--inferno") inferno = true;
    else if (argv[i] === "--raw") raw = true;
    else if (argv[i] === "--top") top = parseInt(argv[++i], 10);
    else files.push(argv[i]);
  }
  if (files.length !== 2) {
    console.error("usage: node diff_folded.js [--json|--inferno|--top N] <A.folded> <B.folded>");
    process.exit(2);
  }
  const A = load(files[0]),
    B = load(files[1]);
  const tA = total(A),
    tB = total(B);
  if (!tA || !tB) {
    console.error("one side has zero samples");
    process.exit(1);
  }

  // inferno differential-folded: `stack baselineCount candidateCount`.
  // Use B's native counts and A scaled to B's total (so widths are comparable).
  if (inferno) {
    const keys = new Set([...A.keys(), ...B.keys()]);
    const lines = [];
    for (const k of keys) {
      const a = Math.round((A.get(k) || 0) * (tB / tA));
      const b = B.get(k) || 0;
      lines.push(`${k} ${a} ${b}`);
    }
    process.stdout.write(lines.join("\n") + "\n");
    return;
  }

  // Normalized deltas: share-of-profile in B minus share in A (percentage points).
  const isRawAddr = (stack) => /^0x[0-9a-f]+$/i.test(stack.split(";").pop());
  const keys = new Set([...A.keys(), ...B.keys()]);
  const rows = [];
  let droppedRaw = 0;
  for (const k of keys) {
    if (!raw && isRawAddr(k)) {
      droppedRaw++;
      continue;
    }
    const sa = (A.get(k) || 0) / tA;
    const sb = (B.get(k) || 0) / tB;
    rows.push({ stack: k, deltaPct: (sb - sa) * 100, shareA: sa * 100, shareB: sb * 100 });
  }
  rows.sort((x, y) => Math.abs(y.deltaPct) - Math.abs(x.deltaPct));

  if (asJson) {
    process.stdout.write(
      JSON.stringify(
        { totalA: tA, totalB: tB, droppedRawAddrStacks: droppedRaw, movers: rows.slice(0, top) },
        null,
        2,
      ),
    );
    return;
  }

  const leaf = (s) => s.split(";").pop();
  console.log(`A (baseline) = ${files[0]}  [${tA} on-CPU samples]`);
  console.log(`B (candidate)= ${files[1]}  [${tB} on-CPU samples]`);
  if (droppedRaw)
    console.log(`(dropped ${droppedRaw} stacks with unsymbolized 0x… leaves; pass --raw to keep)`);
  console.log(`Normalized to equal weight. +red = hotter in B, -blue = hotter in A.\n`);
  console.log(`  Δpp     A%     B%   leaf  (full stack truncated)`);
  for (const r of rows.slice(0, top)) {
    const sign = r.deltaPct >= 0 ? "+" : "";
    console.log(
      `${sign}${r.deltaPct.toFixed(2).padStart(6)}  ${r.shareA.toFixed(2).padStart(5)}  ${r.shareB
        .toFixed(2)
        .padStart(5)}   ${leaf(r.stack)}`,
    );
  }
}

if (require.main === module) main();

module.exports = { load, total };
