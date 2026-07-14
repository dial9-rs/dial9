"use strict";

// Deep-link support: the flamegraph's view state (zoom path + inspect focus)
// must be readable and restorable so a shared URL reproduces the exact focus
// position. This exercises the renderer API that flamegraph.html serializes
// to/from the URL: getZoomPath/zoomToPath and getInspectFocus/focusInspectByKey,
// plus the onZoomChange callback firing on inspect enter/exit.
//
// The repo has no jsdom, so — like test_flamegraph_inspect_dom.js — we install a
// minimal DOM that records listeners and can dispatch synthetic events.

const { assert, test, summarize } = require("./test_harness.js");

function makeCtx() {
  return {
    scale() {}, fillRect() {}, save() {}, restore() {}, beginPath() {},
    rect() {}, clip() {}, fillText() {}, measureText() { return { width: 0 }; },
    fillStyle: "", font: "", textBaseline: "", globalAlpha: 1,
  };
}

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

// root → a(10) → mid(10) → leaf(10 self)
// root → b(5)  → mid(5)  → leaf(5 self)
function sampleTree() {
  const leafA = tree("leaf", 10, 10, []);
  const midA = tree("mid", 10, 0, [leafA]);
  const a = tree("a", 10, 0, [midA]);
  const leafB = tree("leaf", 5, 5, []);
  const midB = tree("mid", 5, 0, [leafB]);
  const b = tree("b", 5, 0, [midB]);
  return tree("", 15, 0, [a, b]);
}

function fire(el, type, props) {
  const ev = Object.assign({
    type, target: el, clientX: 100, clientY: 100,
    preventDefault() {}, stopPropagation() {},
    metaKey: false, ctrlKey: false, altKey: false,
  }, props || {});
  el.dispatchEvent(ev);
  return ev;
}

test("zoom path round-trips through zoomToPath/getZoomPath", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    // Nothing zoomed initially.
    assert.deepStrictEqual(fg.getZoomPath().worker, [], "no zoom initially");

    // Restore a nested zoom the way flamegraph.html does from the URL.
    fg.zoomToPath("worker", ["a", "mid", "leaf"]);
    assert.deepStrictEqual(
      fg.getZoomPath().worker, ["a", "mid", "leaf"],
      "getZoomPath reproduces the restored nested path");
    assert.strictEqual(fg.isZoomed(), true, "isZoomed true after restore");
  } finally {
    dom.restore();
  }
});

test("inspect focus round-trips through focusInspectByKey/getInspectFocus", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    assert.strictEqual(fg.getInspectFocus(), null, "no inspect focus initially");

    // Restore inspect focus the way flamegraph.html does from the ?inspect= key.
    const ok = fg.focusInspectByKey("mid");
    assert.strictEqual(ok, true, "focusInspectByKey found the frame");
    assert.strictEqual(fg.getInspectFocus(), "mid", "getInspectFocus reports the restored focus");

    // The inspect (butterfly) view is visible and names the frame.
    const inspectView = dom.byClass["fg-inspect"] && dom.byClass["fg-inspect"][0];
    assert.strictEqual(inspectView.style.display, "", "inspect view visible after restore");
    const band = dom.byClass["fg-focus-band"][0];
    assert.ok(band.children.some((c) => c.textContent === "mid"), "focus band names 'mid'");

    // Exiting clears the focus so a subsequent link carries no ?inspect=.
    assert.strictEqual(fg.handleEscape(), true, "Esc consumed by inspect exit");
    assert.strictEqual(fg.getInspectFocus(), null, "focus cleared after exit");
  } finally {
    dom.restore();
  }
});

test("focusInspectByKey is a no-op for an absent frame", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);
    assert.strictEqual(fg.focusInspectByKey("does-not-exist"), false, "returns false");
    assert.strictEqual(fg.getInspectFocus(), null, "no focus set");
    assert.strictEqual(fg.focusInspectByKey(null), false, "null key is a no-op");
  } finally {
    dom.restore();
  }
});

test("onZoomChange fires on inspect enter and exit (URL stays in sync)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    let calls = 0;
    const fg = createFlamegraph(dom.makeEl(), () => { calls++; });
    fg.setTreeDirect(sampleTree(), 15);

    const before = calls;
    fg.focusInspectByKey("mid");
    assert.ok(calls > before, "callback fired on inspect enter");

    const afterEnter = calls;
    fg.handleEscape(); // exits inspect
    assert.ok(calls > afterEnter, "callback fired on inspect exit");
  } finally {
    dom.restore();
  }
});

test("resetView clears zoom + inspect without firing the change callback", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    let calls = 0;
    const fg = createFlamegraph(dom.makeEl(), () => { calls++; });
    fg.setTreeDirect(sampleTree(), 15);

    fg.zoomToPath("worker", ["a", "mid"]);
    fg.focusInspectByKey("leaf");
    const afterSetup = calls;

    fg.resetView();
    assert.deepStrictEqual(fg.getZoomPath().worker, [], "zoom cleared");
    assert.strictEqual(fg.getInspectFocus(), null, "inspect cleared");
    assert.strictEqual(calls, afterSetup, "resetView is silent (no URL churn during restore)");
  } finally {
    dom.restore();
  }
});

test("resetView makes repeated zoom restore idempotent (streaming retry safety)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    // Simulate flamegraph.html retrying restore over several streamed snapshots:
    // reset-then-apply each time must converge to the exact path, never append.
    for (let i = 0; i < 3; i++) {
      fg.resetView();
      fg.zoomToPath("worker", ["a", "mid", "leaf"]);
    }
    assert.deepStrictEqual(
      fg.getZoomPath().worker, ["a", "mid", "leaf"],
      "path is exact after repeated reset+restore (no duplicated frames)");
  } finally {
    dom.restore();
  }
});

// --- getViewState / applyViewState: the bridge flamegraph.html uses with the
// flamegraph_view_state.js URL codec. These pin the object shape the codec
// reads/writes and the restore semantics (silent, full-restore, retry-safe). ---

test("getViewState reports the live zoom + inspect focus in codec shape", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    assert.deepStrictEqual(fg.getViewState(), {}, "empty view → empty state");

    fg.zoomToPath("worker", ["a", "mid"]);
    fg.focusInspectByKey("leaf");
    const st = fg.getViewState();
    assert.deepStrictEqual(st.workerZoom, ["a", "mid"], "workerZoom captured");
    // fullName falls back to name for aggregated trees (no symbol).
    assert.deepStrictEqual(st.inspect, { name: "leaf", fullName: "leaf" },
      "inspect captured as {name, fullName}");
  } finally {
    dom.restore();
  }
});

test("applyViewState restores zoom + inspect and is silent (no URL churn)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    let calls = 0;
    const fg = createFlamegraph(dom.makeEl(), () => { calls++; });
    fg.setTreeDirect(sampleTree(), 15);

    const before = calls;
    fg.applyViewState({ workerZoom: ["a", "mid", "leaf"], inspect: { name: "mid", fullName: "mid" } });
    assert.strictEqual(calls, before, "applyViewState does not fire the change callback");
    assert.deepStrictEqual(fg.getZoomPath().worker, ["a", "mid", "leaf"], "zoom restored");
    assert.strictEqual(fg.getInspectFocus(), "mid", "inspect restored");
  } finally {
    dom.restore();
  }
});

test("getViewState → applyViewState round-trips through the codec", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const VS = require("./flamegraph_view_state.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    fg.zoomToPath("worker", ["a", "mid"]);
    fg.focusInspectByKey("leaf");

    // Serialize to a URL and back exactly as flamegraph.html does.
    const params = VS.writeState(new URLSearchParams(), fg.getViewState());

    const fg2 = createFlamegraph(dom.makeEl());
    fg2.setTreeDirect(sampleTree(), 15);
    fg2.applyViewState(VS.readState(params));

    assert.deepStrictEqual(fg2.getZoomPath().worker, ["a", "mid"], "zoom survived the URL round-trip");
    assert.strictEqual(fg2.getInspectFocus(), "leaf", "inspect survived the URL round-trip");
  } finally {
    dom.restore();
  }
});

test("applyViewState is a full restore: absent fields are cleared", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    fg.applyViewState({ workerZoom: ["a", "mid"], inspect: { name: "leaf", fullName: "leaf" } });
    // Re-apply an empty state: prior zoom/inspect must be gone, not merged.
    fg.applyViewState({});
    assert.deepStrictEqual(fg.getZoomPath().worker, [], "zoom cleared by empty restore");
    assert.strictEqual(fg.getInspectFocus(), null, "inspect cleared by empty restore");
  } finally {
    dom.restore();
  }
});

test("repeated applyViewState converges (streamed-snapshot retry safety)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("./flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    fg.setTreeDirect(sampleTree(), 15);

    // flamegraph.html re-applies the URL state on each streamed snapshot until
    // the focus lands; that must be idempotent (no duplicated zoom frames).
    for (let i = 0; i < 3; i++) {
      fg.applyViewState({ workerZoom: ["a", "mid", "leaf"] });
    }
    assert.deepStrictEqual(fg.getZoomPath().worker, ["a", "mid", "leaf"],
      "zoom path exact after repeated restore");
  } finally {
    dom.restore();
  }
});

summarize();
