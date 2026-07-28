// Debug/smoke runner for the J1-J8 journey scripts: runs one journey (or
// all) against a live server and prints the readouts captured at each
// checkpoint. The behavioral differ (behavior-diff.mjs) is the comparing
// consumer; this runner exists to develop and sanity-check journeys.
//
// Usage:
//   node parity/run-journey.mjs --base http://localhost:3021 [--journey J2]

import process from "node:process";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { runSteps } from "./lib/steps.mjs";
import { captureReadouts } from "./lib/readouts.mjs";
import { READOUT_SCHEMA } from "./fixtures/readout-schema.mjs";
import { JOURNEYS } from "./journeys.mjs";

const SPEC = {
  base: { required: true, help: "server base URL (journey defaultPath is appended)" },
  journey: { help: "journey id (J1..J8; default: all)" },
};

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/run-journey.mjs", SPEC));
    process.exit(2);
  }

  await assertServerReady(opts.base);
  const ids = opts.journey ? [opts.journey] : Object.keys(JOURNEYS);
  const browser = await launchBrowser();
  let failed = 0;
  try {
    for (const id of ids) {
      const journey = JOURNEYS[id];
      if (!journey) {
        console.error(`unknown journey ${id} (have ${Object.keys(JOURNEYS).join(", ")})`);
        process.exit(2);
      }
      const { context, page } = await newPage(browser, { fixedClock: journey.fixedClock });
      const url = new URL(journey.defaultPath, opts.base).href;
      console.log(`\n== ${id} (${journey.label}) -> ${url}`);
      try {
        await page.goto(url);
        await runSteps(page, journey, {
          onCheckpoint: async (name) => {
            const readouts = await captureReadouts(page, READOUT_SCHEMA[journey.page]);
            console.log(`  checkpoint ${name}:`);
            for (const [k, v] of Object.entries(readouts)) {
              console.log(`    ${k} = ${JSON.stringify(v)}`);
            }
          },
        });
        console.log(`  ${id} OK`);
      } catch (e) {
        failed++;
        console.error(`  ${id} FAILED: ${e.message}`);
      } finally {
        await context.close();
      }
    }
  } finally {
    await browser.close();
  }
  process.exit(failed === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
