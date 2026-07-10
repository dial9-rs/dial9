// Declarative journey-step executor, shared by the behavioral differ
// (behavior-diff.mjs) and the perf probe (perf-probe.mjs).
//
// A journey (parity/journeys.mjs) is a page kind + a list of steps. Each
// step is an object with exactly one action key:
//
//   { waitLoaded: true }              page-kind-specific "data is on screen"
//   { waitFor: "sel" }                selector visible
//   { waitHidden: "sel" }             selector hidden/absent
//   { waitText: ["sel", "regex"] }    selector's text matches
//   { click: "sel" }                  click
//   { dblclick: "sel" }               double-click
//   { fill: ["sel", "value"] }        fill an input
//   { press: ["sel", "Key"] }         key press on an element
//   { key: "Key" } / { key: ["Key", n] }   page-level key press (xN)
//   { focus: "sel" }                  focus an element
//   { hover: "sel" }                  hover element center
//   { hoverSweep: ["sel", n] }        n mousemove points left->right across el
//   { wheel: { selector, dy, modifier? } }  wheel at element center
//   { drag: { selector, from: [fx,fy], to: [fx,fy], alt? } }  fractional drag
//   { sleep: ms }                     settle wait (keep rare and short)
//   { checkpoint: "name" }            capture readouts here (differ only)
//
// Fractions in drag/hoverSweep are relative to the element's bounding box.

import { waitBrowserBootstrap } from "./actions.mjs";

const LOADED_WAITS = {
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

async function box(page, selector) {
  const b = await page.locator(selector).boundingBox();
  if (!b) throw new Error(`no bounding box for ${selector}`);
  return b;
}

/**
 * Run a journey's steps on `page` (already navigated to the journey URL).
 * `onCheckpoint(name)` is awaited at every checkpoint step.
 */
export async function runSteps(page, journey, { onCheckpoint } = {}) {
  await LOADED_WAITS[journey.page](page);
  for (const step of journey.steps) {
    const [action] = Object.keys(step);
    const arg = step[action];
    switch (action) {
      case "waitLoaded":
        await LOADED_WAITS[journey.page](page);
        break;
      case "waitFor":
        await page.waitForSelector(arg, { state: "visible", timeout: 30_000 });
        break;
      case "waitHidden":
        await page.waitForSelector(arg, { state: "hidden", timeout: 30_000 });
        break;
      case "waitText": {
        const [sel, re] = arg;
        await page.waitForFunction(
          ([s, r]) => new RegExp(r).test(document.querySelector(s)?.textContent ?? ""),
          [sel, re],
          { timeout: 30_000 },
        );
        break;
      }
      case "click":
        await page.click(arg);
        break;
      case "dblclick":
        await page.dblclick(arg);
        break;
      case "fill":
        await page.fill(arg[0], arg[1]);
        break;
      case "press":
        await page.press(arg[0], arg[1]);
        break;
      case "key": {
        const [k, n] = Array.isArray(arg) ? arg : [arg, 1];
        for (let i = 0; i < n; i++) await page.keyboard.press(k);
        break;
      }
      case "focus":
        await page.focus(arg);
        break;
      case "hover":
        await page.hover(arg);
        break;
      case "hoverSweep": {
        const [sel, n] = arg;
        const b = await box(page, sel);
        for (let i = 0; i <= n; i++) {
          await page.mouse.move(b.x + (b.width * i) / n, b.y + b.height / 2);
        }
        break;
      }
      case "wheel": {
        const b = await box(page, arg.selector);
        await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2);
        if (arg.modifier) await page.keyboard.down(arg.modifier);
        await page.mouse.wheel(arg.dx ?? 0, arg.dy ?? 0);
        if (arg.modifier) await page.keyboard.up(arg.modifier);
        break;
      }
      case "drag": {
        const b = await box(page, arg.selector);
        const [fx0, fy0] = arg.from;
        const [fx1, fy1] = arg.to;
        if (arg.alt) await page.keyboard.down("Alt");
        await page.mouse.move(b.x + b.width * fx0, b.y + b.height * fy0);
        await page.mouse.down();
        await page.mouse.move(b.x + b.width * fx1, b.y + b.height * fy1, { steps: 8 });
        await page.mouse.up();
        if (arg.alt) await page.keyboard.up("Alt");
        break;
      }
      case "sleep":
        await page.waitForTimeout(arg);
        break;
      case "checkpoint":
        if (onCheckpoint) await onCheckpoint(arg);
        break;
      default:
        throw new Error(`unknown journey step action "${action}"`);
    }
  }
}
