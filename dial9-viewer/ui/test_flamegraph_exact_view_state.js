"use strict";

// Exact-trace view-state round-trip: search + spawn-location filter. The
// deep-link test uses setTreeDirect (aggregated/API mode), where the spawn and
// runtime filters are hidden and inapplicable. This test drives the OTHER path —
// setData() with raw samples — so getViewState/applyViewState are exercised for
// search and the spawn filter, including the ordering rule that a filter rebuilds
// the trees BEFORE zoom/inspect restore.
//
// The repo has no jsdom, so — like test_flamegraph_inspect_dom.js and
// test_flamegraph_deeplink.js — we install a minimal DOM stub.

const { assert, test, summarize } = require("./test_harness.js");

function makeCtx() {
  return {
    scale() {}, fillRect() {}, save() {}, restore() {}, beginPath() {},
    rect() {}, clip() {}, fillText() {}, measureText() { return { width: 0 }; },
    fillStyle: "", font: "", textBaseline: "", globalAlpha: 1,
  };
}

function makeDom() {
  const byClass = {};
  function makeEl(tag) {
    const listeners = {};
    const el = {
      tagName: tag || "div", _listeners: listeners, style: {}, dataset: {},
      children: [], _className: "", options: [],
      _rect: { left: 0, top: 0, width: 1200, height: 400, right: 1200, bottom: 400 },
      classList: { add() {}, remove() {}, contains() { return false; }, toggle() {} },
      get className() { return el._className; },
      set className(v) {
        el._className = v;
        for (const cls of String(v).split(/\s+/)) {
          if (!cls) continue;
          (byClass[cls] = byClass[cls] || []).push(el);
        }
      },
      innerHTML: "", textContent: "", title: "", value: "",
      offsetWidth: 1200, offsetHeight: 400, clientWidth: 1200, clientHeight: 400,
      width: 0, height: 0,
      querySelector() { return makeEl(); },
      querySelectorAll() { return []; },
      appendChild(c) {
        el.children.push(c); c.parentNode = el; c.parentElement = el;
        // <select> tracks its <option> children so value-set can be validated.
        if (c.tagName === "option") el.options.push(c);
        return c;
      },
      removeChild(c) { return c; },
      insertBefore(c) { return c; },
      remove() {},
      setAttribute() {}, removeAttribute() {}, getAttribute() { return null; },
      contains() { return false; },
      focus() {}, select() {}, blur() {}, click() {},
      getContext() { return makeCtx(); },
      getBoundingClientRect() { return el._rect; },
      addEventListener(type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
      removeEventListener(type, fn) {
        if (!listeners[type]) return;
        listeners[type] = listeners[type].filter((f) => f !== fn);
      },
      dispatchEvent(ev) {
        ev.target = ev.target || el;
        for (const fn of (listeners[ev.type] || []).slice()) fn(ev);
        return true;
      },
    };
    el.parentElement = null; el.parentNode = null;
    return el;
  }

  const prevDocument = global.document;
  const prevNavigatorDesc = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  const prevWindow = global.window;
  const prevDpr = global.devicePixelRatio;

  const doc = makeEl();
  doc.body = makeEl();
  doc.createElement = (tag) => makeEl(tag);
  const docListeners = {};
  doc.addEventListener = (type, fn) => { (docListeners[type] = docListeners[type] || []).push(fn); };
  doc.removeEventListener = (type, fn) => {
    if (docListeners[type]) docListeners[type] = docListeners[type].filter((f) => f !== fn);
  };
  doc._listeners = docListeners;
  global.document = doc;
  Object.defineProperty(globalThis, "navigator", {
    value: { platform: "", clipboard: { writeText() { return Promise.resolve(); } } },
    configurable: true, writable: true,
  });
  global.window = {
    innerWidth: 1600, innerHeight: 900, open() { return null; },
    addEventListener() {}, removeEventListener() {},
  };
  global.devicePixelRatio = 1;
  global.requestAnimationFrame = (fn) => { fn(); return 0; };

  function restore() {
    global.document = prevDocument;
    if (prevNavigatorDesc) Object.defineProperty(globalThis, "navigator", prevNavigatorDesc);
    else delete globalThis.navigator;
    global.window = prevWindow;
    global.devicePixelRatio = prevDpr;
  }
  return { makeEl, restore, byClass };
}

// Two spawn locations, each with a distinct leaf, so a spawn filter changes the
// tree observably. callframeSymbols maps addr → { symbol, location }.
function sampleData() {
  const symbols = new Map([
    [1, { symbol: "root_fn", location: null }],
    [2, { symbol: "alpha_leaf", location: null }],
    [3, { symbol: "beta_leaf", location: null }],
  ]);
  // callchain is innermost-first (buildFlamegraphTree reverses it); workerId
  // != 255 so the samples land in the worker lane.
  const samples = [
    { callchain: [2, 1], workerId: 0, spawnLoc: "src/a.rs:1", weight: 1 },
    { callchain: [2, 1], workerId: 0, spawnLoc: "src/a.rs:1", weight: 1 },
    { callchain: [3, 1], workerId: 0, spawnLoc: "src/b.rs:2", weight: 1 },
  ];
  return { samples, symbols };
}

test("search round-trips through getViewState/applyViewState (exact mode)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    const { samples, symbols } = sampleData();
    fg.setData(samples, symbols, { exportTitle: "t" });

    fg.setSearch("alpha");
    assert.strictEqual(fg.getSearch(), "alpha", "getSearch reports the live query");
    assert.strictEqual(fg.getViewState().search, "alpha", "search in view state");

    const fg2 = createFlamegraph(dom.makeEl());
    fg2.setData(samples, symbols, { exportTitle: "t" });
    fg2.applyViewState(fg.getViewState());
    assert.strictEqual(fg2.getSearch(), "alpha", "search restored");
  } finally {
    dom.restore();
  }
});

test("spawn filter round-trips and rebuilds the tree before zoom restore", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    const { samples, symbols } = sampleData();
    fg.setData(samples, symbols, { exportTitle: "t" });

    // Apply a spawn filter + a zoom into the alpha subtree. applyViewState must
    // set the filter (rebuild) FIRST, then zoom — otherwise the zoom target
    // would resolve against the unfiltered tree.
    const state = { spawn: "src/a.rs:1", workerZoom: ["root_fn", "alpha_leaf"] };
    fg.applyViewState(state);
    assert.strictEqual(fg.getSpawnFilter(), "src/a.rs:1", "spawn filter restored");
    assert.deepStrictEqual(fg.getZoomPath().worker, ["root_fn", "alpha_leaf"],
      "zoom restored against the filtered tree");

    const st = fg.getViewState();
    assert.strictEqual(st.spawn, "src/a.rs:1");
    assert.deepStrictEqual(st.workerZoom, ["root_fn", "alpha_leaf"]);
  } finally {
    dom.restore();
  }
});

test("applyViewState({}) clears a previously-applied spawn filter", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    const { samples, symbols } = sampleData();
    fg.setData(samples, symbols, { exportTitle: "t" });

    fg.applyViewState({ spawn: "src/a.rs:1" });
    assert.strictEqual(fg.getSpawnFilter(), "src/a.rs:1", "filter applied");
    fg.applyViewState({});
    assert.strictEqual(fg.getSpawnFilter(), "", "filter cleared by empty restore");
  } finally {
    dom.restore();
  }
});

test("a stale spawn value not present in this trace is ignored", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    const { samples, symbols } = sampleData();
    fg.setData(samples, symbols, { exportTitle: "t" });

    fg.applyViewState({ spawn: "src/nonexistent.rs:99" });
    assert.strictEqual(fg.getSpawnFilter(), "", "unknown spawn value left unselected");
  } finally {
    dom.restore();
  }
});

summarize();
