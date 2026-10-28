/* Functional browser checks only; no remote dependencies or screenshots. */
const assert = require("node:assert/strict");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { chromium } = require("/tmp/dial9-fleet-browser/node_modules/playwright");
let browser;
(async () => {
  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"],
    executablePath: process.env.CHROMIUM_EXECUTABLE ||
      "/local/home/rcoh/.cache/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-linux64/chrome-headless-shell" });
  const page = await browser.newPage({ viewport: { width: 1500, height: 1000 } });
  const errors = [];
  page.on("pageerror", e => errors.push(e.message));
  await page.route("http://**/*", route => route.abort());
  await page.route("https://**/*", route => route.abort());
  await page.goto(pathToFileURL(path.join(__dirname, "sampling-fleet-laboratory.html")).href);
  await page.waitForFunction(() => document.getElementById("run-status").textContent.startsWith("Done."));
  const count = async text => page.locator("#slice-facts tr").filter({ hasText: text }).locator("td").textContent();
  assert.equal(await count("Physical hosts alive"), "1,000");
  assert.equal(await count("Processes profiling now"), "0");
  assert.match(await page.locator("#weighting-reading").textContent(), /Coverage gap/);
  assert.equal(await page.locator("#historical-facts tr").filter({ hasText: "Observed members with no sampling support" }).locator("td").textContent(), "1,000");
  await page.locator("#history-age").fill("0");
  await page.locator("#history-age").dispatchEvent("input");
  assert.match(await page.locator("#historical-reading").textContent(), /No sufficiently recent/);
  await page.locator("#history-age").fill("180");
  await page.locator("#history-age").dispatchEvent("input");
  assert.match(await page.locator("#census-error-band").textContent(), /exactly zero/);
  assert.equal(await page.locator("#assumed-label").isVisible(), false);
  await page.locator("#fallback").selectOption("assume");
  assert.equal(await page.locator("#assumed-label").isVisible(), true);
  await page.locator("#simulate").click();
  await page.waitForFunction(() => document.getElementById("run-status").textContent.startsWith("Done."));
  assert(Number((await count("Processes profiling now")).replaceAll(",", "")) > 0);
  for (const scenario of ["rolling", "minute", "containers"]) {
    await page.locator("#scenario").selectOption(scenario);
    await page.locator("#simulate").click();
    await page.waitForFunction(() => document.getElementById("run-status").textContent.startsWith("Done."));
    assert(await page.locator("#population-chart path.data").count() === 4);
    assert(await page.locator("#profiling-chart path.data").count() === 2);
    assert(await page.locator("#historical-facts tr").count() === 6);
    if (scenario === "rolling") assert.match(await page.locator("#weight-log").textContent(), /terminal exclusion/);
  }
  assert.equal(await count("Physical hosts alive"), "250");
  assert.equal(await count("Process / container instances alive"), "5,000");
  await page.locator("#time").fill("0");
  await page.locator("#time").dispatchEvent("input");
  assert.equal(await page.locator("#time-label").textContent(), "0:00");
  await page.locator("#lower").fill("100000");
  await page.locator("#simulate").click();
  await page.waitForFunction(() => document.getElementById("run-status").textContent.startsWith("Done."));
  assert.match(await page.locator("#tracking-readout").textContent(), /1 \/ 128/);
  for (const width of [320, 360, 680, 1000, 1500]) {
    await page.setViewportSize({ width, height: 900 });
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
    assert.equal(overflow, false, "No horizontal page overflow at width " + width);
  }
  assert.deepEqual(errors, []);
  await browser.close();
  console.log("Passed: offline page, all scenario buttons, fallback control, slice cursor, sparse curves, chart population units, and responsive widths.");
})().catch(async error => {
  console.error(error);
  await browser?.close();
  process.exitCode = 1;
});
