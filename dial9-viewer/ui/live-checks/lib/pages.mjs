// Page-kind detection and loaded-state waits shared by the live UI checks.

import { waitBrowserBootstrap } from "./actions.mjs";

export const LOADED_WAITS = {
  // Trace parsed and stats line rendered.
  viewer: (page) =>
    page.waitForFunction(
      () => /[\d,]+ events/.test(document.getElementById("toolbar-row-data")?.textContent ?? ""),
      { timeout: 60_000 },
    ),
  // Flamegraph canvases rendered.
  flamegraph: (page) => page.waitForSelector(".fg-canvas", { timeout: 60_000 }),
  // Browser page bootstrap settled.
  index: (page) => waitBrowserBootstrap(page),
};

/** Infer the page kind from a page URL, or null for unsupported pages. */
export function pageKindFor(pageUrl) {
  const p = new URL(pageUrl).pathname;
  if (p.endsWith("/viewer.html")) return "viewer";
  if (p.endsWith("/flamegraph.html")) return "flamegraph";
  if (p.endsWith("/index.html") || p === "/") return "index";
  return null;
}

/**
 * Wait until a page is settled. Unknown pages and trace pages without a
 * `?trace=` fall back to the load event plus a short render settle.
 */
export async function waitLoadedByUrl(page, pageUrl) {
  const kind = pageKindFor(pageUrl);
  const hasTrace = new URL(pageUrl).searchParams.has("trace");
  if (kind === "index" || (kind && hasTrace)) {
    await LOADED_WAITS[kind](page);
  } else {
    await page.waitForLoadState("load");
    await page.waitForTimeout(750);
  }
}
