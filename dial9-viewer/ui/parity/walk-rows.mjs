// Parity layer (a): the row-walker.
//
// Given a feature-inventory file and a live page URL, drives each inventory
// row's access path and emits one of three verdicts per row:
//
//   VERIFIED        — the row's walker drove the access path and observed the
//                     documented behavior (for DEAD-CONFIRMED rows: observed
//                     the documented absence of behavior);
//   FAILED          — the walker drove the access path and the behavior did
//                     not re-derive (this is the gate: green = zero FAILED);
//   NOT-TRIGGERABLE — the row's recorded verdict maps outside the gated set
//                     (shared verdict mapping): listed, not driven.
//
// Usage:
//   node parity/walk-rows.mjs \
//     --inventory ../../docs/ui-inventory/features/01-index-html.md \
//     --url http://localhost:3021/index.html \
//     [--rows A1,F12] [--json parity/out/walk.json] [--md parity/out/walk.md]
//
// See ui/README.md ("Parity gate tooling") for the full contract.

import path from "node:path";
import process from "node:process";
import { parseArgs, usage } from "./lib/cli.mjs";
import { parseInventory, isGated } from "./lib/inventory.mjs";
import { verdictTable, summarize, writeReport, writeJson } from "./lib/report.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { WalkError } from "./lib/actions.mjs";
import { registry as features01 } from "./walkers/features01.mjs";

// Walker registries per inventory file. Each registry maps row id -> walker;
// `fixedClock` pins the page clock to the dev seed date (required for the
// browser page's relative time windows — see lib/browser.mjs).
const REGISTRIES = {
  "01-index-html.md": { walkers: features01, fixedClock: true },
};

const WALKER_TIMEOUT_MS = 90_000;

const SPEC = {
  inventory: { required: true, help: "feature inventory markdown file" },
  url: { required: true, help: "live page URL (e.g. http://localhost:3021/index.html)" },
  rows: { help: "comma-separated row ids to walk (default: all)" },
  json: { help: "write results as JSON to this path" },
  md: { help: "write the verdict table as markdown to this path" },
};

async function withTimeout(promise, ms, what) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new WalkError(`${what} timed out after ${ms}ms`)), ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/walk-rows.mjs", SPEC));
    process.exit(2);
  }

  const invName = path.basename(opts.inventory);
  const reg = REGISTRIES[invName];
  if (!reg) {
    console.error(
      `no walker registry for ${invName} (have: ${Object.keys(REGISTRIES).join(", ")})`,
    );
    process.exit(2);
  }

  const { rows, recorded } = parseInventory(opts.inventory);
  const only = opts.rows ? new Set(opts.rows.split(",").map((s) => s.trim())) : null;
  const pageUrl = opts.url;
  const baseUrl = new URL(pageUrl).origin;
  await assertServerReady(baseUrl);

  const browser = await launchBrowser();
  const results = [];
  try {
    for (const row of rows) {
      if (only && !only.has(row.id)) continue;
      const rec = recorded.get(row.id);
      const recNote = rec
        ? `recorded ${rec.normalized}${rec.source === "refresh" ? " (refresh)" : ""}`
        : "no recorded verdict";

      if (!rec || !isGated(rec.normalized)) {
        results.push({
          id: row.id,
          feature: row.feature,
          verdict: "NOT-TRIGGERABLE",
          note: `${recNote} — listed, not gated`,
        });
        continue;
      }

      const walker = reg.walkers[row.id];
      if (!walker) {
        // A gated row without a walker is a tooling gap, not a page bug —
        // but it must never pass silently.
        results.push({
          id: row.id,
          feature: row.feature,
          verdict: "FAILED",
          note: `${recNote} but NO WALKER IMPLEMENTED — add one to parity/walkers/`,
        });
        continue;
      }

      const { context, page } = await newPage(browser, { fixedClock: reg.fixedClock });
      const started = Date.now();
      try {
        const evidence = await withTimeout(
          walker({ page, context, browser, baseUrl, pageUrl }),
          WALKER_TIMEOUT_MS,
          `walker ${row.id}`,
        );
        results.push({
          id: row.id,
          feature: row.feature,
          verdict: "VERIFIED",
          note: `${evidence} [${recNote}, ${Date.now() - started}ms]`,
        });
        process.stderr.write(`VERIFIED ${row.id}\n`);
      } catch (e) {
        const detail = e instanceof WalkError ? e.message : `${e.name}: ${e.message}`;
        results.push({
          id: row.id,
          feature: row.feature,
          verdict: "FAILED",
          note: `${detail} [${recNote}]`,
        });
        process.stderr.write(`FAILED ${row.id}: ${detail}\n`);
      } finally {
        await context.close();
      }
    }
  } finally {
    await browser.close();
  }

  const table = verdictTable(results);
  const counts = summarize(results);
  const failed = counts.FAILED ?? 0;

  console.log(`# Row-walker: ${invName} against ${pageUrl}\n`);
  console.log(table);
  console.log(
    `\nSummary: ${results.length} rows — ` +
      Object.entries(counts)
        .map(([k, v]) => `${v} ${k}`)
        .join(", "),
  );
  console.log(failed === 0 ? "GREEN: zero FAILED" : `RED: ${failed} FAILED`);

  if (opts.md) writeReport(opts.md, `${table}\n`);
  if (opts.json) writeJson(opts.json, { inventory: invName, pageUrl, counts, results });

  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
