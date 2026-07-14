"use strict";

// Unit tests for flamegraph_view_state.js — the URL codec that is the single
// source of truth for serializing a flamegraph's interactive view state (zoom /
// inspect / search / filters, and the diff view's zoom + highlight). All the
// consuming code (flamegraph.html, the renderer's getViewState/applyViewState,
// the diff view) goes through this module, so these tests pin the wire format:
// exact key names, the tab separator, delete-on-absence, and the round-trip.

const { assert, test, summarize } = require("./test_harness.js");
const VS = require("./flamegraph_view_state.js");

const SEP = "\t";

// --- Single-flamegraph state ---

test("readState: empty query yields empty state", () => {
  assert.deepStrictEqual(VS.readState(""), {});
  assert.deepStrictEqual(VS.readState(new URLSearchParams()), {});
});

test("writeState → readState round-trips a full state", () => {
  const state = {
    workerZoom: ["a", "mid", "leaf"],
    offworkerZoom: ["park"],
    inspect: { name: "poll", fullName: "core::poll::poll" },
    search: "tokio",
    spawn: "src/main.rs:10",
    runtime: "app",
  };
  const p = VS.writeState(new URLSearchParams(), state);
  // Exact wire keys.
  assert.strictEqual(p.get("worker-zoom"), "a\tmid\tleaf");
  assert.strictEqual(p.get("offworker-zoom"), "park");
  assert.strictEqual(p.get("inspect"), "poll");
  assert.strictEqual(p.get("inspect_full"), "core::poll::poll");
  assert.strictEqual(p.get("search"), "tokio");
  assert.strictEqual(p.get("spawn"), "src/main.rs:10");
  assert.strictEqual(p.get("runtime"), "app");
  // Decodes back to the same structure.
  assert.deepStrictEqual(VS.readState(p), state);
});

test("inspect_full is omitted when it equals the display name", () => {
  const p = VS.writeState(new URLSearchParams(), {
    inspect: { name: "leaf", fullName: "leaf" },
  });
  assert.strictEqual(p.get("inspect"), "leaf");
  assert.strictEqual(p.get("inspect_full"), null, "no redundant inspect_full");
  // On read, fullName falls back to the display name.
  assert.deepStrictEqual(VS.readState(p).inspect, { name: "leaf", fullName: "leaf" });
});

test("writeState deletes keys for absent fields (URL stays clean on zoom-out)", () => {
  // Start from a URL that HAS every view key set...
  const p = new URLSearchParams();
  p.set("worker-zoom", "a" + SEP + "b");
  p.set("offworker-zoom", "x");
  p.set("inspect", "poll");
  p.set("inspect_full", "core::poll");
  p.set("search", "q");
  p.set("spawn", "s");
  p.set("runtime", "r");
  // ...then write an empty state: every owned key must be removed.
  VS.writeState(p, {});
  for (const k of VS.STATE_KEYS) {
    assert.strictEqual(p.get(k), null, `${k} deleted on absence`);
  }
});

test("writeState preserves foreign keys it does not own", () => {
  const p = new URLSearchParams();
  p.set("api", "1");
  p.set("bucket", "my-bucket");
  VS.writeState(p, { search: "q" });
  assert.strictEqual(p.get("api"), "1", "foreign key untouched");
  assert.strictEqual(p.get("bucket"), "my-bucket", "foreign key untouched");
  assert.strictEqual(p.get("search"), "q");
});

test("writeState clears inspect_full when only a name is present", () => {
  const p = new URLSearchParams();
  p.set("inspect_full", "stale::symbol");
  VS.writeState(p, { inspect: { name: "leaf", fullName: "leaf" } });
  assert.strictEqual(p.get("inspect"), "leaf");
  assert.strictEqual(p.get("inspect_full"), null, "stale inspect_full cleared");
});

test("readState ignores empty inspect / inspect with no name", () => {
  assert.strictEqual(VS.readState("inspect=").inspect, undefined);
  // A name-less write produces nothing to read back.
  const p = VS.writeState(new URLSearchParams(), { inspect: { name: "", fullName: "x" } });
  assert.strictEqual(p.get("inspect"), null);
});

test("zoom path split filters stray empty segments (leading/trailing tab)", () => {
  const p = new URLSearchParams();
  p.set("worker-zoom", SEP + "a" + SEP + SEP + "b" + SEP);
  assert.deepStrictEqual(VS.readState(p).workerZoom, ["a", "b"]);
});

test("readState accepts a raw query string or URLSearchParams", () => {
  const fromStr = VS.readState("search=hello&worker-zoom=a" + encodeURIComponent(SEP) + "b");
  assert.strictEqual(fromStr.search, "hello");
  assert.deepStrictEqual(fromStr.workerZoom, ["a", "b"]);
});

// --- Diff-view state ---

test("writeDiffState → readDiffState round-trips (root-inclusive zoom)", () => {
  const state = { zoom: ["(all)", "runtime", "poll"], search: "spawn" };
  const p = VS.writeDiffState(new URLSearchParams(), state);
  assert.strictEqual(p.get("diff_zoom"), "(all)\truntime\tpoll");
  assert.strictEqual(p.get("diff_search"), "spawn");
  assert.deepStrictEqual(VS.readDiffState(p), state);
});

test("writeDiffState deletes its keys on absence", () => {
  const p = new URLSearchParams();
  p.set("diff_zoom", "(all)" + SEP + "x");
  p.set("diff_search", "q");
  VS.writeDiffState(p, {});
  for (const k of VS.DIFF_STATE_KEYS) {
    assert.strictEqual(p.get(k), null, `${k} deleted on absence`);
  }
});

test("diff and single state keys are disjoint namespaces", () => {
  const p = new URLSearchParams();
  VS.writeState(p, { search: "single" });
  VS.writeDiffState(p, { search: "diff" });
  assert.strictEqual(p.get("search"), "single");
  assert.strictEqual(p.get("diff_search"), "diff");
});

summarize();
