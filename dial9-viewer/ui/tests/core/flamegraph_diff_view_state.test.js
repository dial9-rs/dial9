import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { afterAll, test } from "vitest";

const require = createRequire(import.meta.url);

// Diff-view URL state wiring: createDiffView(onChange/initialState). The diff
// view is normally browser-only (it owns DOM + live SSE streams), so this test
// installs a minimal DOM and stubs the SSE module to synchronously deliver one
// tree per side. It asserts the two things flamegraph.html relies on:
//   1. initialState { zoom, search } seeds the view (deep-link restore).
//   2. onChange fires with { zoom, search } on user zoom / highlight (persist).
// The zoom path is root-inclusive (element 0 is the merged root), matching the
// canonical URL codec contract.

// --- Stub the SSE module so each side "streams" one fixed tree, synchronously.
const sse = require("../../sse.js");
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

const {
  apiUrlFor,
  createDiffView,
  browserApi,
  isSearchFocusKey,
  refinementLifecycleBadge,
  scopeLabel,
} = require("../../flamegraph_diff_view.js");

function scopes() {
  return { a: new URLSearchParams("service=svc-a"), b: new URLSearchParams("service=svc-b") };
}

test("apiUrlFor forwards server scope and strips client-only flags", () => {
  const scope = new URLSearchParams(
    "api=1&bucket=b&aws_region=us-west-2&service=svc&host=h1&host=h2&max_files=64",
  );
  const url = apiUrlFor({
    scope,
    origin: "https://viewer.example.com",
    maxFiles: 8,
  });
  assert.strictEqual(url.pathname, "/api/flamegraph");
  assert.strictEqual(url.searchParams.get("aws_region"), "us-west-2");
  assert.deepStrictEqual(url.searchParams.getAll("host"), ["h1", "h2"]);
  assert.strictEqual(url.searchParams.get("max_files"), "8");
  assert.strictEqual(url.searchParams.get("api"), null);
});

test("scope labels and search shortcuts describe the current diff side", () => {
  assert.strictEqual(
    scopeLabel(new URLSearchParams("service=svc&host=a&host=b"), "A"),
    "svc @ 2 hosts",
  );
  assert.strictEqual(scopeLabel(new URLSearchParams("host=h1"), "B"), "B @ h1");
  assert.strictEqual(isSearchFocusKey({ key: "/" }, false), true);
  assert.strictEqual(isSearchFocusKey({ key: "/" }, true), false);
  assert.strictEqual(isSearchFocusKey({ key: "f", ctrlKey: true }, false), true);
});

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

test("browser API fallback exposes Refine work-depth and adoption helpers", () => {
  const root = {
    formatCoverageBadge() {},
    foldErrorNotice() {},
    nextMaxFiles() {},
    refinementWorkDepth() {},
    shouldAdoptRefinementSnapshot() {},
  };
  const api = browserApi(root);
  assert.strictEqual(api.refinementWorkDepth, root.refinementWorkDepth,
    "browser fallback carries refinementWorkDepth");
  assert.strictEqual(api.shouldAdoptRefinementSnapshot, root.shouldAdoptRefinementSnapshot,
    "browser fallback carries snapshot adoption helper");
});

test("Load more keeps each side visible until cached coverage recovers its baseline", () => {
  const dom = makeDom();
  sides = [];
  try {
    const s = scopes();
    const view = createDiffView(dom.container, {
      scopeA: s.a,
      scopeB: s.b,
      initialState: { zoom: ["(all)", "runtime"] },
    });
    const coverage = (filesFolded, samplesFolded) => ({
      files_matched: 1000,
      files_folded: filesFolded,
      fold_work_cap: 100,
      samples_folded: samplesFolded,
      hosts_matched: 10,
      hosts_folded: 10,
      fold_errors: 0,
    });
    const sideBTree = {
      name: "(all)", count: 100, self: 0,
      children: [{ name: "side-b", count: 100, self: 100, children: [] }],
    };
    sides.slice(0, 2).forEach((opts, index) => {
      opts.onEvent({
        tree: index === 0 ? serverTree() : sideBTree,
        total_samples: 100,
        metadata: { hosts: 10 },
        coverage: coverage(100, 100),
      });
      opts.onClose();
    });

    const breadcrumb = dom.container.children[1];
    assert.strictEqual(breadcrumb.style.display, "flex", "initial exact zoom is visible");
    const moreBtn = dom.container.children[0]._q[".fgd-more"];
    moreBtn.dispatchEvent({ type: "click" });
    assert.strictEqual(sides.length, 4, "Load more repolls both sides without a browser API error");

    const replacementTree = {
      name: "(all)", count: 24, self: 0,
      children: [{ name: "replacement", count: 24, self: 24, children: [] }],
    };
    sides[2].onEvent({
      tree: replacementTree,
      total_samples: 24,
      metadata: { hosts: 10 },
      coverage: coverage(24, 24),
    });
    assert.strictEqual(
      breadcrumb.style.display,
      "flex",
      "below-baseline side snapshot leaves the old zoomed tree visible",
    );

    sides[2].onEvent({
      tree: replacementTree,
      total_samples: 120,
      metadata: { hosts: 10 },
      coverage: coverage(100, 120),
    });
    assert.strictEqual(
      breadcrumb.style.display,
      "none",
      "at-baseline side snapshot is adopted and may replace the old zoom target",
    );
    view.destroy();
  } finally {
    dom.restore();
  }
});

test("diff lifecycle formatter removes stale markers around fold warnings", () => {
  const warning = "100 / 1000 files · refined · ⚠ 1 fold failed: injected warning";
  const incomplete = refinementLifecycleBadge(warning, "refinement incomplete");
  assert.strictEqual(
    incomplete,
    "100 / 1000 files · ⚠ 1 fold failed: injected warning · refinement incomplete",
    "below-baseline close retains the warning and exactly one incomplete marker",
  );
  assert.ok(!incomplete.includes(" · refined ·"), "incomplete badge has no stale refined marker");

  const interrupted = refinementLifecycleBadge(incomplete, "refinement interrupted");
  assert.strictEqual(
    interrupted,
    "100 / 1000 files · ⚠ 1 fold failed: injected warning · refinement interrupted",
    "error replaces incomplete with exactly one interrupted marker",
  );
});

afterAll(() => {
  sse.openSse = realOpenSse;
});
