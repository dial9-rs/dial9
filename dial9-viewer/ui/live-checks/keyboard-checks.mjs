// Checks that `?` opens the unified help overlay at landing time on each
// page below, and Escape closes it again.
//
// Page instances checked:
//   - index.html                         (browser page)
//   - flamegraph.html?trace=...          (flamegraph, exact mode)
//   - flamegraph.html?api=1&...          (flamegraph, aggregated mode)
//
// Usage (dev-server serving a BUILT dist, dev seed):
//   node live-checks/keyboard-checks.mjs --base http://localhost:3101
//
// Exits 1 on the first page where the overlay fails to open or close.

import process from "node:process";
import { parseArgs, usage } from "./lib/cli.mjs";
import { launchBrowser, newPage, assertServerReady } from "./lib/browser.mjs";
import { waitBrowserBootstrap } from "./lib/actions.mjs";

const SPEC = {
  base: { required: true, help: "server origin (e.g. http://localhost:3101)" },
};

const PAGES = [
  {
    name: "browser page (index.html)",
    path: "/index.html",
    fixedClock: true,
    waitLoaded: (page) => waitBrowserBootstrap(page),
  },
  {
    name: "flamegraph exact mode (flamegraph.html?trace=/demo-trace.bin)",
    path: "/flamegraph.html?trace=/demo-trace.bin",
    fixedClock: false,
    // Keys mount after the widget renders (landing time).
    waitLoaded: (page) => page.waitForSelector(".fg-canvas", { timeout: 60_000 }),
  },
  {
    name: "flamegraph api mode (flamegraph.html?api=1&bucket=demo-traces&prefix=traces)",
    path: "/flamegraph.html?api=1&bucket=demo-traces&prefix=traces",
    fixedClock: false,
    waitLoaded: (page) => page.waitForSelector(".fg-canvas", { timeout: 60_000 }),
  },
];

async function overlayOpen(page) {
  return page.evaluate(() => {
    const el = document.querySelector(".d9-help-backdrop");
    return el !== null && el.classList.contains("open");
  });
}

async function main() {
  let opts;
  try {
    ({ opts } = parseArgs(process.argv.slice(2), SPEC));
  } catch (e) {
    console.error(String(e.message));
    console.error(usage("live-checks/keyboard-checks.mjs", SPEC));
    process.exit(2);
  }

  await assertServerReady(opts.base);
  const browser = await launchBrowser();
  let failed = false;
  try {
    for (const spec of PAGES) {
      const { context, page } = await newPage(browser, {
        fixedClock: spec.fixedClock,
      });
      try {
        await page.goto(opts.base + spec.path);
        await spec.waitLoaded(page);
        // Focus must be body-ish (not a text field) for `?` to route.
        await page.keyboard.press("?");
        await page.waitForTimeout(150);
        const opened = await overlayOpen(page);
        await page.keyboard.press("Escape");
        await page.waitForTimeout(150);
        const closed = !(await overlayOpen(page));
        const ok = opened && closed;
        failed = failed || !ok;
        console.log(
          `${ok ? "PASS" : "FAIL"}  ${spec.name}  ` +
            `(? opened: ${opened}, Esc closed: ${closed})`,
        );
      } finally {
        await context.close();
      }
    }
  } finally {
    await browser.close();
  }
  process.exit(failed ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(2);
});
