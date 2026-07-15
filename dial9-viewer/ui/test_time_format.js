#!/usr/bin/env node
"use strict";

// Tests for the shared viewer time presentation helpers.
// Run with: node dial9-viewer/ui/test_time_format.js

process.env.TZ = "America/New_York";

const { test, assert, summarize } = require("./test_harness.js");
const TimeFormat = require("./time_format.js");

const instant = new Date("2026-06-22T19:19:59Z");
const ns = "1782155999000000000";

test("UTC datetime uses YYYY-MM-DD dates, a 24-hour clock, and an explicit zone", () => {
  assert.strictEqual(TimeFormat.formatDate(instant, false), "2026-06-22");
  assert.strictEqual(TimeFormat.formatTime(instant, false), "19:19:59");
  assert.strictEqual(TimeFormat.formatDateTime(instant, false), "2026-06-22 19:19:59 UTC");
});

test("local datetime uses YYYY-MM-DD dates, a 24-hour clock, and an explicit zone", () => {
  assert.strictEqual(TimeFormat.formatDate(instant, true), "2026-06-22");
  assert.strictEqual(TimeFormat.formatTime(instant, true), "15:19:59");
  assert.strictEqual(TimeFormat.formatDateTime(instant, true), "2026-06-22 15:19:59 Local");
});

test("time mode labels explicitly identify relative, UTC, and local timestamps", () => {
  assert.strictEqual(TimeFormat.timeModeLabel(true, false), "Time: Relative");
  assert.strictEqual(TimeFormat.timeModeLabel(false, false), "Time: UTC");
  assert.strictEqual(TimeFormat.timeModeLabel(false, true), "Time: Local");
});

test("picker values round-trip in UTC without local-time offset drift", () => {
  assert.strictEqual(TimeFormat.nsToPicker(ns, false), "2026-06-22T19:19:59");
  assert.strictEqual(TimeFormat.pickerToNs(TimeFormat.nsToPicker(ns, false), false), ns);
});

test("unchanged local picker values preserve the second occurrence of a DST-fold hour", () => {
  const secondOccurrenceNs = "1793514600000000000"; // 2026/11/01 01:30:00 EST
  const picker = TimeFormat.nsToPicker(secondOccurrenceNs, true);
  assert.strictEqual(picker, "2026-11-01T01:30:00");
  assert.strictEqual(
    TimeFormat.pickerToNs(picker, true),
    "1793511000000000000",
    "plain local parsing selects the first occurrence"
  );
  assert.strictEqual(
    TimeFormat.pickerToNsPreserving(picker, secondOccurrenceNs, true),
    secondOccurrenceNs,
    "an unchanged picker preserves the original instant"
  );
});

test("landing native picker uses YYYY-MM-DD order and preserves UTC/local instants", () => {
  assert.strictEqual(
    TimeFormat.dateToRangePicker(instant, false),
    "2026-06-22T19:19",
    "UTC picker uses native ISO order and a 24-hour clock"
  );
  assert.strictEqual(
    TimeFormat.dateToRangePicker(instant, true),
    "2026-06-22T15:19",
    "local picker uses native ISO order and a 24-hour clock"
  );
  assert.strictEqual(
    TimeFormat.rangePickerToDate("2026-06-22T19:19", false).toISOString(),
    "2026-06-22T19:19:00.000Z",
    "UTC picker parses as UTC"
  );
  assert.strictEqual(TimeFormat.rangePickerToDate("2026-02-30T19:19", false), null);
  assert.strictEqual(TimeFormat.rangePickerToDate("2026/06/22 19:19", false), null);
});

test("landing native picker preserves the second occurrence of a DST-fold hour across time modes", () => {
  const secondOccurrence = new Date("2026-11-01T06:30:00Z");
  const localValue = TimeFormat.dateToRangePicker(secondOccurrence, true);
  assert.strictEqual(localValue, "2026-11-01T01:30");
  const preserved = TimeFormat.rangePickerToDatePreserving(localValue, secondOccurrence, true);
  assert.strictEqual(preserved.toISOString(), "2026-11-01T06:30:00.000Z");
  assert.strictEqual(
    TimeFormat.dateToRangePicker(preserved, false),
    "2026-11-01T06:30",
    "UTC toggle retains the original second-fold instant"
  );
});

test("empty or invalid values are handled explicitly", () => {
  assert.strictEqual(TimeFormat.formatDate(new Date("invalid"), false), "");
  assert.strictEqual(TimeFormat.nsToPicker("", false), "");
  assert.strictEqual(TimeFormat.pickerToNs("", false), null);
  assert.strictEqual(TimeFormat.pickerToNs("not-a-date", false), null);
});

const fs = require("fs");
const path = require("path");
const vm = require("vm");
const FlamegraphDiff = require("./flamegraph_diff.js");

test("differential links retain local time display state", () => {
  const search = FlamegraphDiff.diffSearch(
    new URLSearchParams("api=1&bucket=a"),
    new URLSearchParams("api=1&bucket=b"),
    "local"
  );
  assert.match(search, /&tz=local$/);
  assert.strictEqual(
    FlamegraphDiff.fullScopeQuery(new URLSearchParams("api=1&bucket=a&tz=local")).get("tz"),
    "local",
    "complete aggregate scope links retain local display mode"
  );
});

test("viewer escapes non-string span field values without throwing", () => {
  const viewerSource = fs.readFileSync(path.join(__dirname, "viewer.html"), "utf8");
  const match = viewerSource.match(/function esc\(s\) \{[\s\S]*?\n            \}/);
  assert.ok(match, "viewer escape helper is present");
  const esc = vm.runInNewContext(`(${match[0]})`);
  assert.strictEqual(esc(42), "42");
  assert.strictEqual(esc("<span>"), "&lt;span&gt;");
});

test("every viewer page loads the shared formatter and exposes a time mode", () => {
  const ui = __dirname;
  const indexSource = fs.readFileSync(path.join(ui, "index.html"), "utf8");
  assert.match(
    indexSource,
    /<input type="datetime-local" id="range-from" step="60"/,
    "landing From input retains the native ISO-ordered datetime picker"
  );
  assert.match(
    indexSource,
    /selected-segment footer includes fmtTick values, so refresh it[\s\S]*?updateSelectionCount\(\);/,
    "timezone toggle refreshes the selected-segment range"
  );
  for (const page of ["index.html", "viewer.html", "flamegraph.html", "tokio_stats.html"]) {
    const source = fs.readFileSync(path.join(ui, page), "utf8");
    assert.match(source, /time_format\.js/, `${page} loads shared time formatting`);
    assert.match(source, /Time: (?:Relative|UTC)/, `${page} exposes its current time mode`);
  }
});

summarize();
