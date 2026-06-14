#!/usr/bin/env node
// Diagnose the ROOT CAUSE of a long poll: not just "where" but "why it was long".
//
// The method this automates (see SKILL.md for the reasoning):
//   1. Find the target poll (longest by default, or --task <id>).
//   2. Classify it: on-CPU (doing work) vs off-CPU (parked, waiting).
//   3. For off-CPU polls, run a per-OS-thread CPU census over the poll's
//      window — perf only samples ON-CPU threads, so whatever WAS running is
//      the suspect that held the waiter back.
//      - A thread pinned on-CPU the whole window (samples evenly spaced at
//        ~10ms / 99Hz) is the likely blocker. Read its stack.
//      - No samples anywhere => the box was idle => the poll blocked OFF-BOX
//        (network/disk/futex with no local holder).
//   4. Corroborate: does that same activity coincide with the OTHER long polls
//      in the trace? A cause that explains one spike and recurs is real; a
//      one-off coincidence is not.
//
// Usage:
//   node diagnose_long_poll.js <trace.bin|dir> [--task <id>] [--min-ms 8] [--top 1] [--hz 99]
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
const { buildWorkerSpans, attachCpuSamples } = require(resolve("trace_analysis.js"));

function argVal(flag, def) {
  const i = process.argv.indexOf(flag);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : def;
}

function stackOf(sample, symbols, n = 12) {
  return symbolizeChain(sample.callchain, symbols).slice(0, n).map((f) => formatFrame(f).text);
}

function percentile(sortedAsc, p) {
  if (!sortedAsc.length) return 0;
  const idx = Math.min(sortedAsc.length - 1, Math.floor((p / 100) * sortedAsc.length));
  return sortedAsc[idx];
}

async function main() {
  const dir = process.argv[2];
  const onlyTask = argVal("--task", null);
  // Threshold is RELATIVE to this runtime's own poll distribution by default.
  // "Long" is not an absolute number — a 1ms poll is a severe outlier in a
  // service whose p99 poll is 500µs, and noise in one whose p99 is 40ms.
  // Default: max(N× p99, the floor). Override the multiple with --pctl-mult,
  // or pin an absolute threshold with --min-ms (which then wins).
  const absMinMs = argVal("--min-ms", null);          // absolute override, ms
  const pctlMult = parseFloat(argVal("--pctl-mult", "3")); // N× p99
  const floorMs = parseFloat(argVal("--floor-ms", "1"));   // ignore sub-ms noise
  const top = parseInt(argVal("--top", "1"), 10);
  const hz = parseFloat(argVal("--hz", "99"));             // perf sampling frequency
  const msPerSample = 1000 / hz;
  if (!dir) {
    console.error("usage: node diagnose_long_poll.js <trace.bin|dir> [--task <id>] [--min-ms <abs>] [--pctl-mult 3] [--floor-ms 1] [--top 1] [--hz 99]");
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

    // Build this runtime's poll-duration distribution to calibrate "long".
    const allDurMs = [];
    for (const w of workerIds) {
      for (const p of spans.workerSpans[w].polls) allDurMs.push((p.end - p.start) / 1e6);
    }
    allDurMs.sort((a, b) => a - b);
    const p50 = percentile(allDurMs, 50);
    const p99 = percentile(allDurMs, 99);
    const minMs = absMinMs != null
      ? parseFloat(absMinMs)
      : Math.max(floorMs, pctlMult * p99);
    console.log(`poll distribution: p50=${(p50 * 1000).toFixed(0)}µs p99=${(p99 * 1000).toFixed(0)}µs max=${allDurMs.length ? allDurMs[allDurMs.length - 1].toFixed(2) : 0}ms (${allDurMs.length} polls)`);
    console.log(`threshold for "long": ${minMs.toFixed(2)}ms ${absMinMs != null ? "(absolute, --min-ms)" : `(${pctlMult}× p99, floor ${floorMs}ms)`}`);

    // Collect candidate long polls.
    let polls = [];
    for (const w of workerIds) {
      for (const p of spans.workerSpans[w].polls) {
        const durMs = (p.end - p.start) / 1e6;
        if (durMs >= minMs && (!onlyTask || String(p.taskId) === String(onlyTask))) {
          polls.push({ ...p, w, durMs, p99 });
        }
      }
    }
    polls.sort((a, b) => b.durMs - a.durMs);
    if (!polls.length) {
      console.log(`No polls >= ${minMs.toFixed(2)}ms${onlyTask ? ` for task ${onlyTask}` : ""}.`);
      continue;
    }

    const targets = polls.slice(0, top);
    for (const tp of targets) {
      const lo = tp.start, hi = tp.end;
      const onCpu = (tp.cpuSamples || []).length;
      const offCpu = (tp.schedSamples || []).length;
      const xP99 = tp.p99 > 0 ? ` (${(tp.durMs / tp.p99).toFixed(0)}× this runtime's p99)` : "";
      console.log(`\n${"═".repeat(70)}`);
      console.log(`LONG POLL  task=${tp.taskId}  worker=${tp.w}  dur=${tp.durMs.toFixed(2)}ms${xP99}  [+${((lo - min) / 1e6).toFixed(2)} .. +${((hi - min) / 1e6).toFixed(2)}]ms`);
      console.log(`spawn: ${(tp.spawnLoc || "?").replace(/.*registry\/src\/[^/]+\//, "")}`);

      // ── Step 2: classify ──
      if (onCpu > 0) {
        console.log(`\nCLASSIFICATION: ON-CPU (${onCpu} samples) — the future is doing synchronous work.`);
        console.log(`Fix lives in the spawn location's code: move to spawn_blocking, add yield_now, or optimize the hot path.`);
        console.log(`Hottest stacks during this poll:`);
        const groups = new Map();
        for (const s of tp.cpuSamples) {
          const st = stackOf(s, trace.callframeSymbols);
          const key = st.slice(0, 4).join(" < ");
          let g = groups.get(key);
          if (!g) { g = { count: 0, stack: st }; groups.set(key, g); }
          g.count++;
        }
        for (const [, g] of [...groups.entries()].sort((a, b) => b[1].count - a[1].count).slice(0, 3)) {
          console.log(`  ${g.count}× ${g.stack.join(" < ")}`);
        }
        continue;
      }

      console.log(`\nCLASSIFICATION: OFF-CPU${offCpu > 0 ? ` (${offCpu} sched samples)` : " (no samples)"} — the worker was descheduled by the kernel inside this poll.`);
      if (offCpu > 0) {
        console.log(`Off-CPU sched stacks ARE present — read them directly (this is the blocking syscall/lock):`);
        const groups = new Map();
        for (const s of tp.schedSamples) {
          const st = stackOf(s, trace.callframeSymbols);
          const key = st.slice(0, 4).join(" < ");
          let g = groups.get(key);
          if (!g) { g = { count: 0, stack: st }; groups.set(key, g); }
          g.count++;
        }
        for (const [, g] of [...groups.entries()].sort((a, b) => b[1].count - a[1].count).slice(0, 3)) {
          console.log(`  ${g.count}× ${g.stack.join(" < ")}`);
        }
      }

      // ── Step 3: per-tid CPU census of the window (works WITHOUT sched events) ──
      const inWin = trace.cpuSamples.filter((s) => s.source !== 1 && s.timestamp >= lo && s.timestamp <= hi && s.tid !== tp.tid);
      const byTid = new Map();
      for (const s of inWin) {
        let g = byTid.get(s.tid);
        if (!g) { g = { times: [], leaves: new Map() }; byTid.set(s.tid, g); }
        g.times.push(s.timestamp);
        const st = stackOf(s, trace.callframeSymbols);
        const key = st.join(" < ");
        let lg = g.leaves.get(key);
        if (!lg) { lg = { count: 0, stack: st }; g.leaves.set(key, lg); }
        lg.count++;
      }
      const expectedSamples = tp.durMs / msPerSample; // per always-on thread over the poll window
      console.log(`\nWHO ELSE WAS ON-CPU during this ${tp.durMs.toFixed(0)}ms (per-tid census; ~${msPerSample.toFixed(1)}ms on-CPU per sample at ${hz}Hz; expected ≤${expectedSamples.toFixed(1)} samp/thread):`);
      if (expectedSamples < 3) {
        console.log(`  NOTE: poll is short relative to the sampling period — per-tid counts are noisy.`);
        console.log(`        A thread on-CPU for the full window may still produce 0 samples. Treat absences as UNKNOWN.`);
      }
      if (byTid.size === 0) {
        if (expectedSamples >= 3) {
          console.log(`  *** NO ON-CPU SAMPLES from any other thread. The box looks off-CPU/idle while this poll waited. ***`);
          console.log(`  => No in-process holder. This poll is likely blocked OFF-BOX: a network or disk syscall,`);
          console.log(`     or a futex with the owner also parked. Look at the spawn location's await: it's`);
          console.log(`     waiting on an external round-trip (RPC/DB/journal) or the next inbound bytes.`);
        } else {
          console.log(`  *** UNKNOWN: no on-CPU samples, but the poll is too short for the sampler to confirm an idle box. ***`);
          console.log(`  => Cannot distinguish "blocked off-box" from "sampler missed a brief on-CPU holder".`);
          console.log(`     Re-run with --hz higher, or examine adjacent longer polls.`);
        }
        continue;
      }
      const ranked = [...byTid.entries()].map(([tid, g]) => {
        const estMs = g.times.length * msPerSample;
        const coverage = Math.min(1, estMs / tp.durMs);
        return { tid, samples: g.times.length, estMs, coverage, leaves: g.leaves };
      }).sort((a, b) => b.samples - a.samples);

      // Only call out a thread as "pinned" when there were enough expected samples
      // to make the coverage claim meaningful. Below ~3 samples the inference is noise.
      const canTrustPinned = expectedSamples >= 3;
      for (const r of ranked.slice(0, 5)) {
        const pinned = canTrustPinned && r.coverage >= 0.6 && r.samples >= 3;
        console.log(`  tid=${r.tid}: ${r.samples} samples ≈ ${r.estMs.toFixed(0)}ms on-CPU (${(r.coverage * 100).toFixed(0)}% of the poll)${pinned ? "  <<< PINNED — likely the blocker" : ""}`);
        const topLeaf = [...r.leaves.values()].sort((a, b) => b.count - a.count)[0];
        console.log(`        ${topLeaf.stack.slice(0, 6).join(" < ")}`);
      }

      // ── Step 4: corroborate by correlation across the other long polls ──
      const suspect = ranked[0];
      if (suspect && canTrustPinned && suspect.coverage >= 0.6 && suspect.samples >= 3) {
        const suspectLeaf = [...suspect.leaves.values()].sort((a, b) => b.count - a.count)[0].stack[0];
        // For every OTHER long poll, was the suspect's leaf on-CPU inside its window?
        const others = polls.filter((p) => p !== tp);
        let coincided = 0;
        for (const o of others) {
          const hit = trace.cpuSamples.some((s) =>
            s.source !== 1 && s.timestamp >= o.start && s.timestamp <= o.end &&
            stackOf(s, trace.callframeSymbols).some((f) => f === suspectLeaf)
          );
          if (hit) coincided++;
        }
        console.log(`\nCORROBORATION: suspect leaf "${suspectLeaf}"`);
        console.log(`  coincides with ${coincided}/${others.length} of the other long polls (>=${minMs.toFixed(2)}ms) in this trace.`);
        console.log(coincided > 0
          ? `  Recurring co-occurrence within this trace supports a causal link, not a coincidence.`
          : `  Did not recur within this trace. That is EXPECTED for an infrequent actor\n` +
            `  (e.g. a once-per-60s flusher causes one spike per 60s segment). Corroborate\n` +
            `  ACROSS trace segments/hosts before dismissing — see the SKILL.md caveat.`);
      }
    }
  }
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
