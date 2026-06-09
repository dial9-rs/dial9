"use strict";

const assert = require("assert");

let passed = 0, failed = 0;

function test(name, fn) {
  try {
    fn();
    console.log(`✓ ${name}`);
    passed++;
  } catch (e) {
    console.error(`✗ ${name}: ${e.message}`);
    failed++;
  }
}

function testAsync(name, fn) {
  return fn().then(() => {
    console.log(`✓ ${name}`);
    passed++;
  }).catch(e => {
    console.error(`✗ ${name}: ${e.message}`);
    failed++;
  });
}

function summarize() {
  console.log(`\n${failed === 0 ? "All tests passed" : `${failed} test(s) FAILED`}`);
  process.exit(failed === 0 ? 0 : 1);
}

module.exports = { assert, test, testAsync, summarize };
