#!/usr/bin/env node
// Zoom into a narrow time window of a dial9 trace and dump everything that
// happened in it: every poll across every worker (time-ordered), per-worker
// busy time, a per-OS-thread CPU census, queue depth, and inner spans of the
// dominant poll.
//
// Usage:
//   node zoom.js <trace.bin|dir> <centerMs> [halfWindowMs=20] [--hz N]
//
// centerMs is milliseconds from the start of the trace (the same "+N ms"
// numbers the analyzer and red-flag scan print). Find an interesting centerMs
// first — e.g. the timestamp of a long poll from `red_flag_scan.js`.
//
// --hz overrides the assumed perf sampling frequency (default 99 Hz, dial9's
// default). At 99 Hz a thread accumulates ~1 on-CPU sample per 10ms, so a
// window of ~30ms or less is below the sampler's noise floor; treat the
// per-tid census there as suggestive rather than authoritative.
"use strict";

const fs = require("fs");
const path = require("path");

function resolve(name) {
  const sibling = path.resolve(__dirname, name);
  if (fs.existsSync(sibling)) return sibling;
  const toolkit = path.resolve(__dirname, "..", "..", "dial9-toolkit", "scripts", name);
  if (fs.existsSync(toolkit)) return toolkit;
  return path.resolve(__dirname, "..", "..", "..", "ui", name);
}

const { parseTrace, EVENT_TYPES, symbolizeChain, formatFrame } = require(resolve("trace_parser.js"));
const { buildWorkerSpans, attachCpuSamples, buildSpanData } = require(resolve("trace_analysis.js"));

function leafOf(sample, symbols) {
  const frames = symbolizeChain(sample.callchain, symbols);
  return frames[0] ? formatFrame(frames[0]).text : "(unknown)";
}

function argVal(flag, def) {
  const i = process.argv.indexOf(flag);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : def;
}

async function main() {
  const dir = process.argv[2];
  const centerMs = parseFloat(process.argv[3]);
  // halfWindowMs is optional positional arg 4; ignore it if it looks like a flag.
  const positionalHalf = process.argv[4] && !process.argv[4].startsWith("--") ? process.argv[4] : "20";
  const halfMs = parseFloat(positionalHalf);
  const hz = parseFloat(argVal("--hz", "99"));
  const msPerSample = 1000 / hz;
  if (!dir || Number.isNaN(centerMs)) {
    console.error("usage: node zoom.js <trace.bin|dir> <centerMs> [halfWindowMs=20] [--hz N]");
    process.exit(2);
  }

  for await (const trace of parseTrace(dir)) {
    const workerIds = [...new Set(
      trace.events
        .filter((e) => e.eventType !== EVENT_TYPES.QueueSample && e.eventType !== EVENT_TYPES.WakeEvent)
        .map((e) => e.workerId)
    )].sort((a, b) => a - b);
    const spans = buildWorkerSpans(trace.events, workerIds, trace.maxTs, trace.blockInPlaceGaps);
    attachCpuSamples(trace.cpuSamples, spans.workerSpans);

    const min = trace.minTs;
    const center = min + centerMs * 1e6;
    const half = halfMs * 1e6;
    const lo = center - half;
    const hi = center + half;
    const winMs = 2 * halfMs;
    const relMs = (ns) => ((ns - lo) / 1e6).toFixed(2);

    // ── Every poll overlapping the window, across all workers, time-ordered ──
    const rows = [];
    for (const w of workerIds) {
      for (const p of spans.workerSpans[w].polls) {
        if (p.end >= lo && p.start <= hi) {
          rows.push({
            w,
            start: p.start,
            end: p.end,
            durMs: (p.end - p.start) / 1e6,
            task: p.taskId,
            loc: (p.spawnLoc || "?").replace(/.*registry\/src\/[^/]+\//, ""),
            onCpu: (p.cpuSamples || []).length,
            offCpu: (p.schedSamples || []).length,
          });
        }
      }
    }
    rows.sort((a, b) => a.start - b.start);

    console.log(`\n=== window [${(centerMs - halfMs).toFixed(1)} … ${(centerMs + halfMs).toFixed(1)}] ms  (center ${centerMs}, ±${halfMs}) ===`);
    console.log(`${rows.length} poll(s) across ${workerIds.length} workers; "*" marks polls > 1ms`);
    for (const r of rows) {
      const flag = r.durMs > 1 ? "*" : " ";
      let tag = "";
      if (r.durMs > 1) {
        const kind = r.onCpu > 0 ? `on-CPU (${r.onCpu} samp)` : (r.offCpu > 0 ? `off-CPU sched (${r.offCpu} samp)` : "off-CPU (no samples)");
        tag = `   <<< ${r.durMs.toFixed(2)}ms ${kind}`;
      }
      console.log(`${flag} +${relMs(r.start).padStart(7)}ms  w${String(r.w).padStart(2)}  ${r.durMs.toFixed(3).padStart(8)}ms  task=${String(r.task).padStart(6)}  ${r.loc}${tag}`);
    }

    // ── Per-worker busy time inside the window ──
    const busy = {};
    for (const r of rows) {
      const s = Math.max(r.start, lo);
      const e = Math.min(r.end, hi);
      busy[r.w] = (busy[r.w] || 0) + (e - s) / 1e6;
    }
    console.log(`\nper-worker busy time (of ${winMs}ms window):`);
    for (const w of workerIds) {
      const b = busy[w] || 0;
      console.log(`  w${String(w).padStart(2)}: ${b.toFixed(2).padStart(7)}ms  (${(100 * b / winMs).toFixed(1)}%)`);
    }

    // ── Per-OS-thread CPU census (THE key view for off-CPU long polls) ──
    // perf only samples on-CPU threads, so a tid with samples here was running.
    // At the configured sampling Hz, ~1 sample / msPerSample of on-CPU time.
    // Even spacing => continuously on-CPU.
    const inWin = trace.cpuSamples.filter((s) => s.timestamp >= lo && s.timestamp <= hi);
    const byTid = new Map();
    let onTotal = 0;
    for (const s of inWin) {
      let g = byTid.get(s.tid);
      if (!g) { g = { on: 0, off: 0, leaves: new Map(), times: [] }; byTid.set(s.tid, g); }
      if (s.source === 1) g.off++; else { g.on++; onTotal++; }
      const leaf = leafOf(s, trace.callframeSymbols);
      g.leaves.set(leaf, (g.leaves.get(leaf) || 0) + 1);
      g.times.push(s.timestamp);
    }
    const expectedSamples = winMs / msPerSample; // per always-on thread, per window
    console.log(`\nper-tid CPU census in window (${onTotal} on-CPU samples; ~1 on-CPU samp / ${msPerSample.toFixed(1)}ms at ${hz}Hz; expected ≤${expectedSamples.toFixed(1)} samp/thread):`);
    if (expectedSamples < 3) {
      console.log(`  (window is short relative to sampling period — per-tid counts are noisy; treat absent/sparse tids as UNKNOWN, not idle)`);
    }
    if (onTotal === 0) {
      if (expectedSamples >= 3) {
        console.log("  (no on-CPU samples in window — likely the whole box was off-CPU/idle; a long poll here is probably blocked off-box: network/disk/futex)");
      } else {
        console.log("  (no on-CPU samples in window, but expected sample count is small — UNKNOWN; not enough sampling resolution to call this)");
      }
    }
    // Sort by ON-CPU samples only — off-CPU sched samples don't indicate "running".
    const tids = [...byTid.entries()].sort((a, b) => b[1].on - a[1].on);
    for (const [tid, g] of tids) {
      const estOnMs = g.on * msPerSample;
      const topLeaf = [...g.leaves.entries()].sort((a, b) => b[1] - a[1])[0];
      // gap regularity: mean of inter-sample gaps
      const ts = g.times.sort((a, b) => a - b);
      let cadence = "";
      if (ts.length >= 3) {
        const gaps = [];
        for (let i = 1; i < ts.length; i++) gaps.push((ts[i] - ts[i - 1]) / 1e6);
        const mean = gaps.reduce((a, b) => a + b, 0) / gaps.length;
        cadence = ` gaps~${mean.toFixed(1)}ms`;
      }
      console.log(`  tid=${String(tid).padStart(5)}: on=${String(g.on).padStart(3)} off=${String(g.off).padStart(3)}  ~${estOnMs.toFixed(0)}ms on-CPU${cadence}  leaf: ${topLeaf[0]}`);
    }

    // ── Queue depth across the window ──
    const qs = (spans.queueSamples || []).filter((s) => s.t >= lo && s.t <= hi);
    if (qs.length) {
      const g = qs.map((s) => s.global);
      console.log(`\nglobal queue in window: max=${Math.max(...g)} min=${Math.min(...g)} (${qs.length} samples) — >0 means work was waiting behind the long poll`);
    } else {
      console.log(`\nglobal queue in window: no samples`);
    }

    // ── Inner spans of the dominant (longest) poll, if tracing layer present ──
    const dominant = rows.filter((r) => r.durMs > 1).sort((a, b) => b.durMs - a.durMs)[0];
    if (dominant) {
      let spanData = null;
      try { spanData = buildSpanData(trace.customEvents); } catch (_) { /* no tracing layer */ }
      if (spanData && spanData.allSpans && spanData.allSpans.length) {
        const inner = spanData.allSpans.filter((s) =>
          s.segments.some((seg) => seg.workerId === dominant.w && seg.start >= dominant.start && seg.end <= dominant.end)
        );
        const by = {};
        for (const s of inner) by[s.spanName] = (by[s.spanName] || 0) + 1;
        const summary = Object.entries(by).map(([n, c]) => `${n}×${c}`).join(", ") || "(none)";
        console.log(`\ninner tracing spans of dominant ${dominant.durMs.toFixed(1)}ms poll (task ${dominant.task}): ${summary}`);
      }
    }
  }
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
