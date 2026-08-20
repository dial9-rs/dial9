// Shared page actions + assertion helpers for the browser (index.html) page.
//
// These encode the access paths the inventory walkers drive repeatedly:
// waiting out the load-time bootstrap (config -> prefix discovery ->
// auto-search), running the demo-window browse search that reaches the dev
// seed's single segment, and running a raw search that yields rows.
//
// The page clock is pinned to DEV_SEED_CLOCK (see lib/browser.mjs), which
// keeps raw search's implicit last-30-days window over the seeded date.

export class WalkError extends Error {}

export function expect(cond, msg) {
  if (!cond) throw new WalkError(msg);
}

export async function textOf(page, selector) {
  return (await page.locator(selector).textContent())?.trim() ?? "";
}

/**
 * Wait for the browser page's load-time bootstrap to settle: bucket/prefix
 * prefilled from /api/config, prefix chips discovered, Search enabled, and
 * the load-time auto-search no longer in flight.
 */
export async function waitBrowserBootstrap(page) {
  await page.waitForSelector("#prefix-suggestions button", { timeout: 15_000 });
  await page.waitForSelector("#search-btn:not([disabled])", { timeout: 15_000 });
  // The auto-search fires on load; let it settle so a walker's own search
  // can't race it. A search that found results HIDES the status element and
  // leaves its text as "Searching…" (index.html search handler), so hidden
  // counts as settled when another check waits again after a search.
  await page.waitForFunction(
    () => {
      const el = document.getElementById("browse-status");
      return el.style.display === "none" || !el.textContent.includes("Searching");
    },
    { timeout: 15_000 },
  );
}

/** Load the browser page and wait out the bootstrap. */
export async function gotoBrowserPage(page, pageUrl) {
  await page.goto(pageUrl);
  await waitBrowserBootstrap(page);
}

/**
 * Browse-tab search over a window that contains the dev seed's segment.
 * The narrow window also lets service discovery observe the
 * `version=1/date=2026-08-06/service=demo-service/time=0025` bucket.
 * Waits for the heatmap to render.
 */
export async function searchDemoWindow(page) {
  await page.fill("#range-from", "2026-08-06T00:20");
  await page.fill("#range-to", "2026-08-06T00:30");
  await page.click("#search-btn");
  await page.waitForSelector("#heatmap-view", { state: "visible", timeout: 20_000 });
  await page.waitForSelector("#heatmap-labels .row", { timeout: 20_000 });
}

// Must match ROW_H in index.html (px height of each heatmap host row).
export const HEATMAP_ROW_H = 26;

/** Plain drag across most of row 0 of the heatmap -> region selection. */
export async function dragSelectRowZero(page) {
  const plot = page.locator("#heatmap-plot");
  const box = await plot.boundingBox();
  const y = box.y + HEATMAP_ROW_H / 2;
  await page.mouse.move(box.x + box.width * 0.1, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * 0.9, y, { steps: 5 });
  await page.mouse.up();
  await page.waitForFunction(
    () => /segment/.test(document.getElementById("selection-count").textContent),
    { timeout: 5_000 },
  );
}

/** Alt(Option)+drag horizontally -> zoom the heatmap time axis. */
export async function altDragZoom(page) {
  const plot = page.locator("#heatmap-plot");
  const box = await plot.boundingBox();
  const y = box.y + HEATMAP_ROW_H / 2;
  await page.keyboard.down("Alt");
  await page.mouse.move(box.x + box.width * 0.3, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * 0.6, y, { steps: 5 });
  await page.mouse.up();
  await page.keyboard.up("Alt");
}

/**
 * Raw-search that yields the seeded row. The configured prefix is already
 * applied, so the additional raw sub-prefix stays empty. The pinned clock
 * keeps the seed inside the implicit last-30-days window.
 */
export async function rawSearchSeededRows(page) {
  await page.click("#tab-raw");
  await page.click("#raw-view button:has-text('Search')");
  await page.waitForSelector("#raw-table", { state: "visible", timeout: 30_000 });
  await page.waitForSelector("#raw-body tr", { timeout: 30_000 });
}

/** Open the panel and switch its credential mode to literal inputs. */
export async function openLiteralCreds(page) {
  // The header button TOGGLES the panel — only click it when closed.
  if (!(await page.locator("#creds-panel").isVisible())) {
    await page.click("#creds-btn");
  }
  await page.waitForSelector("#creds-panel", { state: "visible" });
  if (!(await page.locator("#creds-literal-fields").isVisible())) {
    await page.click("#creds-use-literal");
  }
  await page.waitForSelector("#creds-literal-fields", { state: "visible" });
}

/** Apply the dev-server's test credentials. */
export async function applyTestCreds(page) {
  await openLiteralCreds(page);
  await page.fill("#creds-akid", "test");
  await page.fill("#creds-secret", "test");
  await page.click("#creds-apply");
  await page.waitForFunction(
    () => /bucket\(s\)|Rejected/.test(document.getElementById("creds-status").textContent),
    { timeout: 15_000 },
  );
}
