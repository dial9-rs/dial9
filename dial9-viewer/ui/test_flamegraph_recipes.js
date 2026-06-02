#!/usr/bin/env node
"use strict";

// Tests for the flamegraph recipe predicates documented in SKILL.md.
// Each recipe filter is run against demo-trace.bin to verify it produces
// non-empty results (where appropriate).

const fs = require("fs");
const path = require("path");
const { parseTrace, EVENT_TYPES } = require("./trace_parser.js");
const TraceAnalysis = require("./trace_analysis.js");

async function main() {
  let failures = 0;
  function fail(msg) { console.log(`✗ ${msg}`); failures++; }
  function pass(msg) { console.log(`✓ ${msg}`); }

  const tracePath = path.join(__dirname, "demo-trace.bin");
  if (!fs.existsSync(tracePath)) {
    console.error(`demo-trace.bin not found at ${tracePath}`);
    process.exit(1);
  }

  const buf = fs.readFileSync(tracePath);
  const trace = await parseTrace(buf);

  // Build worker spans and attach CPU samples (required setup for all recipes)
  const wSet = new Set();
  trace.events.forEach(e => {
    if (e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
      wSet.add(e.workerId);
  });
  const workerIds = [...wSet].sort((a, b) => a - b);
  const spanResult = TraceAnalysis.buildWorkerSpans(
    trace.events, workerIds, trace.maxTs, trace.blockInPlaceGaps
  );
  const workerSpans = spanResult.workerSpans;
  TraceAnalysis.attachCpuSamples(trace.cpuSamples, workerSpans);

  // ── Recipe 1: Total on-CPU flamegraph ──
  {
    const samples = trace.cpuSamples.filter(s => s.source === 0);
    if (samples.length > 0) pass(`Recipe on-CPU total: ${samples.length} samples`);
    else fail("Recipe on-CPU total: 0 samples");
  }

  // ── Recipe 2: Off-CPU (scheduling) samples only ──
  {
    const samples = trace.cpuSamples.filter(s => s.source === 1);
    if (samples.length > 0) pass(`Recipe off-CPU: ${samples.length} samples`);
    else fail("Recipe off-CPU: 0 samples");
  }

  // ── Recipe 3: Just task X (via poll spans) ──
  {
    // Find a taskId that has CPU samples
    let targetTaskId = null;
    for (const wid of workerIds) {
      for (const p of workerSpans[wid].polls) {
        if (p.taskId && p.cpuSamples && p.cpuSamples.length > 0) {
          targetTaskId = p.taskId;
          break;
        }
      }
      if (targetTaskId) break;
    }
    if (!targetTaskId) {
      fail("Recipe task X: no task with CPU samples found");
    } else {
      // Collect poll time ranges for the target task
      const taskPolls = [];
      for (const wid of workerIds) {
        for (const p of workerSpans[wid].polls) {
          if (p.taskId === targetTaskId) taskPolls.push(p);
        }
      }
      const samples = trace.cpuSamples.filter(s =>
        s.source === 0 && taskPolls.some(p => s.timestamp >= p.start && s.timestamp <= p.end)
      );
      if (samples.length > 0) pass(`Recipe task ${targetTaskId}: ${samples.length} samples`);
      else fail(`Recipe task ${targetTaskId}: 0 samples`);
    }
  }

  // ── Recipe 4: Polls > N ms ──
  {
    const THRESHOLD_NS = 5_000_000; // 5ms
    const longPolls = [];
    for (const wid of workerIds) {
      for (const p of workerSpans[wid].polls) {
        if ((p.end - p.start) > THRESHOLD_NS) longPolls.push(p);
      }
    }
    const samples = trace.cpuSamples.filter(s =>
      s.source === 0 && longPolls.some(p => s.timestamp >= p.start && s.timestamp <= p.end)
    );
    if (samples.length > 0) pass(`Recipe polls > 5ms: ${samples.length} samples (${longPolls.length} polls)`);
    else fail("Recipe polls > 5ms: 0 samples");
  }

  // ── Recipe 5: One specific poll instance (worst poll for a task) ──
  {
    const targetTaskId = 3;
    let worstPoll = null;
    for (const wid of workerIds) {
      for (const p of workerSpans[wid].polls) {
        if (p.taskId === targetTaskId) {
          if (!worstPoll || (p.end - p.start) > (worstPoll.end - worstPoll.start)) {
            worstPoll = p;
          }
        }
      }
    }
    if (!worstPoll) {
      fail("Recipe worst poll: task 3 not found");
    } else {
      const samples = trace.cpuSamples.filter(s =>
        s.source === 0 && s.timestamp >= worstPoll.start && s.timestamp <= worstPoll.end
      );
      if (samples.length > 0) pass(`Recipe worst poll task 3: ${samples.length} samples (${(worstPoll.end-worstPoll.start)/1e6}ms)`);
      else fail("Recipe worst poll task 3: 0 samples");
    }
  }

  // ── Recipe 6: Leaf-frame search ──
  {
    // Find any symbol that exists in the trace to search for
    let searchTerm = null;
    for (const [, v] of trace.callframeSymbols) {
      const sym = Array.isArray(v) ? v[0].symbol : v.symbol;
      if (sym && sym.includes("rustls")) { searchTerm = "rustls"; break; }
    }
    if (!searchTerm) {
      // Fall back to any symbol
      for (const [, v] of trace.callframeSymbols) {
        const sym = Array.isArray(v) ? v[0].symbol : v.symbol;
        if (sym && sym.length > 5 && !sym.startsWith("0x")) { searchTerm = sym.slice(0, 10); break; }
      }
    }
    if (!searchTerm) {
      fail("Recipe leaf-frame: no symbols found to search");
    } else {
      const samples = trace.cpuSamples.filter(s => {
        const leaf = s.callchain[s.callchain.length - 1];
        const sym = trace.callframeSymbols.get(leaf);
        if (!sym) return false;
        const name = Array.isArray(sym) ? sym[0].symbol : sym.symbol;
        return name && name.includes(searchTerm);
      });
      if (samples.length > 0) pass(`Recipe leaf-frame '${searchTerm}': ${samples.length} samples`);
      else {
        // Leaf-frame search may legitimately return 0 if the term only appears
        // in non-leaf positions. Try any-frame instead to validate shape handling.
        const anyFrame = trace.cpuSamples.filter(s => {
          return s.callchain.some(addr => {
            const sym = trace.callframeSymbols.get(addr);
            if (!sym) return false;
            const name = Array.isArray(sym) ? sym[0].symbol : sym.symbol;
            return name && name.includes(searchTerm);
          });
        });
        if (anyFrame.length > 0) pass(`Recipe leaf-frame (any-frame fallback) '${searchTerm}': ${anyFrame.length} samples`);
        else fail(`Recipe leaf-frame '${searchTerm}': 0 samples even with any-frame`);
      }
    }
  }

  // ── Recipe 7: Multi-trace union (parse same trace twice, merge symbols) ──
  {
    const trace2 = await parseTrace(buf);
    // Merge callframeSymbols
    const merged = new Map(trace.callframeSymbols);
    for (const [k, v] of trace2.callframeSymbols) {
      if (!merged.has(k)) merged.set(k, v);
    }
    const union = [...trace.cpuSamples, ...trace2.cpuSamples].filter(s => s.source === 0);
    if (union.length > 0 && merged.size >= trace.callframeSymbols.size) {
      pass(`Recipe multi-trace union: ${union.length} samples, ${merged.size} symbols`);
    } else {
      fail("Recipe multi-trace union: unexpected result");
    }
  }

  // ── Recipe 8: spawnLoc-based filter (via attachCpuSamples) ──
  {
    // Find a spawnLoc that has samples
    const locCounts = new Map();
    trace.cpuSamples.forEach(s => {
      if (s.spawnLoc) locCounts.set(s.spawnLoc, (locCounts.get(s.spawnLoc) || 0) + 1);
    });
    const topLoc = [...locCounts.entries()].sort((a, b) => b[1] - a[1])[0];
    if (topLoc) {
      const samples = trace.cpuSamples.filter(s => s.spawnLoc === topLoc[0]);
      if (samples.length > 0) pass(`Recipe spawnLoc filter '${topLoc[0].split('/').pop()}': ${samples.length} samples`);
      else fail("Recipe spawnLoc filter: 0 samples");
    } else {
      fail("Recipe spawnLoc filter: no spawnLocs in trace");
    }
  }

  // ── Type contract: trace timestamps are Numbers, not BigInts ──
  // The skill's "compute absolute ns" guidance uses plain Number arithmetic
  // (`minTs + 3_900_000_000`). If parseTrace ever returned BigInts, agents
  // following the skill would silently produce blank flamegraphs because
  // `Number + BigInt` throws and the script aborts before rendering.
  {
    const checks = [
      ["trace.minTs", trace.minTs],
      ["trace.maxTs", trace.maxTs],
      ["cpuSamples[0].timestamp", trace.cpuSamples[0]?.timestamp],
      ["events[0].timestamp", trace.events[0]?.timestamp],
    ];
    let bad = checks.filter(([_, v]) => v !== undefined && typeof v !== "number");
    if (bad.length === 0) {
      // Also check that arithmetic doesn't throw with the documented pattern
      try {
        const _ = trace.minTs + 3_900_000_000;
        pass(`Skill type contract: trace timestamps are plain Numbers, arithmetic is safe`);
      } catch (e) {
        fail(`Skill type contract: arithmetic threw: ${e.message}`);
      }
    } else {
      fail(`Skill type contract: ${bad.map(([k, v]) => `${k} is ${typeof v}`).join(", ")}`);
    }
  }

  console.log(`\n${failures === 0 ? "All tests passed" : `${failures} test(s) FAILED`}`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch(err => { console.error(err); process.exit(1); });
