#!/usr/bin/env node
"use strict";

// Unit tests for the SSE frame decoder in sse.js: the pure, incremental
// `SseDecoder.push`/`flush` and `parseFrame`. These cover the tricky bits —
// frames split across chunk boundaries, multiple frames in one chunk, CRLF
// normalization, comment/keep-alive frames, and multi-line data — without a
// network or DOM.

const { SseDecoder, parseFrame } = require("./sse.js");

let passed = 0;
let failed = 0;

function assertDeepEq(actual, expected, desc) {
  if (JSON.stringify(actual) === JSON.stringify(expected)) {
    console.log(`✓ ${desc}`);
    passed++;
  } else {
    console.log(
      `✗ ${desc}\n    expected: ${JSON.stringify(expected)}\n    actual:   ${JSON.stringify(actual)}`,
    );
    failed++;
  }
}

// ── parseFrame ──
assertDeepEq(parseFrame("data: {\"a\":1}"), '{"a":1}', "single data line, leading space stripped");
assertDeepEq(parseFrame("data:{\"a\":1}"), '{"a":1}', "no space after colon");
assertDeepEq(parseFrame("data: a\ndata: b"), "a\nb", "multi-line data joined with \\n");
assertDeepEq(parseFrame(": keep-alive"), null, "comment frame yields no data");
assertDeepEq(parseFrame("event: ping\nid: 5"), null, "event/id without data yields null");
assertDeepEq(
  parseFrame("event: msg\ndata: hello"),
  "hello",
  "event: line is ignored, data still parsed",
);

// ── SseDecoder: one frame per push ──
{
  const d = new SseDecoder();
  assertDeepEq(d.push("data: {\"x\":1}\n\n"), ['{"x":1}'], "one complete frame");
  assertDeepEq(d.push(""), [], "empty push yields nothing");
}

// ── multiple frames in one chunk ──
{
  const d = new SseDecoder();
  assertDeepEq(
    d.push("data: 1\n\ndata: 2\n\ndata: 3\n\n"),
    ["1", "2", "3"],
    "three frames in one chunk",
  );
}

// ── frame split across chunk boundaries ──
{
  const d = new SseDecoder();
  assertDeepEq(d.push("data: {\"big\":"), [], "partial frame buffers, emits nothing");
  assertDeepEq(d.push(" true}"), [], "still no frame terminator");
  assertDeepEq(d.push("\n\n"), ['{"big": true}'], "terminator completes the buffered frame");
}

// ── boundary split across the \n\n itself ──
{
  const d = new SseDecoder();
  assertDeepEq(d.push("data: a\n"), [], "first newline of terminator buffered");
  assertDeepEq(d.push("\ndata: b\n\n"), ["a", "b"], "second newline completes frame a, then b");
}

// ── CRLF normalization ──
{
  const d = new SseDecoder();
  assertDeepEq(d.push("data: a\r\n\r\n"), ["a"], "CRLF frame terminator handled");
}

// ── keep-alive comment between data frames ──
{
  const d = new SseDecoder();
  assertDeepEq(
    d.push("data: 1\n\n: keep-alive\n\ndata: 2\n\n"),
    ["1", "2"],
    "keep-alive comment frame skipped, data frames pass through",
  );
}

// ── flush emits a trailing unterminated frame ──
{
  const d = new SseDecoder();
  assertDeepEq(d.push("data: last"), [], "unterminated frame buffers");
  assertDeepEq(d.flush(), ["last"], "flush emits the trailing frame at EOF");
  assertDeepEq(d.flush(), [], "flush is idempotent once drained");
}

// ── flush with nothing buffered ──
{
  const d = new SseDecoder();
  assertDeepEq(d.flush(), [], "flush on empty decoder yields nothing");
}

// ── realistic: chunk boundary lands mid-JSON across two events ──
{
  const d = new SseDecoder();
  const out = [];
  out.push(...d.push('data: {"files_folded":1}\n\ndata: {"files_fol'));
  out.push(...d.push('ded":2}\n\n'));
  assertDeepEq(out, ['{"files_folded":1}', '{"files_folded":2}'], "split mid-second-event");
}

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
