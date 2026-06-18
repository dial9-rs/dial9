#!/usr/bin/env node
// merge_folded.js — combine per-segment `fold.js --json` outputs into one
// folded-text stream (stdout). Lets you render any union/subset of segments
// without re-parsing the traces.
//
// Usage:
//   node merge_folded.js a.json b.json c.json > pool.folded
//   node merge_folded.js 'dir/*.json'              # shell-expanded globs are fine
//
// Each input is {stack: count} JSON (from `fold.js --json`). Counts sum.

const fs = require("fs");

function loadInto(acc, file) {
  const h = JSON.parse(fs.readFileSync(file, "utf8"));
  for (const k in h) acc[k] = (acc[k] || 0) + h[k];
}

function main() {
  const files = process.argv.slice(2);
  if (!files.length) {
    console.error("usage: node merge_folded.js <fold-json>...  > out.folded");
    process.exit(2);
  }
  const acc = {};
  for (const f of files) loadInto(acc, f);
  const out = Object.entries(acc)
    .sort((a, b) => b[1] - a[1])
    .map(([k, v]) => `${k} ${v}`)
    .join("\n");
  process.stdout.write(out + "\n");
}

if (require.main === module) main();

module.exports = { loadInto };
