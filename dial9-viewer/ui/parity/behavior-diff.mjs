// The behavioral differ — run the SAME journey on two page URLs and compare
// the data readouts (fixtures/readout-schema.mjs) captured at every
// checkpoint, field by field, exactly.
//
// Usage:
//   node parity/behavior-diff.mjs --a <url> --b <url> [--journey J6] [--json p]
//
// --a/--b accept either full page URLs or bare origins; a bare origin gets
// the journey's defaultPath appended, so `--a http://localhost:3021 --b
// http://localhost:3022` runs every journey against its own page on both
// hosts. --journey narrows to one journey (default: all whose page the URLs
// can serve).
//
// Exit 0 only when EVERY checkpoint of every journey run has zero field
// diffs. Volatile schema fields (wall-clock: load time) are captured for
// context but excluded. Two independent runs of a journey against the same
// URL is the self-test: they must produce identical readouts.

import process from "node:process";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { runSteps } from "./lib/steps.mjs";
import { captureReadouts, diffReadouts } from "./lib/readouts.mjs";
import { READOUT_SCHEMA } from "./fixtures/readout-schema.mjs";
import { JOURNEYS } from "./journeys.mjs";
import { writeJson } from "./lib/report.mjs";

const SPEC = {
  a: { required: true, help: "reference page URL or origin" },
  b: { required: true, help: "candidate page URL or origin" },
  journey: { help: "journey id J1..J8 (default: all)" },
  json: { help: "write per-checkpoint readouts + diffs as JSON to this path" },
};

/** A bare origin ("/" path, no query) gets the journey's defaultPath. */
function resolveUrl(input, journey) {
  const u = new URL(input);
  if (u.pathname === "/" && !u.search) return new URL(journey.defaultPath, u).href;
  return u.href;
}

/** Run one journey against one URL; returns Map<checkpoint, readouts>. */
async function journeyReadouts(browser, journey, url) {
  const { context, page } = await newPage(browser, { fixedClock: journey.fixedClock });
  const captures = new Map();
  try {
    await page.goto(url);
    await runSteps(page, journey, {
      onCheckpoint: async (name) => {
        if (captures.has(name)) throw new Error(`duplicate checkpoint "${name}"`);
        captures.set(name, await captureReadouts(page, READOUT_SCHEMA[journey.page]));
      },
    });
  } finally {
    await context.close();
  }
  return captures;
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/behavior-diff.mjs", SPEC));
    process.exit(2);
  }

  const ids = opts.journey ? [opts.journey] : Object.keys(JOURNEYS);
  for (const id of ids) {
    if (!JOURNEYS[id]) {
      console.error(`unknown journey ${id} (have ${Object.keys(JOURNEYS).join(", ")})`);
      process.exit(2);
    }
  }
  for (const origin of new Set([opts.a, opts.b].map((u) => new URL(u).origin))) {
    await assertServerReady(origin);
  }

  const browser = await launchBrowser();
  let totalDiffs = 0;
  const jsonOut = [];
  try {
    for (const id of ids) {
      const journey = JOURNEYS[id];
      const fields = READOUT_SCHEMA[journey.page];
      const urlA = resolveUrl(opts.a, journey);
      const urlB = resolveUrl(opts.b, journey);
      console.log(`\n== ${id} (${journey.label})\n   A: ${urlA}\n   B: ${urlB}`);

      const capA = await journeyReadouts(browser, journey, urlA);
      const capB = await journeyReadouts(browser, journey, urlB);

      const checkpoints = [...capA.keys()];
      for (const cp of checkpoints) {
        const a = capA.get(cp);
        const b = capB.get(cp);
        if (!b) {
          totalDiffs++;
          console.log(`   checkpoint ${cp}: MISSING on B`);
          continue;
        }
        const diffs = diffReadouts(fields, a, b);
        totalDiffs += diffs.length;
        if (diffs.length === 0) {
          console.log(`   checkpoint ${cp}: identical (${fields.filter((f) => !f.volatile).length} fields)`);
        } else {
          console.log(`   checkpoint ${cp}: ${diffs.length} field diff(s)`);
          for (const d of diffs) {
            console.log(`     ${d.field}: A=${JSON.stringify(d.a)} B=${JSON.stringify(d.b)}`);
          }
        }
        jsonOut.push({ journey: id, checkpoint: cp, a, b, diffs });
      }
    }
  } finally {
    await browser.close();
  }

  console.log(totalDiffs === 0 ? "\nZERO DIFF" : `\n${totalDiffs} field diff(s) total`);
  if (opts.json) writeJson(opts.json, { a: opts.a, b: opts.b, journeys: ids, totalDiffs, checkpoints: jsonOut });
  process.exit(totalDiffs === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
