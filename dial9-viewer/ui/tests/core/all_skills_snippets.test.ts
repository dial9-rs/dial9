// Validates skills: extracts code blocks from SKILL.md files and runs them against
// the demo trace. Also validates scripts/ files load correctly and schema docs match
// the actual runtime objects.
//
// Migrated from test_all_skills_snippets.js (T11). SKILL.md discovery and
// recipe extraction happen synchronously at module load so one it() is
// registered per (input mode, recipe) pair; the snippet-execution mechanism
// (strip requires, rewrite const redeclarations, run via `new Function` with
// the prelude context) is preserved verbatim. The `require` handed to
// snippets resolves from the ui/ root, exactly like the original script's.

/* eslint-disable @typescript-eslint/no-explicit-any */

import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { createRequire } from "node:module";
import * as fs from "node:fs";
import * as path from "node:path";
import * as os from "node:os";
import { fileURLToPath } from "node:url";

// Anchored at the ui/ root (= the original test's __dirname), so snippet
// require() calls and skill-script requires resolve identically.
const uiDir = fileURLToPath(new URL("../../", import.meta.url));
const require = createRequire(path.join(uiDir, "package.json"));

const skillsDir = path.resolve(uiDir, "..", "skills");
const demoPath = path.join(uiDir, "public", "demo-trace.bin");

interface Recipe {
  heading: string;
  code: string;
}

// Parse markdown: extract ```javascript blocks with their heading
function extractRecipes(md: string, filename: string): Recipe[] {
  const recipes: Recipe[] = [];
  let currentHeading = "(preamble)";
  let inBlock = false;
  let block = "";

  for (const line of md.split("\n")) {
    if (line.startsWith("## ")) {
      currentHeading = line.slice(3).trim();
    } else if (line.startsWith("```javascript")) {
      inBlock = true;
      block = "";
    } else if (line.startsWith("```") && inBlock) {
      inBlock = false;
      recipes.push({ heading: `${filename}: ${currentHeading}`, code: block });
    } else if (inBlock) {
      block += line + "\n";
    }
  }
  return recipes;
}

// Skip blocks that aren't runnable
function shouldSkip(recipe: Recipe): boolean {
  const code = recipe.code.trim();
  if (code.includes("{ ... }") || code === "..." || code === "") return true;
  if (recipe.heading.includes("Setup boilerplate")) return true;
  if (recipe.heading.includes("Working with large directories")) return true;
  // Slicing operates on a single trace file; meaningless in directory mode.
  if (recipe.heading.includes("Slicing traces")) return true;
  // Skip pure structure/type definitions
  if (/^\{\s*\n\s*(events|workerSpans|eventType|timestamp):/.test(code)) return true;
  // Skip S3 examples (need a running server)
  if (code.includes("localhost:3000")) return true;
  return false;
}

// Replace placeholder values with real ones so examples are runnable
function fixPlaceholders(code: string, tracePath: string): string {
  return code
    .replace(/['"]\/path\/to\/traces?\/['"]/g, JSON.stringify(tracePath))
    .replace(/['"]\/path\/to\/trace\.bin['"]/g, JSON.stringify(tracePath))
    .replace(/['"]trace\.bin['"]/g, JSON.stringify(tracePath));
}

// Runs a recipe snippet exactly as the example tests do: drop `require()` lines
// and demote `const` redeclarations of context vars to assignments (they are
// already bound as function params), then eval with the context. Returns logs.
async function runRecipeSnippet(
  code: string,
  ctxNames: string[],
  ctxValues: unknown[],
  tracePath: string,
): Promise<string[]> {
  const origLog = console.log;
  const logs: string[] = [];
  console.log = (...args: unknown[]) => logs.push(args.join(" "));
  try {
    const cleanCode = code
      .split("\n")
      .filter((line) => !line.match(/^\s*const\s*\{.*\}\s*=\s*require\(/))
      .map((line) => {
        for (const v of ctxNames) {
          if (new RegExp(`^(\\s*)const\\s+${v}\\s*=`).test(line))
            return line.replace(/const\s+/, "");
        }
        return line;
      })
      .join("\n");
    const body = `return (async () => { ${fixPlaceholders(cleanCode, tracePath)} })();`;
    const fn = new Function(...ctxNames, body);
    await fn(...ctxValues);
  } finally {
    console.log = origLog;
  }
  return logs;
}

// Walk skills/ subdirectories and collect all SKILL.md files
function collectSkillMds(): { name: string; path: string }[] {
  const results: { name: string; path: string }[] = [];
  for (const entry of fs.readdirSync(skillsDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const skillMd = path.join(skillsDir, entry.name, "SKILL.md");
    if (fs.existsSync(skillMd)) {
      results.push({ name: entry.name, path: skillMd });
    }
  }
  return results.sort((a, b) => a.name.localeCompare(b.name));
}

const skillMds = collectSkillMds();

// Collect recipes from all SKILL.md files (sync, at collection time).
const allRecipes: Recipe[] = [];
for (const { name, path: mdPath } of skillMds) {
  const md = fs.readFileSync(mdPath, "utf8");
  allRecipes.push(...extractRecipes(md, name));
}

// Resolve analyze.js from the toolkit skill scripts
const analyzeJsPath = path.join(skillsDir, "dial9-toolkit", "scripts", "analyze.js");

// Temp directory for directory-mode testing, seeded at collection time so the
// inputs list (and thus the registered tests) is known synchronously.
const testDir = fs.mkdtempSync(path.join(os.tmpdir(), "d9-recipe-test-"));
fs.copyFileSync(demoPath, path.join(testDir, "t1.bin"));
fs.copyFileSync(demoPath, path.join(testDir, "t2.bin"));

afterAll(() => {
  fs.rmSync(testDir, { recursive: true, force: true });
});

const inputs = [
  { label: "file", path: demoPath },
  { label: "dir", path: testDir },
];

const { parseTrace, EVENT_TYPES, formatFrame, symbolizeChain, deduplicateSamples } =
  require("./trace_parser.js") as any;
const {
  buildWorkerSpans,
  attachCpuSamples,
  buildActiveTaskTimeline,
  computeSchedulingDelays,
  filterPointsOfInterest,
  buildFgData,
  buildSpanData,
  buildFlamegraphTree,
} = require("./trace_analysis.js") as any;

for (const input of inputs) {
  describe(`skills snippets (${input.label} mode)`, { timeout: 120_000 }, () => {
    let ctxNames: string[];
    let ctxValues: unknown[];

    beforeAll(async () => {
      // Run the prelude to get the variables every recipe expects
      let trace: any,
        workerIds: any,
        minTs: any,
        maxTs: any,
        spans: any,
        schedDelays: any,
        taskTimeline: any;
      for await (const t of parseTrace(input.path)) {
        trace = t;
        workerIds = [
          ...new Set(
            trace.events
              .filter(
                (e: any) =>
                  e.eventType !== EVENT_TYPES.QueueSample &&
                  e.eventType !== EVENT_TYPES.WakeEvent,
              )
              .map((e: any) => e.workerId),
          ),
        ].sort((a: any, b: any) => a - b);
        maxTs = trace.maxTs;
        minTs = trace.minTs;
        spans = buildWorkerSpans(trace.events, workerIds, maxTs);
        attachCpuSamples(trace.cpuSamples, spans.workerSpans);
        taskTimeline = buildActiveTaskTimeline(
          trace.taskSpawnTimes,
          trace.taskTerminateTimes,
        );
        schedDelays = computeSchedulingDelays(
          spans.workerSpans,
          workerIds,
          spans.wakesByTask,
        );
        break; // use first trace for prelude variables
      }

      // Context: all variables available to code blocks
      const { analyzeTraces } = require(analyzeJsPath);
      const ctx: Record<string, unknown> = {
        trace,
        workerIds,
        minTs,
        maxTs,
        spans,
        schedDelays,
        taskTimeline,
        EVENT_TYPES,
        formatFrame,
        symbolizeChain,
        deduplicateSamples,
        buildWorkerSpans,
        attachCpuSamples,
        buildActiveTaskTimeline,
        computeSchedulingDelays,
        filterPointsOfInterest,
        buildFgData,
        buildSpanData,
        buildFlamegraphTree,
        require,
        console,
        parseTrace,
        fs,
        path,
        event: trace.events[0],
        sample: trace.cpuSamples[0] || {},
        tracePath: input.path,
        analyzeTraces,
      };
      ctxNames = Object.keys(ctx);
      ctxValues = Object.values(ctx);
    });

    for (const recipe of allRecipes) {
      if (shouldSkip(recipe)) {
        // Same set the original counted as "skipped" (only once, not per mode).
        if (input === inputs[0]) it.skip(recipe.heading, () => {});
        continue;
      }

      it(recipe.heading, async () => {
        await runRecipeSnippet(recipe.code, ctxNames, ctxValues, input.path);
      });
    }
  });
}

// Guards the recipe against a return to O(polls x spans) (its original form,
// which flaked CI as a ~77s timeout). Counts array touches instead of wall-clock
// so pass/fail is machine-independent: filter/some/for-of index each element a
// spec-defined number of times, so the count is the same integer everywhere.
// Linear sweep = ~2N touches, quadratic = ~2N^2; the threshold sits between them.
describe("recipe performance guard", () => {
  const TIGHT_LOOP =
    "dial9-trace-recipes: Detect tight loops (many spans per poll)";
  const recipe = allRecipes.find((r) => r.heading === TIGHT_LOOP);

  function countingArray(arr: any[], counter: { n: number }): any[] {
    return new Proxy(arr, {
      get(target, prop, recv) {
        if (typeof prop === "string" && /^\d+$/.test(prop)) counter.n++;
        return Reflect.get(target, prop, recv);
      },
    });
  }

  function instrumentedBuildSpanData(counter: { n: number }) {
    return (...args: unknown[]) => {
      const res = (buildSpanData as (...a: unknown[]) => any)(...args);
      for (const s of res.allSpans) s.segments = countingArray(s.segments, counter);
      res.allSpans = countingArray(res.allSpans, counter);
      return res;
    };
  }

  // `hotPollSpans` extra spans are packed into poll 0 to trip the >10 branch;
  // every other poll holds one span, disjoint in time.
  function syntheticCtx(
    N: number,
    counter: { n: number },
    hotPollSpans = 0,
  ): { names: string[]; values: unknown[] } {
    const polls: unknown[] = [];
    const customEvents: unknown[] = [];
    let spanId = 0;
    const addSpan = (start: number, end: number) => {
      customEvents.push({
        name: "SpanEnterEvent",
        timestamp: start,
        fields: { worker_id: 0, span_id: spanId, span_name: "work", parent_span_id: null },
      });
      customEvents.push({
        name: "SpanExitEvent",
        timestamp: end,
        fields: { worker_id: 0, span_id: spanId, span_name: "work" },
      });
      spanId++;
    };
    for (let i = 0; i < N; i++) {
      const s = i * 100;
      polls.push({ start: s, end: s + 50, taskId: i + 1, tid: 0, workerId: 0 });
      if (i === 0 && hotPollSpans > 0) {
        for (let k = 0; k < hotPollSpans; k++) addSpan(s + 1 + k, s + 2 + k);
      } else {
        addSpan(s + 10, s + 20);
      }
    }
    const ctx: Record<string, unknown> = {
      trace: { customEvents },
      workerIds: [0],
      minTs: 0,
      spans: { workerSpans: { 0: { polls } } },
      buildSpanData: instrumentedBuildSpanData(counter),
      console,
    };
    return { names: Object.keys(ctx), values: Object.values(ctx) };
  }

  it("recipe is present in SKILL.md", () => {
    expect(recipe, `recipe '${TIGHT_LOOP}' not found`).toBeDefined();
  });

  it("touches spans a linear number of times, not O(polls x spans)", async () => {
    const N = 2000;
    const counter = { n: 0 };
    const { names, values } = syntheticCtx(N, counter);
    await runRecipeSnippet(recipe!.code, names, values, "");
    expect(
      counter.n,
      `recipe indexed span arrays ${counter.n} times for ${N} polls; a linear ` +
        `sweep does ~${2 * N}, an O(polls x spans) regression does ~${2 * N * N}`,
    ).toBeLessThan(50 * N);
  });

  it("still reports polls whose span count exceeds the threshold", async () => {
    const counter = { n: 0 };
    const { names, values } = syntheticCtx(200, counter, 15);
    const logs = await runRecipeSnippet(recipe!.code, names, values, "");
    const hits = logs.filter((l) => /\b15 spans\b/.test(l));
    expect(hits, `expected one poll reported with 15 spans, got: ${JSON.stringify(logs)}`)
      .toHaveLength(1);
  });
});

// ── Script validation ──
// Verify that scripts/ files in each skill load without errors
describe("skill script validation", () => {
  for (const { name } of skillMds) {
    const scriptsDir = path.join(skillsDir, name, "scripts");
    if (!fs.existsSync(scriptsDir)) continue;
    for (const f of fs.readdirSync(scriptsDir).filter((f) => f.endsWith(".js"))) {
      const scriptPath = path.join(scriptsDir, f);
      it(`${name}/scripts/${f} loads`, () => {
        require(scriptPath);
      });
    }
  }
});

// ── Schema validation helpers ──
function validateSchema(
  schemaBlock: string,
  actualObj: any,
): { topLevelErrors: string[]; deepErrors: string[]; docKeyCount: number } {
  const topLevelErrors: string[] = [];
  const deepErrors: string[] = [];

  const actualKeys = new Set(Object.keys(actualObj));
  const topKeyMatches = schemaBlock.match(/^ {2}(\w+):/gm);
  if (!topKeyMatches) {
    topLevelErrors.push("no top-level keys found in schema");
    return { topLevelErrors, deepErrors, docKeyCount: 0 };
  }
  const docKeys = new Set(topKeyMatches.map((m) => m.trim().replace(/:$/, "")));
  for (const k of actualKeys) {
    if (!docKeys.has(k)) topLevelErrors.push(`'${k}' in result but not documented`);
  }
  for (const k of docKeys) {
    if (!actualKeys.has(k)) topLevelErrors.push(`'${k}' documented but missing from result`);
  }

  const schemaJs = schemaBlock
    .replace(/\/\/.*$/gm, "")
    .replace(/\[\w+\]:/g, "_dynamic_:")
    .replace(/:\s*Map<[^,]+,\s*([^>]+)>/g, (_, valType: string) => {
      const v = valType.trim();
      // Array of objects: Map<K, [{a, b}]> → values are arrays whose elements have those keys.
      // Emit a marker string that the later [{...}] pass won't double-process;
      // a separate pass below expands it back to a JS array literal.
      const arrObjMatch = v.match(/^\[\{([^}]+)\}\]$/);
      if (arrObjMatch) {
        const keys = arrObjMatch[1]!.split(",").map((k) => k.trim()).filter(Boolean);
        return ': {"_map_":"__ARR_OBJ__' + keys.join("|") + '__"}';
      }
      const objMatch = v.match(/\{([^}]+)\}/);
      if (objMatch) {
        const keys = objMatch[1]!.split(",").map((k) => k.trim()).filter(Boolean);
        return ': {"_map_":{' + keys.map((k) => `"${k}":"_any_"`).join(",") + "}}";
      }
      const t = v.replace(/\|null$/, "");
      return ': {"_map_":"' + t + '"}';
    })
    .replace(/:\s*(Histogram)(\|null)?/g, (_, __, n) => ': "Histogram' + (n ? "|null" : "") + '"')
    .replace(/:\s*(number\[\])/g, ': "number[]"')
    .replace(/:\s*(number)(\|null)?/g, (_, __, n) => ': "number' + (n ? "|null" : "") + '"')
    .replace(/:\s*(string\[\])/g, ': "string[]"')
    .replace(/:\s*(string)(\|null)?/g, (_, __, n) => ': "string' + (n ? "|null" : "") + '"')
    .replace(/:\s*(boolean)/g, ': "boolean"')
    .replace(/:\s*(\w+)\[\]/g, ': "unknown[]"')
    .replace(/\[\{([^}]+)\}\]/g, (_, inner: string) => {
      const keys = inner.split(",").map((k) => k.trim()).filter(Boolean);
      return "[{" + keys.map((k) => `"${k}":"_any_"`).join(",") + "}]";
    })
    // Expand Map<K, [{...}]> placeholder from earlier pass into a real array literal.
    .replace(/"__ARR_OBJ__([^_"]+)__"/g, (_, keysPipe: string) => {
      const keys = keysPipe.split("|");
      return "[{" + keys.map((k) => `"${k}":"_any_"`).join(",") + "}]";
    });
  let docSkeleton: any;
  try {
    docSkeleton = new Function("return {" + schemaJs + "}")();
  } catch (e) {
    deepErrors.push(`schema parse failed: ${(e as Error).message}`);
  }

  function toSkeleton(val: any): any {
    if (val === null || val === undefined) return "_null_";
    if (val instanceof Map) {
      let rep;
      for (const v of val.values()) {
        if (!Array.isArray(v)) {
          rep = v;
          break;
        }
      }
      if (rep === undefined) rep = val.values().next().value;
      return { _map_: rep !== undefined ? toSkeleton(rep) : "_empty_" };
    }
    if (typeof val === "object" && typeof val.percentile === "function")
      return "Histogram";
    if (Array.isArray(val)) {
      if (val.length === 0) return "[]";
      if (typeof val[0] === "number") return "number[]";
      if (typeof val[0] === "string") return "string[]";
      if (typeof val[0] === "object" && val[0] !== null) return [toSkeleton(val[0])];
      return "unknown[]";
    }
    if (typeof val === "number") return "number";
    if (typeof val === "string") return "string";
    if (typeof val === "boolean") return "boolean";
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(val)) out[k] = toSkeleton(v);
    return out;
  }

  if (docSkeleton) {
    const actualSkeleton = toSkeleton(actualObj);
    function diff(doc: any, actual: any, p: string): void {
      if (
        typeof doc === "object" &&
        doc !== null &&
        !Array.isArray(doc) &&
        "_dynamic_" in doc
      ) {
        if (typeof actual !== "object" || actual === null) {
          deepErrors.push(`${p}: expected object with dynamic keys`);
          return;
        }
        const firstVal = Object.values(actual)[0];
        if (firstVal !== undefined) diff(doc._dynamic_, firstVal, p + "[*]");
        return;
      }
      if (typeof doc === "string" && typeof actual === "string") {
        if (doc === "_any_" || actual === doc) return;
        if (actual === "_empty_") return;
        if (
          doc.endsWith("|null") &&
          (actual === doc.replace("|null", "") || actual === "_null_")
        )
          return;
        if (doc === "number[]" && actual === "[]") return;
        if (doc === "string[]" && actual === "[]") return;
        if (doc === "unknown[]" && actual === "[]") return;
        deepErrors.push(`${p}: type mismatch (documented: ${doc}, actual: ${actual})`);
        return;
      }
      if (doc === "_any_") return;
      if (doc === "unknown[]") return;
      if (Array.isArray(doc) && Array.isArray(actual)) {
        if (doc.length > 0 && actual.length > 0) diff(doc[0], actual[0], p + "[0]");
        return;
      }
      if (Array.isArray(doc) && actual === "[]") return; // empty actual array is compatible
      if (
        typeof doc === "object" &&
        doc !== null &&
        typeof actual === "object" &&
        actual !== null
      ) {
        const dk = new Set(Object.keys(doc)),
          ak = new Set(Object.keys(actual));
        for (const k of dk) {
          if (!ak.has(k)) deepErrors.push(`${p}.${k}: documented but missing from result`);
        }
        for (const k of ak) {
          if (!dk.has(k)) deepErrors.push(`${p}.${k}: in result but not documented`);
        }
        for (const k of dk) {
          if (ak.has(k)) diff(doc[k], actual[k], p + "." + k);
        }
        return;
      }
      if (typeof doc !== typeof actual)
        deepErrors.push(`${p}: shape mismatch (documented: ${typeof doc}, actual: ${typeof actual})`);
    }
    diff(docSkeleton, actualSkeleton, "");
  }
  return { topLevelErrors, deepErrors, docKeyCount: docKeys.size };
}

function runSchemaCheck(
  label: string,
  skillName: string,
  headingRegex: RegExp,
  actualObj: any,
): void {
  const mdPath = path.join(skillsDir, skillName, "SKILL.md");
  const md = fs.readFileSync(mdPath, "utf8");
  const match = md.match(headingRegex);
  expect(
    match,
    `${label}: could not find schema block in ${skillName}/SKILL.md`,
  ).not.toBeNull();
  const { topLevelErrors, deepErrors } = validateSchema(match![1]!, actualObj);
  expect(topLevelErrors, `${label} sync errors`).toEqual([]);
  expect(deepErrors, `${label} deep errors`).toEqual([]);
}

describe("skill schema docs match runtime objects", { timeout: 120_000 }, () => {
  // ── analyzeTraces schema (dial9-trace-analysis) ──
  it("analyzeTraces return schema (dial9-trace-analysis)", async () => {
    const { analyzeTraces: at } = require(analyzeJsPath);
    const analyzeResult = await at(demoPath);
    runSchemaCheck(
      "analyzeTraces",
      "dial9-trace-analysis",
      /## analyzeTraces return schema[\s\S]*?```\n\{([\s\S]*?)\n\}[\s\S]*?```/,
      analyzeResult,
    );
  });

  // ── ParsedTrace schema (dial9-trace-loading) ──
  it("ParsedTrace structure (dial9-trace-loading)", async () => {
    let parsedTrace: unknown;
    for await (const t of parseTrace(demoPath)) {
      parsedTrace = t;
      break;
    }
    runSchemaCheck(
      "ParsedTrace",
      "dial9-trace-loading",
      /## ParsedTrace structure[\s\S]*?```\n\{([\s\S]*?)\n\}[\s\S]*?```/,
      parsedTrace,
    );
  });
});
