// URL-contract verification against a live server. The codec-level pin is
// src/lib/url/url-contract.test.ts; this script is the end-to-end half: it
// CONSTRUCTS the deep-link URLs the contract documents the way an agent
// would - string concatenation from numbers computed in plain Node, no
// browser involved - then drives real pages and asserts they honor them.
//
// Legs:
//   viewer-viewport      recipe 1: start/end restores a viewport without
//                        filtering the loaded events.
//   viewer-data-window   recipe 1: data-start/data-end filters the data;
//                        redundant matching viewport params canonicalize out.
//                        Values are computed from the demo trace in Node.
//   fg-query-zoom        recipe 2, stable query form: the recorded J9
//                        worker-zoom link restores the zoom, zero rewrites.
//   fg-hash-zoom         recipe 2, hash form: #v=1&fg.w=
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

// The recorded J9 worker-zoom URL against the demo trace is the shared zoom
// fixture for both forms; reusing it keeps this script in lockstep with the
// journey.
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

async function captureViewer(browser, url) {
  const { context, page } = await newPage(browser);
  try {
    await page.goto(url);
    await LOADED_WAITS.viewer(page);
    await page.waitForTimeout(250); // trailing URL sync settles at 150ms
    return {
      readouts: await captureReadouts(page, READOUT_SCHEMA.viewer),
      viewLabel: (await page.locator("[data-status-view]").textContent())?.trim() ?? "",
      clearRangeVisible: await page.locator("#d9-btn-clear-range").isVisible(),
    };
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
    const urlFull = `${opts.base}/viewer.html?trace=demo-trace.bin`;
    const full = await loadAndCapture(browser, "viewer", urlFull);
    const fullCount = Number((full["events.count"] ?? "").replace(/,/g, ""));

    // ── Leg 1: recipe 1, viewport only (all events stay loaded) ──────────
    {
      const url =
        `${opts.base}/viewer.html?trace=demo-trace.bin&start=${startNs}&end=${endNs}`;
      const problems = [];
      const { readouts, viewLabel, clearRangeVisible } = await captureViewer(browser, url);
      if (clearRangeVisible) {
        problems.push("Clear Range visible without a data filter");
      }
      const viewCount = Number((readouts["events.count"] ?? "").replace(/,/g, ""));
      if (viewCount !== fullCount) {
        problems.push(`viewport link filtered events: ${viewCount} vs full ${fullCount}`);
      }
      const viewDuration = /\(([^)]+)\)$/.exec(viewLabel)?.[1] ?? null;
      const viewMs = durationToMs(viewDuration);
      if (viewMs === null || viewMs > windowMs * 1.01) {
        problems.push(`viewport readout "${viewLabel}" exceeds the ${windowMs}ms window`);
      }
      const settled = new URLSearchParams(readouts["url.query"] ?? "");
      for (const [key, value] of Object.entries({
        trace: "demo-trace.bin",
        start: String(startNs),
        end: String(endNs),
      })) {
        if (settled.get(key) !== value) {
          problems.push(`URL lost ${key}=${value}: ${readouts["url.query"]}`);
        }
      }
      if (settled.has("data-start") || settled.has("data-end")) {
        problems.push(`viewport link gained a data filter: ${readouts["url.query"]}`);
      }
      legs.push({
        leg: "viewer-viewport",
        url,
        pass: problems.length === 0,
        problems,
        readouts: { events: readouts["events.count"], viewport: viewLabel },
      });
    }

    // ── Leg 2: recipe 1, parse filter plus matching viewport ─────────────
    {
      const url =
        `${opts.base}/viewer.html?trace=demo-trace.bin` +
        `&data-start=${startNs}&data-end=${endNs}&start=${startNs}&end=${endNs}`;
      const problems = [];
      const { readouts, clearRangeVisible } = await captureViewer(browser, url);
      if (!clearRangeVisible) {
        problems.push("Clear Range not visible with a data filter");
      }
      const winCount = Number((readouts["events.count"] ?? "").replace(/,/g, ""));
      if (!(winCount > 0 && winCount < fullCount)) {
        problems.push(
          `windowed event count not a strict subset: ${winCount} vs full ${fullCount}`,
        );
      }
      const durMs = durationToMs(readouts["trace.duration"]);
      // The parsed range is inclusive within [start, end], so the rendered
      // duration must not exceed the requested window (small tolerance for
      // the readout's formatting rounding).
      if (durMs === null || durMs > windowMs * 1.01) {
        problems.push(
          `duration readout "${readouts["trace.duration"]}" exceeds the ${windowMs}ms window`,
        );
      }
      const settled = new URLSearchParams(readouts["url.query"] ?? "");
      const expected = {
        trace: "demo-trace.bin",
        "data-start": String(startNs),
        "data-end": String(endNs),
      };
      for (const [key, value] of Object.entries(expected)) {
        if (settled.get(key) !== value) {
          problems.push(`URL lost ${key}=${value}: ${readouts["url.query"]}`);
        }
      }
      // Once filtered, the requested viewport is the full available extent;
      // the canonical URL therefore omits its redundant start/end pair.
      if (settled.has("start") || settled.has("end")) {
        problems.push(`redundant viewport did not canonicalize out: ${readouts["url.query"]}`);
      }
      legs.push({
        leg: "viewer-data-window",
        url,
        pass: problems.length === 0,
        problems,
        readouts: {
          fullEvents: full["events.count"],
          windowEvents: readouts["events.count"],
          windowDuration: readouts["trace.duration"],
        },
      });
    }

    // ── Leg 3: recipe 2 stable query form ───────────────────────────────
    {
      const url = new URL(J9_PATH, opts.base).href;
      const r = await captureFgLeg(browser, url);
      legs.push({ leg: "fg-query-zoom", url, ...r });
    }

    // ── Leg 4: recipe 2 hash form (+ inert reserved key). Same zoom path,
    //    carried as #v=1&fg.w=; sel.task is reserved vocabulary and must
    //    change nothing. ──────────────────────────────────────────────────
    {
      const url =
        `${opts.base}/flamegraph.html?trace=demo-trace.bin` +
        `#v=1&fg.w=${J9_ZOOM_ENCODED}&sel.task=3`;
      const r = await captureFgLeg(browser, url);
      legs.push({ leg: "fg-hash-zoom", url, ...r });
    }

    // ── Leg 5: foreign version restores nothing, rewrites nothing ───────
    {
      const url =
        `${opts.base}/flamegraph.html?trace=demo-trace.bin` +
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
