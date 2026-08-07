#!/usr/bin/env node
// Import-boundary check (T09; architecture 2.7, constraint S1: no new
// deps, so grep-shaped Node instead of an ESLint plugin).
//
// Rule: the frozen core lives as plain .js files at the ui/ root
// (decode.js, trace_parser.js, trace_analysis.js, panel_layout.js, ...).
// Only `src/lib/trace/` and `src/lib/canvas/` may import them; everything
// else consumes those barrels. This script fails when any other file
// under src/ imports a ui-root .js module.
//
// Exemptions:
// - *.d.ts: ambient declarations describe the core's types via
//   `declare module "*/trace_parser.js"` wildcards; no runtime import.
// - src/types/probe.ts: the T05 compile-time probe exists only for
//   `tsc --noEmit`, is referenced by no Vite input, and never ships.
//
// Runs as the `pretest` npm script, so `npm run test` (local and the ui
// CI job) enforces it with no workflow edit.

import { readFileSync, readdirSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const uiRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const srcRoot = path.join(uiRoot, "src");

const ALLOWED_DIRS = [
  path.join(srcRoot, "lib", "trace") + path.sep,
  path.join(srcRoot, "lib", "canvas") + path.sep,
];
const EXEMPT_FILES = new Set([path.join(srcRoot, "types", "probe.ts")]);

/** Recursively collect the source files to scan. */
function collect(dir, out = []) {
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      collect(full, out);
      continue;
    }
    if (!/\.(ts|tsx|js|mjs)$/.test(entry)) continue;
    if (entry.endsWith(".d.ts")) continue;
    out.push(full);
  }
  return out;
}

// Import specifiers: static import/export-from, bare imports, dynamic
// import(), and require() - the core files are CJS-guarded, so require
// would "work" and must be caught too.
const SPECIFIER_RE =
  /(?:\bimport\s[^"'()]*?from\s*|\bexport\s[^"'()]*?from\s*|\bimport\s*\(\s*|\brequire\s*\(\s*|\bimport\s+)["']([^"']+)["']/g;

/** True when `spec`, resolved from `file`, is a .js module at the ui root. */
function isCoreImport(file, spec) {
  if (!spec.startsWith(".") || !spec.endsWith(".js")) return false;
  const resolved = path.resolve(path.dirname(file), spec);
  return path.dirname(resolved) === uiRoot;
}

let statError = null;
try {
  statSync(srcRoot);
} catch (e) {
  statError = e;
}
if (statError) {
  console.error(`check-core-imports: cannot scan ${srcRoot}: ${statError.message}`);
  process.exit(2);
}

const violations = [];
for (const file of collect(srcRoot)) {
  if (EXEMPT_FILES.has(file)) continue;
  if (ALLOWED_DIRS.some((d) => file.startsWith(d))) continue;
  const text = readFileSync(file, "utf8");
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    for (const m of lines[i].matchAll(SPECIFIER_RE)) {
      if (isCoreImport(file, m[1])) {
        violations.push(
          `${path.relative(uiRoot, file)}:${i + 1}: imports frozen-core module "${m[1]}"`
        );
      }
    }
  }
}

if (violations.length > 0) {
  console.error(
    "Frozen-core import boundary violated (architecture 2.7): only " +
      "src/lib/trace/ and src/lib/canvas/ may import the ui-root core " +
      ".js modules. Import from the lib/trace or lib/canvas barrel instead.\n"
  );
  for (const v of violations) console.error("  " + v);
  process.exit(1);
}

console.log("check-core-imports: OK (no core imports outside lib/trace + lib/canvas)");
