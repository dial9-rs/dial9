// Row walkers for the canonical Tokio runtime stats page (tokio_stats.html).
//
// One walker per row whose recorded verdict GATES (live-checks/lib/inventory.mjs
// isGated): recorded VERIFIED / DEAD-CONFIRMED re-derive VERIFIED. Here that
// is the set that normalizes to plain VERIFIED (not "VERIFIED (API)", which
// maps to the listed-not-gated VERIFIED-API token): A8, A10, D3, D5, I1, J3,
// J5, J6, J11. Every other row's recorded verdict (CODE-READ / VERIFIED (API)
// / CODE-ONLY / NOT-TRIGGERABLE) is listed, not driven. Time-window rows (J9,
// and J6's window half) stay NOT-TRIGGERABLE on the seed - the epoch/date
// catch-22 makes the positive window case unreachable without synthetic
// fixtures.
//
// Environment assumptions (same dev seed as features03): the dev-server
// serves ui/dist and its aggregate endpoints answer against the seeded
// `demo-traces` bucket under prefix `traces`: one segment with nonzero polls
// and coverage 1/1 files. The page auto-loads when the URL carries `bucket`,
// so the scoped URL drives the refinement stream end to end.
//
//   PORT=3071 cargo run -p dial9-viewer --bin dev-server --features dev-server
//   node live-checks/walk-rows.mjs \
//     --inventory ../../docs/ui-inventory/features/04-tokio-stats-html.md \
//     --url http://localhost:3071/tokio_stats.html

import { createRequire } from "node:module";
import { expect, textOf } from "../lib/actions.mjs";

const require = createRequire(import.meta.url);
const { SseDecoder } = require("../../sse.js");

/** The seed scope URL: `bucket` present -> the page auto-loads. */
function seedUrl(pageUrl) {
  return `${pageUrl}?bucket=demo-traces&prefix=traces`;
}

/** Wait until the refine loop settles to its terminal status line. */
async function waitComplete(page) {
  await page.waitForFunction(
    () => document.getElementById("status")?.textContent === "Complete",
    { timeout: 30_000 },
  );
}

/** Decode every JSON snapshot from a completed Playwright API SSE response. */
async function sseSnapshots(response) {
  expect(response.ok(), `SSE request failed with HTTP ${response.status()}`);
  const decoder = new SseDecoder();
  const payloads = decoder.push(await response.text()).concat(decoder.flush());
  const snapshots = payloads.map((payload) => JSON.parse(payload));
  expect(snapshots.length > 0, "SSE response contained no snapshots");
  return snapshots;
}

export const registry = {
  // ── A. Bootstrap ──

  A8: async ({ page, pageUrl }) => {
    // The rollout control was retired with the alternate UI.
    await page.goto(pageUrl);
    expect(
      (await page.locator("#d9-ui-switch").count()) === 0,
      "retired UI switch rendered",
    );
    return "retired #d9-ui-switch is absent";
  },

  A10: async ({ page, pageUrl }) => {
    // Static tab title + zap heading, never updated from data.
    await page.goto(pageUrl);
    const title = await page.title();
    expect(title === "Tokio Stats", `title "${title}" != "Tokio Stats"`);
    const h1 = await textOf(page, "h1");
    expect(h1 === "⚡ Tokio Stats", `heading "${h1}" != "⚡ Tokio Stats"`);
    return `title "${title}", heading "${h1}"`;
  },

  // ── D. Loading + refinement loop ──

  D3: async ({ page, pageUrl }) => {
    // Termination (frozen case): the 1/1-file seed freezes after one refine,
    // so the stream ends at status "Complete" with a positive poll count.
    await page.goto(seedUrl(pageUrl));
    await waitComplete(page);
    await page.waitForSelector("#summary .card", { timeout: 5_000 });
    const total = await textOf(page, "#summary .card:first-child .value");
    expect(Number(total.replace(/,/g, "")) > 0, `Total Polls was "${total}"`);
    // And it STAYS complete (no self-resuming stream).
    await page.waitForTimeout(1200);
    const status = await textOf(page, "#status");
    expect(status === "Complete", `status resumed after freeze: "${status}"`);
    expect(
      (await textOf(page, "#summary .card:first-child .value")) === total,
      "Total Polls changed after completion",
    );
    return `stream completed; Total Polls ${total} stayed stable`;
  },

  D5: async ({ page, pageUrl }) => {
    // Error handling: a scope that 404s paints the status line red with the
    // server's body text.
    await page.goto(`${pageUrl}?bucket=demo-traces&prefix=no-such-prefix`);
    await page.waitForFunction(
      () => /^Error:/.test(document.getElementById("status")?.textContent ?? ""),
      { timeout: 20_000 },
    );
    const status = await textOf(page, "#status");
    expect(
      /no source files match this scope/.test(status),
      `status body unexpected: "${status}"`,
    );
    const cls = (await page.locator("#status").getAttribute("class")) ?? "";
    expect(/\berror\b/.test(cls), `status not styled error: "${cls}"`);
    return `status "${status}" (class "${cls}")`;
  },

  // ── I. XSS hardening ──

  I1: async ({ page, pageUrl }) => {
    // Spawn locations render as INERT text in <code>. The seed's real paths
    // carry no markup chars, so they render literally; hostile-string
    // inertness is owned by the Vitest XSS regression
    // (src/pages/tokio-stats/render.test.ts).
    await page.goto(seedUrl(pageUrl));
    await waitComplete(page);
    // Drop the threshold to its floor so every location surfaces.
    await page.$eval("#threshold-slider", (el) => {
      el.value = "-1";
      el.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const locTable = page
      .locator("#table-container table")
      .filter({ hasText: "Spawn Location" })
      .first();
    await locTable.waitFor({ state: "visible", timeout: 5_000 });
    const codes = await locTable.locator("tbody td:first-child code").allTextContents();
    expect(
      codes.some((c) => /\.rs:\d+:\d+$/.test(c.trim())),
      `spawn_loc not rendered as text: ${JSON.stringify(codes)}`,
    );
    const injected = await page
      .locator("#table-container script, #table-container img")
      .count();
    expect(injected === 0, `unexpected markup element in the table (${injected})`);
    return `spawn locations render inert as <code> text (${codes.length} rows)`;
  },

  // ── J. Backend contract (driven via the live API) ──

  J3: async ({ page, baseUrl }) => {
    // Aggregation gate: no bucket on a non-`--agg` dev-server -> 404 gate msg.
    const r = await page.request.get(`${baseUrl}/api/tokio-stats`);
    expect(r.status() === 404, `no-bucket status ${r.status()} != 404`);
    const body = await r.text();
    expect(/requires aggregation/.test(body), `gate body unexpected: "${body}"`);
    return `no-bucket -> 404 "${body.trim()}"`;
  },

  J5: async ({ page, baseUrl }) => {
    // One SSE request emits the current snapshot and any progressively folded
    // snapshots, then closes at the sampling cap.
    const base = `${baseUrl}/api/tokio-stats?bucket=demo-traces&prefix=traces`;
    const snapshots = await sseSnapshots(await page.request.get(base));
    for (let i = 1; i < snapshots.length; i++) {
      expect(
        snapshots[i].coverage.files_folded >= snapshots[i - 1].coverage.files_folded,
        "SSE coverage regressed",
      );
    }
    const final = snapshots.at(-1);
    expect(
      final.coverage.files_folded === 1 && final.coverage.files_matched === 1,
      `final coverage ${JSON.stringify(final.coverage)}`,
    );
    expect(final.total_polls > 0, `final total_polls ${final.total_polls}`);
    return `${snapshots.length} SSE snapshot(s) -> 1/1 files, total_polls ${final.total_polls}`;
  },

  J6: async ({ page, baseUrl }) => {
    // Scope matching (service + host OR-semantics). The time-window half of
    // this row is NOT-TRIGGERABLE on the seed (epoch/date catch-22).
    const base = `${baseUrl}/api/tokio-stats?bucket=demo-traces&prefix=traces&refine=true`;
    const svc = await page.request.get(`${base}&service=demo-service`);
    expect(svc.status() === 200, `service=demo-service status ${svc.status()}`);
    const hostOk = await page.request.get(`${base}&host=local`);
    expect(hostOk.status() === 200, `host=local status ${hostOk.status()}`);
    const hostOr = await page.request.get(`${base}&host=local&host=nonexistent`);
    expect(hostOr.status() === 200, `host OR status ${hostOr.status()}`);
    const hostBad = await page.request.get(`${base}&host=host-0`);
    expect(hostBad.status() === 404, `host=host-0 status ${hostBad.status()} != 404`);
    return "service/host match, host OR keeps the match, wrong host -> 404";
  },

  J11: async ({ page, baseUrl }) => {
    // Poll classes stay in the documented 0..3 vocabulary and are
    // index-aligned with durations.
    const snapshots = await sseSnapshots(
      await page.request.get(
        `${baseUrl}/api/tokio-stats?bucket=demo-traces&prefix=traces`,
      ),
    );
    const data = snapshots.at(-1);
    const seen = new Set();
    for (const loc of data.by_spawn_loc) {
      expect(
        loc.classes.length === loc.durations_ns.length,
        `class/duration misalignment at ${loc.spawn_loc}`,
      );
      for (const c of loc.classes) {
        expect([0, 1, 2, 3].includes(c), `unknown poll class ${c}`);
        seen.add(c);
      }
    }
    expect(seen.size > 0, "no classified polls");
    return `classes ${JSON.stringify([...seen].sort())}, index-aligned`;
  },
};
