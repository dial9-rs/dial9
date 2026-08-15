// axe + contrast scan: runs axe-core (injected into the page, no CDN)
// against a live page URL and emits the violation list — impact
// (critical/serious/moderate/minor) + rule + affected node count per page,
// plus a dedicated contrast summary line.
//
// Usage:
//   node parity/axe-scan.mjs --url <pageUrl> [--json p] [--md p]
//                            [--fail-on serious]
//
// The scan is a report producer: it exits 0 on completion regardless of
// violations. --fail-on <impact> turns it into a gate: exit 1 when any
// violation at or above that impact exists.

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { createRequire } from "node:module";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { pageKindFor, waitLoadedByUrl } from "./lib/pages.mjs";
import { writeReport, writeJson } from "./lib/report.mjs";

const SPEC = {
  url: { required: true, help: "live page URL to scan" },
  json: { help: "write full axe violations as JSON to this path" },
  md: { help: "write the violation table as markdown to this path" },
  "fail-on": { help: "gate: exit 1 on violations at/above this impact (critical|serious|moderate|minor)" },
  // display:none content is invisible to axe; keyboard-opened surfaces
  // (the unified `?` help overlay) need their key pressed first.
  press: { help: 'comma-separated keys to press after load, before the scan (e.g. "?")' },
};

const IMPACT_ORDER = ["critical", "serious", "moderate", "minor"];

function axeSource() {
  const require = createRequire(import.meta.url);
  return fs.readFileSync(path.join(path.dirname(require.resolve("axe-core")), "axe.min.js"), "utf8");
}

export function violationTable(violations) {
  const lines = [
    "| Impact | Rule | Description | Nodes | Example target |",
    "|---|---|---|---|---|",
  ];
  const esc = (s) => String(s ?? "").replace(/\|/g, "\\|").replace(/\n/g, " ");
  for (const v of violations) {
    const example = v.nodes[0]?.target?.join(" ") ?? "";
    lines.push(
      `| ${v.impact} | ${v.id} | ${esc(v.help)} | ${v.nodes.length} | ${esc(example)} |`,
    );
  }
  return lines.join("\n");
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/axe-scan.mjs", SPEC));
    process.exit(2);
  }
  const failOn = opts["fail-on"];
  if (failOn && !IMPACT_ORDER.includes(failOn)) {
    console.error(`--fail-on must be one of ${IMPACT_ORDER.join("|")}`);
    process.exit(2);
  }

  await assertServerReady(new URL(opts.url).origin);
  const browser = await launchBrowser();
  let violations;
  try {
    const fixedClock = pageKindFor(opts.url) === "index";
    const { context, page } = await newPage(browser, { fixedClock });
    try {
      await page.goto(opts.url);
      await waitLoadedByUrl(page, opts.url);
      if (opts.press) {
        for (const key of opts.press.split(",")) {
          await page.keyboard.press(key.trim());
        }
        // Overlay open/close is a class toggle (no animation); one settle
        // beat is enough.
        await page.waitForTimeout(250);
      }
      await page.addScriptTag({ content: axeSource() });
      violations = await page.evaluate(async () => {
        const res = await window.axe.run(document, { resultTypes: ["violations"] });
        return res.violations;
      });
    } finally {
      await context.close();
    }
  } finally {
    await browser.close();
  }

  violations.sort(
    (a, b) => IMPACT_ORDER.indexOf(a.impact) - IMPACT_ORDER.indexOf(b.impact),
  );
  const table = violationTable(violations);
  const nodeCount = violations.reduce((n, v) => n + v.nodes.length, 0);
  const contrast = violations.filter((v) => v.id.startsWith("color-contrast"));
  const contrastNodes = contrast.reduce((n, v) => n + v.nodes.length, 0);
  const byImpact = IMPACT_ORDER.map((i) => {
    const of = violations.filter((v) => v.impact === i);
    return of.length ? `${of.length} ${i} (${of.reduce((n, v) => n + v.nodes.length, 0)} nodes)` : null;
  }).filter(Boolean);

  console.log(`# axe scan: ${opts.url}\n`);
  console.log(table);
  console.log(
    `\nSummary: ${violations.length} violations, ${nodeCount} nodes — ${byImpact.join(", ") || "none"}`,
  );
  console.log(
    contrastNodes
      ? `Contrast: ${contrast.map((v) => v.impact).join("/")} contrast violations (${contrastNodes} nodes)`
      : "Contrast: no contrast violations",
  );

  if (opts.md) writeReport(opts.md, `${table}\n`);
  if (opts.json) writeJson(opts.json, { pageUrl: opts.url, counts: { violations: violations.length, nodes: nodeCount }, violations });

  if (failOn) {
    const threshold = IMPACT_ORDER.indexOf(failOn);
    const gated = violations.filter((v) => IMPACT_ORDER.indexOf(v.impact) <= threshold);
    if (gated.length) {
      console.log(`\nGATE (--fail-on ${failOn}): ${gated.length} violation(s) at/above ${failOn}`);
      process.exit(1);
    }
  }
  process.exit(0);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
