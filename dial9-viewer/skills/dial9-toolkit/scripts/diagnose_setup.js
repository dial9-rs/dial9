#!/usr/bin/env node
// dial9 setup diagnostic — detects common configuration issues in traces.
// Usage: node diagnose_setup.js <trace.bin or directory>
//
// Checks:
//   1. Missing frame pointers (shallow stacks)
//   2. Missing wake events (tasks not instrumented)
//   3. Missing debug symbols (no source locations)
//   4. No scheduling events (informational)
//   5. Recorded without tokio_unstable (poll events cover only dial9 spawns)
//   6. Runtime activity but no task polls (Runtime::block_on instead of dial9::block_on)
"use strict";

const fs = require('fs');
const path = require('path');

function resolve(name) {
  const sibling = path.resolve(__dirname, name);
  if (fs.existsSync(sibling)) return sibling;
  const toolkit = path.resolve(__dirname, '..', '..', 'dial9-toolkit', 'scripts', name);
  if (fs.existsSync(toolkit)) return toolkit;
  return path.resolve(__dirname, '..', '..', '..', 'ui', name);
}

const { parseTrace, EVENT_TYPES } = require(resolve('trace_parser.js'));

async function diagnoseSetup(tracePath) {
  const findings = [];
  let totalOnCpu = 0, totalOffCpu = 0;
  let callchainDepths = [];
  let totalSymbols = 0, symbolsWithLocation = 0;
  let totalWakeEvents = 0, totalTaskSpawns = 0;
  let totalPollStarts = 0, totalUnparks = 0;
  let uniqueAddresses = new Set();
  let builtWithoutTokioUnstable = false;

  for await (const trace of parseTrace(tracePath)) {
    // How the trace was built. The fix for thin poll data is a build flag, so
    // this check reads the build fact rather than the coverage it implies.
    // Absent on traces written before the key existed, which all had the hooks.
    if (trace.segmentMetadata?.get('tokio.unstable') === 'false') {
      builtWithoutTokioUnstable = true;
    }

    // Wake events
    totalWakeEvents += trace.events.filter(e => e.eventType === EVENT_TYPES.WakeEvent).length;
    totalTaskSpawns += trace.taskSpawnTimes.size;

    // Runtime activity vs task polls
    for (const e of trace.events) {
      if (e.eventType === EVENT_TYPES.PollStart) totalPollStarts++;
      else if (e.eventType === EVENT_TYPES.WorkerUnpark) totalUnparks++;
    }

    // CPU samples
    for (const s of trace.cpuSamples) {
      if (s.source === 0) {
        totalOnCpu++;
        callchainDepths.push(s.callchain.length);
        for (const addr of s.callchain) uniqueAddresses.add(addr);
      } else {
        totalOffCpu++;
      }
    }

    // Symbols
    for (const [addr, entry] of trace.callframeSymbols) {
      totalSymbols++;
      const e = Array.isArray(entry) ? entry[0] : entry;
      if (e && e.location) symbolsWithLocation++;
    }
  }

  // ── Check 1: Missing frame pointers ──
  if (totalOnCpu > 10) {
    const avgDepth = callchainDepths.reduce((a, b) => a + b, 0) / callchainDepths.length;
    if (avgDepth < 3) {
      findings.push({
        severity: 'critical',
        check: 'missing-frame-pointers',
        message: `CPU stack traces are only ${avgDepth.toFixed(1)} frames deep on average (expected 10+). Frame pointers are not enabled.`,
        fix: `Add to .cargo/config.toml:

[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]

Then rebuild your application. This enables the compiler to emit frame pointer instructions,
which dial9 uses to walk the call stack during CPU profiling.`,
      });
    }
  }

  // ── Check 2: Missing wake events ──
  if (totalTaskSpawns > 0 && totalWakeEvents === 0) {
    findings.push({
      severity: 'warning',
      check: 'missing-wake-events',
      message: `${totalTaskSpawns} tasks were spawned but 0 wake events were recorded. Tasks are not instrumented.`,
      fix: `Use dial9's spawn() instead of tokio::spawn() to instrument tasks:

use dial9_tokio_telemetry::telemetry::spawn;

spawn(async { /* your task */ });

Wake events let dial9 measure scheduling delays (time between Waker::wake()
and the task actually being polled). Without them, you cannot diagnose
whether tasks are waiting too long in the queue.`,
    });
  }

  // ── Check 3: Missing debug symbols ──
  if (totalOnCpu > 10 && uniqueAddresses.size > 0) {
    const resolutionRate = totalSymbols / uniqueAddresses.size;
    if (totalSymbols === 0 || (resolutionRate < 0.1 && symbolsWithLocation === 0)) {
      findings.push({
        severity: 'warning',
        check: 'missing-debug-symbols',
        message: `Only ${totalSymbols} symbols resolved out of ${uniqueAddresses.size} unique addresses (${symbolsWithLocation} with source locations). Debug symbols are missing.`,
        fix: `Ensure your release profile includes debug info. In Cargo.toml:

[profile.release]
debug = "line-tables-only"   # minimal size overhead, enough for dial9
strip = false                # do NOT strip symbols

Do NOT pass -C strip=symbols in RUSTFLAGS for builds you want to profile.
Debug info is needed for dial9 to resolve stack addresses to function names
and source locations. Without it, CPU profiles show only hex addresses.`,
      });
    }
  }

  // ── Check 4: No scheduling events (informational) ──
  if (totalOnCpu > 0 && totalOffCpu === 0) {
    findings.push({
      severity: 'info',
      check: 'no-scheduling-events',
      message: `No off-CPU scheduling samples found. Schedule profiling is not enabled or not available.`,
      fix: `To enable scheduling event capture:

1. Set perf_event_paranoid <= 1:
   sudo sysctl kernel.perf_event_paranoid=1

2. Enable sched events in your dial9 config:
   use dial9_tokio_telemetry::telemetry::cpu_profile::SchedEventConfig;

   Dial9Config::builder()
       .with_runtime(|r| r.with_sched_events(SchedEventConfig::default()))
       // ...

   Or via environment variable:
   DIAL9_SCHEDULE_PROFILE_ENABLED=true

Scheduling events show stack traces when the kernel deschedules your worker
threads. This reveals blocking calls (file I/O, DNS, mutexes) that should
use spawn_blocking instead of running on the async runtime.`,
    });
  }

  // ── Check 5: Recorded without tokio_unstable ──
  if (builtWithoutTokioUnstable) {
    findings.push({
      severity: 'warning',
      check: 'no-tokio-unstable',
      message: `This trace was recorded without --cfg tokio_unstable. Poll events cover only tasks spawned through dial9's own helpers, and task lifetimes and per-worker queue depth are unavailable.`,
      fix: `Add to .cargo/config.toml:

[build]
rustflags = ["--cfg", "tokio_unstable", "-C", "force-frame-pointers=yes"]

Then rebuild. Tokio gates its poll and task hooks behind this flag; without it
dial9 only sees tasks spawned through its own helpers (spawn, spawn_in,
block_on, spawn_with).`,
    });
  }

  // ── Check 6: Runtime woke up repeatedly but never polled a task ──
  // An idle runtime parks and stays parked; repeated unparks mean it had work.
  // Skipped without tokio_unstable, where check 5 already explains the gap.
  if (!builtWithoutTokioUnstable && totalUnparks > 10 && totalPollStarts === 0) {
    findings.push({
      severity: 'warning',
      check: 'no-task-polls',
      message: `The runtime unparked ${totalUnparks} times but recorded 0 task polls. Its work is running outside any task.`,
      fix: `Drive the runtime with dial9's block_on instead of Runtime::block_on:

use dial9::block_on;

block_on(&runtime, async { /* your work */ });

Poll and wake events come from Tokio's per-task hooks. Runtime::block_on polls
its future on the calling thread, outside any task, so that future and anything
awaited inline under it never reach the trace. dial9::block_on spawns it first.
#[dial9::main] already does this.`,
    });
  }

  // ── Report ──
  console.log(`\n${'='.repeat(60)}`);
  console.log('DIAL9 SETUP DIAGNOSTIC');
  console.log(`${'='.repeat(60)}`);
  console.log(`CPU samples: ${totalOnCpu} on-CPU, ${totalOffCpu} off-CPU`);
  console.log(`Tasks spawned: ${totalTaskSpawns}, Wake events: ${totalWakeEvents}, Task polls: ${totalPollStarts}`);
  console.log(`Worker unparks: ${totalUnparks}`);
  console.log(`Symbols: ${totalSymbols} resolved (${symbolsWithLocation} with source locations)`);
  console.log();

  if (findings.length === 0) {
    console.log('✅ No setup issues detected');
  } else {
    const icons = { critical: '🔴', warning: '🟡', info: 'ℹ️' };
    for (const f of findings) {
      console.log(`${icons[f.severity]} [${f.check}] ${f.message}`);
      console.log();
      console.log('  Fix:');
      for (const line of f.fix.split('\n')) {
        console.log('  ' + line);
      }
      console.log();
    }
    const crit = findings.filter(f => f.severity === 'critical').length;
    const warn = findings.filter(f => f.severity === 'warning').length;
    const info = findings.filter(f => f.severity === 'info').length;
    console.log(`${crit} critical, ${warn} warnings, ${info} info`);
  }

  return findings;
}

if (require.main === module) {
  const tracePath = process.argv[2];
  if (!tracePath) {
    console.error('Usage: node diagnose_setup.js <trace.bin or directory>');
    process.exit(1);
  }
  diagnoseSetup(tracePath).catch(err => { console.error(err); process.exit(1); });
}

module.exports = { diagnoseSetup };
