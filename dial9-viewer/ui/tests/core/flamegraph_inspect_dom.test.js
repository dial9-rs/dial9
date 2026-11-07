import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { test } from "vitest";

const require = createRequire(import.meta.url);

// DOM-level smoke test for the inspect/butterfly UI (#652), the search
// results dropdown (#653), and the widget's document-level search shortcut.
// The repo has no jsdom, so this installs a minimal DOM. The stub is
// richer: elements record event listeners and can dispatch synthetic events, so
// the test can drive the real event handlers (right-click → context menu →
// Inspect, plain click → re-pivot, Esc → exit) end to end through the renderer.

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
// `style.cssText = "a-b:c"` sets `style.aB`, as in a browser.
function makeStyle() {
  const style = {};
  Object.defineProperty(style, "cssText", {
    set(v) {
      for (const decl of String(v).split(";")) {
        const i = decl.indexOf(":");
        if (i < 0) continue;
        const key = decl.slice(0, i).trim().replace(/-([a-z])/g, (_m, c) => c.toUpperCase());
        style[key] = decl.slice(i + 1).trim();
      }
    },
  });
  return style;
}

function makeDom() {
  const registry = {};
  const byClass = {};
  // Non-empty innerHTML writes, so tests can require trace text never takes that path.
  const htmlWrites = [];
  const opened = [];

  function makeEl(tag) {
    const listeners = {};
    let innerHTML = "";
    const el = {
      tagName: tag || "div",
      _listeners: listeners,
      style: makeStyle(),
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
      // innerHTML is stored, not parsed. Both setters replace the children, as
      // in a browser, and textContent reads back through the children.
      get innerHTML() { return innerHTML; },
      set innerHTML(v) {
        innerHTML = String(v);
        if (innerHTML) htmlWrites.push({ el, html: innerHTML });
        el.children = [];
      },
      get textContent() { return el.children.map((c) => c.textContent).join(""); },
      set textContent(v) {
        innerHTML = "";
        el.children = String(v) ? [makeText(v)] : [];
      },
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

  function makeText(data) {
    return { nodeType: 3, data: String(data), get textContent() { return this.data; } };
  }

  const prevDocument = global.document;
  const prevNavigatorDesc = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  const prevWindow = global.window;
  const prevDpr = global.devicePixelRatio;

  const doc = makeEl();
  doc.body = makeEl();
  doc.createElement = (tag) => makeEl(tag);
  doc.createTextNode = makeText;
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
    innerWidth: 1600, innerHeight: 900, open(url) { opened.push(url); return null; },
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

  return { document: doc, registry, byClass, htmlWrites, opened, makeEl, restore };
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

function exactSamples(frame) {
  const symbols = new Map([
    [1, { symbol: "root", location: null }],
    [2, { symbol: frame, location: null }],
  ]);
  const samples = [
    { callchain: [2, 1], workerId: 0, spawnLoc: null, weight: 1 },
  ];
  return { samples, symbols };
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
    const { createFlamegraph } = require("../../flamegraph.js");
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

test("replacing exact-trace data exits inspect and hides its DOM", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("../../flamegraph.js");
    const fg = createFlamegraph(dom.makeEl());
    const cpu = exactSamples("cpu_frame");
    fg.setData(cpu.samples, cpu.symbols);

    assert.strictEqual(fg.focusInspectByKey("cpu_frame"), true);
    const inspectView = dom.byClass["fg-inspect"][0];
    assert.strictEqual(inspectView.style.display, "", "inspect view is visible");

    const heap = exactSamples("heap_frame");
    fg.setData(heap.samples, heap.symbols);

    assert.strictEqual(fg.getInspectFocus(), null, "inspect focus is cleared");
    assert.strictEqual(inspectView.style.display, "none", "inspect view is hidden");
    assert.strictEqual(fg.handleEscape(), false, "no stale inspect state remains");
  } finally {
    dom.restore();
  }
});

test("search dropdown lists matches with sizes and hides when cleared (#653)", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("../../flamegraph.js");
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
    const { createFlamegraph } = require("../../flamegraph.js");
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

// The `/` shortcut is bound to the DOCUMENT, so an embedded instance would
// swallow the host app's own `/` search whenever it is on screen. Callers that
// do not own the page pass captureSlash: false and keep Cmd/Ctrl + F.
function fireDocKey(dom, key, props) {
  let prevented = false;
  const ev = Object.assign(
    {
      type: "keydown",
      key,
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      preventDefault() {
        prevented = true;
      },
    },
    props || {},
  );
  for (const fn of (dom.document._listeners.keydown || []).slice()) fn(ev);
  return { prevented };
}

/** Count focus() calls on the widget's search field. */
function watchSearchFocus(dom) {
  const input = dom.registry[".fg-search-input"];
  assert.ok(input, "search input exists");
  let focused = 0;
  input.focus = () => {
    focused += 1;
  };
  return { input, focusCount: () => focused };
}

test("captureSlash defaults on: bare / focuses the frame search", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("../../flamegraph.js");
    createFlamegraph(dom.makeEl());
    const watch = watchSearchFocus(dom);

    const { prevented } = fireDocKey(dom, "/");
    assert.strictEqual(watch.focusCount(), 1, "/ focused the frame search");
    assert.strictEqual(prevented, true, "/ was consumed by the widget");
  } finally {
    dom.restore();
  }
});

test("captureSlash: false leaves bare / to the host app", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("../../flamegraph.js");
    createFlamegraph(dom.makeEl(), undefined, { captureSlash: false });
    const watch = watchSearchFocus(dom);

    const { prevented } = fireDocKey(dom, "/");
    assert.strictEqual(watch.focusCount(), 0, "/ ignored by an embedded widget");
    assert.strictEqual(prevented, false, "/ left for the host app's search");

    // Cmd/Ctrl + F still reaches the frame search.
    const meta = fireDocKey(dom, "f", { metaKey: true });
    assert.strictEqual(watch.focusCount(), 1, "Cmd + F focused the frame search");
    assert.strictEqual(meta.prevented, true, "Cmd + F was consumed by the widget");
  } finally {
    dom.restore();
  }
});

test("captureSlash: false drops / from the search placeholder and help", () => {
  const dom = makeDom();
  try {
    const { createFlamegraph } = require("../../flamegraph.js");
    createFlamegraph(dom.makeEl(), undefined, { captureSlash: false });
    const bar = dom.byClass["fg-search-bar"][0];
    assert.ok(
      bar.innerHTML.includes("Search frames... (Ctrl + F)"),
      "placeholder advertises only Cmd/Ctrl + F",
    );
    const help = dom.byClass["fg-help-overlay"][0];
    assert.ok(!help.innerHTML.includes("Ctrl + F or /"), "help drops the / row");
  } finally {
    dom.restore();
  }
});

// Tooltip frame name/fullName/location and docsUrl come from the trace, so the
// tooltip must render them as text and only link an absolute https://docs.rs/ URL.
function elementsUnder(el) {
  return el.children.flatMap((c) => (c.nodeType === 3 ? [] : [c, ...elementsUnder(c)]));
}

// Show a single-frame tree and hover the worker canvas until the tooltip names
// the frame; optionally Alt-click to pin it. Returns the tooltip and a click helper.
function showTooltip(dom, frame, pinned) {
  const { createFlamegraph } = require("../../flamegraph.js");
  const fg = createFlamegraph(dom.makeEl());
  const leaf = { children: new Map(), count: 10, self: 10, ...frame };
  fg.setTreeDirect({ ...tree("", 10, 0, []), children: new Map([[leaf.fullName, leaf]]) }, 10);
  const tooltip = dom.byClass["fg-tooltip"][0];
  const canvas = dom.byClass["fg-canvas"][0];
  for (let y = 0; y < 400; y += 2) {
    fire(canvas, "mousemove", { clientY: y });
    const b = tooltip.children.find((c) => c.tagName === "b");
    if (tooltip.style.display !== "block" || !b || b.textContent !== frame.name) continue;
    const click = (props) => fire(canvas, "click", { clientY: y, ...props });
    if (pinned) click({ altKey: true });
    const tooltipHtml = () =>
      dom.htmlWrites.filter((w) => w.el === tooltip || elementsUnder(tooltip).includes(w.el));
    return { tooltip, click, tooltipHtml };
  }
  throw new Error("no canvas row shows frame " + JSON.stringify(frame.name));
}

test("tooltip renders hostile frame text inertly; pinned location toggle works", () => {
  const dom = makeDom();
  try {
    const frame = {
      name: '<img src=x onerror="globalThis.__d9xss=1">',
      fullName: "</span><script>globalThis.__d9xss=1</script>",
      location: "/src/<img src=x onerror=alert(1)>/lib.rs:7",
    };
    const { tooltip, tooltipHtml } = showTooltip(dom, frame, true);

    assert.deepStrictEqual(tooltipHtml(), [], "no tooltip innerHTML writes");
    const els = elementsUnder(tooltip);
    assert.ok(!els.some((e) => e.tagName === "img" || e.tagName === "script"));
    assert.ok(tooltip.textContent.includes(frame.name), "name shown verbatim");
    assert.ok(tooltip.textContent.includes(frame.fullName), "fullName shown verbatim");

    const toggle = els.find((e) => e.className === "fg-loc-toggle");
    const full = els.find((e) => e.className === "fg-loc-full");
    assert.strictEqual(toggle.textContent, "lib.rs:7 \u25b6");
    assert.strictEqual(full.style.display, "none");
    fire(toggle, "click");
    assert.strictEqual(full.style.display, "block");
    assert.strictEqual(full.textContent, frame.location, "full location kept as text");
    assert.strictEqual(toggle.textContent, "lib.rs:7 \u25bc");
    fire(toggle, "click");
    assert.strictEqual(full.style.display, "none");
    assert.strictEqual(globalThis.__d9xss, undefined);
  } finally {
    dom.restore();
  }
});

test("tooltip links a docs.rs URL when pinned and Ctrl-click opens it", () => {
  const dom = makeDom();
  try {
    const docsUrl = "https://docs.rs/tokio/1.40.0/src/tokio/lib.rs.html#10";
    const { tooltip, click, tooltipHtml } =
      showTooltip(dom, { name: "f", fullName: "f", location: null, docsUrl }, false);
    assert.ok(tooltip.textContent.includes("docs.rs \u2197 (Ctrl + click)"), "unpinned hint");

    click({ ctrlKey: true });
    assert.deepStrictEqual(dom.opened, [docsUrl]);

    click({ altKey: true });
    assert.deepStrictEqual(tooltipHtml(), []);
    const links = elementsUnder(tooltip).filter((e) => e.tagName === "a");
    assert.strictEqual(links.length, 1);
    assert.strictEqual(links[0].href, docsUrl);
    assert.strictEqual(links[0].rel, "noopener");
  } finally {
    dom.restore();
  }
});

test.each([
  "javascript:alert(1)",
  "https://docs.rs.evil.example/tokio",
])("tooltip rejects untrusted docsUrl %j", (docsUrl) => {
  const dom = makeDom();
  try {
    const { tooltip, click } =
      showTooltip(dom, { name: "f", fullName: "f", location: null, docsUrl }, false);
    click({ ctrlKey: true });
    assert.deepStrictEqual(dom.opened, [], "Ctrl-click opens nothing");

    click({ altKey: true });
    assert.ok(!elementsUnder(tooltip).some((e) => e.tagName === "a"), "no link");
    assert.ok(!tooltip.textContent.includes("docs.rs \u2197"), "no docs text");
  } finally {
    dom.restore();
  }
});
