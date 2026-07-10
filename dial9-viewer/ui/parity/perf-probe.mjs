// Parity layer (e): the perf probe — drive a scripted interaction storm
// (hover sweeps / pans / wheel zooms, built from the journey step vocabulary)
// against a live page and record, for the storm window only:
//
//   frames        painted frames (rAF ticks) — the denominator;
//   long tasks    PerformanceObserver 'longtask' entries (count/total/max ms);
//   layouts       from a Chromium trace of the storm: total Layout events and
//                 FORCED layouts (Layout events carrying a JS stack trace —
//                 the DevTools "forced reflow" signal), plus per-frame rates;
//   render        render invocations per frame via a PLUGGABLE source
//                 (lib/render-sources.mjs; default `stub` reports unavailable —
//                 chunk 2 wires the store scheduler's dev-hook source).
//
// Usage:
//   node parity/perf-probe.mjs --url <pageUrl> [--journey J6] [--json p]
//                              [--render-source stub]
//
// The journey runs first to reach a representative state (defaults per page
// kind: index -> J6, viewer -> J1, flamegraph -> J5); the storm and all
// measurements start after it. Chunk-2 DoDs assert against the --json output.

import process from "node:process";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { runSteps, pageKindFor } from "./lib/steps.mjs";
import { JOURNEYS } from "./journeys.mjs";
import { RENDER_SOURCES } from "./lib/render-sources.mjs";
import { writeJson } from "./lib/report.mjs";

const SPEC = {
  url: { required: true, help: "live page URL to probe" },
  journey: { help: "journey to reach state first (default per page kind: J6/J1/J5)" },
  "render-source": { default: "stub", help: `render-invocation source (${Object.keys(RENDER_SOURCES).join("|")})` },
  json: { help: "write the probe record as JSON to this path" },
};

const DEFAULT_JOURNEY = { index: "J6", viewer: "J1", flamegraph: "J5" };

// Interaction storms per page kind, in the journey step vocabulary. Each
// works from its default journey's end state (data on screen).
const STORMS = {
  viewer: [
    { focus: "#main-area" },
    { hoverSweep: ["#lanes-container", 24] },
    { hoverSweep: ["#lanes-container", 24] },
    { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
    { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
    { wheel: { selector: "#lanes-container", dy: -240, modifier: "Control" } },
    { key: ["ArrowRight", 8] },
    { key: ["ArrowLeft", 8] },
    { hoverSweep: ["#lanes-container", 24] },
    { wheel: { selector: "#lanes-container", dy: 240, modifier: "Control" } },
    { wheel: { selector: "#lanes-container", dy: 240, modifier: "Control" } },
    { wheel: { selector: "#lanes-container", dy: 240, modifier: "Control" } },
    { hoverSweep: ["#lanes-container", 24] },
  ],
  index: [
    { hoverSweep: ["#heatmap-plot", 24] },
    { drag: { selector: "#heatmap-plot", from: [0.2, 0.5], to: [0.8, 0.5] } },
    { hoverSweep: ["#heatmap-plot", 24] },
    { drag: { selector: "#heatmap-plot", from: [0.3, 0.5], to: [0.6, 0.5], alt: true } },
    { hoverSweep: ["#heatmap-plot", 24] },
    { dblclick: "#heatmap-plot" },
    { hoverSweep: ["#heatmap-plot", 24] },
  ],
  flamegraph: [
    // The canvas is much taller than the viewport; sweep the top band where
    // the root frames live (fy fraction, clamped into view by the executor).
    { hoverSweep: [".fg-canvas", 32, 0.05] },
    { hoverSweep: [".fg-canvas", 32, 0.1] },
    { hoverSweep: [".fg-canvas", 32, 0.05] },
    { hoverSweep: [".fg-canvas", 32, 0.1] },
  ],
};

// In-page instrumentation: painted-frame counter (rAF loop) + longtask
// observer. Installed pre-goto; the probe reads deltas around the storm.
const INIT_SCRIPT = `
  window.__parityPerf = { frames: 0, longTasks: [] };
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) {
      window.__parityPerf.longTasks.push({ start: e.startTime, duration: e.duration });
    }
  }).observe({ type: "longtask", buffered: true });
  const __parityRaf = () => { window.__parityPerf.frames++; requestAnimationFrame(__parityRaf); };
  requestAnimationFrame(__parityRaf);
`;

function snapshotPerf(page) {
  return page.evaluate(() => ({
    frames: window.__parityPerf.frames,
    longTasks: window.__parityPerf.longTasks.length,
  }));
}

/** Long-task stats over the storm window only (entries after `fromIndex`). */
function collectLongTasks(page, fromIndex) {
  return page.evaluate((from) => {
    const tasks = window.__parityPerf.longTasks.slice(from);
    return {
      count: tasks.length,
      totalMs: +tasks.reduce((n, t) => n + t.duration, 0).toFixed(1),
      maxMs: +tasks.reduce((n, t) => Math.max(n, t.duration), 0).toFixed(1),
    };
  }, fromIndex);
}

/** Count Layout events (total + forced = carrying a JS stack) in a trace. */
function layoutCounts(traceBuffer) {
  const { traceEvents } = JSON.parse(traceBuffer.toString("utf8"));
  let total = 0;
  let forced = 0;
  for (const e of traceEvents) {
    if (e.name !== "Layout") continue;
    total++;
    if (e.args?.beginData?.stackTrace?.length) forced++;
  }
  return { total, forced };
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("parity/perf-probe.mjs", SPEC));
    process.exit(2);
  }

  const source = RENDER_SOURCES[opts["render-source"]];
  if (!source) {
    console.error(
      `unknown render source "${opts["render-source"]}" (have: ${Object.keys(RENDER_SOURCES).join(", ")})`,
    );
    process.exit(2);
  }

  const kind = pageKindFor(opts.url);
  if (!kind) {
    console.error(`cannot infer page kind from ${opts.url} (expect index/viewer/flamegraph.html)`);
    process.exit(2);
  }
  const journeyId = opts.journey ?? DEFAULT_JOURNEY[kind];
  const journey = JOURNEYS[journeyId];
  if (!journey) {
    console.error(`unknown journey ${journeyId} (have ${Object.keys(JOURNEYS).join(", ")})`);
    process.exit(2);
  }
  if (journey.page !== kind) {
    console.error(`journey ${journeyId} targets ${journey.page} pages, but --url is a ${kind} page`);
    process.exit(2);
  }

  await assertServerReady(new URL(opts.url).origin);
  const browser = await launchBrowser();
  let record;
  try {
    const { context, page } = await newPage(browser, { fixedClock: journey.fixedClock });
    try {
      await page.addInitScript(INIT_SCRIPT);
      await source.install(page);
      await page.goto(opts.url);

      // Reach a representative state before measuring.
      await runSteps(page, journey, {});

      // Measure the storm window only.
      await source.reset(page);
      const before = await snapshotPerf(page);
      await browser.startTracing(page, {
        categories: [
          "devtools.timeline",
          "disabled-by-default-devtools.timeline",
          "disabled-by-default-devtools.timeline.stack",
        ],
      });
      const started = Date.now();
      await runSteps(page, { page: kind, steps: STORMS[kind] }, {});
      const durationMs = Date.now() - started;
      const trace = await browser.stopTracing();
      const after = await snapshotPerf(page);
      const longTasks = await collectLongTasks(page, before.longTasks);
      const render = await source.collect(page);

      const frames = after.frames - before.frames;
      const layouts = layoutCounts(trace);
      const per = (n) => (frames > 0 ? +(n / frames).toFixed(3) : null);
      record = {
        pageUrl: opts.url,
        journey: journeyId,
        renderSource: opts["render-source"],
        storm: { steps: STORMS[kind].length, durationMs },
        frames,
        framesPerSecond: durationMs > 0 ? +((frames * 1000) / durationMs).toFixed(1) : null,
        longTasks,
        layouts: { ...layouts, totalPerFrame: per(layouts.total), forcedPerFrame: per(layouts.forced) },
        render: {
          ...render,
          perFrame:
            render.available && render.counters
              ? Object.fromEntries(Object.entries(render.counters).map(([k, v]) => [k, per(v)]))
              : null,
        },
      };
    } finally {
      await context.close();
    }
  } finally {
    await browser.close();
  }

  console.log(`# perf probe: ${record.pageUrl} (journey ${record.journey}, storm ${record.storm.steps} steps, ${record.storm.durationMs}ms)\n`);
  console.log(`frames painted:  ${record.frames} (${record.framesPerSecond}/s)`);
  console.log(`long tasks:      ${record.longTasks.count} (total ${record.longTasks.totalMs}ms, max ${record.longTasks.maxMs}ms)`);
  console.log(`layouts:         ${record.layouts.total} total (${record.layouts.totalPerFrame}/frame), ${record.layouts.forced} forced (${record.layouts.forcedPerFrame}/frame)`);
  console.log(
    record.render.available
      ? `render/frame:    ${JSON.stringify(record.render.perFrame)}`
      : `render/frame:    unavailable — ${record.render.note}`,
  );
  if (opts.json) writeJson(opts.json, record);
  process.exit(0);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
