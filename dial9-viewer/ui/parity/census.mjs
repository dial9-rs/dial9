// The affordance census — dump one page's census, or diff two pages'
// censuses (the "nothing missing, nothing mutated" control-surface gate).
//
// Usage:
//   node parity/census.mjs --url <pageUrl> [--json p] [--md p]      # dump
//   node parity/census.mjs --a <pageUrl> --b <pageUrl> [--json p]   # diff
//
// Diff mode exits 0 only on ZERO diff. Two independent contexts capturing
// the same URL is the self-test: they must produce identical censuses.
//
// The page's loaded-wait is inferred from the URL (lib/pages.mjs
// waitLoadedByUrl); index pages get the pinned dev-seed clock (lib/browser.mjs)
// so time-derived affordance state can't drift between the two captures.

import process from "node:process";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { pageKindFor, waitLoadedByUrl } from "./lib/pages.mjs";
import {
  captureCensus,
  diffCensus,
  censusDiffCount,
  censusTable,
  censusDiffReport,
} from "./lib/census.mjs";
import { writeReport, writeJson } from "./lib/report.mjs";

const SPEC = {
  url: { help: "dump mode: single page URL" },
  a: { help: "diff mode: reference page URL" },
  b: { help: "diff mode: candidate page URL" },
  json: { help: "write census dump / diff as JSON to this path" },
  md: { help: "write census table / diff report as markdown to this path" },
};

async function censusOf(browser, pageUrl) {
  const fixedClock = pageKindFor(pageUrl) === "index";
  const { context, page } = await newPage(browser, { fixedClock });
  try {
    await page.goto(pageUrl);
    await waitLoadedByUrl(page, pageUrl);
    return await captureCensus(page);
  } finally {
    await context.close();
  }
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/census.mjs", SPEC));
    process.exit(2);
  }
  const diffMode = opts.a || opts.b;
  if (diffMode ? !(opts.a && opts.b) || opts.url : !opts.url) {
    console.error("pass either --url (dump) or both --a and --b (diff)");
    console.error(usage("parity/census.mjs", SPEC));
    process.exit(2);
  }

  const urls = diffMode ? [opts.a, opts.b] : [opts.url];
  for (const origin of new Set(urls.map((u) => new URL(u).origin))) {
    await assertServerReady(origin);
  }

  const browser = await launchBrowser();
  let exitCode = 0;
  try {
    if (!diffMode) {
      const census = await censusOf(browser, opts.url);
      const table = censusTable(census);
      console.log(`# Affordance census: ${opts.url} — ${census.length} affordances\n`);
      console.log(table);
      if (opts.md) writeReport(opts.md, `${table}\n`);
      if (opts.json) writeJson(opts.json, { pageUrl: opts.url, census });
    } else {
      const a = await censusOf(browser, opts.a);
      const b = await censusOf(browser, opts.b);
      const diff = diffCensus(a, b);
      const n = censusDiffCount(diff);
      const report = censusDiffReport(diff);
      console.log(`# Affordance census diff\n#   A: ${opts.a} (${a.length} affordances)`);
      console.log(`#   B: ${opts.b} (${b.length} affordances)\n`);
      console.log(report);
      console.log(n === 0 ? "\nZERO DIFF" : `\n${n} diff entries`);
      if (opts.md) writeReport(opts.md, `${report}\n`);
      if (opts.json)
        writeJson(opts.json, { a: opts.a, b: opts.b, counts: { a: a.length, b: b.length }, diff });
      exitCode = n === 0 ? 0 : 1;
    }
  } finally {
    await browser.close();
  }
  process.exit(exitCode);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
