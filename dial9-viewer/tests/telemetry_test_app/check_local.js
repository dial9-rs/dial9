"use strict";

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

const uiDir = path.resolve(__dirname, "../../ui");
const { EVENT_TYPES, parseTrace, symbolizeChain } = require(path.join(
  uiDir,
  "trace_parser.js",
));
const { buildSpanData, buildWorkerSpans, enclosingSpans } = require(
  path.join(uiDir, "trace_analysis.js"),
);

const MARKER_EVENT = "TelemetryFixtureMarkerEvent";
const FIXTURE_SYMBOL = /dial9_fixture_(?:cpu|wait)_.+?_weight_[1-9][0-9]*/g;
const FIXTURE_FUNCTION =
  /dial9_fixture_(?:(?:cpu|wait)_.+?_weight_[1-9][0-9]*|mixed_[A-Za-z0-9_]+)/g;

function readSegments(paths) {
  return Buffer.concat(
    paths.map((tracePath) => {
      const bytes = fs.readFileSync(tracePath);
      return bytes[0] === 0x1f && bytes[1] === 0x8b
        ? zlib.gunzipSync(bytes)
        : bytes;
    }),
  );
}

function measurementWindow(customEvents) {
  const markers = customEvents.filter((event) => event.name === MARKER_EVENT);
  const starts = markers.filter(
    (event) => event.fields.phase === "measurement_start",
  );
  const ends = markers.filter(
    (event) => event.fields.phase === "measurement_end",
  );
  if (starts.length !== 1 || ends.length !== 1) {
    throw new Error(
      `expected one measurement_start and measurement_end, got ${starts.length} and ${ends.length}`,
    );
  }
  const start = starts[0].timestamp;
  const end = ends[0].timestamp;
  if (!(start < end)) {
    throw new Error(`measurement markers are not ordered: ${start}, ${end}`);
  }
  return { start, end };
}

function callerFirstSymbols(callchain, symbols) {
  return symbolizeChain(callchain, symbols)
    .reverse()
    .map((frame) => frame.symbol);
}

function workerIds(trace) {
  const ids = new Set();
  for (const event of trace.events) {
    if (
      event.eventType !== EVENT_TYPES.QueueSample &&
      event.eventType !== EVENT_TYPES.WakeEvent
    ) {
      ids.add(event.workerId);
    }
  }
  return [...ids].sort((left, right) => left - right);
}

function fixtureSymbols(frames) {
  const symbols = new Set();
  for (const frame of frames) {
    for (const match of frame.matchAll(FIXTURE_SYMBOL)) symbols.add(match[0]);
  }
  return symbols;
}

function fixtureFrames(frames) {
  const fixture = [];
  for (const frame of frames) {
    for (const match of frame.matchAll(FIXTURE_FUNCTION)) {
      if (fixture[fixture.length - 1] !== match[0]) fixture.push(match[0]);
    }
  }
  return fixture;
}

function fixtureSpanForStack(frames, spanNames) {
  for (let index = frames.length - 1; index >= 0; index--) {
    const prefix = "dial9_fixture_mixed_";
    if (!frames[index].startsWith(prefix)) continue;
    const spanName = `dial9_fixture_span_${frames[index].slice(prefix.length)}`;
    if (spanNames.has(spanName)) return { spanName };
  }
  return null;
}

function addStack(stacks, feature, frames) {
  const key = `${feature}\0${frames.join("\0")}`;
  const current = stacks.get(key);
  if (current != null) current.count++;
  else stacks.set(key, { feature, frames, count: 1 });
}

function addAssociations(associations, feature, frames, span) {
  if (span == null) return;
  for (const symbol of fixtureSymbols(frames)) {
    associations.add(`${feature}\0${symbol}\0${span.spanName}`);
  }
}

function observedSpans(allSpans, start, end) {
  const byId = new Map(allSpans.map((span) => [span.spanId, span]));
  const facts = new Map();
  for (const span of allSpans) {
    if (span.start < start || span.end > end) continue;
    const parent = byId.get(span.parentSpanId)?.spanName ?? null;
    const fieldNames = Object.keys(span.fields).sort();
    const key = `${span.spanName}\0${parent ?? ""}\0${fieldNames.join("\0")}`;
    facts.set(key, { name: span.spanName, parent, field_names: fieldNames });
  }
  return [...facts.values()].sort((left, right) =>
    `${left.name}\0${left.parent ?? ""}`.localeCompare(
      `${right.name}\0${right.parent ?? ""}`,
    ),
  );
}

async function observeTrace(paths) {
  const trace = await parseTrace(readSegments(paths));
  const { start, end } = measurementWindow(trace.customEvents);
  const workers = buildWorkerSpans(
    trace.events,
    workerIds(trace),
    trace.maxTs,
    trace.blockInPlaceGaps,
  ).workerSpans;
  const allSpans = buildSpanData(
    trace.customEvents,
    workers,
    trace.tidBindings,
    trace.blockInPlaceGaps,
  ).allSpans;
  const measuredSpanNames = new Set(
    allSpans
      .filter((span) => span.start >= start && span.end <= end)
      .map((span) => span.spanName),
  );

  const stacks = new Map();
  const associations = new Set();
  for (const sample of trace.cpuSamples) {
    if (sample.source !== 0 || sample.timestamp < start || sample.timestamp > end)
      continue;
    const frames = fixtureFrames(
      callerFirstSymbols(sample.callchain, trace.callframeSymbols),
    );
    if (frames.length === 0) continue;
    addStack(stacks, "cpu", frames);
    const active = enclosingSpans(allSpans, {
      timestamp: sample.timestamp,
      fields: { worker_id: sample.workerId },
    }).at(-1);
    addAssociations(associations, "cpu", frames, active);
  }

  for (const dumps of trace.taskDumps.values()) {
    for (const dump of dumps) {
      if (dump.timestamp < start || dump.timestamp > end) continue;
      const frames = fixtureFrames(
        callerFirstSymbols(dump.callchain, trace.callframeSymbols),
      );
      if (frames.length === 0) continue;
      addStack(stacks, "task_dump", frames);
      addAssociations(
        associations,
        "task_dump",
        frames,
        // Capture timestamps are poll-start timestamps and can precede the
        // nested SpanEnter; the innermost mixed fixture frame is unambiguous.
        fixtureSpanForStack(frames, measuredSpanNames),
      );
    }
  }

  return {
    stacks: [...stacks.values()].sort((left, right) =>
      `${left.feature}\0${left.frames.join("\0")}`.localeCompare(
        `${right.feature}\0${right.frames.join("\0")}`,
      ),
    ),
    spans: observedSpans(allSpans, start, end),
    associations: [...associations]
      .map((value) => {
        const [feature, symbol, activeSpan] = value.split("\0");
        return { feature, symbol, active_span: activeSpan };
      })
      .sort((left, right) =>
        `${left.feature}\0${left.symbol}\0${left.active_span}`.localeCompare(
          `${right.feature}\0${right.symbol}\0${right.active_span}`,
        ),
      ),
  };
}

async function main() {
  const paths = process.argv.slice(2);
  if (paths.length === 0) throw new Error("usage: node check_local.js TRACE...");
  process.stdout.write(`${JSON.stringify(await observeTrace(paths))}\n`);
}

if (require.main === module) {
  main().catch((error) => {
    console.error(error.stack || error);
    process.exitCode = 1;
  });
}

module.exports = { observeTrace };
