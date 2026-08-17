// axe scan of the unified keyboard components (search palette + help
// overlay) in isolation. They have no live-page mount, so this spins up a
// throwaway Vite dev server (port 0 = OS-assigned) to serve
// live-checks/fixtures/keyboard-harness.html, which mounts the REAL components
// from src/ in their open state, and runs axe against that page. Live-page
// overlay scans stay with axe-scan.mjs (its --press flag opens the overlay
// on a real page first).
//
// Usage:
//   node live-checks/axe-keyboard.mjs [--fail-on serious] [--json p] [--md p]
//
// Exit: same contract as axe-scan.mjs (report producer; --fail-on gates).

import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { createServer } from "vite";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage } from "./lib/browser.mjs";
import { writeReport, writeJson } from "./lib/report.mjs";

// Same table shape as axe-scan.mjs's violationTable, not imported because
// axe-scan.mjs runs its main() at module load.
function violationTable(violations) {
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

const SPEC = {
  json: { help: "write full axe violations as JSON to this path" },
  md: { help: "write the violation table as markdown to this path" },
  "fail-on": { help: "gate: exit 1 on violations at/above this impact (critical|serious|moderate|minor)" },
};

const IMPACT_ORDER = ["critical", "serious", "moderate", "minor"];
const HARNESS_PATH = "/live-checks/fixtures/keyboard-harness.html";

function axeSource() {
  const require = createRequire(import.meta.url);
  return fs.readFileSync(
    path.join(path.dirname(require.resolve("axe-core")), "axe.min.js"),
    "utf8",
  );
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("live-checks/axe-keyboard.mjs", SPEC));
    process.exit(2);
  }
  const failOn = opts["fail-on"];
  if (failOn && !IMPACT_ORDER.includes(failOn)) {
    console.error(`--fail-on must be one of ${IMPACT_ORDER.join("|")}`);
    process.exit(2);
  }

  // The ui/ package root (live-checks/ is one level down).
  const uiRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const vite = await createServer({
    root: uiRoot,
    // The harness makes no /api calls; drop the config's proxy so a
    // missing Rust dev-server can't produce noise.
    server: { port: 0, proxy: undefined },
    logLevel: "error",
  });
  await vite.listen();
  const port = vite.config.server.port === 0
    ? vite.httpServer.address().port
    : vite.config.server.port;
  const url = `http://localhost:${port}${HARNESS_PATH}`;

  const browser = await launchBrowser();
  let violations;
  try {
    const { context, page } = await newPage(browser);
    try {
      await page.goto(url);
      await page.waitForFunction(() => window.__d9HarnessReady === true, {
        timeout: 15_000,
      });
      // Both components report themselves open before the scan runs.
      await page.waitForSelector(".d9-palette.open", { timeout: 5_000 });
      await page.waitForSelector(".d9-help-backdrop.open", { timeout: 5_000 });
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
    await vite.close();
  }

  violations.sort(
    (a, b) => IMPACT_ORDER.indexOf(a.impact) - IMPACT_ORDER.indexOf(b.impact),
  );
  const table = violationTable(violations);
  const nodeCount = violations.reduce((n, v) => n + v.nodes.length, 0);
  console.log(`# axe scan (keyboard components harness): ${HARNESS_PATH}\n`);
  console.log(table);
  console.log(`\nSummary: ${violations.length} violations, ${nodeCount} nodes`);

  if (opts.md) writeReport(opts.md, `${table}\n`);
  if (opts.json) {
    writeJson(opts.json, {
      pageUrl: HARNESS_PATH,
      counts: { violations: violations.length, nodes: nodeCount },
      violations,
    });
  }

  if (failOn) {
    const threshold = IMPACT_ORDER.indexOf(failOn);
    const gated = violations.filter(
      (v) => IMPACT_ORDER.indexOf(v.impact) <= threshold,
    );
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
