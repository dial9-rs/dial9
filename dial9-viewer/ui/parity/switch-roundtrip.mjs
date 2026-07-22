// The dual-UI switch round-trip keeps the bucket/search context: the query
// string is preserved through both hops via click-time href resolution. Not
// a registered parity tool - a one-shot check:
//   node parity/switch-roundtrip.mjs http://localhost:3061
// Exits 0 when both hops preserve the query.

import { launchBrowser, newPage } from "./lib/browser.mjs";
import { waitBrowserBootstrap } from "./lib/actions.mjs";

const base = process.argv[2] ?? "http://localhost:3061";
const browser = await launchBrowser();
let failures = 0;

function check(cond, msg) {
  if (cond) {
    console.log(`ok: ${msg}`);
  } else {
    failures++;
    console.log(`FAIL: ${msg}`);
  }
}

try {
  const { context, page } = await newPage(browser, { fixedClock: true });

  // Legacy page with in-page state: bucket + prefix + Last 24hr.
  await page.goto(`${base}/index.html?bucket=demo-traces&prefix=traces&last=24`);
  await waitBrowserBootstrap(page);
  const legacyQuery = new URL(page.url()).search;
  await page.waitForSelector("#d9-ui-switch", { state: "visible" });
  check(
    (await page.locator("#d9-ui-switch").textContent()) === "Switch to new UI",
    "legacy side renders the switch control",
  );

  // Hop 1: legacy -> new. Click-time href resolution must carry the live
  // query string.
  await page.click("#d9-ui-switch");
  await page.waitForURL(/\/new\/index\.html/);
  await waitBrowserBootstrap(page);
  const u1 = new URL(page.url());
  check(u1.pathname === "/new/index.html", `landed on ${u1.pathname}`);
  check(
    u1.search === legacyQuery,
    `query preserved legacy->new (${legacyQuery} -> ${u1.search})`,
  );
  check(
    (await page.inputValue("#bucket-input")) === "demo-traces",
    "bucket restored on the new page",
  );
  check(
    (await page.inputValue("#prefix-input")) === "traces",
    "prefix restored on the new page",
  );
  check(
    (await page.locator(".quick-btns button.selected").textContent()) === "Last 24hr",
    "last=24 quick range restored on the new page",
  );

  // Hop 2: new -> legacy. ui=legacy pinned, rest of the query preserved.
  await page.waitForSelector("#d9-ui-switch", { state: "visible" });
  check(
    (await page.locator("#d9-ui-switch").textContent()) === "Switch to legacy UI",
    "new side renders the way back",
  );
  await page.click("#d9-ui-switch");
  await page.waitForURL(/\/index\.html/);
  const u2 = new URL(page.url());
  check(u2.pathname === "/index.html", `landed back on ${u2.pathname}`);
  await waitBrowserBootstrap(page);
  // The legacy page's first URL sync strips the ui=legacy pin; the state
  // params must survive it.
  const u3 = new URL(page.url());
  check(
    u3.search === legacyQuery,
    `query preserved new->legacy after sync (${u3.search})`,
  );
  check(
    (await page.inputValue("#bucket-input")) === "demo-traces",
    "bucket restored back on legacy",
  );

  await context.close();
} finally {
  await browser.close();
}

console.log(failures === 0 ? "ROUND-TRIP OK" : `${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
