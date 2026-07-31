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
import {
  diffTableTemplate,
  locTableTemplate,
  longPollsTemplate,
  schedulingDelaysTemplate,
  workerActivityTemplate,
} from "./render.js";

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
    const t = split(longPollsTemplate(data, 1_000_000, null, () => {}, 10, () => {}));
    expect(t.leaves).toContain(42); // the 5ms poll's task id survives
    expect(t.leaves).not.toContain(7); // the 0.2ms poll is filtered out
  });

  it("returns nothing when no poll exceeds the threshold", () => {
    expect(longPollsTemplate(data, 10_000_000, null, () => {}, 10, () => {})).toBe(nothing);
  });

  it("caps the rendered rows at the requested limit", () => {
    const many: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 10,
      bucket: "b",
      by_spawn_loc: [],
      top_long_polls: Array.from({ length: 20 }, (_, i) => ({
        duration_ns: 5_000_000,
        worker_id: 0,
        task_id: 1000 + i,
        spawn_loc: "app::f:f.rs:1",
        host: "h1",
        source_key: "k",
        start_ns: 1,
        end_ns: 2,
      })),
    };
    const t = split(longPollsTemplate(many, 0, null, () => {}, 3, () => {}));
    // Only the first 3 task ids should appear.
    expect(t.leaves).toContain(1000);
    expect(t.leaves).toContain(1002);
    expect(t.leaves).not.toContain(1003);
  });

  it("keeps the legacy heading, emoji included (affordance parity)", () => {
    const t = split(longPollsTemplate(data, 1_000_000, null, () => {}, 10, () => {}));
    expect(t.strings.join("")).toContain("🕒 Longest polls");
  });

  it("each row deep-links its poll via a non-destructive focus link", () => {
    const t = split(longPollsTemplate(data, 1_000_000, null, () => {}, 10, () => {}));
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
    const t = split(longPollsTemplate(hostile, 0, null, () => {}, 10, () => {}));
    expect(t.leaves).toContain(HOSTILE_TAG);
    for (const chunk of t.strings) expect(chunk).not.toContain(HOSTILE_TAG);
  });
});

describe("schedulingDelaysTemplate (Scheduling delay card)", () => {
  const coverage = {
    observed_polls: 80,
    unmeasured_polls: 20,
    over_1ms_polls: 5,
    spawn_inferred_polls: 30,
    wake_observed_polls: 40,
    wake_during_poll_polls: 10,
    uninstrumented_unmeasured_polls: 12,
    instrumentation_unknown_unmeasured_polls: 3,
    missing_readiness_unmeasured_polls: 5,
  };
  const data: TokioStatsResponse = {
    time_span_ns: 60e9,
    total_polls: 100,
    bucket: "b",
    by_spawn_loc: [],
    scheduling_delay_coverage: coverage,
    top_scheduling_delays: [
      { delay_ns: 5_000_000, ready_at_ns: 1000, poll_start_ns: 6000, poll_end_ns: 7000, worker_id: 3, task_id: 42, kind: "spawn", spawn_loc: "app::f:f.rs:1", host: "h1", source_key: "traces/a.bin.gz" },
      { delay_ns: 200_000, ready_at_ns: 2000, poll_start_ns: 2200, poll_end_ns: 2400, worker_id: 1, task_id: 7, kind: "wake", spawn_loc: "app::g:g.rs:2", host: "h1", source_key: "traces/b.bin.gz" },
    ],
  };

  it("returns nothing when the server sent no coverage (predates the rollup)", () => {
    const bare: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 10,
      bucket: "b",
      by_spawn_loc: [],
    };
    expect(schedulingDelaysTemplate(bare, null, () => {}, 10, () => {})).toBe(nothing);
  });

  it("renders the delays and their evidence labels", () => {
    const t = split(schedulingDelaysTemplate(data, null, () => {}, 10, () => {}));
    expect(t.leaves).toContain(42);
    expect(t.leaves).toContain(7);
    expect(t.leaves).toContain("spawn → first poll");
    expect(t.leaves).toContain("wake → poll");
  });

  it("caps the rendered rows at the requested limit", () => {
    const many: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 100,
      bucket: "b",
      by_spawn_loc: [],
      scheduling_delay_coverage: coverage,
      top_scheduling_delays: Array.from({ length: 20 }, (_, i) => ({
        delay_ns: 5_000_000,
        ready_at_ns: 1000,
        poll_start_ns: 6000,
        poll_end_ns: 7000,
        worker_id: 0,
        task_id: 2000 + i,
        kind: "spawn" as const,
        spawn_loc: "app::f:f.rs:1",
        host: "h1",
        source_key: "k",
      })),
    };
    const t = split(schedulingDelaysTemplate(many, null, () => {}, 3, () => {}));
    expect(t.leaves).toContain(2000);
    expect(t.leaves).toContain(2002);
    expect(t.leaves).not.toContain(2003);
  });

  it("each row deep-links via the ready -> poll-end focus window", () => {
    const t = split(schedulingDelaysTemplate(data, null, () => {}, 10, () => {}));
    const url = t.leaves.find(
      (v): v is string => typeof v === "string" && v.startsWith("viewer.html?"),
    );
    expect(url).toBeDefined();
    expect(url).toContain("focus_start=1000"); // ready_at_ns
    expect(url).not.toContain("/api/trace");
  });

  it("still renders the coverage header when no delay could be measured", () => {
    const empty: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 100,
      bucket: "b",
      by_spawn_loc: [],
      scheduling_delay_coverage: coverage,
      top_scheduling_delays: [],
    };
    const t = split(schedulingDelaysTemplate(empty, null, () => {}, 10, () => {}));
    const joined = t.strings.join("");
    expect(joined).toContain("No scheduling delay could be safely measured");
  });

  it("hostile spawn_loc renders as an interpolated VALUE (XSS-safe)", () => {
    const hostile: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 1,
      bucket: "b",
      by_spawn_loc: [],
      scheduling_delay_coverage: coverage,
      top_scheduling_delays: [
        { delay_ns: 5_000_000, ready_at_ns: 1, poll_start_ns: 2, poll_end_ns: 3, worker_id: 0, task_id: 1, kind: "spawn", spawn_loc: HOSTILE_TAG, host: HOSTILE_ATTR, source_key: "k" },
      ],
    };
    const t = split(schedulingDelaysTemplate(hostile, null, () => {}, 10, () => {}));
    expect(t.leaves).toContain(HOSTILE_TAG);
    for (const chunk of t.strings) {
      expect(chunk).not.toContain(HOSTILE_TAG);
      expect(chunk).not.toContain("onerror");
    }
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

describe("workerActivityTemplate (Worker activity card)", () => {
  const worker = (over: Record<string, unknown> = {}) => ({
    worker_id: 0,
    host: "h1",
    total_polls: 10,
    busy_ns: 1_000,
    span_ns: 10_000,
    busy_pct: 10,
    notable_polls: 1,
    worst_poll_ns: 5_000,
    ...over,
  });
  const state = (over: Record<string, unknown> = {}) => ({
    sortKey: "busyPct" as const,
    sortDesc: true,
    expandedHosts: new Set<string>(),
    onSort: () => {},
    onToggleHost: () => {},
    ...over,
  });
  const data = (workers: unknown[]): TokioStatsResponse =>
    ({
      time_span_ns: 60e9,
      total_polls: 100,
      bucket: "b",
      by_spawn_loc: [],
      worker_activity: workers,
    }) as TokioStatsResponse;

  it("returns nothing when the response carries no worker activity", () => {
    const bare: TokioStatsResponse = {
      time_span_ns: 60e9,
      total_polls: 10,
      bucket: "b",
      by_spawn_loc: [],
    };
    expect(workerActivityTemplate(bare, null, () => {}, state())).toBe(nothing);
  });

  it("renders one collapsed host row, hiding per-worker detail", () => {
    const t = split(
      workerActivityTemplate(
        data([worker({ worker_id: 0 }), worker({ worker_id: 1 })]),
        null,
        () => {},
        state(),
      ),
    );
    expect(t.leaves).toContain("h1");
    expect(t.leaves).toContain("▸"); // collapsed affordance
    // The per-worker rows are not rendered while the host is collapsed.
    expect(t.leaves).not.toContain("w0");
  });

  it("expands per-worker detail for a host in expandedHosts", () => {
    const t = split(
      workerActivityTemplate(
        data([worker({ worker_id: 0 }), worker({ worker_id: 1 })]),
        null,
        () => {},
        state({ expandedHosts: new Set(["h1"]) }),
      ),
    );
    expect(t.leaves).toContain("▾"); // expanded affordance
    expect(t.leaves).toContain(0); // worker ids reach the detail rows
    expect(t.leaves).toContain(1);
  });

  it("an expanded worker row deep-links its worst-poll exemplar", () => {
    const t = split(
      workerActivityTemplate(
        data([
          worker({
            worst_exemplar: {
              start_ns: 1000,
              end_ns: 2000,
              duration_ns: 1000,
              host: "h1",
              source_key: "traces/a.bin.gz",
            },
          }),
        ]),
        null,
        () => {},
        state({ expandedHosts: new Set(["h1"]) }),
      ),
    );
    const url = t.leaves.find(
      (v): v is string => typeof v === "string" && v.startsWith("viewer.html?"),
    );
    expect(url).toBeDefined();
    expect(url).toContain("focus_start=1000");
  });

  it("marks the active sort column with a direction indicator", () => {
    const asc = split(
      workerActivityTemplate(data([worker()]), null, () => {}, state({ sortDesc: false })),
    );
    expect(asc.leaves).toContain(" ▲");
    const desc = split(
      workerActivityTemplate(data([worker()]), null, () => {}, state({ sortDesc: true })),
    );
    expect(desc.leaves).toContain(" ▼");
  });

  it("a hostile host name is an interpolated VALUE, never an inline handler", () => {
    // The legacy page built onclick="toggleWorkerHost('${host}')", so a quote in
    // a host name could break out into script. lit-html binds a real listener,
    // so the name is only ever text/attribute data.
    const t = split(
      workerActivityTemplate(
        data([worker({ host: HOSTILE_TAG })]),
        null,
        () => {},
        state({ expandedHosts: new Set([HOSTILE_TAG]) }),
      ),
    );
    expect(t.leaves).toContain(HOSTILE_TAG);
    for (const chunk of t.strings) {
      expect(chunk).not.toContain(HOSTILE_TAG);
      expect(chunk).not.toContain("onerror");
      expect(chunk).not.toContain("onclick");
      expect(chunk).not.toContain("toggleWorkerHost");
    }
  });
});
