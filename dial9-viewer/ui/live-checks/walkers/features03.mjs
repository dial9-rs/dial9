// Row walkers for the canonical standalone CPU-profile flamegraph page
// (flamegraph.html).
//
// One walker per row the inventory records as gated. This page's aggregated
// (?api=1) mode gates exactly three rows; every other row's recorded verdict
// maps to NOT-TRIGGERABLE: listed, not driven. Exact-mode behavior is covered
// by focused Vitest suites and the live URL/accessibility checks.
//
// Environment assumptions: dev-server seed = ONE folded demo segment in
// bucket `demo-traces` under prefix `traces`, with coverage 1 / 1 files - so
// refinement freezes after the first refine=true poll (the auto-stop is
// deterministic on this seed).

import { expect, textOf } from "../lib/actions.mjs";

/** The aggregated-mode URL for the dev seed's scope. */
function apiUrl(pageUrl, extra = "") {
  return `${pageUrl}?api=1&bucket=demo-traces&prefix=traces${extra}`;
}

/** Wait until the api-mode refinement loop reaches its auto-stop state. */
async function waitRefined(page) {
  await page.waitForFunction(
    () => /refined$/.test(document.getElementById("fg-stats")?.textContent ?? ""),
    { timeout: 30_000 },
  );
}

export const registry = {
  // ── P. Aggregated server-side mode (?api=1) ──

  F171: async ({ page, pageUrl }) => {
    // UTC time pickers: URL ns params seed the pickers as UTC wall-clock,
    // and reading them back (Apply -> URL sync) returns the same ns.
    // Nonzero seconds: the browser normalizes a datetime-local VALUE by
    // dropping trailing ":00" seconds on read-back, which would make the
    // string comparison fail for a reason that is not this row's contract.
    const startMs = Date.UTC(2026, 3, 9, 19, 0, 7);
    const endMs = Date.UTC(2026, 3, 9, 20, 0, 8);
    // Exact decimal ns strings (ms + 6 zeros), no float round-trip.
    const startNs = `${startMs}000000`;
    const endNs = `${endMs}000000`;
    await page.goto(apiUrl(pageUrl, `&start_ns=${startNs}&end_ns=${endNs}`));
    await page.waitForSelector("#f-start", { timeout: 15_000 });
    const startVal = await page.locator("#f-start").inputValue();
    const endVal = await page.locator("#f-end").inputValue();
    const wantStart = new Date(startMs).toISOString().slice(0, 19);
    const wantEnd = new Date(endMs).toISOString().slice(0, 19);
    expect(startVal === wantStart, `From picker: ${startVal} != ${wantStart}`);
    expect(endVal === wantEnd, `To picker: ${endVal} != ${wantEnd}`);
    // Symmetry: Apply re-serializes the untouched pickers to the SAME ns.
    await page.click("#f-apply");
    const u = new URL(page.url());
    expect(
      u.searchParams.get("start_ns") === startNs,
      `start_ns after Apply: ${u.searchParams.get("start_ns")} != ${startNs}`,
    );
    expect(
      u.searchParams.get("end_ns") === endNs,
      `end_ns after Apply: ${u.searchParams.get("end_ns")} != ${endNs}`,
    );
    return `pickers show ${wantStart}/${wantEnd} UTC; Apply round-trips both ns params exactly`;
  },

  F176: async ({ page, pageUrl }) => {
    // Stats bar + coverage badge: base stats (samples, UTC range +
    // human duration) then the folded/matched badge with byte size.
    await page.goto(apiUrl(pageUrl));
    await waitRefined(page);
    const stats = await textOf(page, "#fg-stats");
    const baseSamples = /^(\d+) samples \u00b7 /.exec(stats)?.[1];
    expect(Number(baseSamples) > 0, `no base sample count: "${stats}"`);
    expect(
      /\d{2}:\d{2}:\d{2}Z? \u2192 \d{2}:\d{2}:\d{2}/.test(stats) || /\u2192/.test(stats),
      `no time range in stats: "${stats}"`,
    );
    expect(
      stats.includes("1 / 1 files (100.0%)"),
      `coverage badge missing/unexpected: "${stats}"`,
    );
    const coverageSamples = /\u00b7 (\d+) samples \u00b7 [\d.]+ MB/.exec(stats)?.[1];
    expect(coverageSamples === baseSamples, `coverage sample count differs: "${stats}"`);
    // Single-host scope: the host fraction must be omitted (1/1 hosts
    // carries no information).
    expect(!/hosts/.test(stats), `host fraction should be omitted: "${stats}"`);
    return `stats: "${stats}"`;
  },

  F177: async ({ page, pageUrl }) => {
    // Auto-stop: the dev seed freezes after one refine (1 / 1 files - a
    // second refine cannot increase files_folded), so the loop must end
    // with the "refined" suffix and flip the Stop/Refine-more pair.
    await page.goto(apiUrl(pageUrl));
    await waitRefined(page);
    const stats = await textOf(page, "#fg-stats");
    expect(/ \u00b7 refined$/.test(stats), `no refined suffix: "${stats}"`);
    const stopDisabled = await page.locator("#f-stop").isDisabled();
    const moreDisabled = await page.locator("#f-more").isDisabled();
    expect(stopDisabled, "Stop should be disabled after auto-stop");
    expect(!moreDisabled, "Refine more should be enabled after auto-stop");
    // And it STAYS stopped: no refining spinner reappears on its own.
    await page.waitForTimeout(1800); // > 2 poll intervals
    const after = await textOf(page, "#fg-stats");
    expect(/ \u00b7 refined$/.test(after), `loop resumed after auto-stop: "${after}"`);
    return `auto-stopped at "${after.slice(-60)}"; Stop disabled, Refine more enabled`;
  },
};
