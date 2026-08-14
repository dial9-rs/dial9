// Ad-hoc shell verification (dev-only): loads the canonical viewer shell,
// waits for the demo trace, then dumps the relevant observations - track
// slots (label + canvas drawW sized by layout), landmark/tab order
// (toolbar -> minimap -> tracks -> inspector), help-overlay toggle, and a
// toast round-trip. Run against a dev-server serving ui/dist:
//   node parity/verify-viewer-shell.mjs --url http://localhost:3210/viewer.html
import { chromium } from "playwright";

const url =
  process.argv.includes("--url")
    ? process.argv[process.argv.indexOf("--url") + 1]
    : "http://localhost:3210/viewer.html";

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = [];
page.on("console", (m) => {
  if (m.type() === "error") errors.push(m.text());
});
page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));

await page.goto(url);
// Loaded-wait: the toolbar file-meta shows the event count once the demo
// trace parses in the worker.
await page.waitForFunction(
  () => /[\d,]+ events/.test(document.querySelector("[data-file-meta]")?.textContent ?? ""),
  { timeout: 60_000 },
);

const fileMeta = await page.textContent("[data-file-meta]");

// Track slots: label + sized empty canvas.
const tracks = await page.evaluate(() =>
  [...document.querySelectorAll(".d9-track")].map((t) => {
    const canvas = t.querySelector("canvas.d9-track-canvas");
    return {
      id: t.getAttribute("data-track-id"),
      label: t.querySelector(".d9-track-name")?.textContent?.trim(),
      drawW: canvas?.dataset.drawW ?? null,
      canvasCssW: canvas ? Math.round(canvas.getBoundingClientRect().width) : null,
      backingW: canvas?.width ?? null,
    };
  }),
);

// Landmark order + tab order.
const landmarks = await page.evaluate(() =>
  [...document.querySelectorAll("[role]")]
    .filter((e) =>
      ["banner", "region", "application", "complementary", "contentinfo"].includes(
        e.getAttribute("role"),
      ),
    )
    .map((e) => `${e.getAttribute("role")}:${e.getAttribute("aria-label") ?? ""}`),
);

const tabOrder = [];
for (let i = 0; i < 6; i++) {
  await page.keyboard.press("Tab");
  const active = await page.evaluate(() => {
    const a = document.activeElement;
    if (!a) return null;
    const region =
      a.closest("[role=banner]") ? "toolbar" :
      a.closest(".d9-minimap") ? "minimap" :
      a.closest(".d9-track-column") ? "tracks" :
      a.closest(".d9-inspector") ? "inspector" :
      a.closest("[role=contentinfo]") ? "status" : "other";
    return `${region}:${(a.getAttribute("aria-label") || a.textContent || a.tagName).trim().slice(0, 24)}`;
  });
  tabOrder.push(active);
}

// Help overlay toggle via `?`.
await page.keyboard.press("?");
const helpOpenAfterKey = await page.evaluate(
  () => document.querySelector(".d9-help-backdrop")?.classList.contains("open") ?? false,
);
const helpTitle = await page.evaluate(
  () => document.querySelector(".d9-help h2")?.textContent ?? null,
);
await page.keyboard.press("Escape");
const helpOpenAfterEsc = await page.evaluate(
  () => document.querySelector(".d9-help-backdrop")?.classList.contains("open") ?? false,
);

// Toast round-trip via the mounted manager: driving it by dispatching
// through the shell's toast region directly is not exposed, so assert the
// region exists + is a status live region instead.
const toastRegion = await page.evaluate(() => {
  const el = document.querySelector(".d9-toast-region");
  return el
    ? { role: el.getAttribute("role"), live: el.getAttribute("aria-live") }
    : null;
});
const announceRegion = await page.evaluate(() => {
  const el = document.querySelector(".d9-announce-region");
  return el ? { live: el.getAttribute("aria-live") } : null;
});

console.log(JSON.stringify(
  {
    fileMeta,
    tracks,
    landmarks,
    tabOrder,
    help: { helpOpenAfterKey, helpTitle, helpOpenAfterEsc },
    toastRegion,
    announceRegion,
    consoleErrors: errors,
  },
  null,
  2,
));

await browser.close();
