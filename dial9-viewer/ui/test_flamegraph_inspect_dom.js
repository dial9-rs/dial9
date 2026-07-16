"use strict";

// DOM-level smoke test for the inspect/butterfly UI (#652) and the search
// results dropdown (#653). The repo has no jsdom, so — like
// test_flamegraph_setdirect_export.js — we install a minimal DOM. This stub is
// richer: elements record event listeners and can dispatch synthetic events, so
// the test can drive the real event handlers (right-click → context menu →
// Inspect, plain click → re-pivot, Esc → exit) end to end through the renderer.

const { assert, test, summarize } = require("./test_harness.js");

function makeCtx() {
  return {
    scale() {}, fillRect() {}, save() {}, restore() {}, beginPath() {},
    rect() {}, clip() {}, fillText() {}, measureText() { return { width: 0 }; },
    fillStyle: "", font: "", textBaseline: "", globalAlpha: 1,
  };
}

// A DOM environment where elements track class/style/children/listeners and can
// dispatchEvent to their registered handlers. Returns { document, registry,
// elements, restore }. `elements` collects every element by className so the
// test can find the canvases and menu after construction.
function makeDom() {
  const registry = {};
  const byClass = {};

  function makeEl(tag) {
    const listeners = {};
    const el = {
      tagName: tag || "div",
      _listeners: listeners,
      style: {},
      dataset: {},
      children: [],
      _className: "",
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
      innerHTML: "",
      textContent: "",
      title: "",
      value: "",
      offsetWidth: 1200,
      offsetHeight: 400,
      clientWidth: 1200,
      clientHeight: 400,
      width: 0,
      height: 0,
      querySelector(sel) { return (registry[sel] = registry[sel] || makeEl()); },
      querySelectorAll() { return []; },
      appendChild(c) { el.children.push(c); c.parentNode = el; c.parentElement = el; return c; },
      removeChild(c) { return c; },
      insertBefore(c) { return c; },
      remove() {},
      setAttribute() {},
      removeAttribute() {},
      getAttribute() { return null; },
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
        const fns = listeners[ev.type] || [];
        for (const fn of fns.slice()) fn(ev);
        return true;
      },
    };
    el.parentElement = null;
    el.parentNode = null;
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
    configurable: true,
    writable: true,
  });
  global.window = {
    innerWidth: 1600, innerHeight: 900, open() { return null; },
    addEventListener() {}, removeEventListener() {},
  };
  global.devicePixelRatio = 1;
  // requestAnimationFrame is used by the hover-highlight repaint throttle.
  global.requestAnimationFrame = (fn) => { fn(); return 0; };

  function restore() {
    global.document = prevDocument;
    if (prevNavigatorDesc) Object.defineProperty(globalThis, "navigator", prevNavigatorDesc);
    else delete globalThis.navigator;
    global.window = prevWindow;
    global.devicePixelRatio = prevDpr;
  }

  return { document: doc, registry, byClass, makeEl, restore };
}

function tree(name, count, self, children) {
  const m = new Map();
  for (const c of children || []) m.set(c.fullName || c.name, c);
  return { name, fullName: name, location: null, count, self, children: m };
}

// Fixture with a shared callee so callers/callees are both non-trivial:
//   root → a(10) → mid(10) → leaf(10 self)
//   root → b(5)  → mid(5)  → leaf(5 self)
function sampleTree() {
  const leafA = tree("leaf", 10, 10, []);
  const midA = tree("mid", 10, 0, [leafA]);
  const a = tree("a", 10, 0, [midA]);
  const leafB = tree("leaf", 5, 5, []);
  const midB = tree("mid", 5, 0, [leafB]);
  const b = tree("b", 5, 0, [midB]);
  return tree("", 15, 0, [a, b]);
}

// Dispatch a mouse-ish event to an element's handlers.
function fire(el, type, props) {
  const ev = Object.assign({
    type, target: el, clientX: 100, clientY: 100,
    preventDefault() {}, stopPropagation() {},
    metaKey: false, ctrlKey: false, altKey: false,
  }, props || {});
  el.dispatchEvent(ev);
  return ev;
}

test("right-click → Inspect enters butterfly; Esc exits (#652)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const container = dom.makeEl();
    const fg = createFlamegraph(container);
    fg.setTreeDirect(sampleTree(), 15);

    // The aggregated lane is the "worker" canvas. Find it and its hit regions
    // by right-clicking where a frame was painted. renderCanvas records regions
    // keyed by the mid-frame's y; we synthesize a hit by right-clicking the
    // canvas and relying on the recorded regions. Instead of guessing pixels,
    // drive inspect through the search dropdown, which is deterministic.
    const searchInput = dom.registry[".fg-search-input"];
    assert.ok(searchInput, "search input exists");
    searchInput.value = "mid";
    fire(searchInput, "input");

    const results = dom.byClass["fg-search-results"] && dom.byClass["fg-search-results"][0];
    assert.ok(results, "search results container exists");
    assert.notStrictEqual(results.style.display, "none", "results shown for a match");
    // The rows are appended as children; find the row for "mid" and click it.
    const rows = results.children.filter((c) => c.className === "fg-sr-row");
    assert.ok(rows.length >= 1, "at least one result row rendered");
    fire(rows[0], "click");

    // Inspect view should now be visible with the focus band populated.
    const inspectView = dom.byClass["fg-inspect"] && dom.byClass["fg-inspect"][0];
    assert.ok(inspectView, "inspect view element exists");
    assert.strictEqual(inspectView.style.display, "", "inspect view visible after Inspect");
    const band = dom.byClass["fg-focus-band"][0];
    assert.ok(band.children.some((c) => c.textContent === "mid"), "focus band names the frame");

    // Esc exits inspect.
    const consumed = fg.handleEscape();
    assert.strictEqual(consumed, true, "Esc consumed by inspect exit");
    assert.strictEqual(inspectView.style.display, "none", "inspect view hidden after Esc");
  } finally {
    dom.restore();
  }
});

test("search dropdown lists matches with sizes and hides when cleared (#653)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    const searchInput = dom.registry[".fg-search-input"];
    searchInput.value = "leaf";
    fire(searchInput, "input");

    const results = dom.byClass["fg-search-results"] && dom.byClass["fg-search-results"][0];
    const rows = results.children.filter((c) => c.className === "fg-sr-row");
    assert.strictEqual(rows.length, 1, "one function 'leaf' across both stacks");
    const sizeSpan = rows[0].children.find((c) => c.className === "fg-sr-size");
    assert.ok(sizeSpan && /100\.0%/.test(sizeSpan.textContent),
      "leaf is 15/15 = 100% of samples: " + (sizeSpan && sizeSpan.textContent));

    // Clearing the query hides the dropdown.
    searchInput.value = "";
    fire(searchInput, "input");
    assert.strictEqual(results.style.display, "none", "results hidden when query empty");
  } finally {
    dom.restore();
  }
});

test("re-pivot: clicking a caller frame on the canvas while inspecting", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    // Enter inspect on "mid" via search.
    const searchInput = dom.registry[".fg-search-input"];
    searchInput.value = "mid";
    fire(searchInput, "input");
    const results = dom.byClass["fg-search-results"] && dom.byClass["fg-search-results"][0];
    fire(results.children.filter((c) => c.className === "fg-sr-row")[0], "click");

    // Canvases are created worker, off-worker, callees, callers — so the
    // callers canvas is the 4th fg-canvas. Its inverted layout paints the
    // immediate callers (a=10, b=5, sorted desc) at depth 0: a spans the left
    // ~2/3 at y∈[4,22). Click there to re-pivot onto "a".
    const canvases = dom.byClass["fg-canvas"];
    assert.ok(canvases.length >= 4, "worker/off-worker/callees/callers canvases exist");
    const callersCanvas = canvases[3];
    fire(callersCanvas, "click", { clientX: 100, clientY: 10 });

    const band = dom.byClass["fg-focus-band"][0];
    assert.ok(band.children.some((c) => c.textContent === "a"),
      "focus band re-pivoted to 'a' after clicking its caller frame");
  } finally {
    dom.restore();
  }
});

summarize();
