// URL-contract verification against a live server. The codec-level pin is
// src/lib/url/url-contract.test.ts; this script is the end-to-end half: it
// CONSTRUCTS the deep-link URLs the contract documents the way an agent
// would - string concatenation from numbers computed in plain Node, no
// browser involved - then drives real pages and asserts they honor them.
//
// Legs:
//   viewer-window        recipe 1: viewer.html?trace&start&end opens the
//                        viewer at exactly that window (?start/?end are its
//                        stable parse-time filters). The start/end values are
//                        computed from the demo trace parsed in Node
//                        (minTs + relative-ms * 1e6).
//   fg-legacy-zoom       recipe 2, query form, legacy page: the recorded J9
//                        worker-zoom link restores the zoom, zero rewrites.
//   fg-hash-zoom         recipe 2, hash form, migrated page: #v=1&fg.w=
//                        restores the same zoom, zero rewrites; a reserved
//                        key riding along (sel.task) is inert and preserved.
//   fg-foreign-version   version rule: #v=2 restores nothing, rewrites
//                        nothing (the hash stays byte-identical).
//
// Exit 0 only when every leg passes.
//
// Usage:
//   npm run build   # the dev-server serves ui/dist from disk
//   PORT=3111 cargo run -p dial9-viewer --bin dev-server --features dev-server
//   node parity/url-contract.mjs --base http://localhost:3111 [--json out.json]

import process from "node:process";
import * as fs from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { LOADED_WAITS } from "./lib/steps.mjs";
import { captureReadouts } from "./lib/readouts.mjs";
import { READOUT_SCHEMA } from "./fixtures/readout-schema.mjs";
import { JOURNEYS } from "./journeys.mjs";

const require = createRequire(import.meta.url);
const DEMO_TRACE = fileURLToPath(new URL("../public/demo-trace.bin", import.meta.url));

const SPEC = {
  base: { required: true, help: "server base URL (dev-server or dial9 serve)" },
  json: { help: "write leg results as JSON to this path" },
};

// The recorded J9 zoom link (a legacy worker-zoom URL recorded from the
// legacy page against the demo trace) is the shared zoom fixture for both
// zoom legs; reusing it keeps this script in lockstep with the journey.
const J9_PATH = JOURNEYS.J9.defaultPath;
const J9_ZOOM_ENCODED = new URL(J9_PATH, "http://x").search.match(
  /worker-zoom=([^&]*)/,
)[1];

/** Parse the viewer's duration readout ("150ms", "4.20s", "980µs") to ms. */
function durationToMs(text) {
  const m = /^([\d.]+)\s*(ns|µs|us|ms|s)$/.exec((text ?? "").trim());
  if (!m) return null;
  const n = Number(m[1]);
  const scale = { ns: 1e-6, "µs": 1e-3, us: 1e-3, ms: 1, s: 1e3 }[m[2]];
  return n * scale;
}

/** Load `url`, wait for the page kind's loaded state, capture readouts. */
async function loadAndCapture(browser, kind, url, { settleMs = 0 } = {}) {
  const { context, page } = await newPage(browser);
  try {
    await page.goto(url);
    await LOADED_WAITS[kind](page);
    if (settleMs > 0) await page.waitForTimeout(settleMs);
    return await captureReadouts(page, READOUT_SCHEMA[kind]);
  } finally {
    await context.close();
  }
}

function queryAndHash(url) {
  const u = new URL(url);
  return u.search + u.hash;
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/url-contract.mjs", SPEC));
    process.exit(2);
  }
  await assertServerReady(opts.base);

  // ── Agent-side computation (no browser): window in absolute ns ─────────
  // The dial9-zoom-window conversion: absoluteNs = minTs + relMs * 1e6.
  const { parseTrace } = require("../trace_parser.js");
  let minTs;
  for await (const t of parseTrace(DEMO_TRACE)) {
    minTs = t.minTs;
    break;
  }
  const centerMs = 100;
  const halfMs = 50;
  const startNs = Math.round(minTs + (centerMs - halfMs) * 1e6);
  const endNs = Math.round(minTs + (centerMs + halfMs) * 1e6);
  const windowMs = 2 * halfMs;

  const legs = [];
  const browser = await launchBrowser();
  try {
    // ── Leg 1: recipe 1, the legacy viewer at an exact window ────────────
    {
      const urlFull = `${opts.base}/viewer.html?trace=demo-trace.bin`;
      const urlWin =
        `${opts.base}/viewer.html?trace=demo-trace.bin&start=${startNs}&end=${endNs}`;
      const full = await loadAndCapture(browser, "viewer", urlFull);
      const { context, page } = await newPage(browser);
      const problems = [];
      let win;
      try {
        await page.goto(urlWin);
        await LOADED_WAITS.viewer(page);
        win = await captureReadouts(page, READOUT_SCHEMA.viewer);
        // A range present on load reveals Clear Range immediately.
        if (!(await page.locator("#btn-clear-range").isVisible())) {
          problems.push("Clear Range button not visible with ?start/?end present");
        }
      } finally {
        await context.close();
      }
      const fullCount = Number((full["events.count"] ?? "").replace(/,/g, ""));
      const winCount = Number((win["events.count"] ?? "").replace(/,/g, ""));
      if (!(winCount > 0 && winCount < fullCount)) {
        problems.push(
          `windowed event count not a strict subset: ${winCount} vs full ${fullCount}`,
        );
      }
      const durMs = durationToMs(win["trace.duration"]);
      // The parsed range is inclusive within [start, end], so the rendered
      // duration must not exceed the requested window (small tolerance for
      // the readout's formatting rounding).
      if (durMs === null || durMs > windowMs * 1.01) {
        problems.push(
          `duration readout "${win["trace.duration"]}" exceeds the ${windowMs}ms window`,
        );
      }
      if (win["url.query"] !== `?trace=demo-trace.bin&start=${startNs}&end=${endNs}`) {
        problems.push(`URL rewritten on load: ${win["url.query"]}`);
      }
      legs.push({
        leg: "viewer-window",
        url: urlWin,
        pass: problems.length === 0,
        problems,
        readouts: {
          fullEvents: full["events.count"],
          windowEvents: win["events.count"],
          windowDuration: win["trace.duration"],
        },
      });
    }

    // ── Leg 2: recipe 2 query form on the LEGACY flamegraph page ────────
    {
      const url = new URL(J9_PATH, opts.base).href;
      const r = await captureFgLeg(browser, url);
      legs.push({ leg: "fg-legacy-zoom", url, ...r });
    }

    // ── Leg 3: recipe 2 hash form (+ inert reserved key) on the MIGRATED
    //    page. Same zoom path, carried as #v=1&fg.w=; sel.task is reserved
    //    vocabulary and must change nothing. ──────────────────────────────
    {
      const url =
        `${opts.base}/new/flamegraph.html?trace=demo-trace.bin` +
        `#v=1&fg.w=${J9_ZOOM_ENCODED}&sel.task=3`;
      const r = await captureFgLeg(browser, url);
      legs.push({ leg: "fg-hash-zoom", url, ...r });
    }

    // ── Leg 4: foreign version restores nothing, rewrites nothing ───────
    {
      const url =
        `${opts.base}/new/flamegraph.html?trace=demo-trace.bin` +
        `#v=2&fg.w=${J9_ZOOM_ENCODED}`;
      const readouts = await loadAndCapture(browser, "flamegraph", url, {
        settleMs: 300,
      });
      const problems = [];
      if (readouts["fg.breadcrumb"]) {
        problems.push(
          `zoom restored from a FOREIGN version: breadcrumb "${readouts["fg.breadcrumb"]}"`,
        );
      }
      if (readouts["url.query"] !== queryAndHash(url)) {
        problems.push(`foreign hash rewritten: ${readouts["url.query"]}`);
      }
      legs.push({ leg: "fg-foreign-version", url, pass: problems.length === 0, problems });
    }
  } finally {
    await browser.close();
  }

  // ── Report ─────────────────────────────────────────────────────────────
  let failed = 0;
  for (const l of legs) {
    console.log(`${l.pass ? "PASS" : "FAIL"}  ${l.leg}`);
    console.log(`      url: ${l.url}`);
    if (l.readouts) {
      for (const [k, v] of Object.entries(l.readouts)) {
        console.log(`      ${k} = ${JSON.stringify(v)}`);
      }
    }
    for (const p of l.problems ?? []) console.log(`      problem: ${p}`);
    if (!l.pass) failed++;
  }
  if (opts.json) {
    fs.writeFileSync(opts.json, JSON.stringify(legs, null, 2) + "\n");
    console.log(`wrote ${opts.json}`);
  }
  console.log(
    failed === 0
      ? `url-contract: all ${legs.length} legs green`
      : `url-contract: ${failed}/${legs.length} legs FAILED`,
  );
  process.exit(failed === 0 ? 0 : 1);
}

/** Shared zoom-leg assertions: breadcrumb shows a zoom, URL is byte-stable. */
async function captureFgLeg(browser, url) {
  const readouts = await loadAndCapture(browser, "flamegraph", url, { settleMs: 300 });
  const problems = [];
  if (!readouts["fg.breadcrumb"]) {
    problems.push("no breadcrumb: zoom was not restored");
  }
  if (readouts["url.query"] !== queryAndHash(url)) {
    problems.push(
      `URL not byte-stable after restore: ${readouts["url.query"]}`,
    );
  }
  return {
    pass: problems.length === 0,
    problems,
    readouts: { breadcrumb: readouts["fg.breadcrumb"] },
  };
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
