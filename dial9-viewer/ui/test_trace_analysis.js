#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const { EVENT_TYPES, parseTrace } = require("./trace_parser.js");
const {
  buildWorkerSpans,
  attachCpuSamples,
  buildActiveTaskTimeline,
  computeSchedulingDelays,
  filterPointsOfInterest,
  buildFlamegraphTree,
  flattenFlamegraph,
  buildFgData,
  buildSpanData,
  collectDescendants,
  selectSpanRenderSet,
  computeSpanLayout,
  getTraceTimeRange,
  hasCpuProfileSamples,
  analyzeAllocations,
  makeBarCoalescer,
  computePollWakes,
  pixelDownsampleSpans,
  pixelCoverage,
} = require("./trace_analysis.js");

async function main() {
  const tracePath = process.argv[2] || path.join(__dirname, "demo-trace.bin");

  if (!fs.existsSync(tracePath)) {
    console.error(`Trace file not found: ${tracePath}`);
    process.exit(1);
  }

  function fail(msg) {
    console.log(`✗ ${msg}`);
    process.exit(1);
  }

  function pass(msg) {
    console.log(`✓ ${msg}`);
  }

  function testProfilerOnlyTraceRangeUsesCpuSamples() {
    const profilerOnlyTrace = {
      events: [],
      cpuSamples: [
        { timestamp: 300, source: 0, callchain: ["0x3"] },
        { timestamp: 100, source: 0, callchain: ["0x1"] },
        { timestamp: 200, source: 1, callchain: ["0x2"] },
      ],
    };

    if (!hasCpuProfileSamples(profilerOnlyTrace.cpuSamples)) {
      fail("CPU profile samples should make a trace displayable without runtime events");
    }
    const range = getTraceTimeRange(profilerOnlyTrace.events, profilerOnlyTrace.cpuSamples);
    if (!range || range.minTs !== 100 || range.maxTs !== 300 || range.durationNs !== 200) {
      fail(`profiler-only range should come from CPU profile samples, got ${JSON.stringify(range)}`);
    }
    pass("Profiler-only trace range uses CPU profile samples");
  }

  function testProfilerOnlyTraceRangeExpandsSingleCpuSample() {
    const range = getTraceTimeRange([], [
      { timestamp: 100, source: 0, callchain: ["0x1"] },
    ]);
    if (!range || range.minTs !== 100 || range.maxTs !== 101 || range.durationNs !== 1) {
      fail(`single-sample profiler-only range should be non-zero, got ${JSON.stringify(range)}`);
    }
    pass("Single-sample profiler-only trace range is non-zero");
  }

  testProfilerOnlyTraceRangeUsesCpuSamples();
  testProfilerOnlyTraceRangeExpandsSingleCpuSample();

  const trace = await parseTrace(fs.readFileSync(tracePath));
  const evts = trace.events;

  const wSet = new Set();
  evts.forEach((e) => {
    if (
      e.eventType !== EVENT_TYPES.QueueSample &&
      e.eventType !== EVENT_TYPES.WakeEvent
    )
      wSet.add(e.workerId);
  });
  const workerIds = [...wSet].sort((a, b) => a - b);

  let minTs = evts[0].timestamp;
  let maxTs = evts[evts.length - 1].timestamp;
  for (const e of evts) {
    if (e.timestamp < minTs) minTs = e.timestamp;
    if (e.timestamp > maxTs) maxTs = e.timestamp;
  }

  // ── buildWorkerSpans ──

  const { workerSpans, perWorker, queueSamples, workerQueueSamples, maxLocalQueue, wakesByTask, wakesByWorker } = buildWorkerSpans(
    evts,
    workerIds,
    maxTs
  );

  function testPollsHaveValidRange() {
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        if (p.start > p.end)
          fail(`Worker ${w}: poll start > end (${p.start} > ${p.end})`);
      }
    }
    pass("All polls have start <= end");
  }

  function testNoOverlappingPolls() {
    for (const w of workerIds) {
      const polls = workerSpans[w].polls;
      for (let i = 1; i < polls.length; i++) {
        if (polls[i].start < polls[i - 1].end)
          fail(`Worker ${w}: overlapping polls at index ${i}`);
      }
    }
    pass("No overlapping polls on same worker");
  }

  function testActiveRatiosInRange() {
    for (const w of workerIds) {
      for (const a of workerSpans[w].actives) {
        if (a.ratio < 0 || a.ratio > 1)
          fail(`Worker ${w}: active ratio ${a.ratio} out of [0, 1]`);
      }
    }
    pass("Active period ratios in [0, 1]");
  }

  function testParksHaveValidRange() {
    for (const w of workerIds) {
      for (const p of workerSpans[w].parks) {
        if (p.start > p.end) fail(`Worker ${w}: park start > end`);
      }
    }
    pass("All parks have start <= end");
  }

  function testQueueSamplesExist() {
    if (queueSamples.length === 0) fail("No queue samples");
    pass(`${queueSamples.length} queue samples`);
  }

  // ── attachCpuSamples ──

  const cpuResult = attachCpuSamples(trace.cpuSamples, workerSpans);

  function testAttachedSamplesWithinPollBounds() {
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        if (p.cpuSamples) {
          for (const s of p.cpuSamples) {
            if (s.timestamp < p.start || s.timestamp > p.end)
              fail(
                `Worker ${w}: cpu sample at ${s.timestamp} outside poll [${p.start}, ${p.end}]`
              );
          }
        }
        if (p.schedSamples) {
          for (const s of p.schedSamples) {
            if (s.timestamp < p.start || s.timestamp > p.end)
              fail(
                `Worker ${w}: sched sample at ${s.timestamp} outside poll [${p.start}, ${p.end}]`
              );
          }
        }
      }
    }
    pass("All attached samples fall within poll bounds");
  }

  function testCpuResultCounts() {
    if (
      cpuResult.pollsWithCpuSamples < 0 ||
      cpuResult.pollsWithSchedSamples < 0
    )
      fail("Negative sample counts");
    pass(
      `${cpuResult.pollsWithCpuSamples} polls with cpu samples, ${cpuResult.pollsWithSchedSamples} with sched samples`
    );
  }

  function testInPollFlagMatchesAttachment() {
    // attachCpuSamples must set sample.inPoll === true iff the sample was
    // attached to a poll (the in-poll = real-blocking signal). Cross-check the
    // flag against ground truth: the set of samples actually attached to polls.
    const attached = new Set();
    for (const w of workerIds) {
      for (const p of workerSpans[w].polls) {
        for (const s of p.cpuSamples || []) attached.add(s);
        for (const s of p.schedSamples || []) attached.add(s);
      }
    }
    let mismatches = 0;
    for (const s of trace.cpuSamples) {
      if (!!s.inPoll !== attached.has(s)) mismatches++;
    }
    if (mismatches > 0)
      fail(`${mismatches} sample(s) have inPoll inconsistent with poll attachment`);
    pass(`inPoll flag matches poll attachment for all ${trace.cpuSamples.length} samples`);
  }

  function testOffCpuSplitIsExhaustive() {
    // Splitting off-CPU samples by inPoll must partition them exactly: every
    // off-CPU sample is either in-poll (real blocking) or idle-park, never both
    // or neither. This is the invariant the BLOCKING CALLS report relies on.
    const offCpu = trace.cpuSamples.filter((s) => s.source === 1);
    const inPoll = offCpu.filter((s) => s.inPoll);
    const idle = offCpu.filter((s) => !s.inPoll);
    if (inPoll.length + idle.length !== offCpu.length)
      fail(
        `off-CPU split not exhaustive: ${inPoll.length} + ${idle.length} != ${offCpu.length}`
      );
    pass(
      `off-CPU split exhaustive: ${offCpu.length} = ${inPoll.length} in-poll + ${idle.length} idle-park`
    );
  }

  function testSchedDelayMidPollWakeAdjustment() {
    // Bug-1 regression guard: the mid-poll wake adjustment. If a wake lands
    // inside an earlier poll of the same task, the delay must be measured from
    // that poll's end, not the wake itself. The demo trace may not exercise
    // this branch, so drive it with a synthetic two-poll task.
    const ws = {
      0: {
        polls: [
          { taskId: 1, start: 100, end: 200 },
          { taskId: 1, start: 500, end: 600 },
        ],
      },
    };
    // Wake at t=150 lands inside the first poll [100,200].
    const wakes = { 1: [{ timestamp: 150, wakerTaskId: 9 }] };
    const r = computeSchedulingDelays(ws, [0], wakes);
    // For poll #2 (start=500), effectiveWake should snap to poll #1.end = 200,
    // giving delay = 500 - 200 = 300 (not 500 - 150 = 350).
    if (r.length !== 1) fail(`expected 1 sched delay, got ${r.length}`);
    if (r[0].wakeTime !== 200 || r[0].delay !== 300)
      fail(
        `mid-poll wake not adjusted: wakeTime=${r[0].wakeTime} delay=${r[0].delay} (expected 200/300)`
      );
    pass("mid-poll wake adjusted to containing poll's end (binary search)");
  }

  function testSchedDelayWakeInGapUnadjusted() {
    // Counterpart: a wake that falls in the gap between polls (inside no poll)
    // must NOT be adjusted — delay is measured straight from the wake.
    const ws = {
      0: {
        polls: [
          { taskId: 1, start: 100, end: 200 },
          { taskId: 1, start: 500, end: 600 },
        ],
      },
    };
    const wakes = { 1: [{ timestamp: 300, wakerTaskId: 9 }] }; // in gap (200,500)
    const r = computeSchedulingDelays(ws, [0], wakes);
    if (r.length !== 1) fail(`expected 1 sched delay, got ${r.length}`);
    if (r[0].wakeTime !== 300 || r[0].delay !== 200)
      fail(
        `gap wake wrongly adjusted: wakeTime=${r[0].wakeTime} delay=${r[0].delay} (expected 300/200)`
      );
    pass("wake in inter-poll gap left unadjusted");
  }

  // ── extractLocalQueueSamples (via buildWorkerSpans) ──

  function testLocalQueueNonNegative() {
    for (const w of workerIds) {
      for (const s of workerQueueSamples[w]) {
        if (s.local < 0) fail(`Worker ${w}: negative local queue ${s.local}`);
      }
    }
    pass("All local queue depths non-negative");
  }

  function testMaxLocalQueue() {
    if (maxLocalQueue < 1) fail(`maxLocalQueue ${maxLocalQueue} < 1`);
    pass(`maxLocalQueue = ${maxLocalQueue}`);
  }

  // ── buildActiveTaskTimeline ──

  const { activeTaskSamples, taskFirstPoll } = buildActiveTaskTimeline(
    trace.taskSpawnTimes,
    trace.taskTerminateTimes
  );

  function testTimelineSorted() {
    for (let i = 1; i < activeTaskSamples.length; i++) {
      if (activeTaskSamples[i].t < activeTaskSamples[i - 1].t)
        fail(`Timeline not sorted at index ${i}`);
    }
    pass("Timeline sorted by timestamp");
  }

  function testCountNonNegative() {
    for (const s of activeTaskSamples) {
      if (s.count < 0) fail(`Negative task count ${s.count}`);
    }
    pass("Task counts non-negative");
  }

  // ── indexWakeEvents (via buildWorkerSpans) ──

  function testWakesByTaskSorted() {
    for (const arr of Object.values(wakesByTask)) {
      for (let i = 1; i < arr.length; i++) {
        if (arr[i].timestamp < arr[i - 1].timestamp)
          fail("wakesByTask not sorted");
      }
    }
    pass("wakesByTask arrays sorted by timestamp");
  }

  function testWakesByWorkerSorted() {
    for (const arr of Object.values(wakesByWorker)) {
      for (let i = 1; i < arr.length; i++) {
        if (arr[i].timestamp < arr[i - 1].timestamp)
          fail("wakesByWorker not sorted");
      }
    }
    pass("wakesByWorker arrays sorted by timestamp");
  }

  function testWakeCountsConsistent() {
    let taskTotal = 0;
    for (const arr of Object.values(wakesByTask)) taskTotal += arr.length;
    let workerTotal = 0;
    for (const arr of Object.values(wakesByWorker)) workerTotal += arr.length;
    if (taskTotal !== workerTotal)
      fail(
        `wakesByTask total ${taskTotal} != wakesByWorker total ${workerTotal}`
      );
    pass(`${taskTotal} wake events indexed consistently`);
  }

  // ── computeSchedulingDelays ──

  const schedDelays = computeSchedulingDelays(
    workerSpans,
    workerIds,
    wakesByTask
  );

  function testDelaysPositive() {
    for (const sd of schedDelays) {
      if (sd.delay <= 0) fail(`Non-positive delay: ${sd.delay}`);
    }
    pass("All delays positive");
  }

  function testDelaysBounded() {
    for (const sd of schedDelays) {
      if (sd.delay >= 1e9) fail(`Delay >= 1s: ${sd.delay}`);
    }
    pass("All delays < 1s");
  }

  function testWakeBeforePoll() {
    for (const sd of schedDelays) {
      if (sd.wakeTime >= sd.pollTime)
        fail(`wakeTime ${sd.wakeTime} >= pollTime ${sd.pollTime}`);
    }
    pass("wakeTime < pollTime for all delays");
  }

  function testDelaysSorted() {
    for (let i = 1; i < schedDelays.length; i++) {
      if (schedDelays[i].wakeTime < schedDelays[i - 1].wakeTime)
        fail("schedDelays not sorted by wakeTime");
    }
    pass("schedDelays sorted by wakeTime");
  }

  // ── filterPointsOfInterest ──

  function testLongPollFilter() {
    const pois = filterPointsOfInterest(
      "long-poll",
      workerSpans,
      workerIds,
      schedDelays,
      { hasSchedWait: trace.hasSchedWait }
    );
    if (pois.length === 0) fail("No long-poll points of interest found");
    for (const p of pois) {
      if (p.type !== "long-poll") fail(`Wrong type: ${p.type}`);
      if (p.value <= 1) fail(`long-poll value ${p.value} <= 1ms`);
    }
    pass(`long-poll filter: ${pois.length} results, all > 1ms`);
  }

  function testCpuSampledFilter() {
    const pois = filterPointsOfInterest(
      "cpu-sampled",
      workerSpans,
      workerIds,
      schedDelays,
      { hasSchedWait: trace.hasSchedWait }
    );
    if (pois.length === 0) fail("No cpu-sampled points of interest found");
    for (const p of pois) {
      if (p.type !== "cpu-sampled") fail(`Wrong type: ${p.type}`);
      if (p.value <= 0) fail(`cpu-sampled value ${p.value} <= 0`);
    }
    pass(`cpu-sampled filter: ${pois.length} results, all with samples`);
  }

  function testWakeDelayFilter() {
    const pois = filterPointsOfInterest(
      "wake-delay",
      workerSpans,
      workerIds,
      schedDelays,
      { hasSchedWait: trace.hasSchedWait }
    );
    if (pois.length === 0) fail("No wake-delay points of interest found");
    for (const p of pois) {
      if (p.type !== "wake-delay") fail(`Wrong type: ${p.type}`);
      if (p.value <= 100) fail(`wake-delay value ${p.value} <= 100µs`);
    }
    pass(`wake-delay filter: ${pois.length} results, all > 100µs`);
  }

  function testSortByWorst() {
    const pois = filterPointsOfInterest(
      "long-poll",
      workerSpans,
      workerIds,
      schedDelays,
      { hasSchedWait: trace.hasSchedWait, sortByWorst: true }
    );
    for (let i = 1; i < pois.length; i++) {
      if (pois[i].value > pois[i - 1].value) fail("sortByWorst not descending");
    }
    pass("sortByWorst produces descending order");
  }

  // ── buildFlamegraphTree / flattenFlamegraph ──

  function testFlamegraphTree() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0) fail("No CPU samples found");

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    if (root.count !== cpuSamples.length)
      fail(`Root count ${root.count} != sample count ${cpuSamples.length}`);
    pass(`Root count matches sample count (${root.count})`);
  }

  function testFlattenFlamegraph() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0) fail("No CPU samples found");

    const root = buildFlamegraphTree(cpuSamples, trace.callframeSymbols);
    const { nodes, maxDepth } = flattenFlamegraph(root, cpuSamples.length);
    for (const n of nodes) {
      if (n.x < 0 || n.x >= 1) fail(`Node x=${n.x} out of [0, 1)`);
      if (n.w <= 0) fail(`Node w=${n.w} <= 0`);
    }
    if (maxDepth < 0) fail(`maxDepth ${maxDepth} < 0`);
    pass(`${nodes.length} flamegraph nodes, maxDepth=${maxDepth}`);
  }

  function testBuildFgData() {
    const cpuSamples = trace.cpuSamples.filter((s) => s.source !== 1);
    if (cpuSamples.length === 0) fail("No CPU samples found");

    const data = buildFgData(cpuSamples, trace.callframeSymbols);
    if (!data) fail("buildFgData returned null for non-empty samples");
    if (data.totalSamples !== cpuSamples.length)
      fail(`totalSamples ${data.totalSamples} != ${cpuSamples.length}`);
    pass(
      `buildFgData: ${data.nodes.length} nodes, ${data.totalSamples} samples`
    );
  }

  function testBuildFgDataEmpty() {
    const data = buildFgData([], trace.callframeSymbols);
    if (data !== null) fail("buildFgData should return null for empty samples");
    pass("buildFgData returns null for empty samples");
  }

  // Inlined frames: when callframeSymbols.get(addr) returns an array, per
  // blazesym the array is ordered [outermost, ..., innermost]. entry[0] is the
  // real function at the address; entry[i>0] are inlined callees so the call
  // chain goes entry[0] -> entry[1] -> entry[2]. The flamegraph tree must
  // descend in that same order (outermost as parent, innermost as leaf).
  function testFlamegraphInlineOrder() {
    const callframeSymbols = new Map([
      ["0x1000", [
        { symbol: "outer_fn", location: "outer.rs:10" },
        { symbol: "mid_fn", location: "mid.rs:20" },
        { symbol: "leaf_fn", location: "leaf.rs:30" },
      ]],
    ]);
    const samples = [{ callchain: ["0x1000"], workerId: 0 }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    if (tree.children.size !== 1) fail(`root has ${tree.children.size} children, expected 1`);
    const outer = [...tree.children.values()][0];
    if (!outer.fullName.includes("outer_fn")) fail(`child of root is "${outer.fullName}", expected "outer_fn"`);
    if (outer.children.size !== 1) fail(`outer has ${outer.children.size} children, expected 1`);
    const mid = [...outer.children.values()][0];
    if (!mid.fullName.includes("mid_fn")) fail(`child of outer is "${mid.fullName}", expected "mid_fn"`);
    const leaf = [...mid.children.values()][0];
    if (!leaf.fullName.includes("leaf_fn")) fail(`child of mid is "${leaf.fullName}", expected "leaf_fn"`);
    if (leaf.self !== 1) fail(`leaf.self = ${leaf.self}, expected 1 (innermost frame is where the sample lands)`);
    pass("Inlined frames expand outermost→innermost as parent→child in the flamegraph");
  }

  // The inline-expansion code must not crash when an address maps to an array
  // with nullish elements (can happen with sparse SymbolTableEntry events or
  // when a child inline is resolved before its parent frame).
  function testFlamegraphInlineTolerantOfNullSlots() {
    // arr[0] present, arr[1] undefined, arr[2] present. The iteration should
    // skip the undefined slot rather than creating a "(unknown)" level.
    const sparse = new Array(3);
    sparse[0] = { symbol: "outer_fn", location: null };
    sparse[2] = { symbol: "leaf_fn", location: null };
    const callframeSymbols = new Map([["0x2000", sparse]]);
    const samples = [{ callchain: ["0x2000"], workerId: 0 }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    // Expected: (all) -> outer_fn -> leaf_fn (sparse slot skipped)
    const outer = [...tree.children.values()][0];
    if (!outer || !outer.fullName.includes("outer_fn")) fail(`expected outer_fn child, got ${outer && outer.fullName}`);
    if (outer.children.size !== 1) fail(`outer has ${outer.children.size} children, expected 1 (sparse slot should be skipped)`);
    const leaf = [...outer.children.values()][0];
    if (!leaf.fullName.includes("leaf_fn")) fail(`expected leaf_fn after outer_fn, got ${leaf.fullName}`);
    pass("Sparse inline arrays do not produce phantom tree levels");
  }

  // An address that is not present in callframeSymbols should still produce
  // a single-level child using the raw address as the key (so unresolved
  // traces remain visible rather than collapsing).
  function testFlamegraphUnknownAddress() {
    const callframeSymbols = new Map(); // empty — address resolves to undefined
    const samples = [{ callchain: ["0x3000"], workerId: 0 }];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    if (tree.children.size !== 1) fail(`root has ${tree.children.size} children for single unresolved address`);
    const node = [...tree.children.values()][0];
    if (node.self !== 1) fail(`unresolved node.self = ${node.self}, expected 1`);
    pass("Unresolved addresses still produce a single tree level");
  }

  function testFlamegraphWeightedSamples() {
    const callframeSymbols = new Map([
      ["0xA", [{ symbol: "alloc_fn", location: "alloc.rs:1" }]],
      ["0xB", [{ symbol: "caller_fn", location: "caller.rs:1" }]],
    ]);
    // callchain is leaf-first: [leaf, ..., root]. Reversed internally to root→leaf.
    const samples = [
      { callchain: ["0xA", "0xB"], weight: 1000, allocWeight: 2 },
      { callchain: ["0xA", "0xB"], weight: 500, allocWeight: 1.5 },
      { callchain: ["0xA"], weight: 200, allocWeight: 1 },
    ];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    if (tree.count !== 1700) fail(`root.count = ${tree.count}, expected 1700`);
    if (tree.allocCount !== 4.5) fail(`root.allocCount = ${tree.allocCount}, expected 4.5`);
    // First two samples: caller_fn -> alloc_fn. Third: alloc_fn only.
    const caller = tree.children.get("caller_fn");
    if (!caller) fail("expected caller_fn as child of root");
    if (caller.count !== 1500) fail(`caller.count = ${caller.count}, expected 1500`);
    if (caller.self !== 0) fail(`caller.self = ${caller.self}, expected 0`);
    const alloc = caller.children.get("alloc_fn");
    if (!alloc) fail("expected alloc_fn as child of caller_fn");
    if (alloc.count !== 1500) fail(`alloc.count = ${alloc.count}, expected 1500`);
    if (alloc.self !== 1500) fail(`alloc.self = ${alloc.self}, expected 1500`);
    if (alloc.selfAllocCount !== 3.5) fail(`alloc.selfAllocCount = ${alloc.selfAllocCount}, expected 3.5`);
    // Third sample: alloc_fn is root-level child
    const allocDirect = tree.children.get("alloc_fn");
    if (!allocDirect) fail("expected alloc_fn as direct child of root for single-frame sample");
    if (allocDirect.count !== 200) fail(`allocDirect.count = ${allocDirect.count}, expected 200`);
    if (allocDirect.self !== 200) fail(`allocDirect.self = ${allocDirect.self}, expected 200`);
    if (allocDirect.selfAllocCount !== 1) fail(`allocDirect.selfAllocCount = ${allocDirect.selfAllocCount}, expected 1`);
    pass("Weighted samples accumulate count, self, allocCount, selfAllocCount correctly");
  }

  function testFlamegraphDefaultWeightBackcompat() {
    const callframeSymbols = new Map([
      ["0xC", [{ symbol: "cpu_fn", location: "cpu.rs:1" }]],
    ]);
    const samples = [
      { callchain: ["0xC"] },
      { callchain: ["0xC"] },
    ];
    const tree = buildFlamegraphTree(samples, callframeSymbols);
    if (tree.count !== 2) fail(`root.count = ${tree.count}, expected 2`);
    const node = tree.children.get("cpu_fn");
    if (node.count !== 2) fail(`node.count = ${node.count}, expected 2`);
    if (node.self !== 2) fail(`node.self = ${node.self}, expected 2`);
    if (node.allocCount != null) fail(`node.allocCount should be undefined for unweighted samples`);
    pass("Unweighted samples default to weight=1, no allocCount fields");
  }

  // ── TaskDumpEvent parsing (verified against the demo trace) ──

  function testTaskDumpsParsed() {
    if (!trace.taskDumps) fail("trace.taskDumps should be a Map");
    if (!(trace.taskDumps instanceof Map)) fail("trace.taskDumps should be an instance of Map");
    pass(`trace.taskDumps is a Map with ${trace.taskDumps.size} task IDs`);
  }

  function testTaskDumpsSortedByTimestamp() {
    // Every value in taskDumps is an array sorted by timestamp — the renderer
    // relies on this for its O(n) sweep across idle gaps.
    for (const [tid, dumps] of trace.taskDumps) {
      for (let i = 1; i < dumps.length; i++) {
        if (dumps[i].timestamp < dumps[i - 1].timestamp) {
          fail(`taskDumps for task ${tid} not sorted (index ${i})`);
        }
      }
    }
    pass("All taskDumps arrays are sorted by timestamp");
  }

  function testTaskDumpsShape() {
    // Each dump is {timestamp, callchain} where callchain is an array of hex address strings.
    for (const [tid, dumps] of trace.taskDumps) {
      for (const d of dumps) {
        if (typeof d.timestamp !== "number") fail(`dump.timestamp for task ${tid} is ${typeof d.timestamp}`);
        if (!Array.isArray(d.callchain)) fail(`dump.callchain for task ${tid} is not an array`);
        for (const addr of d.callchain) {
          if (typeof addr !== "string" || !addr.startsWith("0x")) {
            fail(`dump.callchain entry ${addr} not a hex string`);
          }
        }
        break; // sample one per task is enough
      }
    }
    pass("TaskDumps have expected {timestamp, callchain} shape with hex-string addresses");
  }

  function testTaskDumpsTaskIdsKnown() {
    // Every task ID that has a dump should be a known spawned task (no orphans).
    for (const tid of trace.taskDumps.keys()) {
      if (!trace.taskSpawnTimes.has(tid)) {
        fail(`task ${tid} has taskDumps but is not in taskSpawnTimes`);
      }
    }
    pass("All taskDump task IDs refer to tasks that appear in taskSpawnTimes");
  }

  // ── buildSpanData ──

  function testBuildSpanDataPairing() {
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "handle_request", fields: { user_id: "42" } } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "redis_get", fields: { key: "foo" } } },
      { name: "SpanExitEvent",  timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "redis_get", fields: { key: "foo" } } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "handle_request", fields: { user_id: "42" } } },
    ];
    const { allSpans, spanMeta } = buildSpanData(customEvents);
    if (allSpans.length !== 2) fail(`Expected 2 spans, got ${allSpans.length}`);
    const redis = allSpans.find(s => s.spanName === "redis_get");
    const handle = allSpans.find(s => s.spanName === "handle_request");
    if (!redis || !handle) fail("Missing expected spans");
    if (redis.start !== 1100 || redis.end !== 1200) fail("redis_get timing wrong");
    if (redis.segments.length !== 1) fail(`Expected 1 segment, got ${redis.segments.length}`);
    if (redis.segments[0].workerId !== 0) fail("segment workerId wrong");
    if (!spanMeta.has("1") || !spanMeta.has("2")) fail("spanMeta missing entries");
    // Verify sorted by start time
    if (allSpans[0].start > allSpans[1].start) fail("Spans not sorted by start time");
    pass(`${allSpans.length} spans paired correctly`);
  }

  function testBuildSpanDataParent() {
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 10, parent_span_id: null, span_name: "root", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 20, parent_span_id: 10, span_name: "child", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1200, fields: { worker_id: 0, span_id: 20, span_name: "child", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 10, span_name: "root", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    const child = allSpans.find(s => s.spanName === "child");
    if (child.parentSpanId !== "10") fail(`Expected parentSpanId="10", got ${child.parentSpanId}`);
    const root = allSpans.find(s => s.spanName === "root");
    if (root.parentSpanId !== null) fail(`Expected root parentSpanId=null, got ${root.parentSpanId}`);
    pass("Parent span IDs preserved correctly");
  }

  function testBuildSpanDataEmpty() {
    const { allSpans, spanMeta } = buildSpanData([]);
    if (allSpans.length !== 0) fail("Expected empty allSpans");
    if (spanMeta.size !== 0) fail("Expected empty spanMeta");
    pass("Empty input produces empty output");
  }

  function testBuildSpanDataDepth() {
    // Three levels of nesting via explicit parent
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "root", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "mid", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1200, fields: { worker_id: 0, span_id: 3, parent_span_id: 2, span_name: "leaf", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 3, span_name: "leaf", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1400, fields: { worker_id: 0, span_id: 2, span_name: "mid", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "root", fields: {} } },
    ];
    const { allSpans, maxDepth } = buildSpanData(customEvents);
    const root = allSpans.find(s => s.spanName === "root");
    const mid = allSpans.find(s => s.spanName === "mid");
    const leaf = allSpans.find(s => s.spanName === "leaf");
    if (root.depth !== 0) fail(`root depth=${root.depth}, expected 0`);
    if (mid.depth !== 1) fail(`mid depth=${mid.depth}, expected 1`);
    if (leaf.depth !== 2) fail(`leaf depth=${leaf.depth}, expected 2`);
    if (maxDepth !== 2) fail(`maxDepth=${maxDepth}, expected 2`);
    pass("Depth computed correctly for 3-level nesting");
  }

  function testBuildSpanDataCycleDetection() {
    // Cyclic parent chain: A -> B -> A (should not stack overflow)
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: 2, span_name: "a", fields: {} } },
      { name: "SpanEnterEvent", timestamp: 1100, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "b", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1200, fields: { worker_id: 0, span_id: 2, span_name: "b", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1300, fields: { worker_id: 0, span_id: 1, span_name: "a", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 2) fail("Expected 2 spans");
    // Just verify it didn't crash; depths may be arbitrary due to cycle
    pass("Cyclic parent chain does not stack overflow");
  }

  function testBuildSpanDataRecycledId() {
    // Span ID 1 used first as "alpha", closed, then recycled as "beta"
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "alpha", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "alpha", fields: {} } },
      { name: "SpanCloseEvent", timestamp: 1150, fields: { span_id: 1 } },
      // Same span_id reused with different name
      { name: "SpanEnterEvent", timestamp: 2000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "beta", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 2100, fields: { worker_id: 0, span_id: 1, span_name: "beta", fields: {} } },
      { name: "SpanCloseEvent", timestamp: 2150, fields: { span_id: 1 } },
      // Child of the recycled span
      { name: "SpanEnterEvent", timestamp: 3000, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "child", fields: {} } },
      { name: "SpanExitEvent",  timestamp: 3100, fields: { worker_id: 0, span_id: 2, span_name: "child", fields: {} } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 3) fail(`Expected 3 spans, got ${allSpans.length}`);
    const alpha = allSpans.find(s => s.spanName === "alpha");
    const beta = allSpans.find(s => s.spanName === "beta");
    if (!alpha || !beta) fail("Missing alpha or beta span");
    if (alpha.start !== 1000 || beta.start !== 2000) fail("Span intervals not distinct");
    // The viewer's per-lane highlight loop relies on grouping allSpans by
    // spanId into a multimap (spansByIdAll) and lighting up EVERY instance —
    // not a single last-wins entry. Assert that recycled id 1 indeed yields
    // two grouped instances, so the highlight stays correct under recycling.
    const byId = new Map();
    for (const s of allSpans) {
      let b = byId.get(s.spanId);
      if (!b) { b = []; byId.set(s.spanId, b); }
      b.push(s);
    }
    // spanId is keyed exactly as stored on the span objects (a string here),
    // the same value the viewer puts in selectedSpanIds — so the multimap key
    // is alpha.spanId, not a numeric literal.
    if ((byId.get(alpha.spanId) || []).length !== 2)
      fail(`Expected id ${JSON.stringify(alpha.spanId)} to group 2 recycled spans, got ${(byId.get(alpha.spanId) || []).length}`);
    if (alpha.spanId !== beta.spanId) fail("Recycled spans should share the same spanId key");
    pass("Recycled span IDs produce separate intervals (and group by id)");
  }

  function testBuildSpanDataPerCallsiteSchema() {
    // New format: schema names are "SpanEnter:target::name:file:line"
    // User fields are top-level (not nested in a "fields" StringMap)
    const customEvents = [
      { name: "SpanEnter:myapp::handle:src/main.rs:10", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "handle", request_id: "abc-123" } },
      { name: "SpanExit:myapp::handle:src/main.rs:10",  timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "handle", request_id: "abc-123" } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (!allSpans || allSpans.length !== 1) fail(`Expected 1 span, got ${allSpans?.length}`);
    if (allSpans[0].spanName !== "handle") fail(`Expected span name 'handle', got '${allSpans[0].spanName}'`);
    if (allSpans[0].fields.request_id !== "abc-123") fail(`Expected request_id='abc-123', got '${allSpans[0].fields.request_id}'`);
    // Base fields should NOT appear in the user fields
    if (allSpans[0].fields.worker_id) fail("worker_id should not be in user fields");
    if (allSpans[0].fields.span_name) fail("span_name should not be in user fields");
    pass("Per-callsite schema with typed fields parsed correctly");
  }

  function testBuildSpanDataUnmatched() {
    const customEvents = [
      { name: "SpanEnter:app::a:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "a" } },
      { name: "SpanExit:app::a:f:1",  timestamp: 1100, fields: { worker_id: 0, span_id: 1, span_name: "a" } },
      // This enter has no matching exit (trace ended mid-span)
      { name: "SpanEnter:app::b:f:2", timestamp: 1200, fields: { worker_id: 0, span_id: 2, parent_span_id: null, span_name: "b" } },
    ];
    const { allSpans, unmatchedSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 1) fail(`Expected 1 matched span, got ${allSpans.length}`);
    if (!unmatchedSpans || unmatchedSpans.length !== 1) fail(`Expected 1 unmatched span, got ${unmatchedSpans?.length}`);
    if (unmatchedSpans[0].spanName !== "b") fail(`Expected unmatched span 'b', got '${unmatchedSpans[0].spanName}'`);
    if (unmatchedSpans[0].spanId !== "2") fail(`Expected unmatched spanId "2", got ${unmatchedSpans[0].spanId}`);
    pass("Unmatched spans (enter without exit) detected correctly");
  }

  function testBuildSpanDataChildrenIndex() {
    // Root r1 has children c1, c2. c1 has grandchild g1. r2 is childless.
    const customEvents = [
      { name: "SpanEnterEvent", timestamp: 100, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "r1" } },
      { name: "SpanEnterEvent", timestamp: 110, fields: { worker_id: 0, span_id: 2, parent_span_id: 1, span_name: "c1" } },
      { name: "SpanEnterEvent", timestamp: 120, fields: { worker_id: 0, span_id: 3, parent_span_id: 2, span_name: "g1" } },
      { name: "SpanExitEvent",  timestamp: 130, fields: { worker_id: 0, span_id: 3, span_name: "g1" } },
      { name: "SpanExitEvent",  timestamp: 140, fields: { worker_id: 0, span_id: 2, span_name: "c1" } },
      { name: "SpanEnterEvent", timestamp: 150, fields: { worker_id: 0, span_id: 4, parent_span_id: 1, span_name: "c2" } },
      { name: "SpanExitEvent",  timestamp: 160, fields: { worker_id: 0, span_id: 4, span_name: "c2" } },
      { name: "SpanExitEvent",  timestamp: 170, fields: { worker_id: 0, span_id: 1, span_name: "r1" } },
      { name: "SpanEnterEvent", timestamp: 200, fields: { worker_id: 0, span_id: 5, parent_span_id: null, span_name: "r2" } },
      { name: "SpanExitEvent",  timestamp: 210, fields: { worker_id: 0, span_id: 5, span_name: "r2" } },
    ];
    const { childrenByParent } = buildSpanData(customEvents);
    if (!childrenByParent) fail("childrenByParent not exposed from buildSpanData");
    const roots = childrenByParent.get(null) || [];
    if (!roots.includes("1") || !roots.includes("5")) fail(`Roots should include "1" and "5", got ${[...roots]}`);
    const c1Children = childrenByParent.get("1") || [];
    if (!c1Children.includes("2") || !c1Children.includes("4")) fail(`r1 should have children "2" and "4", got ${[...c1Children]}`);
    const g1Children = childrenByParent.get("2") || [];
    if (!g1Children.includes("3")) fail(`c1 should have child "3", got ${[...g1Children]}`);
    // Childless spans should have no entry (or empty array)
    const r2Children = childrenByParent.get("5") || [];
    if (r2Children.length !== 0) fail(`r2 should be childless, got ${[...r2Children]}`);
    pass("childrenByParent index built correctly");
  }

  function testCollectDescendants() {
    // Same tree: r1 → {c1 → g1, c2}, r2 (no children)
    const childrenByParent = new Map([
      [null, ["1", "5"]],
      ["1", ["2", "4"]],
      ["2", ["3"]],
    ]);
    const d1 = collectDescendants(["1"], childrenByParent);
    // Should include 1, 2, 3, 4 (but not 5)
    if (!d1.has("1") || !d1.has("2") || !d1.has("3") || !d1.has("4")) {
      fail(`Expected {"1","2","3","4"} in descendants of "1", got ${[...d1]}`);
    }
    if (d1.has("5")) fail("r2 should not be in descendants of r1");
    if (d1.size !== 4) fail(`Expected size 4, got ${d1.size}`);

    const d5 = collectDescendants(["5"], childrenByParent);
    if (d5.size !== 1 || !d5.has("5")) fail(`Expected only {"5"}, got ${[...d5]}`);

    // Guard against cycles (children references ancestor)
    const cyclic = new Map([
      ["1", ["2"]],
      ["2", ["1"]], // cycle
    ]);
    const dc = collectDescendants(["1"], cyclic);
    if (!dc.has("1") || !dc.has("2")) fail("Cycle should still produce set");
    pass("collectDescendants returns id plus all descendants (cycle-safe)");
  }

  function testSelectSpanRenderSetRoots() {
    // When no focus, return only spans whose parent is null OR whose parent is absent
    const spans = [
      { spanId: "1", parentSpanId: null, spanName: "r1" },
      { spanId: "2", parentSpanId: "1",    spanName: "c1" },
      { spanId: "3", parentSpanId: "99",   spanName: "orphan" }, // parent not in set
      { spanId: "5", parentSpanId: null, spanName: "r2" },
    ];
    const childrenByParent = new Map([
      [null, ["1", "5"]],
      ["1", ["2"]],
    ]);
    const result = selectSpanRenderSet({
      allSpans: spans,
      focusedSpanId: null,
      childrenByParent,
    });
    const ids = new Set(result.map(s => s.spanId));
    if (!ids.has("1") || !ids.has("5") || !ids.has("3")) fail(`Expected {"1","3","5"}, got ${[...ids]}`);
    if (ids.has("2")) fail("Child span 2 should not be rendered in root view");
    pass("selectSpanRenderSet returns only root-like spans when focus is null");
  }

  function testSelectSpanRenderSetFocused() {
    const spans = [
      { spanId: "1", parentSpanId: null, spanName: "r1" },
      { spanId: "2", parentSpanId: "1",    spanName: "c1" },
      { spanId: "3", parentSpanId: "2",    spanName: "g1" },
      { spanId: "4", parentSpanId: "1",    spanName: "c2" },
      { spanId: "5", parentSpanId: null, spanName: "r2" },
    ];
    const childrenByParent = new Map([
      [null, ["1", "5"]],
      ["1", ["2", "4"]],
      ["2", ["3"]],
    ]);
    const result = selectSpanRenderSet({
      allSpans: spans,
      focusedSpanId: "1",
      childrenByParent,
    });
    const ids = new Set(result.map(s => s.spanId));
    // Focus on 1: should include 1 itself and all descendants (2, 3, 4). Not 5.
    if (!ids.has("1") || !ids.has("2") || !ids.has("3") || !ids.has("4")) {
      fail(`Expected focused set to include {"1","2","3","4"}, got ${[...ids]}`);
    }
    if (ids.has("5")) fail("Sibling root 5 should not be in focused set");
    pass("selectSpanRenderSet returns focused span + descendants");
  }

  function testComputeSpanLayoutDurationY() {
    // Three spans with very different durations. Panel: 100 px wide, 60 px tall.
    // Longest → smallest y (near top). Shortest → largest y (near bottom).
    const spans = [
      { spanId: 1, start: 0,   end: 100,   spanName: "tiny",   segments: [], activeNs: 100 },
      { spanId: 2, start: 10,  end: 1010,  spanName: "medium", segments: [], activeNs: 1000 },
      { spanId: 3, start: 20,  end: 10020, spanName: "huge",   segments: [], activeNs: 10000 },
    ];
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 10020,
      drawW: 1000,
      panelH: 60,
      clusterXPx: 2,
      barH: 4,
    });
    if (!layout || !layout.buckets) fail("computeSpanLayout must return {buckets}");
    // Should produce one bucket per span (no clustering at this wide view).
    if (layout.buckets.length !== 3) fail(`Expected 3 buckets, got ${layout.buckets.length}`);
    // Find buckets by representative spanId.
    const byId = new Map();
    for (const b of layout.buckets) byId.set(b.representative.spanId, b);
    const yTiny = byId.get(1).y;
    const yMed = byId.get(2).y;
    const yHuge = byId.get(3).y;
    // Larger duration → smaller y (higher on screen)
    if (!(yHuge < yMed && yMed < yTiny)) {
      fail(`Expected y(huge) < y(medium) < y(tiny), got ${yHuge} < ${yMed} < ${yTiny}`);
    }
    // All y within panel
    for (const b of layout.buckets) {
      if (b.y < 0 || b.y + b.h > 60 + 1) fail(`Bucket y=${b.y}, h=${b.h} outside panel 60`);
    }
    pass("computeSpanLayout places longer spans higher (smaller y)");
  }

  function testComputeSpanLayoutClusters() {
    // Many spans with identical duration piled at the same x — should cluster.
    const spans = [];
    for (let i = 0; i < 10; i++) {
      spans.push({ spanId: i + 1, start: 100, end: 200, spanName: "same", segments: [], activeNs: 100 });
    }
    // Add one outlier with different duration (far away on y axis)
    spans.push({ spanId: 100, start: 100, end: 10000, spanName: "outlier", segments: [], activeNs: 9900 });
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 10000,
      drawW: 500,
      panelH: 60,
      clusterXPx: 4,
      barH: 4,
    });
    // Expect the 10 identical spans to cluster into 1 bucket, plus the outlier in its own bucket.
    if (layout.buckets.length !== 2) {
      fail(`Expected 2 buckets (cluster + outlier), got ${layout.buckets.length}`);
    }
    const cluster = layout.buckets.find(b => b.spans.length > 1);
    if (!cluster) fail("Expected a cluster bucket");
    if (cluster.spans.length !== 10) fail(`Expected cluster size 10, got ${cluster.spans.length}`);
    // representative should be one of the clustered spans
    if (!cluster.spans.includes(cluster.representative)) fail("Representative should be a member of cluster.spans");
    pass("computeSpanLayout clusters overlapping spans into single bucket");
  }

  function testComputeSpanLayoutRepresentativeIsLongest() {
    // Several spans at the same position. Representative should be the longest.
    // All have the same start/end (same duration → same y), so they cluster.
    // We differentiate by activeNs to verify representative selection uses total duration.
    const spans = [
      { spanId: 1, start: 100, end: 200, spanName: "a", segments: [], activeNs: 50 },
      { spanId: 2, start: 100, end: 200, spanName: "b", segments: [], activeNs: 100 },
      { spanId: 3, start: 100, end: 200, spanName: "c", segments: [], activeNs: 80 },
    ];
    const layout = computeSpanLayout({
      spans,
      viewStart: 0,
      viewEnd: 500,
      drawW: 10,
      panelH: 60,
      clusterXPx: 100,
      barH: 4,
    });
    // Same duration → same y → same cell → one cluster
    const clustered = layout.buckets.find(b => b.spans.length === 3);
    if (!clustered) fail(`Expected single 3-span cluster, got ${JSON.stringify(layout.buckets.map(b => b.spans.length))}`);
    // All have same duration, so any is valid as representative (first encountered wins tie)
    // The key property: representative is a member of the cluster
    if (!clustered.spans.includes(clustered.representative)) {
      fail("Representative should be a member of cluster.spans");
    }
    pass("computeSpanLayout picks representative from cluster members");
  }

  function testBuildSpanDataMultiplePolls() {
    // A span entered/exited multiple times (async future polled 3 times with sleep gap)
    const customEvents = [
      { name: "SpanEnter:app::f:f:1", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1",  timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "my_fn" } },
      { name: "SpanEnter:app::f:f:1", timestamp: 100000, fields: { worker_id: 1, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1",  timestamp: 100200, fields: { worker_id: 1, span_id: 1, span_name: "my_fn" } },
      { name: "SpanEnter:app::f:f:1", timestamp: 100300, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "my_fn" } },
      { name: "SpanExit:app::f:f:1",  timestamp: 100400, fields: { worker_id: 0, span_id: 1, span_name: "my_fn" } },
      { name: "SpanCloseEvent",        timestamp: 100500, fields: { span_id: 1 } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 1) fail(`Expected 1 span, got ${allSpans.length}`);
    const s = allSpans[0];
    if (s.segments.length !== 3) fail(`Expected 3 segments, got ${s.segments.length}`);
    if (s.start !== 1000) fail(`Expected start=1000, got ${s.start}`);
    if (s.end !== 100400) fail(`Expected end=100400, got ${s.end}`);
    // activeNs = 500 + 200 + 100 = 800
    if (s.activeNs !== 800) fail(`Expected activeNs=800, got ${s.activeNs}`);
    // Workers: polled on both 0 and 1
    const workers = [...new Set(s.segments.map(seg => seg.workerId))].sort();
    if (workers.length !== 2 || workers[0] !== 0 || workers[1] !== 1) fail(`Expected workers [0,1], got ${workers}`);
    pass("Multiple polls grouped into single span with segments");
  }

  function testBuildSpanDataOutOfOrder() {
    // Events arrive out of order across workers — buildSpanData sorts by timestamp.
    // Also tests the defensive guard: span 1 enters on worker 0, then enters again
    // on worker 1 before exiting on worker 0 (the second enter should be ignored).
    const customEvents = [
      // Worker 1 events arrive first in the array but have later timestamps
      { name: "SpanEnterEvent", timestamp: 2000, fields: { worker_id: 1, span_id: 2, parent_span_id: null, span_name: "b" } },
      { name: "SpanExitEvent",  timestamp: 2500, fields: { worker_id: 1, span_id: 2, span_name: "b" } },
      // Worker 0 events arrive second but have earlier timestamps
      { name: "SpanEnterEvent", timestamp: 1000, fields: { worker_id: 0, span_id: 1, parent_span_id: null, span_name: "a" } },
      // Duplicate enter on worker 1 before exit on worker 0 (should be ignored)
      { name: "SpanEnterEvent", timestamp: 1200, fields: { worker_id: 1, span_id: 1, parent_span_id: null, span_name: "a" } },
      { name: "SpanExitEvent",  timestamp: 1500, fields: { worker_id: 0, span_id: 1, span_name: "a" } },
      { name: "SpanCloseEvent", timestamp: 3000, fields: { span_id: 1 } },
      { name: "SpanCloseEvent", timestamp: 3001, fields: { span_id: 2 } },
    ];
    const { allSpans } = buildSpanData(customEvents);
    if (allSpans.length !== 2) fail(`Expected 2 spans, got ${allSpans.length}`);
    const spanA = allSpans.find(s => s.spanName === "a");
    const spanB = allSpans.find(s => s.spanName === "b");
    if (!spanA || !spanB) fail("Expected spans 'a' and 'b'");
    // Span A: entered at 1000, exited at 1500 (duplicate enter at 1200 ignored)
    if (spanA.segments.length !== 1) fail(`Expected 1 segment for 'a', got ${spanA.segments.length}`);
    if (spanA.segments[0].start !== 1000) fail(`Expected segment start=1000, got ${spanA.segments[0].start}`);
    if (spanA.segments[0].end !== 1500) fail(`Expected segment end=1500, got ${spanA.segments[0].end}`);
    // Span B: entered at 2000, exited at 2500
    if (spanB.segments[0].start !== 2000 || spanB.segments[0].end !== 2500) {
      fail(`Span B segment wrong: ${spanB.segments[0].start}-${spanB.segments[0].end}`);
    }
    pass("Out-of-order events sorted correctly; duplicate enter on different worker ignored");
  }

  // ── Regression: open PollStart at trace end must not create phantom poll (#194) ──

  function testOpenPollStartDiscarded() {
    // Simulate a rotated segment where PollStart is the last event (no PollEnd).
    const syntheticEvents = [
      { eventType: EVENT_TYPES.PollStart, timestamp: 1000, workerId: 0, taskId: 1, spawnLocId: null, spawnLoc: null, localQueue: 0 },
      { eventType: EVENT_TYPES.PollEnd,   timestamp: 2000, workerId: 0 },
      // This PollStart has no matching PollEnd — file rotated
      { eventType: EVENT_TYPES.PollStart, timestamp: 3000, workerId: 0, taskId: 2, spawnLocId: null, spawnLoc: null, localQueue: 0 },
    ];
    const syntheticMaxTs = 1_000_000; // 1ms later — would create a huge phantom poll
    const result = buildWorkerSpans(syntheticEvents, [0], syntheticMaxTs);
    const polls = result.workerSpans[0].polls;
    if (polls.length !== 1) fail(`Expected 1 poll, got ${polls.length} — open PollStart was not discarded`);
    if (polls[0].start !== 1000 || polls[0].end !== 2000) fail(`Unexpected poll range`);
    pass("Open PollStart at trace end is discarded (no phantom long poll)");
  }

  // ── Block-in-place active-span suppression ──

  function testBlockInPlaceActiveSpanSuppression() {
    // Synthetic events: worker 0 unparks on tid=42, then parks on tid=99
    // (a block_in_place handoff). The active span [10, 50) crosses the gap
    // and must be discarded. A subsequent normal active span [60, 70) on
    // tid=99 should be preserved.
    const syntheticEvents = [
      { eventType: EVENT_TYPES.WorkerUnpark, timestamp: 10, workerId: 0, tid: 42, cpuTime: 100, schedWait: 0, localQueue: 0, globalQueue: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
      { eventType: EVENT_TYPES.WorkerPark, timestamp: 50, workerId: 0, tid: 99, cpuTime: 500, localQueue: 0, globalQueue: 0, schedWait: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
      { eventType: EVENT_TYPES.WorkerUnpark, timestamp: 60, workerId: 0, tid: 99, cpuTime: 600, schedWait: 0, localQueue: 0, globalQueue: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
      { eventType: EVENT_TYPES.WorkerPark, timestamp: 70, workerId: 0, tid: 99, cpuTime: 700, localQueue: 0, globalQueue: 0, schedWait: 0, taskId: 0, spawnLocId: null, spawnLoc: null },
    ];
    const gaps = [{ workerId: 0, fromTid: 42, toTid: 99, startNs: 10, endNs: 50 }];
    const result = buildWorkerSpans(syntheticEvents, [0], 100, gaps);
    const actives = result.workerSpans[0].actives;
    // The first active [10,50) crosses the gap → suppressed.
    // The second active [60,70) is clean → preserved.
    if (actives.length !== 1) {
      fail(`Expected 1 active span (gap-crossing suppressed), got ${actives.length}: ${JSON.stringify(actives)}`);
      return;
    }
    if (actives[0].start !== 60 || actives[0].end !== 70) {
      fail(`Expected active [60,70), got [${actives[0].start},${actives[0].end})`);
      return;
    }
    pass("Active span crossing block-in-place gap is suppressed; clean span preserved");
  }

  // ── Run all tests ──

  console.log("\nbuildWorkerSpans:");
  testOpenPollStartDiscarded();
  testPollsHaveValidRange();
  testNoOverlappingPolls();
  testActiveRatiosInRange();
  testParksHaveValidRange();
  testQueueSamplesExist();

  console.log("\nattachCpuSamples:");
  testAttachedSamplesWithinPollBounds();
  testCpuResultCounts();
  testInPollFlagMatchesAttachment();
  testOffCpuSplitIsExhaustive();

  console.log("\nextractLocalQueueSamples:");
  testLocalQueueNonNegative();
  testMaxLocalQueue();

  console.log("\nbuildActiveTaskTimeline:");
  testTimelineSorted();
  testCountNonNegative();

  console.log("\nindexWakeEvents:");
  testWakesByTaskSorted();
  testWakesByWorkerSorted();
  testWakeCountsConsistent();

  console.log("\ncomputeSchedulingDelays:");
  testDelaysPositive();
  testDelaysBounded();
  testWakeBeforePoll();
  testDelaysSorted();
  testSchedDelayMidPollWakeAdjustment();
  testSchedDelayWakeInGapUnadjusted();

  console.log("\nfilterPointsOfInterest:");
  testLongPollFilter();
  testCpuSampledFilter();
  testWakeDelayFilter();
  testSortByWorst();

  console.log("\nflamegraph:");
  testFlamegraphTree();
  testFlattenFlamegraph();
  testBuildFgData();
  testBuildFgDataEmpty();
  testFlamegraphInlineOrder();
  testFlamegraphInlineTolerantOfNullSlots();
  testFlamegraphUnknownAddress();
  testFlamegraphWeightedSamples();
  testFlamegraphDefaultWeightBackcompat();

  console.log("\ntaskDumps:");
  testTaskDumpsParsed();
  testTaskDumpsSortedByTimestamp();
  testTaskDumpsShape();
  testTaskDumpsTaskIdsKnown();

  console.log("\nbuildSpanData:");
  testBuildSpanDataPairing();
  testBuildSpanDataParent();
  testBuildSpanDataEmpty();
  testBuildSpanDataDepth();
  testBuildSpanDataCycleDetection();
  testBuildSpanDataRecycledId();
  testBuildSpanDataPerCallsiteSchema();
  testBuildSpanDataUnmatched();
  testBuildSpanDataChildrenIndex();
  testBuildSpanDataMultiplePolls();
  testBuildSpanDataOutOfOrder();

  console.log("\nspan pane layout:");
  testCollectDescendants();
  testSelectSpanRenderSetRoots();
  testSelectSpanRenderSetFocused();
  testComputeSpanLayoutDurationY();
  testComputeSpanLayoutClusters();
  testComputeSpanLayoutRepresentativeIsLongest();

  console.log("\nblock-in-place active-span suppression:");
  testBlockInPlaceActiveSpanSuppression();

  console.log("\ncomputePollWakes:");
  testPollWakesMatchesBruteForce();
  testPollWakesNoWakes();
  testPollWakesMidPollBump();
  testPollWakesSharedBoundary();

  // Reference O(P^2) implementation — the original viewer loop, kept here to
  // prove the binary-search version produces identical results.
  function pollWakesBruteForce(polls, wakes) {
    const out = [];
    for (let pi = 0; pi < polls.length; pi++) {
      const s = polls[pi];
      let best = null;
      if (wakes.length) {
        let lo = 0, hi = wakes.length - 1, bi = -1;
        while (lo <= hi) {
          const mid = (lo + hi) >> 1;
          if (wakes[mid].timestamp <= s.start) { bi = mid; lo = mid + 1; }
          else hi = mid - 1;
        }
        if (bi >= 0) {
          const w = wakes[bi];
          let effectiveWake = w.timestamp;
          for (let j = 0; j < pi; j++) {
            if (w.timestamp >= polls[j].start && w.timestamp <= polls[j].end) {
              effectiveWake = polls[j].end;
              break;
            }
          }
          const delay = s.start - effectiveWake;
          if (delay >= 0 && delay < 1e9) best = { wake: w, effectiveWake };
        }
      }
      out.push(best);
    }
    return out;
  }

  function testPollWakesMatchesBruteForce() {
    // Deterministic pseudo-random non-overlapping polls + scattered wakes.
    const polls = [];
    let t = 0;
    let seed = 12345;
    const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
    for (let i = 0; i < 400; i++) {
      const gap = Math.floor(rnd() * 50);
      const dur = 1 + Math.floor(rnd() * 80);
      const start = t + gap;
      polls.push({ start, end: start + dur });
      t = start + dur;
    }
    const wakes = [];
    for (let i = 0; i < 600; i++) {
      wakes.push({ timestamp: Math.floor(rnd() * t), wakerTaskId: i });
    }
    // Deliberately seed wakes EXACTLY on poll boundaries (poll.start, which for
    // a zero-gap poll equals the previous poll's end). This is the case where a
    // wake is contained by two adjacent polls and where the binary-search and
    // O(P^2) versions can disagree on which poll "owns" it — so the comparison
    // below actually exercises the first-match tie-break, not just interiors.
    for (let i = 0; i < polls.length; i += 7) {
      wakes.push({ timestamp: polls[i].start, wakerTaskId: 1000 + i });
      wakes.push({ timestamp: polls[i].end, wakerTaskId: 2000 + i });
    }
    wakes.sort((a, b) => a.timestamp - b.timestamp);

    const fast = computePollWakes(polls, wakes);
    const slow = pollWakesBruteForce(polls, wakes);
    if (fast.length !== slow.length) fail(`pollWakes: length mismatch ${fast.length} vs ${slow.length}`);
    for (let i = 0; i < slow.length; i++) {
      const a = fast[i], b = slow[i];
      if ((a == null) !== (b == null)) fail(`pollWakes[${i}]: null mismatch`);
      if (a && b) {
        if (a.effectiveWake !== b.effectiveWake)
          fail(`pollWakes[${i}]: effectiveWake ${a.effectiveWake} vs ${b.effectiveWake}`);
        if (a.wake.wakerTaskId !== b.wake.wakerTaskId)
          fail(`pollWakes[${i}]: wake mismatch`);
      }
    }
    pass("binary-search computePollWakes matches O(P^2) reference (400 polls, 600 wakes)");
  }

  function testPollWakesNoWakes() {
    const polls = [{ start: 0, end: 10 }, { start: 20, end: 30 }];
    const out = computePollWakes(polls, []);
    if (out.length !== 2 || out[0] !== null || out[1] !== null)
      fail("pollWakes: empty wakes should yield all-null");
    pass("no wakes yields all-null result");
  }

  function testPollWakesMidPollBump() {
    // Wake at t=5 lands inside poll[0] [0,10]; poll[1] starts at 20.
    // effectiveWake for poll[1] must bump to poll[0].end (10), not 5.
    const polls = [{ start: 0, end: 10 }, { start: 20, end: 30 }];
    const wakes = [{ timestamp: 5, wakerTaskId: 99 }];
    const out = computePollWakes(polls, wakes);
    // poll[0]: wake at 5 <= start 0? no — rightmost wake <= 0 is none → null.
    if (out[0] !== null) fail("pollWakes: poll[0] should have no qualifying wake");
    if (!out[1] || out[1].effectiveWake !== 10)
      fail(`pollWakes: poll[1] effectiveWake should bump to 10, got ${out[1] && out[1].effectiveWake}`);
    pass("wake landing mid-earlier-poll bumps effectiveWake to that poll's end");
  }

  function testPollWakesSharedBoundary() {
    // A wake landing on a shared poll boundary (poll0.end == poll1.start == t)
    // is contained by BOTH adjacent polls. The original O(P^2) loop took the
    // FIRST (lowest-index) match, so effectiveWake = poll0.end == t (no bump).
    // The binary search finds the rightmost poll with start <= t (poll1), so it
    // must walk left to poll0 to stay faithful. Without that walk it would
    // wrongly report poll1.end.
    const polls = [{ start: 0, end: 10 }, { start: 10, end: 20 }, { start: 30, end: 40 }];
    const wakes = [{ timestamp: 10, wakerTaskId: 7 }];
    const out = computePollWakes(polls, wakes);
    if (!out[2] || out[2].effectiveWake !== 10)
      fail(`pollWakes: shared-boundary effectiveWake should be 10 (first match), got ${out[2] && out[2].effectiveWake}`);

    // Zero-width poll chain all touching t=10: lowest-index match is poll0.
    const chain = [{ start: 0, end: 10 }, { start: 10, end: 10 }, { start: 10, end: 20 }, { start: 30, end: 40 }];
    const cout = computePollWakes(chain, wakes);
    if (!cout[3] || cout[3].effectiveWake !== 10)
      fail(`pollWakes: boundary-chain effectiveWake should be 10, got ${cout[3] && cout[3].effectiveWake}`);
    pass("wake on a shared poll boundary matches first-match (lowest-index) semantics");
  }

  console.log("\npixelDownsampleSpans:");
  const _dur = (s) => s.end - s.start;
  function ffvByStart(spans, vs) {
    let lo = 0, hi = spans.length - 1;
    while (lo <= hi) { const m = (lo + hi) >> 1; if (spans[m].end < vs) lo = m + 1; else hi = m - 1; }
    return lo;
  }
  testDownsampleBoundsOutputByPixels();
  testDownsampleKeepsLongestPerColumn();
  testDownsamplePassThroughWhenSparse();
  testDownsampleStartIdxAndBreak();
  testDownsampleEmpty();

  function testDownsampleBoundsOutputByPixels() {
    // 100k spans over a 200px lane → at most 200 representatives.
    const spans = [];
    for (let i = 0; i < 100000; i++) spans.push({ start: i, end: i + 1 });
    const reps = pixelDownsampleSpans(spans, 0, 0, 100000, 200, _dur);
    if (reps.length > 200) fail(`downsample: expected ≤200 reps, got ${reps.length}`);
    if (reps.length < 1) fail("downsample: expected some reps");
    pass(`100k spans over 200px → ${reps.length} reps (≤200)`);
  }

  function testDownsampleKeepsLongestPerColumn() {
    // Three spans in the same pixel column; the longest must be the rep.
    // viewDur=1000 over pw=10 → 100ns per pixel. All three start in [0,100).
    const spans = [
      { start: 0,  end: 5,  id: "a" },
      { start: 10, end: 90, id: "b" }, // longest
      { start: 20, end: 25, id: "c" },
    ];
    const reps = pixelDownsampleSpans(spans, 0, 0, 1000, 10, _dur);
    if (reps.length !== 1) fail(`downsample: expected 1 rep in column, got ${reps.length}`);
    if (reps[0].id !== "b") fail(`downsample: expected longest 'b', got '${reps[0].id}'`);
    pass("longest span wins its pixel column");
  }

  function testDownsamplePassThroughWhenSparse() {
    // Spans already spread > 1px apart: all survive, order preserved.
    const spans = [
      { start: 0,   end: 10 },
      { start: 500, end: 510 },
      { start: 999, end: 1000 },
    ];
    const reps = pixelDownsampleSpans(spans, 0, 0, 1000, 1000, _dur);
    if (reps.length !== 3) fail(`downsample: expected 3 reps when sparse, got ${reps.length}`);
    if (reps[0].start !== 0 || reps[2].start !== 999) fail("downsample: order/identity not preserved");
    pass("sparse spans pass through unchanged");
  }

  function testDownsampleStartIdxAndBreak() {
    // startIdx skips earlier spans; iteration breaks past viewEnd.
    const spans = [];
    for (let i = 0; i < 1000; i++) spans.push({ start: i * 10, end: i * 10 + 5 });
    // view [2000, 3000]; binary-search start, downsample over wide pw so no merging.
    const startIdx = ffvByStart(spans, 2000);
    const reps = pixelDownsampleSpans(spans, startIdx, 2000, 3000, 100000, _dur);
    for (const r of reps) {
      if (r.start > 3000) fail(`downsample: rep past viewEnd (${r.start})`);
      if (r.end < 2000) fail(`downsample: rep before viewStart (${r.end})`);
    }
    if (reps.length < 1) fail("downsample: expected reps in window");
    pass("respects startIdx and breaks past viewEnd");
  }

  function testDownsampleEmpty() {
    if (pixelDownsampleSpans([], 0, 0, 1000, 100, _dur).length !== 0) fail("downsample: empty in → empty out");
    if (pixelDownsampleSpans([{start:0,end:1}], 0, 0, 0, 100, _dur).length !== 0) fail("downsample: zero viewDur → empty");
    if (pixelDownsampleSpans([{start:0,end:1}], 0, 0, 1000, 0, _dur).length !== 0) fail("downsample: zero pw → empty");
    pass("empty / degenerate inputs yield no reps");
  }

  console.log("\npixelCoverage:");
  testCoverageFullColumn();
  testCoverageHalf();
  testCoverageSparseStaysSparse();
  testCoverageSpanAcrossColumns();
  testCoverageClampedToView();
  testCoverageDegenerate();

  function approx(a, b, eps) { return Math.abs(a - b) <= (eps || 1e-9); }

  function testCoverageFullColumn() {
    // One span exactly filling the whole view → every column ≈ 1.
    const cov = pixelCoverage([{ start: 0, end: 100 }], 0, 0, 100, 10);
    for (let i = 0; i < cov.length; i++)
      if (!approx(cov[i], 1)) fail(`coverage: col ${i} expected ~1, got ${cov[i]}`);
    pass("span filling the view → all columns fully covered");
  }

  function testCoverageHalf() {
    // 100ns view over 10px = 10ns/px. A span covering [0,50) fills cols 0-4,
    // leaves cols 5-9 empty.
    const cov = pixelCoverage([{ start: 0, end: 50 }], 0, 0, 100, 10);
    for (let i = 0; i < 5; i++) if (!approx(cov[i], 1)) fail(`coverage: col ${i} expected ~1, got ${cov[i]}`);
    for (let i = 5; i < 10; i++) if (!approx(cov[i], 0)) fail(`coverage: col ${i} expected 0, got ${cov[i]}`);
    pass("half-covered view → half the columns full, half empty");
  }

  function testCoverageSparseStaysSparse() {
    // 10 tiny polls (1ns each) scattered across a 1000ns view at 10px.
    // Each column is 100ns wide; total covered time per column ≪ 1 → faint,
    // NOT solid. This is the misleading-solid-band case the helper fixes.
    const polls = [];
    for (let i = 0; i < 10; i++) polls.push({ start: i * 100, end: i * 100 + 1 });
    const cov = pixelCoverage(polls, 0, 0, 1000, 10);
    for (let i = 0; i < cov.length; i++) {
      if (cov[i] > 0.05) fail(`coverage: sparse col ${i} should be faint, got ${cov[i]}`);
      if (cov[i] <= 0) fail(`coverage: sparse col ${i} should be > 0 (one poll present)`);
    }
    pass("sparse polls produce faint (≪1) coverage, not a solid band");
  }

  function testCoverageSpanAcrossColumns() {
    // Span [5,25) over 100ns/10px (10ns/col): col0 covered [5,10)=0.5,
    // col1 fully [10,20)=1.0, col2 [20,25)=0.5.
    const cov = pixelCoverage([{ start: 5, end: 25 }], 0, 0, 100, 10);
    if (!approx(cov[0], 0.5)) fail(`coverage: col0 expected 0.5, got ${cov[0]}`);
    if (!approx(cov[1], 1.0)) fail(`coverage: col1 expected 1.0, got ${cov[1]}`);
    if (!approx(cov[2], 0.5)) fail(`coverage: col2 expected 0.5, got ${cov[2]}`);
    for (let i = 3; i < 10; i++) if (!approx(cov[i], 0)) fail(`coverage: col ${i} expected 0`);
    pass("span straddling columns splits coverage proportionally");
  }

  function testCoverageClampedToView() {
    // Span extends beyond both edges; only the in-view portion counts and
    // no value exceeds 1.
    const cov = pixelCoverage([{ start: -1000, end: 1000 }], 0, 0, 100, 10);
    for (let i = 0; i < cov.length; i++) {
      if (cov[i] > 1) fail(`coverage: col ${i} exceeds 1 (${cov[i]})`);
      if (!approx(cov[i], 1)) fail(`coverage: col ${i} expected ~1, got ${cov[i]}`);
    }
    pass("spans wider than the view clamp to ≤1 per column");
  }

  function testCoverageDegenerate() {
    if (pixelCoverage([], 0, 0, 100, 10).some(v => v !== 0)) fail("coverage: empty → all zero");
    if (pixelCoverage([{start:0,end:1}], 0, 0, 0, 10).length !== 0 &&
        pixelCoverage([{start:0,end:1}], 0, 0, 0, 10).some(v => v !== 0)) fail("coverage: zero viewDur");
    if (pixelCoverage([{start:0,end:1}], 0, 0, 100, 0).length !== 0) fail("coverage: zero pw → empty");
    pass("degenerate inputs yield empty/zero coverage");
  }

  console.log("\nmakeBarCoalescer:");
  testCoalescerMergesSubPixelRun();
  testCoalescerBreaksOnColorChange();
  testCoalescerBreaksOnGap();
  testCoalescerMinWidth();
  testCoalescerEmpty();
  testCoalescerExtendsRunRightEdge();

  function collectRuns(pushFn, minWidth) {
    const runs = [];
    const c = makeBarCoalescer((x, w, color) => runs.push({ x, w, color }), minWidth);
    pushFn(c);
    c.flush();
    return runs;
  }

  function testCoalescerMergesSubPixelRun() {
    // 50 same-color sub-pixel bars all landing in [10, 11) must collapse to
    // a single fillRect, not 50 of them.
    const runs = collectRuns((c) => {
      for (let i = 0; i < 50; i++) c.push(10, 10.2, "#abc");
    });
    if (runs.length !== 1) fail(`coalescer: expected 1 run, got ${runs.length}`);
    if (runs[0].color !== "#abc") fail("coalescer: wrong color");
    pass("sub-pixel same-color burst collapses to one rect");
  }

  function testCoalescerBreaksOnColorChange() {
    const runs = collectRuns((c) => {
      c.push(0, 5, "#aaa");
      c.push(5, 10, "#bbb");
      c.push(10, 15, "#aaa");
    });
    if (runs.length !== 3) fail(`coalescer: expected 3 runs on color change, got ${runs.length}`);
    if (runs.map(r => r.color).join() !== "#aaa,#bbb,#aaa")
      fail("coalescer: colors out of order");
    pass("adjacent bars of different colors stay separate");
  }

  function testCoalescerBreaksOnGap() {
    // Same color but a >1px gap between them must NOT merge.
    const runs = collectRuns((c) => {
      c.push(0, 5, "#aaa");
      c.push(20, 25, "#aaa");
    });
    if (runs.length !== 2) fail(`coalescer: expected 2 runs across gap, got ${runs.length}`);
    pass("same-color bars separated by a gap stay separate");
  }

  function testCoalescerMinWidth() {
    const runs = collectRuns((c) => c.push(10, 10, "#aaa")); // zero-width bar
    if (runs.length !== 1) fail("coalescer: expected 1 run");
    if (runs[0].w !== 1) fail(`coalescer: expected min width 1, got ${runs[0].w}`);
    const wide = collectRuns((c) => c.push(10, 10, "#aaa"), 3);
    if (wide[0].w !== 3) fail(`coalescer: expected min width 3, got ${wide[0].w}`);
    pass("min width enforced for sub-pixel runs");
  }

  function testCoalescerEmpty() {
    const runs = collectRuns(() => {});
    if (runs.length !== 0) fail(`coalescer: expected 0 runs, got ${runs.length}`);
    pass("no pushes emits nothing");
  }

  function testCoalescerExtendsRunRightEdge() {
    // Overlapping/adjacent same-color bars extend the run; the emitted rect
    // spans the union [0, 30).
    const runs = collectRuns((c) => {
      c.push(0, 10, "#aaa");
      c.push(8, 20, "#aaa");
      c.push(20, 30, "#aaa");
    });
    if (runs.length !== 1) fail(`coalescer: expected 1 merged run, got ${runs.length}`);
    if (runs[0].x !== 0 || runs[0].w !== 30)
      fail(`coalescer: expected x=0 w=30, got x=${runs[0].x} w=${runs[0].w}`);
    pass("contiguous same-color bars merge to their union");
  }

  console.log("\nanalyzeAllocations:");
  testAnalyzeAllocationsEmpty();
  testAnalyzeAllocationsBasicSummary();
  testAnalyzeAllocationsPerTask();
  testAnalyzeAllocationsNonWorkerTid();
  testAnalyzeAllocationsEstimatedBytes();
  testAnalyzeAllocationsAddressReuse();

  function testAnalyzeAllocationsEmpty() {
    const r = analyzeAllocations(null, null);
    if (r.summary.totalAllocCount !== 0) fail("empty: expected 0 allocs");
    if (r.perTask.size !== 0) fail("empty: expected empty perTask");
    pass("null inputs produce empty result");
  }

  function testAnalyzeAllocationsBasicSummary() {
    const allocs = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xa"] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xa"] },
    ];
    const frees = [
      { timestamp: 300, tid: 10, addr: "0x1", size: 1024, allocTimestampNs: 100 },
    ];
    const r = analyzeAllocations(allocs, frees);
    if (r.summary.totalAllocCount !== 2) fail("basic: expected 2 allocs");
    if (r.summary.totalFreeCount !== 1) fail("basic: expected 1 free");
    if (r.summary.leakedCount !== 1) fail("basic: expected 1 leak");
    if (r.summary.totalAllocBytes !== 3072) fail("basic: expected 3072 bytes");
    // With default R=524288, small allocs (s<<R) have weight ≈ R each
    // so estimatedTotalBytes ≈ 2 * 524288 (slightly above due to s/(1-exp(-s/R)) > R)
    if (Math.abs(r.summary.estimatedTotalBytes - 2 * 524288) > 5000) fail("basic: wrong estimatedTotalBytes");
    pass("basic summary correct");
  }

  function testAnalyzeAllocationsPerTask() {
    // Worker 0 has tid=10, polling task 42 from t=50..500 and task 99 from t=600..900
    const events = [
      { eventType: 0, timestamp: 50, workerId: 0, taskId: 42 },
      { eventType: 0, timestamp: 600, workerId: 0, taskId: 99 },
    ];
    const tidToWorker = new Map([[10, 0]]);
    const allocs = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xa"] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xb"] },
      { timestamp: 700, tid: 10, size: 512, addr: "0x3", callchain: ["0xc"] },
    ];
    const frees = [];
    const r = analyzeAllocations(allocs, frees, { events, tidToWorker });
    if (r.perTask.size !== 2) fail(`perTask: expected 2 tasks, got ${r.perTask.size}`);
    const t42 = r.perTask.get(42);
    if (!t42) fail("perTask: missing task 42");
    if (t42.count !== 2) fail(`perTask: task 42 count=${t42.count}, expected 2`);
    if (t42.sampledBytes !== 3072) fail(`perTask: task 42 sampledBytes=${t42.sampledBytes}`);
    // estimatedBytes should be > sampledBytes (weight > size for small allocs)
    if (t42.estimatedBytes <= t42.sampledBytes) fail("perTask: estimatedBytes should exceed sampledBytes for small allocs");
    const t99 = r.perTask.get(99);
    if (!t99) fail("perTask: missing task 99");
    if (t99.count !== 1) fail(`perTask: task 99 count=${t99.count}, expected 1`);
    pass("per-task attribution correct");
  }

  function testAnalyzeAllocationsNonWorkerTid() {
    // Alloc from tid=99 which is not in tidToWorker → should not appear in perTask
    const events = [
      { eventType: 0, timestamp: 50, workerId: 0, taskId: 42 },
    ];
    const tidToWorker = new Map([[10, 0]]);
    const allocs = [
      { timestamp: 100, tid: 99, size: 1024, addr: "0x1", callchain: ["0xa"] },
    ];
    const frees = [];
    const r = analyzeAllocations(allocs, frees, { events, tidToWorker });
    if (r.perTask.size !== 0) fail("nonWorkerTid: expected empty perTask");
    pass("non-worker tid allocations excluded from perTask");
  }

  function testAnalyzeAllocationsEstimatedBytes() {
    const events = [
      { eventType: 0, timestamp: 50, workerId: 0, taskId: 7 },
    ];
    const tidToWorker = new Map([[10, 0]]);
    // Use size = sampleRateBytes so weight = s/(1-exp(-1)) ≈ 1.582*s
    const sampleRateBytes = 1000;
    const allocs = [
      { timestamp: 100, tid: 10, size: 1000, addr: "0x1", callchain: [] },
      { timestamp: 200, tid: 10, size: 1000, addr: "0x2", callchain: [] },
      { timestamp: 300, tid: 10, size: 1000, addr: "0x3", callchain: [] },
    ];
    const r = analyzeAllocations(allocs, [], { events, tidToWorker, sampleRateBytes });
    const t7 = r.perTask.get(7);
    if (!t7) fail("estimated: missing task 7");
    // weight(1000) = 1000 / (1 - exp(-1)) ≈ 1581.98
    const expectedPerSample = 1000 / (1 - Math.exp(-1));
    if (Math.abs(t7.estimatedBytes - 3 * expectedPerSample) > 1) fail(`estimated: expected ~${3*expectedPerSample}, got ${t7.estimatedBytes}`);
    if (r.sampleRateBytes !== 1000) fail("estimated: sampleRateBytes not returned");
    // For s >> R, weight ≈ s (large allocs represent themselves)
    const bigAllocs = [{ timestamp: 100, tid: 10, size: 100000, addr: "0x1", callchain: [] }];
    const r2 = analyzeAllocations(bigAllocs, [], { events, tidToWorker, sampleRateBytes });
    const t7b = r2.perTask.get(7);
    if (Math.abs(t7b.estimatedBytes - 100000) > 10) fail(`large alloc: weight should ≈ size, got ${t7b.estimatedBytes}`);
    pass("weight(s) = s / (1 - exp(-s/R)) applied correctly");
  }

  function testAnalyzeAllocationsAddressReuse() {
    // Two allocs at the same address (reuse after free). Only the first is freed.
    const allocs = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xa"] },
      { timestamp: 300, tid: 10, size: 2048, addr: "0x1", callchain: ["0xa"] }, // reused addr
    ];
    const frees = [
      { timestamp: 200, tid: 10, addr: "0x1", size: 1024, allocTimestampNs: 100 }, // frees first alloc only
    ];
    const r = analyzeAllocations(allocs, frees);
    if (r.summary.leakedCount !== 1) fail(`addressReuse: expected 1 leak, got ${r.summary.leakedCount}`);
    if (r.leaks.length !== 1) fail(`addressReuse: expected 1 leak entry, got ${r.leaks.length}`);
    if (r.leaks[0].timestamp !== 300) fail(`addressReuse: leaked alloc should be the second one (t=300)`);
    pass("address reuse: only the matching alloc is considered freed");
  }

  console.log("\nheap flamegraph from alloc events:");
  testHeapFlamegraphFromAllocEvents();
  testHeapFlamegraphEmptyCallchains();

  function testHeapFlamegraphFromAllocEvents() {
    // Simulate the viewer's heap flamegraph: convert alloc events to samples
    // and build a flamegraph tree from them.
    const allocEvents = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: ["0xaaa", "0xbbb", "0xccc"] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xaaa", "0xbbb", "0xddd"] },
      { timestamp: 300, tid: 10, size: 512, addr: "0x3", callchain: ["0xaaa", "0xeee"] },
    ];
    const samples = allocEvents
      .filter(a => a.callchain.length > 0)
      .map(a => ({ callchain: a.callchain, workerId: 0 }));
    const symbols = new Map(); // no symbols — raw addresses used as names
    const tree = buildFlamegraphTree(samples, symbols);
    if (tree.count !== 3) fail(`heapFg: root count should be 3, got ${tree.count}`);
    // All samples share "0xaaa" as the bottom frame (reversed: it becomes the first child)
    // buildFlamegraphTree reverses callchains, so 0xccc/0xddd/0xeee are at the bottom
    // and 0xaaa is at the top of the tree
    if (!tree.children.has("0xccc") && !tree.children.has("0xaaa")) {
      // The tree reverses callchains, so the deepest frame becomes the root child
      fail("heapFg: expected reversed callchain structure in tree");
    }
    const flat = flattenFlamegraph(tree, tree.count);
    if (flat.nodes.length === 0) fail("heapFg: flattenFlamegraph produced no nodes");
    if (flat.maxDepth < 1) fail("heapFg: expected depth > 0");
    pass("alloc events produce valid flamegraph tree");
  }

  function testHeapFlamegraphEmptyCallchains() {
    // Alloc events with empty callchains should be filtered out
    const allocEvents = [
      { timestamp: 100, tid: 10, size: 1024, addr: "0x1", callchain: [] },
      { timestamp: 200, tid: 10, size: 2048, addr: "0x2", callchain: ["0xaaa"] },
    ];
    const samples = allocEvents
      .filter(a => a.callchain.length > 0)
      .map(a => ({ callchain: a.callchain, workerId: 0 }));
    if (samples.length !== 1) fail(`heapFgEmpty: expected 1 sample after filter, got ${samples.length}`);
    const tree = buildFlamegraphTree(samples, new Map());
    if (tree.count !== 1) fail(`heapFgEmpty: root count should be 1, got ${tree.count}`);
    pass("empty callchains filtered out correctly");
  }

  console.log("\n✓ All analysis checks passed!");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
