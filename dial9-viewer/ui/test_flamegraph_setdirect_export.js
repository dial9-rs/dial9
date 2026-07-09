"use strict";

// Regression test for #625: the Export button must be enabled on aggregated
// (API-mode) flamegraphs. Those go through createFlamegraph().setTreeDirect(),
// which historically never touched the export control, so it stayed stuck in
// its initial disabled state. This drives setTreeDirect() against a minimal DOM
// stub and asserts the button toggles with hasExportableData().
//
// The repo has no jsdom, so we install just enough of a DOM to construct the
// (DOM-heavy) flamegraph. Every element is a Proxy that returns sensible
// defaults and no-op methods; querySelector resolves against a per-graph
// registry keyed by selector so the test can inspect the export button object.

const { assert, test, summarize } = require("./test_harness.js");

function makeCtx() {
  return {
    scale() {}, fillRect() {}, save() {}, restore() {}, beginPath() {},
    rect() {}, clip() {}, fillText() {}, measureText() { return { width: 0 }; },
    fillStyle: "", font: "", textBaseline: "", globalAlpha: 1,
  };
}

// Build one flamegraph against a fresh DOM stub. Returns the created renderer
// plus the selector registry so the test can read exportBtn.disabled.
function newGraph() {
  const registry = {};

  function makeEl() {
    const store = {};
    return new Proxy({}, {
      get(_t, prop) {
        if (prop in store) return store[prop];
        switch (prop) {
          case "style": return (store.style = store.style || {});
          case "dataset": return (store.dataset = store.dataset || {});
          case "classList": return { add() {}, remove() {}, contains() { return false; }, toggle() {} };
          case "querySelector": return (sel) => (registry[sel] = registry[sel] || makeEl());
          case "querySelectorAll": return () => [];
          case "appendChild": case "removeChild": case "insertBefore": return (c) => c;
          case "addEventListener": case "removeEventListener": return () => {};
          case "setAttribute": case "removeAttribute": return () => {};
          case "getAttribute": return () => null;
          case "focus": case "select": case "blur": case "click": case "remove": return () => {};
          case "getContext": return () => makeCtx();
          case "getBoundingClientRect": return () => ({ left: 0, top: 0, width: 1200, height: 400, right: 1200, bottom: 400 });
          case "contains": return () => false;
          case "parentElement": case "parentNode": return (store.parentElement = store.parentElement || makeEl());
          case "children": return [];
          case "clientWidth": case "offsetWidth": return 1200;
          case "clientHeight": case "offsetHeight": return 400;
          case "innerHTML": case "textContent": case "className": case "value": return "";
          case "width": case "height": return 0;
          default: return undefined;
        }
      },
      set(_t, prop, val) { store[prop] = val; return true; },
    });
  }

  const prevDocument = global.document;
  // navigator is a getter-only global on Node 24 (CI), so plain assignment
  // throws. Save/restore its property descriptor and stub via defineProperty
  // so this works on both Node 18 (local) and Node 24 (CI).
  const prevNavigatorDesc = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  const prevWindow = global.window;
  const prevDpr = global.devicePixelRatio;

  const doc = makeEl();
  doc.body = makeEl();
  doc.createElement = () => makeEl();
  global.document = doc;
  Object.defineProperty(globalThis, "navigator", {
    value: { platform: "" },
    configurable: true,
    writable: true,
  });
  global.window = { innerWidth: 1600, open() { return null; } };
  global.devicePixelRatio = 1;

  // Load lazily so the globals above are in place before module init.
  const { createFlamegraph } = require("./flamegraph.js");
  const fg = createFlamegraph(makeEl());

  // Restore globals; the renderer already captured what it needs.
  global.document = prevDocument;
  if (prevNavigatorDesc) {
    Object.defineProperty(globalThis, "navigator", prevNavigatorDesc);
  } else {
    delete globalThis.navigator;
  }
  global.window = prevWindow;
  global.devicePixelRatio = prevDpr;

  return { fg, exportBtn: registry[".fg-export-btn"] };
}

function tree(name, count, self, children) {
  const m = new Map();
  for (const c of children || []) m.set(c.name, c);
  return { name, fullName: name, location: null, count, self, children: m };
}

test("setTreeDirect enables Export on a non-empty aggregated tree (#625)", () => {
  const { fg, exportBtn } = newGraph();
  assert.ok(exportBtn, "export button stub was created");
  const t = tree("", 100, 0, [tree("main", 100, 0, [tree("work", 100, 100, [])])]);
  fg.setTreeDirect(t, 100);
  assert.strictEqual(exportBtn.disabled, false, "button must be enabled after a non-empty aggregated render");
});

test("setTreeDirect keeps Export disabled on an empty aggregated tree", () => {
  const { fg, exportBtn } = newGraph();
  fg.setTreeDirect(tree("", 0, 0, []), 0);
  assert.strictEqual(exportBtn.disabled, true, "button must stay disabled when there are no samples");
});

summarize();
