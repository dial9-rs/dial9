"use strict";

// Diff-view URL state wiring: createDiffView(onChange/initialState). The diff
// view is normally browser-only (it owns DOM + live SSE streams), so this test
// installs a minimal DOM and stubs the SSE module to synchronously deliver one
// tree per side. It asserts the two things flamegraph.html relies on:
//   1. initialState { zoom, search } seeds the view (deep-link restore).
//   2. onChange fires with { zoom, search } on user zoom / highlight (persist).
// The zoom path is root-inclusive (element 0 is the merged root), matching the
// flamegraph_view_state.js diff codec contract.

const { assert, test, summarize } = require("./test_harness.js");

// --- Stub the SSE module so each side "streams" one fixed tree, synchronously.
const sse = require("./sse.js");
const realOpenSse = sse.openSse;
// tree: (all) → runtime → poll(leaf). Server JSON shape {name,count,self,children}.
function serverTree() {
  return {
    name: "(all)", count: 10, self: 0,
    children: [
      { name: "runtime", count: 10, self: 0, children: [
        { name: "poll", count: 10, self: 10, children: [] },
      ]},
    ],
  };
}
// Capture each side's callbacks so a test can control WHEN data arrives. This
// matters for the deep-link case: in the browser the merged tree is empty until
// the first SSE snapshot lands (well after construction), so a seeded zoom must
// survive the initial empty render and land later — not be delivered inline.
let sides = [];
sse.openSse = function (url, opts) {
  sides.push(opts);
  return Promise.resolve();
};
// Deliver one snapshot (+ close) to every open side, as the server would.
function deliverSnapshot() {
  for (const opts of sides) {
    if (opts.onEvent) opts.onEvent({ tree: serverTree(), total_samples: 10, metadata: { hosts: 1 }, coverage: null });
    if (opts.onClose) opts.onClose();
  }
}

function makeCtx() {
  return {
    scale() {}, fillRect() {}, save() {}, restore() {}, beginPath() {},
    rect() {}, clip() {}, fillText() {}, measureText() { return { width: 0 }; },
    setTransform() {}, strokeRect() {}, stroke() {},
    fillStyle: "", strokeStyle: "", lineWidth: 1, font: "", textBaseline: "", globalAlpha: 1,
  };
}

function makeDom() {
  function makeEl(tag) {
    const listeners = {};
    const el = {
      tagName: tag || "div", _listeners: listeners, style: {}, dataset: {},
      children: [], _className: "", value: "",
      _rect: { left: 0, top: 0, width: 600, height: 400, right: 600, bottom: 400 },
      classList: { add() {}, remove() {}, contains() { return false; }, toggle() {} },
      get className() { return el._className; },
      set className(v) { el._className = v; },
      innerHTML: "", textContent: "", title: "",
      offsetWidth: 600, offsetHeight: 400, clientWidth: 600, clientHeight: 400,
      width: 0, height: 0,
      // Every querySelector returns a fresh stable child so header controls exist.
      _q: {},
      querySelector(sel) { return (el._q[sel] = el._q[sel] || makeEl()); },
      querySelectorAll() { return []; },
      appendChild(c) { el.children.push(c); c.parentNode = el; c.parentElement = el; return c; },
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

  const prev = { doc: global.document, win: global.window, dpr: global.devicePixelRatio, raf: global.requestAnimationFrame };
  const prevNav = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  const doc = makeEl();
  doc.body = makeEl();
  doc.createElement = (tag) => makeEl(tag);
  doc.activeElement = null;
  const docListeners = {};
  doc.addEventListener = (t, fn) => { (docListeners[t] = docListeners[t] || []).push(fn); };
  doc.removeEventListener = () => {};
  global.document = doc;
  Object.defineProperty(globalThis, "navigator", { value: { platform: "" }, configurable: true, writable: true });
  const winListeners = {};
  global.window = {
    innerWidth: 1600, innerHeight: 900, location: { origin: "http://localhost" },
    addEventListener(t, fn) { (winListeners[t] = winListeners[t] || []).push(fn); },
    removeEventListener() {},
    devicePixelRatio: 1,
  };
  global.window._listeners = winListeners;
  global.devicePixelRatio = 1;
  global.requestAnimationFrame = (fn) => { fn(); return 0; };

  function restore() {
    global.document = prev.doc; global.window = prev.win;
    global.devicePixelRatio = prev.dpr; global.requestAnimationFrame = prev.raf;
    if (prevNav) Object.defineProperty(globalThis, "navigator", prevNav);
    else delete globalThis.navigator;
  }
  return { makeEl, restore, container: makeEl() };
}

const { createDiffView } = require("./flamegraph_diff_view.js");

function scopes() {
  return { a: new URLSearchParams("service=svc-a"), b: new URLSearchParams("service=svc-b") };
}

test("seeded zoom SURVIVES the empty initial render and lands once data arrives", () => {
  const dom = makeDom();
  sides = [];
  try {
    const s = scopes();
    const view = createDiffView(dom.container, {
      scopeA: s.a, scopeB: s.b,
      initialState: { zoom: ["(all)", "runtime"], search: "poll" },
    });
    assert.ok(view && typeof view.destroy === "function", "view constructed with initial state");

    // The seeded highlight query is applied immediately (no data needed).
    const searchInput = dom.container.children[0]._q[".fgd-search"];
    assert.strictEqual(searchInput.value, "poll", "seeded highlight query set");

    // Data has NOT arrived yet: the merged tree is empty, so the deep zoom
    // target cannot resolve. The regression was render() clobbering the seed to
    // root here; the breadcrumb (shown only when zoomed past root) must stay
    // hidden but the seed must be RETAINED, not discarded.
    const breadcrumb = dom.container.children[1];
    assert.strictEqual(breadcrumb.style.display, "none",
      "before data: not zoomed yet (breadcrumb hidden)");

    // Now the sides stream in. render() re-tries the pending target, which now
    // resolves against the merged tree, so the view jumps to the seeded focus.
    deliverSnapshot();
    assert.strictEqual(breadcrumb.style.display, "flex",
      "after data: seeded zoom landed (breadcrumb visible)");
    view.destroy();
  } finally {
    dom.restore();
  }
});

test("onChange fires with { zoom, search } when the highlight box changes", () => {
  const dom = makeDom();
  sides = [];
  try {
    const s = scopes();
    let last = null;
    const view = createDiffView(dom.container, {
      scopeA: s.a, scopeB: s.b,
      onChange: (st) => { last = st; },
    });
    // The header is the first child appended to the container; the search input
    // is its ".fgd-search" query child.
    const header = dom.container.children[0];
    const searchInput = header._q[".fgd-search"];
    assert.ok(searchInput, "search input exists");
    searchInput.value = "runtime";
    searchInput.dispatchEvent({ type: "input" });
    assert.ok(last, "onChange fired on highlight input");
    assert.strictEqual(last.search, "runtime", "onChange carries the highlight query");
    assert.deepStrictEqual(last.zoom, [], "no zoom → empty zoom array (clean URL)");
    view.destroy();
  } finally {
    dom.restore();
  }
});

test("highlight typed before a deep-link zoom lands keeps the zoom in the URL", () => {
  const dom = makeDom();
  sides = [];
  try {
    const s = scopes();
    let last = null;
    // Deep-link seeds a zoom target that can't resolve until data arrives.
    const view = createDiffView(dom.container, {
      scopeA: s.a, scopeB: s.b,
      initialState: { zoom: ["(all)", "runtime"] },
      onChange: (st) => { last = st; },
    });
    // User types a highlight BEFORE any snapshot arrives (pendingZoom still in
    // flight, zoomPath still root-only). persistState must carry the pending
    // target, not wipe diff_zoom — otherwise the URL loses the zoom the view
    // will still jump to once data lands.
    const searchInput = dom.container.children[0]._q[".fgd-search"];
    searchInput.value = "poll";
    searchInput.dispatchEvent({ type: "input" });
    assert.ok(last, "onChange fired on highlight input");
    assert.strictEqual(last.search, "poll", "highlight persisted");
    assert.deepStrictEqual(last.zoom, ["(all)", "runtime"],
      "pending zoom target preserved in the URL, not wiped");
    view.destroy();
  } finally {
    dom.restore();
  }
});

test("onChange fires with a root-inclusive zoom on Escape reset", () => {
  const dom = makeDom();
  sides = [];
  try {
    const s = scopes();
    let last = null;
    const view = createDiffView(dom.container, {
      scopeA: s.a, scopeB: s.b,
      initialState: { zoom: ["(all)", "runtime"] },
      onChange: (st) => { last = st; },
    });
    deliverSnapshot();
    // Escape resets zoom to the root and persists — the window keydown listener.
    const winListeners = global.window._listeners.keydown || [];
    assert.ok(winListeners.length > 0, "diff view registered a keydown listener");
    for (const fn of winListeners) fn({ key: "Escape", preventDefault() {} });
    assert.ok(last, "onChange fired on Escape reset");
    assert.deepStrictEqual(last.zoom, [], "reset → empty zoom array");
    view.destroy();
  } finally {
    dom.restore();
  }
});

test("a user zoom cancels a not-yet-landed pending restore", () => {
  const dom = makeDom();
  sides = [];
  try {
    const s = scopes();
    let last = null;
    // Seed a DEEP target that never appears in the streamed tree (only "runtime"
    // does). Before it can land, the user resets — which must cancel the pending
    // restore so a later snapshot can't snap the view back to the seed.
    const view = createDiffView(dom.container, {
      scopeA: s.a, scopeB: s.b,
      initialState: { zoom: ["(all)", "runtime", "poll"] },
      onChange: (st) => { last = st; },
    });
    // User hits Escape (reset to root) before any data arrives.
    for (const fn of (global.window._listeners.keydown || [])) fn({ key: "Escape", preventDefault() {} });
    assert.deepStrictEqual(last.zoom, [], "user reset persisted a root zoom");
    // Data now arrives; the (cancelled) seed must NOT re-zoom the view.
    deliverSnapshot();
    const breadcrumb = dom.container.children[1];
    assert.strictEqual(breadcrumb.style.display, "none",
      "view stays at root — cancelled restore did not snap back");
    view.destroy();
  } finally {
    dom.restore();
  }
});

// Restore the real SSE fn so requiring this module elsewhere is side-effect free.
sse.openSse = realOpenSse;

summarize();
