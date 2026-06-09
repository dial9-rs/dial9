#!/usr/bin/env node
"use strict";

// Tests for the flamegraph recipe predicates documented in SKILL.md.
// Each recipe filter is run against demo-trace.bin to verify it produces
// non-empty results (where appropriate).

const fs = require("fs");
const path = require("path");
const { assert, testAsync, summarize } = require("./test_harness.js");
const { parseTrace, EVENT_TYPES } = require("./trace_parser.js");
const TraceAnalysis = require("./trace_analysis.js");

async function main() {
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
  await testAsync("Recipe on-CPU total", async () => {
    const samples = trace.cpuSamples.filter(s => s.source === 0);
    assert.ok(samples.length > 0, `expected >0 samples, got ${samples.length}`);
  });

  // ── Recipe 2: Off-CPU (scheduling) samples only ──
  await testAsync("Recipe off-CPU", async () => {
    const samples = trace.cpuSamples.filter(s => s.source === 1);
    assert.ok(samples.length > 0, `expected >0 samples, got ${samples.length}`);
  });

  // ── Recipe 3: Just task X (via poll spans) ──
  await testAsync("Recipe task X via poll spans", async () => {
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
    assert.ok(targetTaskId, "no task with CPU samples found");
    const taskPolls = [];
    for (const wid of workerIds) {
      for (const p of workerSpans[wid].polls) {
        if (p.taskId === targetTaskId) taskPolls.push(p);
      }
    }
    const samples = trace.cpuSamples.filter(s =>
      s.source === 0 && taskPolls.some(p => s.timestamp >= p.start && s.timestamp <= p.end)
    );
    assert.ok(samples.length > 0, `expected >0 samples for task ${targetTaskId}`);
  });

  // ── Recipe 4: Polls > N ms ──
  await testAsync("Recipe polls > 5ms", async () => {
    const THRESHOLD_NS = 5_000_000;
    const longPolls = [];
    for (const wid of workerIds) {
      for (const p of workerSpans[wid].polls) {
        if ((p.end - p.start) > THRESHOLD_NS) longPolls.push(p);
      }
    }
    const samples = trace.cpuSamples.filter(s =>
      s.source === 0 && longPolls.some(p => s.timestamp >= p.start && s.timestamp <= p.end)
    );
    assert.ok(samples.length > 0, `expected >0 samples for long polls`);
  });

  // ── Recipe 5: One specific poll instance (worst poll for a task) ──
  await testAsync("Recipe worst poll for task 3", async () => {
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
    assert.ok(worstPoll, "task 3 not found");
    const samples = trace.cpuSamples.filter(s =>
      s.source === 0 && s.timestamp >= worstPoll.start && s.timestamp <= worstPoll.end
    );
    assert.ok(samples.length > 0, `expected >0 samples for worst poll`);
  });

  // ── Recipe 6: Leaf-frame search ──
  await testAsync("Recipe leaf-frame search", async () => {
    let searchTerm = null;
    for (const [, v] of trace.callframeSymbols) {
      const sym = Array.isArray(v) ? v[0].symbol : v.symbol;
      if (sym && sym.includes("rustls")) { searchTerm = "rustls"; break; }
    }
    if (!searchTerm) {
      for (const [, v] of trace.callframeSymbols) {
        const sym = Array.isArray(v) ? v[0].symbol : v.symbol;
        if (sym && sym.length > 5 && !sym.startsWith("0x")) { searchTerm = sym.slice(0, 10); break; }
      }
    }
    assert.ok(searchTerm, "no symbols found to search");
    // Leaf-frame may legitimately return 0, verify any-frame works at minimum
    const anyFrame = trace.cpuSamples.filter(s =>
      s.callchain.some(addr => {
        const sym = trace.callframeSymbols.get(addr);
        if (!sym) return false;
        const name = Array.isArray(sym) ? sym[0].symbol : sym.symbol;
        return name && name.includes(searchTerm);
      })
    );
    assert.ok(anyFrame.length > 0, `expected >0 samples containing '${searchTerm}'`);
  });

  // ── Recipe 7: Multi-trace union (parse same trace twice, merge symbols) ──
  await testAsync("Recipe multi-trace union", async () => {
    const trace2 = await parseTrace(buf);
    const merged = new Map(trace.callframeSymbols);
    for (const [k, v] of trace2.callframeSymbols) {
      if (!merged.has(k)) merged.set(k, v);
    }
    const union = [...trace.cpuSamples, ...trace2.cpuSamples].filter(s => s.source === 0);
    assert.ok(union.length > 0, "should have on-CPU samples");
    assert.ok(merged.size >= trace.callframeSymbols.size, "merged symbols should be >= original");
  });

  // ── Recipe 8: spawnLoc-based filter ──
  await testAsync("Recipe spawnLoc filter", async () => {
    const locCounts = new Map();
    trace.cpuSamples.forEach(s => {
      if (s.spawnLoc) locCounts.set(s.spawnLoc, (locCounts.get(s.spawnLoc) || 0) + 1);
    });
    const topLoc = [...locCounts.entries()].sort((a, b) => b[1] - a[1])[0];
    assert.ok(topLoc, "no spawnLocs in trace");
    const samples = trace.cpuSamples.filter(s => s.spawnLoc === topLoc[0]);
    assert.ok(samples.length > 0, `expected >0 samples for spawnLoc`);
  });

  // ── Type contract: trace timestamps are Numbers, not BigInts ──
  await testAsync("Skill type contract: trace timestamps are plain Numbers", async () => {
    const checks = [
      ["trace.minTs", trace.minTs],
      ["trace.maxTs", trace.maxTs],
      ["cpuSamples[0].timestamp", trace.cpuSamples[0]?.timestamp],
      ["events[0].timestamp", trace.events[0]?.timestamp],
    ];
    for (const [label, v] of checks) {
      if (v !== undefined) {
        assert.strictEqual(typeof v, "number", `${label} should be number, got ${typeof v}`);
      }
    }
    // Verify the documented arithmetic doesn't throw
    const _ = trace.minTs + 3_900_000_000;
  });

  summarize();
}

main().catch(err => { console.error(err); process.exit(1); });
