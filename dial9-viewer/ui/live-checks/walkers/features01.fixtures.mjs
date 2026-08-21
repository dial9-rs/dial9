// Fixture-backed row walkers for rows the demo seed cannot trigger. Driven
// only by `walk-rows.mjs --fixtures`, against a dev-server seeded with the
// generated fixture tree:
//
//   cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures
//   DIAL9_SEED_DIR=dial9-viewer/ui/live-checks/fixtures/generated/s3 \
//     DIAL9_DEFAULT_PREFIX= PORT=3022 \
//     cargo run -p dial9-viewer --features dev-server --bin dev-server
//
// Each entry names the fixture family it needs; the runner preflights the
// families against the live server before walking (fail fast with the
// generation instructions instead of a wall of walker timeouts).
//
// The heatmap rows assert the RENDERED canvas, census-style: pixel
// signatures at positions derived from the page's own domain mapping
// (`timeToX`/`heatmapDomain` are top-level bindings of the classic inline
// script, so `page.evaluate` sees them). The fixture geometry these walkers
// hardcode (hosts, epochs, mtimes) is defined in gen_fixtures.rs — the two
// files change together.
//
// Environment: fixture mode pins FIXTURE_CLOCK to 2026-04-09T21:00Z; the
// fixture day is 2026-04-09, so "Last 24hr" covers
// every scenario window.

import { expect, textOf, gotoBrowserPage, dragSelectRowZero, applyTestCreds } from "../lib/actions.mjs";

/** Unix seconds for HH:MM on the fixture day (matches gen_fixtures.rs). */
const epoch = (h, m) => Date.UTC(2026, 3, 9, h, m, 0) / 1000;

const BROWSE_BUCKET = "dial9-fixtures";
const DATES_BUCKET = "dial9-fixtures-dates";
const LARGE_BUCKET = "dial9-fixtures-large";

// ── Fixture families + server preflight ──────────────────────────────────

export const FAMILIES = {
  browse: {
    bucket: BROWSE_BUCKET,
    what: "multi-host/-service heatmap scenarios (boots/gap/seam/window hosts)",
    probe: async (baseUrl) => {
      const prefixes = await getJson(baseUrl, `/api/prefixes?bucket=${BROWSE_BUCKET}`);
      expect(
        Array.isArray(prefixes) && prefixes.includes("traces/"),
        `${BROWSE_BUCKET} not seeded (prefixes: ${JSON.stringify(prefixes)})`,
      );
    },
  },
  dates: {
    bucket: DATES_BUCKET,
    what: "date partitions at the bucket root (#471)",
    probe: async (baseUrl) => {
      const prefixes = await getJson(baseUrl, `/api/prefixes?bucket=${DATES_BUCKET}`);
      expect(
        Array.isArray(prefixes) && prefixes.some((p) => /^\d{4}-\d{2}-\d{2}\/$/.test(p)),
        `${DATES_BUCKET} not seeded with a date layer (prefixes: ${JSON.stringify(prefixes)})`,
      );
      const config = await getJson(baseUrl, "/api/config");
      expect(
        config.default_prefix == null,
        `server has default_prefix "${config.default_prefix}" — the D4 walk needs ` +
          "DIAL9_DEFAULT_PREFIX= (empty) so discovery can see the date layer",
      );
    },
  },
  large: {
    bucket: LARGE_BUCKET,
    what: ">200 MB multi-segment set (selection cap)",
    probe: async (baseUrl) => {
      const from = epoch(18, 0);
      const to = epoch(20, 0);
      const resp = await getJson(
        baseUrl,
        `/api/browse?bucket=${LARGE_BUCKET}&prefix=traces&from=${from}&to=${to}`,
      );
      const total = (resp.objects ?? []).reduce((sum, o) => sum + o.size, 0);
      expect(
        total > 200 * 1024 * 1024,
        `${LARGE_BUCKET} listing sums to ${total} bytes — need >200 MiB ` +
          "(generate WITHOUT --skip-large)",
      );
    },
  },
  // The truncation banner needs no seeded data (any bucket + a >2000-hour
  // window overflows the server's prefix fan-out cap).
  none: { bucket: null, what: "no fixture data required", probe: async () => {} },
};

async function getJson(baseUrl, path) {
  const resp = await fetch(new URL(path, baseUrl));
  expect(resp.ok, `GET ${path} -> HTTP ${resp.status}`);
  return resp.json();
}

/** Fail fast (with generation instructions) if a needed family is missing. */
export async function preflightFamilies(baseUrl, familyNames) {
  for (const name of familyNames) {
    const fam = FAMILIES[name];
    try {
      await fam.probe(baseUrl);
    } catch (e) {
      throw new Error(
        `fixture family "${name}" (${fam.what}) not available at ${baseUrl}: ${e.message}\n` +
          "Generate + serve the fixtures:\n" +
          "  cargo run --release -p dial9-viewer --features dev-server --bin gen-fixtures\n" +
          "  DIAL9_SEED_DIR=dial9-viewer/ui/live-checks/fixtures/generated/s3 \\\n" +
          "    DIAL9_DEFAULT_PREFIX= PORT=3022 \\\n" +
          "    cargo run -p dial9-viewer --features dev-server --bin dev-server",
      );
    }
  }
}

// ── Shared page actions ──────────────────────────────────────────────────

/** Point the page at a fixture bucket and search the fixture day's window. */
async function searchFixtureBucket(page, pageUrl, bucket) {
  await gotoBrowserPage(page, pageUrl);
  await page.fill("#bucket-input", bucket);
  await page.fill("#prefix-input", "traces");
  await page.click(".quick-btns button:has-text('Last 24hr')");
  await page.click("#search-btn");
  await page.waitForSelector("#heatmap-view", { state: "visible", timeout: 20_000 });
  await page.waitForSelector("#heatmap-labels .row", { timeout: 20_000 });
}

/**
 * Canvas readout for one heatmap row, identified by its label text. Returns
 * pixel-signature summaries computed in-page (the full ImageData never
 * crosses the browser boundary). All geometry comes from the page's own
 * `timeToX` + `heatmapDomain`.
 */
async function rowPixels(page, labelIncludes, times) {
  return page.evaluate(
    ({ labelIncludes, times }) => {
      const labels = [...document.querySelectorAll("#heatmap-labels .row")].map(
        (r) => r.textContent,
      );
      const row = labels.findIndex((t) => t.includes(labelIncludes));
      if (row < 0) return { error: `no heatmap row labelled ~"${labelIncludes}" (have: ${labels.join(" | ")})` };
      const canvas = document.getElementById("heatmap-canvas");
      const ctx = canvas.getContext("2d");
      const dpr = window.devicePixelRatio || 1;
      const ROW_H = 26;
      const y0 = Math.round(row * ROW_H * dpr);
      const h = Math.round(ROW_H * dpr);
      const img = ctx.getImageData(0, y0, canvas.width, h).data;
      const at = (x, y) => {
        const i = (y * canvas.width + x) * 4;
        return [img[i], img[i + 1], img[i + 2]];
      };
      // Map fixture epochs -> device-px columns via the page's own scale.
      /* global timeToX, heatmapDomain */
      const { tMin, tMax } = heatmapDomain;
      const cssW = canvas.width / dpr;
      const xFor = {};
      for (const [name, t] of Object.entries(times)) {
        xFor[name] = Math.round(timeToX(t, tMin, tMax, cssW) * dpr);
      }
      return { row, width: canvas.width, height: h, dpr, xFor,
        // Pre-computed probes (functions don't serialize — compute here):
        computed: (() => {
          const out = {};
          const near = (px, rgb, tol) =>
            Math.abs(px[0] - rgb[0]) <= tol &&
            Math.abs(px[1] - rgb[1]) <= tol &&
            Math.abs(px[2] - rgb[2]) <= tol;
          // 1. Cyan boot-divider columns (#00e5ff), clustered by x.
          const cyanCols = new Set();
          for (let y = 1; y < h - 1; y++) {
            for (let x = 0; x < canvas.width; x++) {
              if (near(at(x, y), [0, 229, 255], 40)) cyanCols.add(x);
            }
          }
          const sortedCyan = [...cyanCols].sort((a, b) => a - b);
          const clusters = [];
          let prev = -10;
          for (const x of sortedCyan) {
            if (x - prev > 3) clusters.push(x);
            prev = x;
          }
          out.cyanClusters = clusters;
          // 2. Mid-row scanline colors (density uniformity probes).
          const midY = Math.floor(h / 2);
          out.scanline = [];
          for (let x = 0; x < canvas.width; x++) out.scanline.push(at(x, midY));
          // 3. Gap-band census over the full row height: exact band bg
          //    #15152a and the 0.6-alpha edge-tick blend family.
          const bandBg = [21, 21, 42];
          out.bandBgByCol = new Array(canvas.width).fill(0);
          out.tickByCol = new Array(canvas.width).fill(0);
          for (let y = 1; y < h - 1; y++) {
            for (let x = 0; x < canvas.width; x++) {
              const px = at(x, y);
              if (near(px, bandBg, 5)) out.bandBgByCol[x] += 1;
              const tickish =
                Math.abs(px[0] - px[1]) <= 3 &&
                px[0] >= 90 &&
                px[0] <= 120 &&
                px[2] - px[0] >= 25 &&
                px[2] - px[0] <= 45;
              if (tickish) out.tickByCol[x] += 1;
            }
          }
          return out;
        })(),
      };
    },
    { labelIncludes, times },
  );
}

const approx = (x, target, tol) => Math.abs(x - target) <= tol;

// ── Walkers ──────────────────────────────────────────────────────────────

export const registry = {
  // Boot-count annotation — the boots host carries three boot ids in the
  // window; its label must annotate "3 boots" while single-boot rows stay
  // unannotated.
  F5: {
    family: "browse",
    walk: async ({ page, pageUrl }) => {
      await searchFixtureBucket(page, pageUrl, BROWSE_BUCKET);
      const bootsRow = page.locator("#heatmap-labels .row", { hasText: "svc-fix / boots" });
      expect((await bootsRow.count()) === 1, "boots host row missing");
      const note = bootsRow.locator(".boot");
      expect((await note.count()) === 1, "boot-count annotation missing on the boots row");
      const text = (await note.textContent()).trim();
      expect(/3 boots$/.test(text), `annotation was "${text}" (expected "3 boots")`);
      const gapRow = page.locator("#heatmap-labels .row", { hasText: "svc-fix / gap" });
      expect((await gapRow.locator(".boot").count()) === 0, "single-boot row got an annotation");
      return `boots row annotated "${text}"; single-boot rows unannotated`;
    },
  },

  // Seam tiling — the seam host's first segment's last_modified overshoots
  // the second's start by 5 minutes. Tiling clamps it, so the rendered
  // density is UNIFORM across both halves; without tiling the overlap would
  // double-count into a visibly brighter band.
  F7: {
    family: "browse",
    walk: async ({ page, pageUrl }) => {
      await searchFixtureBucket(page, pageUrl, BROWSE_BUCKET);
      const r = await rowPixels(page, "svc-fix / seam", {
        start: epoch(19, 0),
        seam: epoch(19, 5),
        end: epoch(19, 10),
      });
      expect(!r.error, r.error);
      const { xFor, computed } = r;
      const colorsIn = (x0, x1) => computed.scanline.slice(x0, x1);
      const half1 = colorsIn(xFor.start + 3, xFor.seam - 3);
      const half2 = colorsIn(xFor.seam + 3, xFor.end - 3);
      expect(half1.length > 10 && half2.length > 10, "seam halves too narrow to sample");
      const ref = half1[Math.floor(half1.length / 2)];
      // Density painted at all (not the base row background family).
      expect(!(ref[0] === ref[1] && ref[2] - ref[0] <= 30), `no density at seam row (ref ${ref})`);
      let maxDev = 0;
      for (const px of [...half1, ...half2]) {
        for (let c = 0; c < 3; c++) maxDev = Math.max(maxDev, Math.abs(px[c] - ref[c]));
      }
      expect(
        maxDev <= 8,
        `density not uniform across the seam (max channel deviation ${maxDev}; ` +
          "an untiled overlap doubles the [19:05,19:10] half)",
      );
      return `uniform density across the seam (ref rgb(${ref}), max channel deviation ${maxDev})`;
    },
  },

  // Coverage-gap hatching — the gap host stops reporting 19:05-19:15; that
  // interval must render the hatched no-data band (#15152a fill + diagonal
  // hatch) framed by edge ticks, distinct from painted density.
  F8: {
    family: "browse",
    walk: async ({ page, pageUrl }) => {
      await searchFixtureBucket(page, pageUrl, BROWSE_BUCKET);
      const r = await rowPixels(page, "svc-fix / gap", {
        gapStart: epoch(19, 5),
        gapEnd: epoch(19, 15),
        midSegment: epoch(19, 2),
      });
      expect(!r.error, r.error);
      const { xFor, computed, height } = r;
      const gapCols = computed.bandBgByCol.slice(xFor.gapStart + 3, xFor.gapEnd - 3);
      expect(gapCols.length > 20, "gap interval too narrow to sample");
      // Band background dominates the gap interior (hatch/ticks overlay it).
      const bandy = gapCols.filter((n) => n >= (height - 2) * 0.5).length;
      expect(
        bandy / gapCols.length > 0.8,
        `gap interior not painted as the no-data band (${bandy}/${gapCols.length} columns)`,
      );
      // Edge ticks frame the hole. The start tick sits over the band
      // background, so its exact 0.6-alpha blend is checkable; the end tick
      // blends over the next segment's density spill, so assert the
      // structure instead: the band terminates at the expected x with a
      // strictly brighter 1-px boundary column before the density resumes.
      const tickNear = (x) =>
        computed.tickByCol.slice(Math.max(0, x - 2), x + 3).some((n) => n >= (height - 2) * 0.5);
      expect(tickNear(xFor.gapStart), "no edge tick at the gap start");
      const bandDominant = (x) => computed.bandBgByCol[x] >= (height - 2) * 0.5;
      let lastBandCol = -1;
      for (let x = xFor.gapStart; x <= xFor.gapEnd + 3 && x < computed.bandBgByCol.length; x++) {
        if (bandDominant(x)) lastBandCol = x;
      }
      expect(
        approx(lastBandCol, xFor.gapEnd, 6),
        `band terminus at x=${lastBandCol} (expected ~${xFor.gapEnd})`,
      );
      const sum = (px) => px[0] + px[1] + px[2];
      const bandSum = sum([21, 21, 42]);
      const boundary = computed.scanline
        .slice(Math.max(0, xFor.gapEnd - 3), xFor.gapEnd)
        .some((px) => sum(px) > bandSum + 100);
      expect(boundary, "no bright edge boundary before the density resumes at the gap end");
      // Control: mid-segment density is NOT the band.
      expect(
        computed.bandBgByCol[xFor.midSegment] < (height - 2) * 0.2,
        "control column inside a segment looks like the no-data band",
      );
      return (
        `no-data band across [19:05,19:15] (${bandy}/${gapCols.length} band columns), ` +
        `start tick + crisp end boundary (band terminus x=${lastBandCol}), density control distinct`
      );
    },
  },

  // Boot-change dividers — dashed cyan verticals exactly at the boots host's
  // two boot transitions (19:05 and 19:10), absent from single-boot rows.
  F9: {
    family: "browse",
    walk: async ({ page, pageUrl }) => {
      await searchFixtureBucket(page, pageUrl, BROWSE_BUCKET);
      const boots = await rowPixels(page, "svc-fix / boots", {
        t1905: epoch(19, 5),
        t1910: epoch(19, 10),
      });
      expect(!boots.error, boots.error);
      const clusters = boots.computed.cyanClusters;
      expect(clusters.length === 2, `expected 2 cyan dividers, found ${clusters.length} (${clusters})`);
      expect(
        approx(clusters[0], boots.xFor.t1905, 3) && approx(clusters[1], boots.xFor.t1910, 3),
        `dividers at x=${clusters} (expected ~${boots.xFor.t1905} and ~${boots.xFor.t1910})`,
      );
      const gap = await rowPixels(page, "svc-fix / gap", {});
      expect(
        gap.computed.cyanClusters.length === 0,
        `single-boot row has ${gap.computed.cyanClusters.length} divider(s)`,
      );
      return `2 dashed cyan dividers at the boot transitions (x=${clusters}); none on single-boot rows`;
    },
  },

  // 200 MB selection cap — selecting the large host's full row (~224 MB)
  // disables View; on an aggregation-enabled server (this one: BYO creds =>
  // aggregation on) the warning is suppressed and Flamegraph is exempt from
  // the cap.
  H4: {
    family: "large",
    walk: async ({ page, pageUrl }) => {
      await searchFixtureBucket(page, pageUrl, LARGE_BUCKET);
      await dragSelectRowZero(page);
      const count = await textOf(page, "#selection-count");
      expect(/^8 segments · /.test(count), `selection count was "${count}"`);
      const mb = Number(count.match(/· ([\d.]+) MB/)?.[1]);
      expect(mb > 200, `selected ${mb} MB — not over the 200 MB cap`);
      expect(!(await page.locator("#view-btn").isEnabled()), "View stayed enabled over the cap");
      expect(
        await page.locator("#cpu-btn").isEnabled(),
        "Flamegraph disabled despite the aggregation-mode cap exemption (#570)",
      );
      const warn = await textOf(page, "#selection-warn");
      expect(
        warn === "",
        `warning "${warn}" shown despite aggregation mode (suppressed per #570)`,
      );
      return `${count} selected: View disabled; Flamegraph exempt; warning suppressed (aggregation mode)`;
    },
  },

  // Bucket picker -> region detect — the dial9-named fixture buckets appear
  // in the picker's filtered view; clicking one fills the bucket field,
  // resolves the region via /api/credentials/check, and re-discovers
  // prefixes.
  C7: {
    family: "browse",
    walk: async ({ page, pageUrl }) => {
      await gotoBrowserPage(page, pageUrl);
      await applyTestCreds(page);
      const chip = page
        .locator("#creds-buckets button")
        .filter({ hasText: new RegExp(`^${BROWSE_BUCKET}$`) });
      expect((await chip.count()) === 1, `no "${BROWSE_BUCKET}" chip in the picker`);
      const [checkReq] = await Promise.all([
        page.waitForRequest((r) => r.url().includes("/api/credentials/check"), {
          timeout: 10_000,
        }),
        chip.click(),
      ]);
      expect(checkReq !== undefined, "no region check fired");
      expect(
        (await page.inputValue("#bucket-input")) === BROWSE_BUCKET,
        "bucket field not filled from the picker",
      );
      await page.waitForFunction(
        (b) => document.getElementById("creds-status").textContent.includes(`Using ${b}`),
        BROWSE_BUCKET,
        { timeout: 10_000 },
      );
      const status = await textOf(page, "#creds-status");
      // Prefix re-discovery for the selected bucket.
      await page.waitForSelector("#prefix-suggestions button:has-text('traces')", {
        timeout: 10_000,
      });
      return `chip filled bucket, region check fired, status "${status}", prefixes re-discovered`;
    },
  },

  // Date-layer auto-empty — a bucket whose root children are all date
  // partitions gets an EMPTY prefix (dates are not selectable prefixes).
  // Requires the server to have no default prefix.
  D4: {
    family: "dates",
    walk: async ({ page, pageUrl }) => {
      await gotoBrowserPage(page, pageUrl);
      await page.fill("#bucket-input", DATES_BUCKET);
      await page.fill("#prefix-input", "stale-prefix");
      // Commit the bucket change (the change listener re-runs discovery).
      await page.dispatchEvent("#bucket-input", "change");
      await page.waitForFunction(
        () => {
          const p = document.getElementById("prefix-input");
          return p.value === "" && p.placeholder.includes("dates at root");
        },
        { timeout: 15_000 },
      );
      const placeholder = await page.getAttribute("#prefix-input", "placeholder");
      return `prefix auto-emptied with placeholder "${placeholder}"`;
    },
  },

  // Truncation warning banner — a window wider than the server's prefix
  // fan-out cap (2000 hourly prefixes) comes back truncated and the banner
  // renders. No seeded data needed: the range alone overflows.
  F20: {
    family: "none",
    walk: async ({ page, pageUrl }) => {
      await gotoBrowserPage(page, pageUrl);
      await page.fill("#range-from", "2026-01-01T00:00");
      await page.fill("#range-to", "2026-04-09T21:00");
      const [resp] = await Promise.all([
        page.waitForResponse((r) => r.url().includes("/api/browse"), { timeout: 30_000 }),
        page.click("#search-btn"),
      ]);
      const body = await resp.json();
      expect(body.truncated === true, "browse response not truncated for a 99-day window");
      await page.waitForSelector("#browse-warning", { state: "visible", timeout: 10_000 });
      const banner = await textOf(page, "#browse-warning");
      expect(/omitted|listing limit/i.test(banner), `banner text was "${banner}"`);
      return `truncated:true; banner "${banner.slice(0, 80)}..."`;
    },
  },
};
