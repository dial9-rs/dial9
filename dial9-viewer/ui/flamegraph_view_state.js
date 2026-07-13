"use strict";

// flamegraph_view_state.js — serialize/restore the *interactive* state of a
// flamegraph view (zoom, inspect focus, search, filters) to and from the URL
// query string, so ANY position a user navigates to is a shareable link that
// reproduces the exact same focus for a coworker.
//
// This is deliberately separate from url_state.js (which owns the landing
// page's search/scope) and from flamegraph_diff.js's scope codec (which owns
// the server-bound aggregate scope). Those describe WHICH samples to load; this
// describes WHERE THE USER IS LOOKING once they are loaded. Keeping it here and
// dependency-free lets both the browser (via <script src>) and the unit tests
// (via require) share one source of truth.
//
// Single-flamegraph state shape (all fields optional):
//   { workerZoom, offworkerZoom, inspect, search, spawn, runtime }
//     workerZoom    : array of frame names from the worker-lane root down to the
//                     zoom target (NOT including the synthetic root). Serialized
//                     as `worker-zoom`, tab-joined — the pre-existing exact-mode
//                     key, kept verbatim so old shared links still restore.
//     offworkerZoom : same, for the off-worker lane (`offworker-zoom`). Empty in
//                     aggregated/API mode (single tree).
//     inspect       : { name, fullName } of the focused ("butterfly") frame, or
//                     null. Serialized as `inspect` (display name) plus
//                     `inspect_full` (symbol) — the latter only when it differs
//                     from the name, since that pair is what re-derives the
//                     frame's identity key (fullName || name) on restore.
//     search        : the frames search query (`search`).
//     spawn         : spawn-location filter value (`spawn`), exact mode only.
//     runtime       : runtime filter value (`runtime`), exact mode only.
//
// Diff-view state shape:
//   { zoom, search }
//     zoom   : array of frame names from the merged root down to the zoom focus,
//              INCLUDING the root as element 0 (the diff renderer's zoomPath
//              carries it). Serialized as `diff_zoom`, tab-joined.
//     search : the highlight regex (`diff_search`).
//
// A tab (	) separates path segments: it can't occur in a Rust symbol or a
// frame display name, and it matches the separator the exact-mode zoom links
// already used, so nothing here changes how a pre-existing link decodes.

(function (exports) {
  var SEP = "\t";

  // Keys this module owns in the query string. Callers that rebuild the URL
  // from scratch (API mode) delete these first, then re-add via writeState, so
  // a view key never leaks from a previous navigation.
  var STATE_KEYS = [
    "worker-zoom", "offworker-zoom",
    "inspect", "inspect_full", "search", "spawn", "runtime",
  ];
  var DIFF_STATE_KEYS = ["diff_zoom", "diff_search"];

  function splitPath(v) {
    if (!v) return [];
    // Filter out empty segments so a stray leading/trailing tab can't inject a
    // "" frame that would never match a real node.
    return v.split(SEP).filter(function (s) { return s.length > 0; });
  }

  // Parse the single-flamegraph view state out of a query string or
  // URLSearchParams. Missing/empty fields are simply absent from the result so
  // callers keep their own defaults.
  function readState(search) {
    var p = search instanceof URLSearchParams
      ? search
      : new URLSearchParams(search || "");
    var out = {};

    var wz = splitPath(p.get("worker-zoom"));
    if (wz.length) out.workerZoom = wz;
    var oz = splitPath(p.get("offworker-zoom"));
    if (oz.length) out.offworkerZoom = oz;

    var inspect = p.get("inspect");
    if (inspect) {
      // `inspect_full` is the symbol (identity key); it is only serialized when
      // it differs from the display name, so fall back to the name otherwise.
      var full = p.get("inspect_full");
      out.inspect = { name: inspect, fullName: full || inspect };
    }

    var q = p.get("search");
    if (q) out.search = q;

    var spawn = p.get("spawn");
    if (spawn) out.spawn = spawn;

    var runtime = p.get("runtime");
    if (runtime) out.runtime = runtime;

    return out;
  }

  // Write the single-flamegraph view state INTO an existing URLSearchParams,
  // mutating it in place: present fields are set, absent ones deleted. Returns
  // the same params for chaining. Deleting on absence is what keeps a URL clean
  // as the user zooms back out or exits inspect (matching the pre-existing
  // exact-mode zoom behavior).
  function writeState(params, state) {
    var s = state || {};

    setOrDelete(params, "worker-zoom", (s.workerZoom && s.workerZoom.length) ? s.workerZoom.join(SEP) : null);
    setOrDelete(params, "offworker-zoom", (s.offworkerZoom && s.offworkerZoom.length) ? s.offworkerZoom.join(SEP) : null);

    if (s.inspect && s.inspect.name) {
      params.set("inspect", s.inspect.name);
      // Only emit the symbol when it carries information beyond the name.
      if (s.inspect.fullName && s.inspect.fullName !== s.inspect.name) {
        params.set("inspect_full", s.inspect.fullName);
      } else {
        params.delete("inspect_full");
      }
    } else {
      params.delete("inspect");
      params.delete("inspect_full");
    }

    setOrDelete(params, "search", s.search || null);
    setOrDelete(params, "spawn", s.spawn || null);
    setOrDelete(params, "runtime", s.runtime || null);

    return params;
  }

  // Parse the diff-view state (zoom path incl. root, highlight regex).
  function readDiffState(search) {
    var p = search instanceof URLSearchParams
      ? search
      : new URLSearchParams(search || "");
    var out = {};
    var zoom = splitPath(p.get("diff_zoom"));
    if (zoom.length) out.zoom = zoom;
    var q = p.get("diff_search");
    if (q) out.search = q;
    return out;
  }

  function writeDiffState(params, state) {
    var s = state || {};
    setOrDelete(params, "diff_zoom", (s.zoom && s.zoom.length) ? s.zoom.join(SEP) : null);
    setOrDelete(params, "diff_search", s.search || null);
    return params;
  }

  function setOrDelete(params, key, value) {
    if (value != null && value !== "") params.set(key, value);
    else params.delete(key);
  }

  exports.readState = readState;
  exports.writeState = writeState;
  exports.readDiffState = readDiffState;
  exports.writeDiffState = writeDiffState;
  exports.STATE_KEYS = STATE_KEYS;
  exports.DIFF_STATE_KEYS = DIFF_STATE_KEYS;

  // Browser: expose on window as the stable scripting contract.
  if (typeof window !== "undefined") {
    window.Dial9FlamegraphViewState = exports;
  }
})(typeof exports === "undefined" ? {} : exports);
