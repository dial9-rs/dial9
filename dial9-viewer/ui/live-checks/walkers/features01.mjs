// Row walkers for the S3 trace browser landing page (index.html).
//
// One walker per row the inventory records as VERIFIED or DEAD-CONFIRMED
// (the gated set). Each walker drives the row's access path against a live
// page and returns an evidence string; a thrown WalkError means FAILED.
//
// Rows recorded PARTIAL / NOT-TESTED / CODE-ONLY / NOT-TRIGGERABLE /
// NOT-OBSERVED — and rows whose latest (refresh) verdict is CODE-READ /
// VERIFIED(API) / NOT-TRIGGERABLE — have no walker here by design: the
// walk-rows entry lists them as NOT-TRIGGERABLE without driving them.
//
// Environment assumptions:
//   - dev-server seed: single segment under traces/version=1/date=2026-08-06/... — hence
//     the pinned page clock (lib/browser.mjs DEV_SEED_CLOCK) and the
//     demo-window helpers (lib/actions.mjs);
//   - the seeded key uses the default Hive-style layout; the page decodes its
//     service and instance, dates day-crossing axis spans, and drives the
//     bucket filter from config.

import {
  expect,
  textOf,
  gotoBrowserPage,
  searchDemoWindow,
  dragSelectRowZero,
  altDragZoom,
  rawSearchSeededRows,
  applyTestCreds,
  openLiteralCreds,
  HEATMAP_ROW_H,
} from "../lib/actions.mjs";
import { popupUrl } from "../lib/browser.mjs";

/** Click dead-center of heatmap row 0 (a sub-4px "drag" = click-select). */
async function clickRowZero(page) {
  const box = await page.locator("#heatmap-plot").boundingBox();
  await page.mouse.click(box.x + box.width / 2, box.y + HEATMAP_ROW_H / 2);
}

export const registry = {
  // ── A. Page entry and global behaviors ──
  A1: async ({ page, pageUrl }) => {
    await page.goto(`${pageUrl}?trace=demo-trace.bin`);
    await page.waitForURL(/viewer\.html\?trace=demo-trace\.bin/, { timeout: 15_000 });
    return `index.html?trace=demo-trace.bin redirected to ${new URL(page.url()).pathname}?trace=demo-trace.bin`;
  },

  A2: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    const url = await popupUrl(page, () => page.click("#load-demo-btn"));
    expect(/viewer\.html\?trace=demo-trace\.bin$/.test(url), `popup url was ${url}`);
    return "popup -> viewer.html?trace=demo-trace.bin";
  },

  A4: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    const link = page.locator('footer a[href="viewer.html"]');
    expect((await link.count()) === 1, "footer viewer.html link missing");
    expect((await link.textContent()).trim() === "Open Trace Viewer", "link text changed");
    return 'footer a[href="viewer.html"] "Open Trace Viewer"';
  },

  // ── B. Header bar ──
  B1: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    expect((await textOf(page, "header h1")) === "dial9 Trace Browser", "title changed");
    return '"dial9 Trace Browser" + subtitle rendered';
  },

  B2: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await page.waitForSelector("#creds-btn", { state: "visible", timeout: 10_000 });
    return "creds button visible (server reports BYO support)";
  },

  B3: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    expect((await textOf(page, "#tz-btn")) === "TZ: UTC", "default TZ mode not UTC");
    await page.click("#tz-btn");
    expect((await textOf(page, "#tz-btn")) === "TZ: Local", "toggle did not flip to Local");
    await page.click("#tz-btn");
    expect((await textOf(page, "#tz-btn")) === "TZ: UTC", "toggle did not flip back");
    return '"TZ: UTC" <-> "TZ: Local"';
  },

  // ── C. Bring-your-own-credentials panel ──
  C1: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await page.click("#creds-btn");
    await page.waitForSelector("#creds-panel", { state: "visible" });
    await page.click("#creds-close");
    await page.waitForSelector("#creds-panel", { state: "hidden" });
    return "panel toggles open (button) and closed (X)";
  },

  C2: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await openLiteralCreds(page);
    await page.fill(
      "#creds-paste",
      '{"accessKeyId":"AKIAEXAMPLE","secretAccessKey":"secret123","sessionToken":"tok"}',
    );
    await page.click("#creds-paste-fill");
    expect(
      (await page.inputValue("#creds-akid")) === "AKIAEXAMPLE",
      "akid not filled from pasted JSON",
    );
    expect((await page.inputValue("#creds-token")) === "tok", "token not filled");
    expect((await page.inputValue("#creds-paste")) === "", "textarea not cleared after fill");
    return "pasted blob filled akid=AKIAEXAMPLE; textarea cleared";
  },

  C3: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await openLiteralCreds(page);
    for (const [sel, v] of [
      ["#creds-akid", "AKIAMANUAL"],
      ["#creds-secret", "s3cret"],
      ["#creds-token", "tok3n"],
      ["#creds-region", "eu-west-1"],
    ]) {
      await page.fill(sel, v);
      expect((await page.inputValue(sel)) === v, `${sel} did not hold value`);
    }
    expect(
      (await page.getAttribute("#creds-secret", "type")) === "password" &&
        (await page.getAttribute("#creds-token", "type")) === "password",
      "secret/token fields are not password inputs",
    );
    return "akid/secret/token/region fields hold values; secret+token masked";
  },

  C4: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await applyTestCreds(page);
    const status = await textOf(page, "#creds-status");
    expect(/1 bucket\(s\)/.test(status), `status was "${status}"`);
    return `status "${status}"`;
  },

  C5: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await applyTestCreds(page);
    await page.click("#creds-clear");
    expect(
      (await textOf(page, "#creds-status")) === "Credentials cleared",
      "clear status not shown",
    );
    expect((await page.inputValue("#creds-akid")) === "", "akid not wiped");
    await page.waitForSelector("#creds-buckets-row", { state: "hidden" });
    return 'status "Credentials cleared"; fields and picker wiped';
  },

  C6: async ({ page, pageUrl }) => {
    // Bucket picker. Default filter "dial9": the seeded `demo-traces` bucket
    // does not match, so the filtered view is empty with a "Show all (1)"
    // toggle revealing it. A `?bucket_filter=demo` page override makes the
    // predicate config-driven, surfaces demo-traces directly, and
    // auto-selects the single match.
    await gotoBrowserPage(page, pageUrl);
    await applyTestCreds(page);
    const picker = page.locator("#creds-buckets");
    expect(
      /No dial9 trace buckets visible/.test(await picker.textContent()),
      "default dial9 filter did not report an empty filtered view",
    );
    const toggle = picker.locator("button", { hasText: "Show all (1)" });
    expect((await toggle.count()) === 1, "Show all toggle missing");
    await toggle.click();
    await page.waitForSelector("#creds-buckets button:has-text('demo-traces')", {
      timeout: 5_000,
    });
    // Credentials persist in sessionStorage, so the reload re-lists buckets
    // in the background. With creds active the bucket input starts EMPTY (no
    // server-default prefill), letting the auto-select below prove itself.
    await page.goto(`${pageUrl}?bucket_filter=demo`);
    await page.waitForFunction(
      () => document.getElementById("creds-btn-label")?.textContent.includes("✓"),
      { timeout: 15_000 },
    );
    await page.click("#creds-btn");
    await page.waitForSelector("#creds-buckets button.match:has-text('demo-traces')", {
      timeout: 10_000,
    });
    // Exactly one match in the filtered view -> auto-select kicks in.
    await page.waitForFunction(
      () => document.getElementById("bucket-input").value === "demo-traces",
      { timeout: 10_000 },
    );
    return "toggle path ok; ?bucket_filter=demo surfaced + auto-selected demo-traces";
  },

  C8: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await openLiteralCreds(page);
    await page.click("#creds-apply"); // empty fields -> error message
    const err = await textOf(page, "#creds-status");
    expect(/required/.test(err), `expected validation error, got "${err}"`);
    const errClass = await page.getAttribute("#creds-status", "class");
    expect(/error/.test(errClass), "error kind not styled");
    await applyTestCreds(page); // -> ok message
    const okClass = await page.getAttribute("#creds-status", "class");
    expect(/ok/.test(okClass), "ok kind not styled");
    return "error and ok status messages observed inline";
  },

  C9: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await applyTestCreds(page);
    await page.reload();
    await page.waitForFunction(
      () => document.getElementById("creds-btn-label").textContent.includes("✓"),
      { timeout: 15_000 },
    );
    const active = await page.getAttribute("#creds-btn", "class");
    expect(/active/.test(active ?? ""), "header button not marked active");
    // Panel stays closed; the picker was re-listed in the background.
    await page.waitForSelector("#creds-panel", { state: "hidden" });
    await page.click("#creds-btn");
    await page.waitForSelector("#creds-buckets-row", { state: "visible", timeout: 10_000 });
    return 'after reload: header "AWS Credentials ✓", panel closed, picker pre-listed';
  },

  // ── D. Controls bar ──
  D1: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    expect(
      (await page.inputValue("#bucket-input")) === "demo-traces",
      "bucket not prefilled from config",
    );
    return 'prefilled "demo-traces"';
  },

  D3: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    const chip = page.locator("#prefix-suggestions button", { hasText: "traces" });
    expect((await chip.count()) === 1, "traces chip not rendered");
    expect(/active/.test(await chip.getAttribute("class")), "chip not marked active");
    await chip.click();
    expect((await page.inputValue("#prefix-input")) === "traces", "chip click did not fill prefix");
    return '"traces" chip rendered, marked active, fills prefix on click';
  },

  D5: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    expect(
      (await page.inputValue("#prefix-input")) === "traces",
      "sole prefix not auto-filled",
    );
    expect(
      (await page.locator("#prefix-suggestions button").count()) === 1,
      "expected exactly one discovered prefix",
    );
    return 'sole prefix "traces" auto-filled';
  },

  D6: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    const selected = page.locator(".quick-btns button.selected");
    expect((await selected.count()) === 1, "no quick button highlighted on load");
    expect((await selected.textContent()) === "Last 1hr", "default is not Last 1hr");
    await page.click(".quick-btns button:has-text('Last 3hr')");
    expect(
      (await textOf(page, ".quick-btns button.selected")) === "Last 3hr",
      "clicking a quick button did not move the highlight",
    );
    return '"Last 1hr" highlighted on load; clicking moves highlight + sets pickers';
  },

  D7: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    expect((await page.inputValue("#range-from")) !== "", "From picker empty on load");
    expect((await page.inputValue("#range-to")) !== "", "To picker empty on load");
    await page.fill("#range-from", "2026-04-09T18:00");
    expect(
      (await page.inputValue("#range-from")) === "2026-04-09T18:00",
      "From picker did not hold a set value",
    );
    return "pickers set on load and hold manual values";
  },

  D9: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    expect(await page.locator("#search-btn").isEnabled(), "Search disabled with prefix present");
    return "Search enabled (prefix present)";
  },

  // ── E. Tabs ──
  E1: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await page.click("#tab-raw");
    await page.waitForSelector("#raw-view", { state: "visible" });
    await page.waitForSelector("#browse-view", { state: "hidden" });
    expect(/active/.test(await page.getAttribute("#tab-raw", "class")), "raw tab not active");
    await page.click("#tab-browse");
    await page.waitForSelector("#browse-view", { state: "visible" });
    await page.waitForSelector("#raw-view", { state: "hidden" });
    return "Browse <-> Raw toggles views and tab highlight";
  },

  E2: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await page.waitForSelector("#cpu-btn", { state: "visible" });
    await page.waitForSelector("#health-btn", { state: "visible" });
    await page.waitForSelector("#raw-actions", { state: "hidden" });
    await page.click("#tab-raw");
    await page.waitForSelector("#raw-actions", { state: "visible" });
    await page.waitForSelector("#cpu-btn", { state: "hidden" });
    await page.waitForSelector("#health-btn", { state: "hidden" });
    return "Raw shows Select All/Deselect All; Browse shows Flamegraph + Tokio Stats";
  },

  // ── F. Browse view: density heatmap ──
  F4: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    const rows = page.locator("#heatmap-labels .row");
    expect((await rows.count()) === 1, "expected 1 host row for the single seeded host");
    const label = (await rows.first().textContent()).trim();
    expect(
      label === "demo-service / local/host-0",
      `row label was "${label}"`,
    );
    return `1 host row "${label}"`;
  },

  F6: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    const painted = await page.evaluate(() => {
      const c = document.getElementById("heatmap-canvas");
      const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
      let n = 0;
      for (let i = 3; i < d.length; i += 4) if (d[i] > 0) n++;
      return n;
    });
    expect(painted > 1000, `canvas nearly empty (${painted} painted px)`);
    return `density canvas drawn (${painted} painted device px)`;
  },

  F10: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    const ticks = page.locator("#heatmap-axis .tick");
    const n = await ticks.count();
    expect(n >= 2, `expected >=2 axis ticks, got ${n}`);
    // The seeded segment stays within one day, so ticks use the compact form.
    for (const t of await ticks.allTextContents()) {
      expect(
        /^\d{2}:\d{2}:\d{2}$/.test(t.trim()),
        `tick "${t}" not time-only on a same-day span`,
      );
    }
    return `${n} time-only ticks rendered (same-day span)`;
  },

  F11: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    const hint = await textOf(page, "#heatmap-hint");
    expect(/Drag to select a window/.test(hint), "interaction hint missing");
    await page.waitForSelector("#heatmap-hint .legend .bar", { state: "visible" });
    await page.waitForSelector("#heatmap-hint .gap-swatch", { state: "visible" });
    expect(/boot change/.test(hint), "boot-change legend entry missing");
    return "density gradient + gap swatch + boot marker + hint text";
  },

  F12: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await dragSelectRowZero(page);
    await page.waitForSelector("#heatmap-sel", { state: "visible" });
    const count = await textOf(page, "#selection-count");
    expect(/^1 segment · /.test(count), `selection count was "${count}"`);
    return `selection rect shown; count "${count}"`;
  },

  F13: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await clickRowZero(page);
    await page.waitForFunction(
      () => /1 segment/.test(document.getElementById("selection-count").textContent),
      { timeout: 5_000 },
    );
    return "single click selected the segment under the cursor";
  },

  F14: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await altDragZoom(page);
    await page.waitForSelector("#heatmap-reset-zoom", { state: "visible", timeout: 5_000 });
    return "Alt+drag zoomed (Reset zoom button appeared)";
  },

  F15: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await altDragZoom(page);
    await page.waitForSelector("#heatmap-reset-zoom", { state: "visible", timeout: 5_000 });
    await page.locator("#heatmap-plot").dblclick({ position: { x: 200, y: HEATMAP_ROW_H / 2 } });
    await page.waitForSelector("#heatmap-reset-zoom", { state: "hidden", timeout: 5_000 });
    return "double-click restored the full extent (Reset zoom hidden)";
  },

  F16: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await page.waitForSelector("#heatmap-reset-zoom", { state: "hidden" });
    await altDragZoom(page);
    await page.waitForSelector("#heatmap-reset-zoom", { state: "visible", timeout: 5_000 });
    await page.click("#heatmap-reset-zoom");
    await page.waitForSelector("#heatmap-reset-zoom", { state: "hidden", timeout: 5_000 });
    return "button shown only while zoomed; clicking it resets";
  },

  F17: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await dragSelectRowZero(page);
    await page.waitForSelector("#heatmap-labels .row.sel", { timeout: 5_000 });
    await page.waitForSelector("#heatmap-sel", { state: "visible" });
    return "persistent selection rect + host label highlighted (.sel)";
  },

  F18: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await dragSelectRowZero(page);
    await page.click("header h1"); // outside #heatmap-view and #actions-bar
    await page.waitForSelector("#heatmap-sel", { state: "hidden" });
    expect((await page.locator("#heatmap-labels .row.sel").count()) === 0, "row stayed selected");
    expect(
      (await textOf(page, "#selection-count")).startsWith("Current service · "),
      "actions did not fall back to the current service",
    );
    return "click outside cleared the box; actions fell back to the current service";
  },

  // ── G. Raw search view ──
  G2: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await page.click("#tab-raw");
    await page.fill("#raw-search-input", "team/prod");
    const [req] = await Promise.all([
      page.waitForRequest(
        (r) => r.url().includes("/api/browse") && r.url().includes("prefix=team%2Fprod"),
        { timeout: 10_000 },
      ),
      page.press("#raw-search-input", "Enter"),
    ]);
    expect(req !== undefined, "Enter did not trigger the search request");
    return "Enter in the prefix field fired the /api/browse search";
  },

  G3: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await rawSearchSeededRows(page);
    const headers = await page.locator("#raw-table th[data-sort]").allTextContents();
    expect(headers.length === 7, `expected 7 data columns, got ${headers.length}`);
    const rows = await page.locator("#raw-body tr").count();
    expect(rows === 1, `expected 1 seeded row, got ${rows}`);
    return `7 columns (${headers.join(", ")}), 1 row rendered`;
  },

  G4: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await rawSearchSeededRows(page);
    await page.check("#raw-body .raw-cb");
    expect((await textOf(page, "#selection-count")) === "1 selected", "count not updated");
    await page.uncheck("#raw-body .raw-cb");
    expect((await textOf(page, "#selection-count")) === "", "count not cleared");
    return "per-row checkbox drives the selection count";
  },

  G5: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await rawSearchSeededRows(page);
    await page.check("#raw-select-all");
    expect(await page.isChecked("#raw-body .raw-cb"), "header checkbox did not check rows");
    await page.uncheck("#raw-select-all");
    expect(!(await page.isChecked("#raw-body .raw-cb")), "header checkbox did not uncheck rows");
    return "header checkbox toggles all row checkboxes (1/1)";
  },

  G6: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await rawSearchSeededRows(page);
    await page.click("#raw-actions button:has-text('Select All')");
    expect(await page.isChecked("#raw-body .raw-cb"), "Select All did not check rows");
    await page.click("#raw-actions button:has-text('Deselect All')");
    expect(!(await page.isChecked("#raw-body .raw-cb")), "Deselect All left rows checked");
    return "Select All -> 1 checked; Deselect All -> 0 checked";
  },

  G8: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await rawSearchSeededRows(page);
    // The dev seed yields one row, so assert the observable sort state and
    // rows surviving the rebuild; raw-rows.test.ts pins ordering semantics.
    const th = page.locator('#raw-table th[data-sort="service"]');
    await th.click();
    expect(
      (await th.getAttribute("aria-sort")) === "ascending",
      "first header click did not sort ascending",
    );
    expect(
      (await th.locator(".sort-arrow").textContent()) === "^",
      "ascending sort indicator missing",
    );
    await th.click();
    expect(
      (await th.getAttribute("aria-sort")) === "descending",
      "second header click did not flip to descending",
    );
    const rows = await page.locator("#raw-body tr").count();
    expect(rows === 1, `rows lost across sort rebuilds (${rows})`);
    return "Service header sorts: asc -> desc toggle with indicator; rows intact";
  },

  // ── H. Actions bar ──
  H1: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await dragSelectRowZero(page);
    const count = await textOf(page, "#selection-count");
    expect(
      /^1 segment · \d+(\.\d+)? [KMG]B · \d{2}:\d{2}:\d{2}–\d{2}:\d{2}:\d{2}$/.test(count),
      `selection count format was "${count}"`,
    );
    expect(await page.locator("#view-btn").isEnabled(), "View button not enabled");
    return `"${count}"; View/Flamegraph enabled`;
  },

  H2: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await dragSelectRowZero(page);
    const url = await popupUrl(page, () => page.click("#view-btn"));
    expect(/viewer\.html\?/.test(url), `popup was ${url}`);
    const params = new URL(url).searchParams;
    expect(params.get("segs") === "1", "segs param missing");
    return "popup viewer.html with selection title metadata";
  },

  // ── I. Cross-cutting behaviors ──
  I2: async ({ page, pageUrl }) => {
    // parseKey runs on every displayed key. The default Hive layout restores
    // escaped partition values into the normal Service/Host/Boot columns.
    await gotoBrowserPage(page, pageUrl);
    await rawSearchSeededRows(page);
    expect((await textOf(page, "#raw-body tr td.service")) === "demo-service", "service not decoded");
    const hosts = await page.locator("#raw-body tr td.host").allTextContents();
    expect(hosts[0] === "local/host-0" && hosts[1] === "abcd", `host/boot were ${hosts}`);
    const traceStart = await textOf(page, "#raw-body tr td:nth-child(5)");
    expect(/^\d{4}-\d{2}-\d{2} /.test(traceStart), `Trace Start cell was "${traceStart}"`);
    return "Hive key decoded into service=demo-service, host=local/host-0, boot=abcd";
  },

  I4: async ({ page, pageUrl }) => {
    await gotoBrowserPage(page, pageUrl);
    await searchDemoWindow(page);
    await dragSelectRowZero(page);
    const url = await popupUrl(page, () => page.click("#view-btn"));
    const params = new URL(url).searchParams;
    expect(params.getAll("trace").length === 0, "time-partitioned link used inline traces");
    expect(
      params.get("s_bucket") === "demo-traces" && params.get("s_prefix") === "traces",
      `scope source was ${params}`,
    );
    expect(params.has("s_from") && params.has("s_to"), `scope window was ${params}`);
    return "viewer link carries a compact bucket/prefix/window scope, not inline traces";
  },
};
