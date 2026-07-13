// Row walkers for docs/ui-inventory/features/04-tokio-stats-html.md (the
// Tokio runtime stats page), added by T41 to gate the migrated page
// (new/tokio_stats.html; the same access paths hold on the legacy page,
// which is the self-test). This is the `features04` walker registry the T40
// inventory flagged as T41's implementation-time deliverable.
//
// One walker per row whose recorded verdict GATES under the shared verdict
// mapping (chunk-1 header; parity/lib/inventory.mjs isGated): recorded
// VERIFIED / DEAD-CONFIRMED re-derive VERIFIED. For features/04 that is the
// set that normalizes to plain VERIFIED (not "VERIFIED (API)", which maps to
// the listed-not-gated VERIFIED-API token): A8, A10, D3, D5, I1, J3, J5,
// J6, J11. Every other row's recorded verdict (CODE-READ / VERIFIED (API) /
// CODE-ONLY / NOT-TRIGGERABLE) is listed, not driven. Time-window rows (J9,
// and J6's window half) stay NOT-TRIGGERABLE on the seed - the epoch/date
// catch-22 (features/04 validation section) makes the positive window case
// unreachable until T42's synthetic fixtures.
//
// Environment assumptions (ui/README.md; same dev seed as features03): the
// dev-server serves ui/dist and its aggregate endpoints answer against the
// seeded `demo-traces` bucket under prefix `traces` - ONE folded segment
// that yields total_polls 94212, time_span_ns 4143811668, five spawn
// locations (top: axum_traced.rs:243:33 with 3319 notable polls), coverage
// 1/1 files, classes {1,2,3} (no off-CPU). The page auto-loads when the URL
// carries `bucket` (features/04 A6), so the scoped URL drives the refine
// loop end to end.
//
//   PORT=3071 cargo run -p dial9-viewer --bin dev-server --features dev-server
//   node parity/walk-rows.mjs \
//     --inventory ../../docs/ui-inventory/features/04-tokio-stats-html.md \
//     --url http://localhost:3071/new/tokio_stats.html

import { expect, textOf } from "../lib/actions.mjs";

const AXUM = "examples/metrics-service/src/axum_traced.rs:243:33";

/** The seed scope URL: `bucket` present -> the page auto-loads (A6). */
function seedUrl(pageUrl) {
  return `${pageUrl}?bucket=demo-traces&prefix=traces`;
}

/** Wait until the refine loop settles to its terminal status line (C2). */
async function waitComplete(page) {
  await page.waitForFunction(
    () => document.getElementById("status")?.textContent === "Complete",
    { timeout: 30_000 },
  );
}

export const registry = {
  // ── A. Bootstrap ──

  A8: async ({ page, pageUrl, side }) => {
    // The dual-UI switch control renders on BOTH generations now that
    // tokio_stats is registered (T38); the label is per-side.
    await page.goto(pageUrl);
    await page.waitForSelector("#d9-ui-switch", { state: "visible", timeout: 15_000 });
    const label = await textOf(page, "#d9-ui-switch");
    const want = side === "new" ? "Switch to legacy UI" : "Switch to new UI";
    expect(label === want, `switch label "${label}" != "${want}" (side ${side})`);
    return `#d9-ui-switch renders "${label}" (side ${side})`;
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
    // so the loop ends at status "Complete" with the full poll count stable.
    await page.goto(seedUrl(pageUrl));
    await waitComplete(page);
    await page.waitForSelector("#summary .card", { timeout: 5_000 });
    const summary = await textOf(page, "#summary");
    expect(/94,?212/.test(summary), `Total Polls card missing 94212: "${summary}"`);
    // And it STAYS complete (no self-resuming refine spinner).
    await page.waitForTimeout(1200);
    const status = await textOf(page, "#status");
    expect(status === "Complete", `status resumed after freeze: "${status}"`);
    return `refine froze at "Complete"; Total Polls 94,212 stable`;
  },

  D5: async ({ page, pageUrl }) => {
    // Error handling: a scope that 404s paints the status line red with the
    // server's body text (C2/D5/J7).
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
    // Spawn locations render as INERT text in <code> (the #587 escape path).
    // The seed's real paths carry no markup chars, so they render literally;
    // hostile-string inertness is owned by the Vitest XSS regression
    // (src/pages/tokio-stats/render.test.ts) as the inventory records.
    await page.goto(seedUrl(pageUrl));
    await waitComplete(page);
    // Drop the threshold to its floor so every location surfaces.
    await page.$eval("#threshold-slider", (el) => {
      el.value = "-1";
      el.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await page.waitForSelector("#table-container table td code", { timeout: 5_000 });
    const codes = await page.locator("#table-container td code").allTextContents();
    expect(
      codes.map((c) => c.trim()).includes(AXUM),
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
    // Refinement loop (small scope): cold read-only empty, then refine folds
    // the single seed segment (1/1 files, total_polls 94212).
    const base = `${baseUrl}/api/tokio-stats?bucket=demo-traces&prefix=traces`;
    const cold = await (await page.request.get(base)).json();
    expect(cold.total_polls === 0, `cold total_polls ${cold.total_polls} != 0`);
    expect(
      cold.coverage.files_matched === 1 && cold.coverage.files_folded === 0,
      `cold coverage ${JSON.stringify(cold.coverage)}`,
    );
    const fold = await (await page.request.get(`${base}&refine=true`)).json();
    expect(
      fold.coverage.files_folded === 1 && fold.coverage.files_matched === 1,
      `fold coverage ${JSON.stringify(fold.coverage)}`,
    );
    expect(fold.total_polls === 94212, `folded total_polls ${fold.total_polls} != 94212`);
    return `cold 0 -> refine folds 1/1, total_polls ${fold.total_polls}`;
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
    // Poll classification: the demo carries classes {1,2,3} only (its longest
    // poll ~1ms is under the 10ms off-CPU bound, so class 0 never occurs),
    // and classes are index-aligned with durations (E4 depends on it).
    const data = await (
      await page.request.get(
        `${baseUrl}/api/tokio-stats?bucket=demo-traces&prefix=traces&refine=true`,
      )
    ).json();
    const seen = new Set();
    for (const loc of data.by_spawn_loc) {
      expect(
        loc.classes.length === loc.durations_ns.length,
        `class/duration misalignment at ${loc.spawn_loc}`,
      );
      for (const c of loc.classes) seen.add(c);
    }
    expect(!seen.has(0), `unexpected off-CPU (class 0) in the demo: ${[...seen]}`);
    expect(
      seen.has(1) || seen.has(2) || seen.has(3),
      `no classified polls: ${[...seen]}`,
    );
    return `classes ${JSON.stringify([...seen].sort())} (no off-CPU), index-aligned`;
  },
};
