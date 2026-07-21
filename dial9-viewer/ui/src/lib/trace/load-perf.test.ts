import { describe, expect, it } from "vitest";
import { isLoadPerfEnabled, startLoadPerf } from "./load-perf.js";
import type { PerfLike } from "./load-perf.js";

/** Records the User Timing calls and reports fixed durations, so span math is
 *  asserted without depending on real elapsed time. */
function fakePerf(durations: Record<string, number> = {}): PerfLike & {
  marks: string[];
  measures: string[];
  cleared: string[];
} {
  const marks: string[] = [];
  const measures: string[] = [];
  const cleared: string[] = [];
  return {
    marks,
    measures,
    cleared,
    mark(name: string) {
      marks.push(name);
      return undefined;
    },
    measure(name: string, start: string, end: string) {
      measures.push(`${name}|${start}|${end}`);
      const label = name.slice(name.lastIndexOf(" ") + 1);
      return { duration: durations[label] ?? 0 };
    },
    clearMarks(name?: string) {
      if (name !== undefined) cleared.push(name);
    },
    clearMeasures(name?: string) {
      if (name !== undefined) cleared.push(name);
    },
  };
}

describe("isLoadPerfEnabled", () => {
  it("is off when neither the query flag nor storage opts in", () => {
    expect(isLoadPerfEnabled("?trace=a.bin")).toBe(false);
  });

  it("accepts perf=1 and perf=true", () => {
    expect(isLoadPerfEnabled("?perf=1")).toBe(true);
    expect(isLoadPerfEnabled("?perf=true")).toBe(true);
  });

  it("ignores other perf values", () => {
    expect(isLoadPerfEnabled("?perf=0")).toBe(false);
  });
});

describe("startLoadPerf", () => {
  it("returns a no-op recorder when disabled, marking nothing", () => {
    const perf = fakePerf();
    const rec = startLoadPerf({ enabled: false, perf });
    rec.mark("start");
    rec.mark("parse-done");
    rec.finish();
    expect(rec.enabled).toBe(false);
    expect(perf.marks).toEqual([]);
    expect(perf.measures).toEqual([]);
  });

  it("measures fetch, parse, derive and total for a buffered load", () => {
    const perf = fakePerf({ fetch: 200, parse: 9000, derive: 1500, total: 10700 });
    const logs: string[] = [];
    const rec = startLoadPerf({ enabled: true, perf, log: (m) => logs.push(m) });

    for (const phase of ["start", "fetch-done", "parse-done", "store-updated", "first-paint"] as const) {
      rec.mark(phase);
    }
    rec.finish({ mode: "buffered", urlCount: 3, events: 13_000_000, bytes: 250e6 });

    // parse is measured from fetch-done (not start) when the fetch was separate.
    expect(perf.measures.some((m) => m.includes("parse|") && m.includes(":fetch-done|"))).toBe(true);
    expect(perf.measures.some((m) => m.includes("derive|") && m.includes(":store-updated|"))).toBe(true);

    const line = logs[0]!;
    expect(line).toContain("fetch 200ms");
    expect(line).toContain("parse 9000ms");
    expect(line).toContain("derive 1500ms");
    expect(line).toContain("3 file(s)");
    expect(line).toContain("13,000,000 events");
  });

  it("measures parse from start when streaming never marks fetch-done", () => {
    const perf = fakePerf({ parse: 8000, total: 8000 });
    const logs: string[] = [];
    const rec = startLoadPerf({ enabled: true, perf, log: (m) => logs.push(m) });

    rec.mark("start");
    rec.mark("parse-done");
    rec.finish({ mode: "stream" });

    expect(logs[0]).not.toContain("fetch ");
    expect(logs[0]).toContain("parse 8000ms");
    expect(perf.measures.some((m) => m.includes("parse|") && m.includes(":start|"))).toBe(true);
  });

  it("ignores a repeated phase so a second first-paint cannot skew the span", () => {
    const perf = fakePerf();
    const rec = startLoadPerf({ enabled: true, perf, log: () => {} });
    rec.mark("start");
    rec.mark("first-paint");
    rec.mark("first-paint");
    expect(perf.marks.filter((m) => m.endsWith(":first-paint"))).toHaveLength(1);
  });

  it("releases its marks and measures on finish", () => {
    const perf = fakePerf();
    const rec = startLoadPerf({ enabled: true, perf, log: () => {} });
    rec.mark("start");
    rec.mark("parse-done");
    rec.finish();
    expect(perf.cleared.some((c) => c.endsWith(":start"))).toBe(true);
    expect(perf.cleared.some((c) => c.endsWith(" total"))).toBe(true);
  });

  it("scopes marks per run so a superseded load cannot collide", () => {
    const perf = fakePerf();
    startLoadPerf({ enabled: true, perf, log: () => {} }).mark("start");
    startLoadPerf({ enabled: true, perf, log: () => {} }).mark("start");
    expect(new Set(perf.marks).size).toBe(2);
  });
});
