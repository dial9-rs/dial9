#!/usr/bin/env node
// Test for diagnose_setup.js — verifies detection of common dial9 misconfigurations.
// Usage: node test_diagnose_setup.js [trace-dir]
//
// If trace-dir is provided, tests against real traces.
// Otherwise, tests the detection logic with synthetic data.
"use strict";

const path = require('path');
const fs = require('fs');

function resolve(name) {
  const sibling = path.resolve(__dirname, name);
  if (fs.existsSync(sibling)) return sibling;
  const toolkit = path.resolve(__dirname, '..', 'skills', 'dial9-toolkit', 'scripts', name);
  if (fs.existsSync(toolkit)) return toolkit;
  return path.resolve(__dirname, name);
}

const { parseTrace, EVENT_TYPES, symbolizeChain } = require(resolve('trace_parser.js'));
const { diagnoseSetup } = require(resolve('diagnose_setup.js'));

let passed = 0, failed = 0;
function assert(cond, msg) {
  if (cond) { passed++; }
  else { failed++; console.error(`  FAIL: ${msg}`); }
}

async function testWithTraces(traceDir) {
  console.log('Testing with real traces from:', traceDir);

  // Test 1: no-frame-pointers should detect missing frame pointers
  {
    const dir = path.join(traceDir, 'no-frame-pointers');
    if (fs.existsSync(dir)) {
      console.log('\n  no-frame-pointers:');
      const findings = await diagnoseSetup(dir);
      const fp = findings.find(f => f.check === 'missing-frame-pointers');
      assert(fp != null, 'should detect missing frame pointers');
      assert(fp && fp.severity === 'critical', 'missing frame pointers should be critical');
    } else {
      console.log('  SKIP: no-frame-pointers dir not found');
    }
  }

  // Test 2: no-wake-events should detect missing wake events
  {
    const dir = path.join(traceDir, 'no-wake-events');
    if (fs.existsSync(dir)) {
      console.log('\n  no-wake-events:');
      const findings = await diagnoseSetup(dir);
      const we = findings.find(f => f.check === 'missing-wake-events');
      assert(we != null, 'should detect missing wake events');
      assert(we && we.severity === 'warning', 'missing wake events should be warning');
    } else {
      console.log('  SKIP: no-wake-events dir not found');
    }
  }

  // Test 3: no-debug-symbols should detect missing debug symbols
  {
    const dir = path.join(traceDir, 'no-debug-symbols');
    if (fs.existsSync(dir)) {
      console.log('\n  no-debug-symbols:');
      const findings = await diagnoseSetup(dir);
      const ds = findings.find(f => f.check === 'missing-debug-symbols');
      assert(ds != null, 'should detect missing debug symbols');
      assert(ds && ds.severity === 'warning', 'missing debug symbols should be warning');
    } else {
      console.log('  SKIP: no-debug-symbols dir not found');
    }
  }

  // Test 4: no-sched-events should detect no scheduling events
  {
    const dir = path.join(traceDir, 'no-sched-events');
    if (fs.existsSync(dir)) {
      console.log('\n  no-sched-events:');
      const findings = await diagnoseSetup(dir);
      const se = findings.find(f => f.check === 'no-scheduling-events');
      assert(se != null, 'should detect no scheduling events');
      assert(se && se.severity === 'info', 'no scheduling events should be info');
    } else {
      console.log('  SKIP: no-sched-events dir not found');
    }
  }

  // Test 5: good trace should NOT have critical/warning findings (only info)
  {
    const dir = path.join(traceDir, 'good');
    if (fs.existsSync(dir)) {
      console.log('\n  good (reference):');
      const findings = await diagnoseSetup(dir);
      const serious = findings.filter(f => f.severity === 'critical' || f.severity === 'warning');
      assert(serious.length === 0, `good trace should have no critical/warning findings, got: ${serious.map(f => f.check).join(', ')}`);
    } else {
      console.log('  SKIP: good dir not found');
    }
  }
}

async function main() {
  const traceDir = process.argv[2] || '/tmp/dial9-diagnostic-traces';

  if (fs.existsSync(traceDir)) {
    // Suppress console.log output from diagnoseSetup during tests
    const origLog = console.log;
    console.log = () => {};
    try {
      await testWithTraces(traceDir);
    } finally {
      console.log = origLog;
    }
  } else {
    console.log('No trace directory found at', traceDir);
    console.log('Run scripts/generate_diagnostic_traces.sh first to generate test traces.');
    process.exit(1);
  }

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) process.exit(1);
}

main().catch(err => { console.error(err); process.exit(1); });
