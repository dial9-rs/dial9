import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { chromium, type Browser, type BrowserContext, type Page, type Route } from "playwright";
import { fileURLToPath } from "node:url";
import { spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import type { SpanStatsResponse, SpanTypeStats } from "../../lib/trace/index.js";
import type { TailResponse, TailSignal } from "../../lib/trace/span-tail-analysis.js";

let vite: ChildProcess;
let browser: Browser;
let context: BrowserContext;
let page: Page;
let origin: string;
let errors: string[];
let requests: URL[];

const uid = "01".repeat(16);
const otherUid = "02".repeat(16);
const spanType: SpanTypeStats = {
  span_type_uid: uid, name: "Synthetic request", kind: "tracing", count: 10000,
  histogram: [{ lo_ns: 0, hi_ns: 1000000, count: 5000 }, { lo_ns: 1000000, hi_ns: 100000000, count: 5000 }],
  details_complete_count: 10000, partial_count: 0, exemplars: [],
  attribute_facets: [], attribute_keys_overflow: false,
};
const catalog: SpanStatsResponse = { span_types: [
  spanType, { ...spanType, span_type_uid: otherUid, name: "Synthetic sibling" },
], types_truncated: false, total_span_types_tracked: 2, types_overflow_instances: 0, coverage: {
  files_matched: 1, files_folded: 1, samples_folded: 10000,
  total_bytes: 1000, hosts_matched: 1, hosts_folded: 1,
  folded_set_id: "synthetic-set",
} };
const signal = (key: string): TailSignal => ({
  key, unit: "bytes", typical: 10, slow: 80, typicalCount: 5000, slowCount: 100,
  typicalCoverage: 1, slowCoverage: 1, separation: .8, correlation: .8,
  correlationCount: 10000, duplicatesLatency: false, filesCompared: 1, filesAgree: 1,
  valueAxis: { unit: "bytes", multiplier: 1, minimum: 0, maximum: 100 },
  density: { elapsedEdgesMs: [0, 1, 100], valueEdges: [0, 100], counts: [5000, 5000],
    count: 10000, maxCount: 5000 },
  points: [[10, 80, { elapsedMs: 10, insideMs: null, startNs: "1", endNs: "10000001",
    sourceUrl: "/synthetic.bin", requestId: null, attributes: [["phase", "value-a"]], attributesOmitted: 0 }]],
});
const reply: TailResponse = {
  filesUsed: 1, filesMatched: 1, truncated: false, fieldsTruncated: false,
  report: {
    count: 10000, typicalCount: 5000, slowCount: 100, medianMs: 1, boundaryMs: 10,
    typicalElapsedMs: 1, slowElapsedMs: 12, typicalInsideMs: null, slowInsideMs: null,
    typicalOutsideMs: null, slowOutsideMs: null, typicalAttributed: 0, slowAttributed: 0,
    typicalComposition: null, slowComposition: null, mixChanges: [], examples: [],
    groups: ["phase", "result"].map(key => ({ key, values: ["value-a", "value-b"],
      typical: { "value-a": .5, "value-b": .5 }, slow: { "value-a": .1, "value-b": .9 } })),
    histograms: ["phase", "result"].map(key => ({
      key, count: 10000, missingCount: 0, binEdgesMs: [0, 1, 100], maxShare: .9,
      values: [
        { value: "value-a", count: 5000, shares: [.9, .1], binCounts: [4500, 500], medianMs: .5, p99Ms: 10 },
        { value: "value-b", count: 5000, shares: [.1, .9], binCounts: [500, 4500], medianMs: 10, p99Ms: 90 },
      ],
    })),
    signals: ["work_bytes", "payload_bytes", "output_bytes", "buffer_bytes"].map(signal),
  },
};

beforeAll(async () => {
  const env = { ...process.env };
  delete env.VITEST;
  vite = spawn(process.execPath, ["--input-type=module", "-e", `
    import { createServer } from "vite";
    const server = await createServer({ server: { host: "127.0.0.1", port: 0 }, logLevel: "error" });
    await server.listen();
    process.send(server.httpServer.address().port);
    const close = async () => { await server.close(); process.exit(); };
    process.on("message", close);
    process.on("disconnect", close);
  `], { cwd: fileURLToPath(new URL("../../../", import.meta.url)), env,
    stdio: ["ignore", "pipe", "pipe", "ipc"] });
  let output = "";
  vite.stdout?.on("data", chunk => { output += String(chunk); });
  vite.stderr?.on("data", chunk => { output += String(chunk); });
  const port = await new Promise<number>((resolve, reject) => {
    vite.once("message", value => resolve(Number(value)));
    vite.once("error", reject);
    vite.once("exit", () => reject(new Error(`Browser fixture server exited before readiness:\n${output}`)));
  });
  origin = `http://127.0.0.1:${port}`;
  browser = await chromium.launch(process.env.DIAL9_TEST_CHROMIUM_EXECUTABLE
    ? { executablePath: process.env.DIAL9_TEST_CHROMIUM_EXECUTABLE } : {});
});
afterAll(async () => {
  await browser?.close();
  if (vite?.connected) {
    const exited = once(vite, "exit");
    vite.send("close");
    await exited;
  }
});
beforeEach(async () => {
  context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  page = await context.newPage();
  errors = [];
  requests = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.route("**/api/span-stats?**", route => route.fulfill({
    contentType: "text/event-stream", body: `data: ${JSON.stringify(catalog)}\n\n`,
  }));
  await page.route("**/api/span-tail?**", route => {
    requests.push(new URL(route.request().url()));
    return route.fulfill({ json: reply });
  });
});
afterEach(async () => {
  await context?.close();
  expect(errors).toEqual([]);
});

async function open(query = "") {
  await page.goto(`${origin}/span_explorer.html?api=1&data_dir=synthetic&span_type_uid=${uid}${query}`);
  await page.getByRole("group", { name: "Property histogram view" }).waitFor();
}

describe("Span Explorer analysis URL state", () => {
  it("starts with stacked distributions using the exact pooled counts", async () => {
    await open();
    expect(await page.getByRole("button", { name: "Stacked", exact: true }).getAttribute("aria-pressed")).toBe("true");
    expect(await page.getByRole("button", { name: "Overlay", exact: true }).getAttribute("aria-pressed")).toBe("false");
    expect(await page.locator(".tail-category-chart svg").getAttribute("aria-label")).toContain("stacked contributions");
    await page.locator(".tail-category-chart svg").focus();
    expect(await page.locator(".tail-category-tooltip-total").textContent()).toContain("5,000 spans");
    expect(await page.locator(".tail-category-tooltip-total").textContent()).toContain("50% of 10,000");
    expect(requests).toHaveLength(1);
  });

  it("restores the exact grouped percentile request from a shared URL through source refresh", async () => {
    await open("&tail_group_key=phase&tail_group_value=value-a&tail_percentile=95");
    expect(requests[0]!.searchParams.get("group_key")).toBe("phase");
    expect(requests[0]!.searchParams.get("group_value")).toBe("value-a");
    expect(requests[0]!.searchParams.get("slow_percentile")).toBe("95");
    expect(await page.getByRole("combobox", { name: "Comparison group" }).inputValue()).toBe("95");
    expect(await page.locator(".tail-active-scope").textContent()).toContain("phase = value-a");
    await page.getByRole("button", { name: "Apply", exact: true }).click();
    await expect.poll(() => requests.length).toBe(2);
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(requests[1]!.searchParams.get("group_value")).toBe("value-a");
    expect(requests[1]!.searchParams.get("slow_percentile")).toBe("95");
    expect(new URL(page.url()).searchParams.get("tail_group_value")).toBe("value-a");
  });

  it("restores grouped requests through Back, Forward, reset and reload without duplicates", async () => {
    await open("&extra=preserved&aws_region=us-west-2");
    const group = page.getByRole("combobox", { name: "Scope spans by property value" });
    await group.selectOption(JSON.stringify(["phase", "value-b"]));
    await expect.poll(() => requests.length).toBe(2);
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    const shared = page.url();
    expect(new URL(shared).searchParams.get("tail_group_value")).toBe("value-b");
    expect(new URL(shared).searchParams.get("extra")).toBe("preserved");
    expect(new URL(shared).searchParams.get("aws_region")).toBe("us-west-2");
    await page.goBack();
    await expect.poll(() => requests.length).toBe(3);
    await expect.poll(() => group.inputValue()).toBe("");
    expect(requests[2]!.searchParams.has("group_key")).toBe(false);
    await page.goForward();
    await expect.poll(() => requests.length).toBe(4);
    await expect.poll(() => group.inputValue()).toBe(JSON.stringify(["phase", "value-b"]));
    expect(requests[3]!.searchParams.get("group_value")).toBe("value-b");
    await page.getByRole("button", { name: "All instances", exact: true }).click();
    await expect.poll(() => requests.length).toBe(5);
    expect(new URL(page.url()).searchParams.has("tail_group_key")).toBe(false);
    await page.goto(shared);
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(requests).toHaveLength(6);
    expect(requests[5]!.searchParams.get("group_value")).toBe("value-b");
    await page.reload();
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(requests).toHaveLength(7);
    expect(requests[6]!.searchParams.get("group_value")).toBe("value-b");
  });

  it("clears span-specific property scope on an explicit span switch and restores it with Back", async () => {
    await open("&tail_group_key=phase&tail_group_value=value-a&tail_property=result&tail_percentile=95&tail_mode=overlay&tail_fields=all");
    await page.getByRole("button", { name: /^Change span/ }).click();
    await page.locator(`tr[data-uid="${otherUid}"]`).click();
    await expect.poll(() => requests.length).toBe(2);
    const next = new URL(page.url()).searchParams;
    expect(next.get("span_type_uid")).toBe(otherUid);
    expect(next.has("tail_group_key")).toBe(false);
    expect(next.has("tail_group_value")).toBe(false);
    expect(next.has("tail_property")).toBe(false);
    expect(next.get("tail_percentile")).toBe("95");
    expect(next.get("tail_mode")).toBe("overlay");
    expect(next.get("tail_fields")).toBe("all");
    expect(requests[1]!.searchParams.has("group_key")).toBe(false);
    await page.goBack();
    await expect.poll(() => requests.length).toBe(3);
    expect(requests[2]!.searchParams.get("span_type_uid")).toBe(uid);
    expect(requests[2]!.searchParams.get("group_value")).toBe("value-a");
    await expect.poll(() => page.locator(".tail-active-scope").textContent()).toContain("phase = value-a");
  });

  it("restores presentation controls and changes them without recomputing analysis", async () => {
    await open("&tail_property=result&tail_mode=overlay&tail_x_scale=linear&tail_fields=all");
    const axis = page.getByRole("combobox", { name: "Elapsed axis", exact: true });
    expect(await axis.count()).toBe(1);
    expect(await axis.inputValue()).toBe("linear");
    expect(await page.getByRole("button", { name: "result", exact: true }).getAttribute("aria-pressed")).toBe("true");
    expect(await page.getByRole("button", { name: "Overlay", exact: true }).getAttribute("aria-pressed")).toBe("true");
    expect(await page.locator(".tail-grid .tail-field").count()).toBe(4);
    await axis.selectOption("log");
    expect(new URL(page.url()).searchParams.has("tail_x_scale")).toBe(false);
    await page.getByRole("button", { name: "Stacked", exact: true }).click();
    await page.getByRole("button", { name: "phase", exact: true }).click();
    await page.getByRole("button", { name: "Show fewer fields", exact: true }).click();
    const query = new URL(page.url()).searchParams;
    expect(query.has("tail_mode")).toBe(false);
    expect(query.get("tail_property")).toBe("phase");
    expect(query.has("tail_fields")).toBe(false);
    expect(requests).toHaveLength(1);
    await page.reload();
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(await axis.inputValue()).toBe("log");
    expect(await page.getByRole("button", { name: "Stacked", exact: true }).getAttribute("aria-pressed")).toBe("true");
    expect(await page.getByRole("button", { name: "phase", exact: true }).getAttribute("aria-pressed")).toBe("true");
    expect(await page.locator(".tail-grid .tail-field").count()).toBe(3);
    expect(requests).toHaveLength(2);
  });

  it("reports an unavailable service and retries the same grouped URL without losing state", async () => {
    let attempts = 0;
    await page.route("**/api/span-tail?**", route => {
      requests.push(new URL(route.request().url()));
      return ++attempts === 1
        ? route.fulfill({ status: 503, body: "Synthetic backend unavailable" })
        : route.fulfill({ json: reply });
    });
    await page.goto(`${origin}/span_explorer.html?api=1&data_dir=synthetic&span_type_uid=${uid}&tail_group_key=phase&tail_group_value=value-a`);
    const status = page.locator(".tail-status");
    await expect.poll(() => status.textContent()).toContain("Analysis service unavailable (503)");
    expect(await status.textContent()).toContain("Synthetic backend unavailable");
    expect(new URL(page.url()).searchParams.get("tail_group_value")).toBe("value-a");
    expect(await page.locator(".tail-active-scope").textContent()).toContain("phase = value-a");
    await page.getByRole("button", { name: "Analyse source traces", exact: true }).click();
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(requests).toHaveLength(2);
    expect(requests[1]!.searchParams.get("group_value")).toBe("value-a");
    expect(await page.locator(".tail-status").count()).toBe(0);
  });

  it.each([
    [400, "Grouping by phase is unavailable: more than 12 distinct values."],
    [404, "no source files match this scope"],
  ])("preserves the actual %s response and keeps the grouped scope reset usable", async (status, body) => {
    await page.route("**/api/span-tail?**", route => {
      const url = new URL(route.request().url());
      requests.push(url);
      return url.searchParams.has("group_key")
        ? route.fulfill({ status, body })
        : route.fulfill({ json: reply });
    });
    await page.goto(`${origin}/span_explorer.html?api=1&data_dir=synthetic&span_type_uid=${uid}&tail_group_key=phase&tail_group_value=value-a`);
    await expect.poll(() => page.locator(".tail-status").textContent()).toContain(`Backend analysis failed (${status}): ${body}`);
    expect(await page.locator(".tail-status").textContent()).not.toContain("service unavailable");
    expect(new URL(page.url()).searchParams.get("tail_group_value")).toBe("value-a");
    await page.getByRole("button", { name: "All instances", exact: true }).click();
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(requests).toHaveLength(2);
    expect(requests[1]!.searchParams.has("group_key")).toBe(false);
    expect(new URL(page.url()).searchParams.has("tail_group_key")).toBe(false);
  });

  it("aborts a pending group request on Back and ignores its late response", async () => {
    let held: Route | null = null;
    await page.route("**/api/span-tail?**", route => {
      const url = new URL(route.request().url());
      requests.push(url);
      if (url.searchParams.get("group_value") === "value-b") {
        held = route;
        return;
      }
      return route.fulfill({ json: reply });
    });
    await open();
    await page.getByRole("combobox", { name: "Scope spans by property value" })
      .selectOption(JSON.stringify(["phase", "value-b"]));
    await expect.poll(() => held != null).toBe(true);
    expect(await page.locator(".tail-results").getAttribute("inert")).not.toBeNull();
    const cancelled = page.waitForEvent("requestfailed", request =>
      new URL(request.url()).searchParams.get("group_value") === "value-b");
    await page.goBack();
    await cancelled;
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    await expect.poll(() => page.getByRole("combobox", { name: "Scope spans by property value" }).inputValue()).toBe("");
    const oldReply = structuredClone(reply);
    oldReply.report.signals = [signal("Superseded field")];
    await (held as unknown as Route).fulfill({ json: oldReply });
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    expect(await page.locator(".span-tail-analysis").textContent()).not.toContain("Superseded field");
    expect(requests).toHaveLength(3);
    expect(requests[2]!.searchParams.has("group_key")).toBe(false);
  });

  it("persists a native histogram brush and clears it atomically when choosing a percentile", async () => {
    const body = structuredClone(reply);
    body.report.histograms.forEach(histogram => {
      histogram.binEdgesMs = [0, 1, 10, 100];
      histogram.values[0]!.binCounts = [4000, 900, 100];
      histogram.values[0]!.shares = [.8, .18, .02];
      histogram.values[1]!.binCounts = [1000, 1500, 2500];
      histogram.values[1]!.shares = [.2, .3, .5];
    });
    await page.route("**/api/span-tail?**", route => {
      const url = new URL(route.request().url());
      requests.push(url);
      const response = structuredClone(body);
      const min = url.searchParams.get("min_span_ns");
      if (min != null) response.report.boundaryMs = Number(min) / 1e6;
      return route.fulfill({ json: response });
    });
    await open("&tail_group_key=phase&tail_group_value=value-a&tail_percentile=95");
    await page.locator(".tail-category-chart svg").focus();
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("Enter");
    await expect.poll(() => requests.length).toBe(2);
    const bandQuery = new URL(page.url()).searchParams;
    expect(bandQuery.get("min_span_ns")).toBe("1000000");
    expect(bandQuery.get("max_span_ns")).toBe("10000000");
    expect(requests[1]!.searchParams.get("min_span_ns")).toBe("1000000");
    expect(requests[1]!.searchParams.get("max_span_ns")).toBe("10000000");
    const comparison = page.getByRole("combobox", { name: "Comparison group" });
    await expect.poll(() => comparison.inputValue()).toBe("custom");
    await comparison.selectOption("99.9");
    await expect.poll(() => requests.length).toBe(3);
    await expect.poll(() => comparison.inputValue()).toBe("99.9");
    const percentileQuery = new URL(page.url()).searchParams;
    expect(percentileQuery.get("tail_percentile")).toBe("99.9");
    expect(percentileQuery.has("min_span_ns")).toBe(false);
    expect(percentileQuery.has("max_span_ns")).toBe(false);
    expect(requests[2]!.searchParams.get("slow_percentile")).toBe("99.9");
    expect(requests[2]!.searchParams.has("min_span_ns")).toBe(false);
    expect(requests[2]!.searchParams.has("max_span_ns")).toBe(false);
    expect(requests[2]!.searchParams.get("group_value")).toBe("value-a");
    await page.goBack();
    await expect.poll(() => requests.length).toBe(4);
    await expect.poll(() => comparison.inputValue()).toBe("custom");
    expect(requests[3]!.searchParams.get("slow_percentile")).toBe("95");
    expect(requests[3]!.searchParams.get("min_span_ns")).toBe("1000000");
    await page.reload();
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(requests).toHaveLength(5);
    expect(requests[4]!.searchParams.get("max_span_ns")).toBe("10000000");
    expect(await comparison.inputValue()).toBe("custom");
  });

  it("copies the complete shareable URL including preserved hash state", async () => {
    await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin });
    await open("&extra=preserved#v=1&tz=utc");
    await page.getByRole("combobox", { name: "Scope spans by property value" })
      .selectOption(JSON.stringify(["phase", "value-a"]));
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(new URL(page.url()).hash).toBe("#v=1&tz=utc");
    await page.locator("#btn-copylink").click();
    await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(page.url());
  });

  it("restores grouped raw analysis after async loading and keeps raw mode through control changes", async () => {
    await page.route("**/synthetic.bin", route => route.fulfill({ body: Buffer.from([1, 2, 3]) }));
    await page.route(`${origin}/api/span-stats`, route => route.fulfill({ json: catalog }));
    const methods: string[] = [];
    await page.route("**/api/span-tail?**", route => {
      requests.push(new URL(route.request().url()));
      methods.push(route.request().method());
      return route.fulfill({ json: reply });
    });
    await page.goto(`${origin}/span_explorer.html?trace=/synthetic.bin&span_type_uid=${uid}&tail_group_key=phase&tail_group_value=value-a&tail_percentile=95`);
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(methods).toEqual(["POST"]);
    expect(requests[0]!.searchParams.get("group_value")).toBe("value-a");
    await page.getByRole("combobox", { name: "Elapsed axis", exact: true }).selectOption("linear");
    const query = new URL(page.url()).searchParams;
    expect(query.get("trace")).toBe("/synthetic.bin");
    expect(query.has("api")).toBe(false);
    expect(query.get("tail_group_value")).toBe("value-a");
    expect(query.get("tail_x_scale")).toBe("linear");
    expect(requests).toHaveLength(1);
    await page.reload();
    await page.getByRole("group", { name: "Property histogram view" }).waitFor();
    expect(methods).toEqual(["POST", "POST"]);
    expect(requests[1]!.searchParams.get("group_value")).toBe("value-a");
    expect(await page.getByRole("combobox", { name: "Elapsed axis", exact: true }).inputValue()).toBe("linear");
  });
});

describe("Span Explorer selected-span layout", () => {
  it.each(["", "&min_span_ns=5000000&max_span_ns=20000000"])(
    "omits the examples link list while retaining recorded-instance browsing (%s)", async query => {
      const body = structuredClone(reply);
      body.report.examples = [body.report.signals[0]!.points[0]![2]];
      await page.route("**/api/span-tail?**", route => route.fulfill({ json: body }));
      await open(query);
      expect(await page.locator(".tail-examples, .span-tail-examples-mount").count()).toBe(0);
      expect(await page.getByRole("heading", { name: /^(Slow|Selected) examples$/ }).count()).toBe(0);
      expect(await page.locator(".span-recorded-instances summary").textContent()).toContain("Browse recorded instances");
      expect(await page.locator(".tail-trace-point").count()).toBeGreaterThan(0);
    });

  it("keeps one heading and a keyboard-accessible catalog toggle without duplicate requests", async () => {
    await open();
    expect(await page.locator(".catalog-selection, #catalog-selected-name").count()).toBe(0);
    expect(await page.getByText(spanType.name, { exact: true }).evaluateAll(elements =>
      elements.filter(element => element.checkVisibility()).length)).toBe(1);
    const toggle = page.locator(".detail-header button[aria-controls='catalog-wrap']");
    expect((await toggle.textContent())?.trim()).toMatch(/^Change span/);
    expect(await toggle.getAttribute("aria-expanded")).toBe("false");
    await toggle.click();
    expect(await page.locator("#catalog-wrap").isVisible()).toBe(true);
    expect(await toggle.getAttribute("aria-expanded")).toBe("true");
    expect((await toggle.textContent())?.trim()).toBe("Hide span list");
    await toggle.press("Space");
    expect(await page.locator("#catalog-wrap").isVisible()).toBe(false);
    await toggle.press("Enter");
    expect(await page.locator(`tr[data-uid="${uid}"]`).isVisible()).toBe(true);
    expect(requests).toHaveLength(1);
    await page.locator(`tr[data-uid="${otherUid}"]`).click();
    await expect.poll(() => requests.length).toBe(2);
    await page.getByRole("heading", { name: "Synthetic sibling", exact: true }).waitFor();
    expect(await page.locator("#catalog-wrap").isVisible()).toBe(false);
    expect(await toggle.getAttribute("aria-expanded")).toBe("false");
    expect(new URL(page.url()).searchParams.get("span_type_uid")).toBe(otherUid);
    expect(await toggle.evaluate(element => document.activeElement === element)).toBe(true);
    expect(await page.getByText("Synthetic sibling", { exact: true }).evaluateAll(elements =>
      elements.filter(element => element.checkVisibility()).length)).toBe(1);
  });

  it("wraps a long mobile heading and reveals Change span when focusing from scrolled analysis", async () => {
    await page.setViewportSize({ width: 390, height: 844 });
    const body = structuredClone(catalog);
    const name = "SyntheticSpanWithALongRecordedName/".repeat(6);
    body.span_types[0]!.name = name;
    await page.route("**/api/span-stats?**", route => route.fulfill({
      contentType: "text/event-stream", body: `data: ${JSON.stringify(body)}\n\n`,
    }));
    await open();
    expect(await page.locator(".catalog-selection").count()).toBe(0);
    expect(await page.getByRole("heading", { name, exact: true }).count()).toBe(1);
    expect(await page.locator(".detail-header").evaluate(element =>
      element.scrollWidth <= element.clientWidth)).toBe(true);
    expect(await page.locator(".detail-header h2").evaluate(element => {
      const rect = element.getBoundingClientRect();
      return rect.left >= 0 && rect.right <= innerWidth;
    })).toBe(true);
    await page.locator("#detail-panel").evaluate(element => { element.scrollTop = element.scrollHeight; });
    const toggle = page.locator(".detail-header button[aria-controls='catalog-wrap']");
    await toggle.focus();
    expect(await toggle.evaluate(element => {
      const panel = element.closest(".detail-panel")!.getBoundingClientRect();
      const rect = element.getBoundingClientRect();
      return document.activeElement === element && rect.top >= panel.top && rect.bottom <= panel.bottom;
    })).toBe(true);
    await toggle.press("Enter");
    expect(await page.locator("#catalog-wrap").isVisible()).toBe(true);
    expect(requests).toHaveLength(1);
  });
});


describe("Categorical exact elapsed endpoints", () => {
  it.each([
    { width: 390, lower: .1, maximum: 145, label: "145ms" },
    { width: 320, lower: .1, maximum: 145, label: "145ms" },
    { width: 1440, lower: .1, maximum: 145, label: "145ms" },
    { width: 390, lower: .1, maximum: 100.000001, label: "100ms" },
    { width: 1440, lower: .1, maximum: 100.000001, label: "100ms" },
    { width: 390, lower: 100, maximum: 101, label: "101ms" },
    { width: 390, lower: 100, maximum: 100.000001, label: "100ms" },
    { width: 390, lower: 100, maximum: 100, label: "100ms" },
    { width: 390, lower: 0, maximum: .000001, label: "1ns" },
    { width: 320, lower: 130000, maximum: 1e9, label: "11d 13h 46m" },
  ])("keeps zero and exact $maximum ms endpoint labels distinct at $width px", async ({ width, lower, maximum, label }) => {
    await page.setViewportSize({ width, height: 844 });
    const body = structuredClone(reply);
    const histogram = body.report.histograms![0]!;
    histogram.binEdgesMs = lower === 0 || lower === maximum ? [0, maximum]
      : [0, ...Array.from({ length: 36 }, (_, index) =>
        index === 35 ? maximum : lower * (maximum / lower) ** (index / 35))];
    const bins = histogram.binEdgesMs.length - 1;
    for (const value of histogram.values) {
      const count = Math.floor(value.count / bins);
      value.binCounts = Array.from({ length: bins }, (_, index) =>
        index === bins - 1 ? value.count - count * (bins - 1) : count);
      value.shares = value.binCounts.map(count => count / value.count);
      value.medianMs = lower;
      value.p99Ms = lower === 0 ? 0 : Math.max(lower, Math.min(maximum, value.p99Ms));
    }
    histogram.maxShare = Math.max(...histogram.values.flatMap(value => value.shares));
    body.report.histograms = [histogram];
    await page.route("**/api/span-tail?**", route => {
      requests.push(new URL(route.request().url()));
      return route.fulfill({ json: body });
    });
    await open();
    const chart = page.locator(".tail-category-chart");
    const labels = await chart.locator(".tail-category-x > span").evaluateAll(elements =>
      elements.filter(element => element.checkVisibility()).map(element => {
        const rect = element.getBoundingClientRect();
        return { text: element.textContent, left: rect.left, right: rect.right };
      }));
    expect(labels[0]!.text).toBe("0");
    expect(labels.at(-1)!.text).toBe(label);
    expect(labels).toHaveLength(new Set(labels.map(value => value.text)).size);
    for (let index = 1; index < labels.length; index++) {
      expect(labels[index]!.left - labels[index - 1]!.right,
        `${labels[index - 1]!.text} and ${labels[index]!.text} need distinct label space`).toBeGreaterThanOrEqual(4);
    }
    expect(await chart.locator(".tail-category-x .last").evaluate(element => element.style.left)).toBe("100%");
    const plot = chart.locator("svg");
    expect(await plot.getAttribute("viewBox")).toBe(`0 0 ${bins} 1`);
    const markers = await chart.locator(".tail-category-percentile").evaluateAll(elements =>
      elements.map(element => [element.getAttribute("x1"), element.getAttribute("x2"), element.textContent]));
    const finalCount = histogram.values.reduce((total, value) => total + value.binCounts![bins - 1]!, 0);
    await plot.focus();
    await page.keyboard.press("End");
    expect((await chart.locator(".tail-category-tooltip-range").textContent())!.replace(/\s+/g, "")).toContain(`${label.replace(/\s+/g, "")}inclusive`);
    expect(await chart.locator(".tail-category-tooltip-total strong").textContent()).toBe(`${finalCount.toLocaleString()} spans`);
    await page.getByRole("button", { name: "Overlay", exact: true }).click();
    expect(await plot.getAttribute("aria-label")).toContain("overlaid elapsed-time distributions");
    expect(await plot.getAttribute("viewBox")).toBe(`0 0 ${bins} 1`);
    expect(await chart.locator(".tail-category-percentile").evaluateAll(elements =>
      elements.map(element => [element.getAttribute("x1"), element.getAttribute("x2"), element.textContent]))).toEqual(markers);
    await plot.focus();
    await page.keyboard.press("End");
    expect(await chart.locator(".tail-category-tooltip-series strong").allTextContents()).toEqual(
      histogram.values.map(value => `${value.binCounts![bins - 1]!.toLocaleString()} spans`));
    await page.keyboard.press("Enter");
    if (bins === 1) {
      // Selecting the complete population preserves the existing unbounded scope.
      expect(requests).toHaveLength(1);
      expect(new URL(page.url()).searchParams.has("min_span_ns")).toBe(false);
      expect(new URL(page.url()).searchParams.has("max_span_ns")).toBe(false);
    } else {
      await expect.poll(() => requests.length).toBe(2);
      expect(requests[1]!.searchParams.get("min_span_ns")).toBe(
        Math.round(histogram.binEdgesMs[bins - 1]! * 1e6).toString());
      expect(requests[1]!.searchParams.has("max_span_ns")).toBe(false);
    }
  });
});
