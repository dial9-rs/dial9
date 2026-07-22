// XSS regression test (a real past XSS on this page). The structural guard:
// all server/URL-derived strings (spawn locations from traced code, the
// exemplar deep-link URL, the diff-% cell) reach the DOM only through lit-html
// interpolation, which treats a `${value}` as DATA - text-escaped in element
// position, setAttribute'd in attribute position - NEVER as parsed markup.
//
// Rather than execute a hostile string in a DOM (the repo keeps no DOM test
// env; the live render is covered by the browser layer), this proves the
// invariant at the template level: a hostile string is always a lit-html VALUE
// (dynamic, escaped at render) and NEVER a substring of the static HTML chunks
// (which are our fixed template text). A regression that reintroduced the vuln
// - string-concatenating data into HTML, or wrapping it in unsafeHTML - would
// break one of these assertions or the source guard.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, it, expect } from "vitest";
import type { TokioStatsResponse } from "../../lib/trace/index.js";
import type { PeriodStats } from "./stats.js";
import { nothing } from "lit-html";
import { diffTableTemplate, locTableTemplate, longPollsTemplate } from "./render.js";

// Two classic breakout payloads: an element-context inject and an
// attribute-breakout inject.
const HOSTILE_TAG = `<img src=x onerror="alert(document.cookie)">`;
const HOSTILE_ATTR = `"><script>alert(1)</script>`;

/** Recursively split a lit-html TemplateResult into static HTML vs values. */
interface Split {
  strings: string[];
  leaves: unknown[];
}
function isTemplateResult(n: unknown): n is { strings: readonly string[]; values: unknown[] } {
  return (
    typeof n === "object" &&
    n !== null &&
    "strings" in n &&
    "values" in n &&
    Array.isArray((n as { values: unknown }).values)
  );
}
function walk(node: unknown, out: Split): void {
  if (node == null) return;
  if (Array.isArray(node)) {
    for (const n of node) walk(n, out);
    return;
  }
  if (isTemplateResult(node)) {
    for (const s of node.strings) out.strings.push(String(s));
    for (const v of node.values) walk(v, out);
    return;
  }
  out.leaves.push(node);
}
function split(node: unknown): Split {
  const out: Split = { strings: [], leaves: [] };
  walk(node, out);
  return out;
}

const HOSTILE_STATS: PeriodStats = {
  totalLong: 1,
  totalOffCpu: 1,
  totalOnCpu: 0,
  timeMinutes: 1,
  rate: 1,
  total_polls: 10,
  byLoc: {
    [HOSTILE_TAG]: {
      long: 1,
      offCpu: 1,
      onCpu: 0,
      mixed: 0,
      rate: 1,
      p50: 100,
      p99: 100,
      max: 100,
      // off-CPU exemplar so linkedStat builds a real <a data-url>; host and
      // source_key are attacker-influenceable and flow through exemplarLink.
      exemplars: [
        { start_ns: 1000, end_ns: 2000, duration_ns: 1000, host: HOSTILE_ATTR, source_key: HOSTILE_TAG },
        null,
        null,
        null,
      ],
    },
  },
};

const HOSTILE_DATA: TokioStatsResponse = {
  time_span_ns: 60e9,
  total_polls: 10,
  bucket: HOSTILE_ATTR,
  by_spawn_loc: [],
};

describe("XSS: hostile spawn_loc renders inert (I1, #587 regression)", () => {
  const t = split(locTableTemplate(HOSTILE_STATS, HOSTILE_DATA, null, () => {}));

  it("the hostile spawn_loc is an interpolated VALUE, not baked into HTML", () => {
    expect(t.leaves).toContain(HOSTILE_TAG);
    for (const chunk of t.strings) {
      expect(chunk).not.toContain(HOSTILE_TAG);
      expect(chunk).not.toContain("onerror");
    }
  });

  it("the exemplar deep-link URL is a URL-encoded attribute value (no raw markup)", () => {
    const url = t.leaves.find(
      (v): v is string => typeof v === "string" && v.startsWith("viewer.html?"),
    );
    expect(url).toBeDefined();
    // encodeURIComponent neutralized the breakout chars before they ever
    // reached the data-url attribute value.
    expect(url).not.toContain("<");
    expect(url).not.toContain(">");
    for (const chunk of t.strings) {
      expect(chunk).not.toContain(HOSTILE_ATTR);
    }
  });

  it("the static HTML chunks are our fixed template text only", () => {
    const joined = t.strings.join("");
    expect(joined).toContain("<code>"); // the spawn-loc cell wrapper is ours
    expect(joined).not.toContain("<script");
    expect(joined).not.toContain("<img");
  });
});

describe("longPollsTemplate (Longest polls card)", () => {
  const data: TokioStatsResponse = {
    time_span_ns: 60e9,
    total_polls: 10,
    bucket: "b",
    by_spawn_loc: [],
    top_long_polls: [
      { duration_ns: 5_000_000, worker_id: 3, task_id: 42, spawn_loc: "app::f:f.rs:1", host: "h1", source_key: "traces/a.bin.gz", start_ns: 1000, end_ns: 6000 },
      { duration_ns: 200_000, worker_id: 1, task_id: 7, spawn_loc: "app::g:g.rs:2", host: "h1", source_key: "traces/b.bin.gz", start_ns: 2000, end_ns: 2200 },
    ],
  };

  it("filters rows below the threshold, keeps those above", () => {
    const t = split(longPollsTemplate(data, 1_000_000, null, () => {}));
    expect(t.leaves).toContain(42); // the 5ms poll's task id survives
    expect(t.leaves).not.toContain(7); // the 0.2ms poll is filtered out
  });

  it("returns nothing when no poll exceeds the threshold", () => {
    expect(longPollsTemplate(data, 10_000_000, null, () => {})).toBe(nothing);
  });

  it("each row deep-links its poll via a non-destructive focus link", () => {
    const t = split(longPollsTemplate(data, 1_000_000, null, () => {}));
    const url = t.leaves.find(
      (v): v is string => typeof v === "string" && v.startsWith("viewer.html?"),
    );
    expect(url).toBeDefined();
    expect(url).toContain("focus_start=1000");
    expect(url).not.toContain("/api/trace");
  });

  it("hostile spawn_loc renders as an interpolated VALUE (XSS-safe)", () => {
    const hostile: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 1,
      bucket: "b",
      by_spawn_loc: [],
      top_long_polls: [
        { duration_ns: 5_000_000, worker_id: 0, task_id: 1, spawn_loc: HOSTILE_TAG, host: "h1", source_key: "k", start_ns: 1, end_ns: 2 },
      ],
    };
    const t = split(longPollsTemplate(hostile, 0, null, () => {}));
    expect(t.leaves).toContain(HOSTILE_TAG);
    for (const chunk of t.strings) expect(chunk).not.toContain(HOSTILE_TAG);
  });
});

describe("XSS: hostile diff-table cells render inert (G9, both #587 sinks)", () => {
  it("hostile loc and pct are interpolated values, never static HTML", () => {
    const rows = [
      {
        loc: HOSTILE_TAG,
        fRate: 1,
        lRate: 2,
        delta: 1,
        pct: HOSTILE_ATTR,
        first: undefined,
        last: undefined,
      },
    ];
    const t = split(diffTableTemplate(rows, "delta-bad", 2));
    expect(t.leaves).toContain(HOSTILE_TAG);
    expect(t.leaves).toContain(HOSTILE_ATTR);
    for (const chunk of t.strings) {
      expect(chunk).not.toContain(HOSTILE_TAG);
      expect(chunk).not.toContain(HOSTILE_ATTR);
      expect(chunk).not.toContain("<script");
    }
  });
});

describe("XSS: source guard against the #587 innerHTML class", () => {
  it("render.ts never uses innerHTML or an unsafe lit-html directive", () => {
    const src = readFileSync(
      fileURLToPath(new URL("./render.ts", import.meta.url)),
      "utf8",
    );
    // Usage-specific forms (a property access / directive call), so the
    // module's own prose ("never innerHTML with interpolated data") does not
    // trip the guard.
    for (const banned of [
      ".innerHTML",
      ".outerHTML",
      ".insertAdjacentHTML",
      "insertAdjacentHTML(",
      "unsafeHTML(",
      "unsafeSVG(",
      "unsafeStatic(",
    ]) {
      expect(src, `render.ts must not use ${banned}`).not.toContain(banned);
    }
  });
});
