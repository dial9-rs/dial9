// Shared headless-Chromium plumbing for the live UI checks.
//
// Every tool drives the pages the same way: one chromium instance, a fresh
// browser context per independent unit of work (walker / contract / scan), a
// fixed viewport, and — for the S3-browser page — a fixed clock pinned to the
// dev seed's date (see DEV_SEED_CLOCK below).

import { chromium } from "playwright";

// Deterministic viewport for every live-check run: readouts, censuses, and canvas
// hit-testing all depend on layout, so the viewport is part of the contract.
export const VIEWPORT = { width: 1440, height: 900 };

// The dev-server seeds exactly one segment under the fixed partition path
// `traces/version=1/date=2026-08-06/service=demo-service/time=0025/...`
// (dial9-viewer/src/bin/dev_server.rs). The
// page's relative time windows ("Last 1hr" quick range, raw search's implicit
// last-30-days window) are computed from Date.now(), so on a real clock the
// seeded key drifts out of every reachable window. Checks that
// target the browser page pin the page's clock (Date only — timers keep
// running, so debounces behave) near the seeded segment. This keeps the
// recorded access paths re-derivable against the same seed forever.
export const DEV_SEED_CLOCK = "2026-08-06T01:00:00Z";
export const FIXTURE_CLOCK = "2026-04-09T21:00:00Z";

export async function launchBrowser() {
  return chromium.launch();
}

/**
 * Fresh context + page. `fixedClock: true` pins Date to DEV_SEED_CLOCK;
 * passing an ISO string pins it to that instant instead.
 */
export async function newPage(browser, { fixedClock = false } = {}) {
  const context = await browser.newContext({ viewport: VIEWPORT });
  const page = await context.newPage();
  if (fixedClock) {
    await page.clock.setFixedTime(new Date(fixedClock === true ? DEV_SEED_CLOCK : fixedClock));
  }
  return { context, page };
}

/**
 * Readiness gate: the dev-server (or `dial9 serve`) answering /api/config
 * with JSON.
 */
export async function assertServerReady(baseUrl) {
  let resp;
  try {
    resp = await fetch(new URL("/api/config", baseUrl));
  } catch (e) {
    throw new Error(
      `no server at ${baseUrl} (${e.message}). Start it with:\n` +
        `  npm run build   # dev-server serves ui/dist from disk\n` +
        `  PORT=3021 cargo run -p dial9-viewer --bin dev-server --features dev-server`,
    );
  }
  if (!resp.ok) throw new Error(`GET /api/config -> HTTP ${resp.status}`);
  return resp.json();
}

/**
 * Perform `action` and capture the popup it opens. Returns the popup's
 * post-navigation URL and closes the popup (these checks only need the
 * URL; letting e.g. the viewer parse a trace would just slow the run down).
 */
export async function popupUrl(page, action) {
  const [popup] = await Promise.all([page.waitForEvent("popup"), action()]);
  // window.open(url) starts at about:blank; wait for the real navigation.
  await popup.waitForURL((u) => u.href !== "about:blank", { timeout: 10_000 });
  const url = popup.url();
  await popup.close();
  return url;
}
