#!/usr/bin/env node
"use strict";

// Tests for computeRuntimeGroups: grouping worker lanes by Tokio runtime.
//
// The default ("main") runtime is not named on the wire — its workers carry
// no runtime.<name> segment metadata — so it is inferred as the block of
// present workers that no named runtime claims.

const assert = require("assert");
const { computeRuntimeGroups, buildRuntimeFilterData } = require("./trace_analysis.js");

function names(groups) {
  return groups.map((g) => g.name);
}

// 1. No runtime metadata at all → single inferred "main" group with everyone.
{
  const groups = computeRuntimeGroups([0, 1, 2, 3], new Map());
  assert.strictEqual(groups.length, 1, "one group when no metadata");
  assert.strictEqual(groups[0].name, "main");
  assert.strictEqual(groups[0].inferred, true);
  assert.deepStrictEqual(groups[0].workerIds, [0, 1, 2, 3]);
}

// 2. The reported bug: a named "journal" runtime (64..71) plus an unnamed
//    main block (0..7). Main must be inferred and lead the ordering.
{
  const wids = [0, 1, 2, 3, 4, 5, 6, 7, 64, 65, 66, 67, 68, 69, 70, 71];
  const rt = new Map([["journal", [64, 65, 66, 67, 68, 69, 70, 71]]]);
  const groups = computeRuntimeGroups(wids, rt);
  assert.deepStrictEqual(names(groups), ["main", "journal"], "main leads journal");
  assert.deepStrictEqual(groups[0].workerIds, [0, 1, 2, 3, 4, 5, 6, 7]);
  assert.strictEqual(groups[0].inferred, true);
  assert.deepStrictEqual(groups[1].workerIds, [64, 65, 66, 67, 68, 69, 70, 71]);
  assert.strictEqual(groups[1].inferred, false);
}

// 3. Multiple named runtimes, no inferred main (every worker is claimed).
{
  const rt = new Map([
    ["io", [2, 3]],
    ["main", [0, 1]],
  ]);
  const groups = computeRuntimeGroups([0, 1, 2, 3], rt);
  // Ordered by lowest worker id, so explicitly-named "main" (0,1) leads "io".
  assert.deepStrictEqual(names(groups), ["main", "io"]);
  assert.strictEqual(groups.find((g) => g.name === "main").inferred, false,
    "explicitly named main is not inferred");
}

// 4. Named runtime "main" coexisting with an unnamed block → the inferred
//    group must not collide with the real "main".
{
  const rt = new Map([["main", [10, 11]]]);
  const groups = computeRuntimeGroups([0, 1, 10, 11], rt);
  const inferred = groups.find((g) => g.inferred);
  assert.strictEqual(inferred.name, "main (untracked)");
  assert.deepStrictEqual(inferred.workerIds, [0, 1]);
  assert.deepStrictEqual(
    groups.find((g) => g.name === "main").workerIds, [10, 11]);
}

// 5. Metadata references workers not present in the trace → only present
//    workers are grouped (lanes always match rendered workers).
{
  const rt = new Map([["journal", [64, 65, 66, 67]]]);
  // Only 64 and 65 actually appear in the trace.
  const groups = computeRuntimeGroups([0, 1, 64, 65], rt);
  assert.deepStrictEqual(groups.find((g) => g.name === "journal").workerIds, [64, 65]);
  assert.deepStrictEqual(groups.find((g) => g.inferred).workerIds, [0, 1]);
}

// 6. Empty trace → no groups.
{
  assert.deepStrictEqual(computeRuntimeGroups([], new Map()), []);
}

// 7. runtimeWorkers undefined (older parser / legacy ParsedTrace) → all main.
{
  const groups = computeRuntimeGroups([5, 6], undefined);
  assert.strictEqual(groups.length, 1);
  assert.strictEqual(groups[0].name, "main");
  assert.deepStrictEqual(groups[0].workerIds, [5, 6]);
}

// ── buildRuntimeFilterData (flamegraph runtime filter) ──

const sample = (workerId) => ({ workerId, callchain: [1], timestamp: 0 });

// 8. Multi-runtime CPU samples → options with per-runtime sample counts and a
//    workerId → runtime map. Off-worker samples (255) are excluded.
{
  const rt = new Map([["journal", [64, 65]]]);
  const samples = [
    sample(0), sample(0), sample(1), // main: 3
    sample(64), sample(65),          // journal: 2
    sample(255),                     // off-worker: excluded
  ];
  const { workerRuntime, options } = buildRuntimeFilterData(samples, rt);
  assert.strictEqual(workerRuntime.get(0), "main");
  assert.strictEqual(workerRuntime.get(64), "journal");
  assert.strictEqual(workerRuntime.has(255), false, "off-worker not mapped");
  assert.deepStrictEqual(options.map((o) => o.name), ["main", "journal"]);
  const main = options.find((o) => o.name === "main");
  const journal = options.find((o) => o.name === "journal");
  assert.strictEqual(main.sampleCount, 3);
  assert.strictEqual(main.inferred, true);
  assert.strictEqual(journal.sampleCount, 2);
  assert.strictEqual(journal.inferred, false);
}

// 9. Single runtime (only the inferred main block present) → no options, so
//    the caller hides the filter. Empty workerRuntime map.
{
  const samples = [sample(0), sample(1), sample(255)];
  const { workerRuntime, options } = buildRuntimeFilterData(samples, new Map());
  assert.strictEqual(options.length, 0);
  assert.strictEqual(workerRuntime.size, 0);
}

// 10. A named runtime exists in metadata but produced no samples in this
//     window → only one runtime is actually present → filter hidden.
{
  const rt = new Map([["journal", [64, 65]]]);
  const samples = [sample(0), sample(1)]; // no journal samples here
  const { options } = buildRuntimeFilterData(samples, rt);
  assert.strictEqual(options.length, 0, "absent runtime does not appear");
}

// 11. runtimeWorkers undefined (e.g. heap view passes nothing) → hidden.
{
  const { options } = buildRuntimeFilterData([sample(0)], undefined);
  assert.strictEqual(options.length, 0);
}

// 12. Custom off-worker id is honored.
{
  const rt = new Map([["io", [10, 11]]]);
  const samples = [sample(0), sample(10), sample(99)];
  const { workerRuntime, options } = buildRuntimeFilterData(samples, rt, 99);
  assert.strictEqual(workerRuntime.has(99), false, "custom off-worker excluded");
  assert.deepStrictEqual(options.map((o) => o.name).sort(), ["io", "main"]);
}

console.log("test_runtime_groups: all assertions passed");
