// Tests for the store->URL view-state sync binding (T19): debounced
// single-write coalescing, flush-for-copy-link, no-op write suppression
// (restore-on-load must not dirty history), legacy query mirroring,
// unknown-hash-key preservation, and foreign-hash refusal. Store scheduler
// and debounce timer are both injected (Node has neither RAF nor a reason
// to sleep in tests).

import { describe, it, expect } from "vitest";
import { createStore, type FrameScheduler } from "../../store/store.js";
import { bindViewStateToUrl, type DebounceTimer, type UrlHost, type UrlParts } from "./sync.js";
import { applyLegacyZoomToQuery, type ViewState } from "./view-state.js";

// Controllable RAF stand-in (same shape as store.test.ts's).
function fakeRaf(): { scheduler: FrameScheduler; frame: () => void } {
  const queue: (() => void)[] = [];
  return {
    scheduler: (cb) => {
      queue.push(cb);
    },
    frame: () => {
      for (const cb of queue.splice(0)) cb();
    },
  };
}

// Controllable one-shot timer: fire() runs the latest pending callback.
function fakeTimer(): DebounceTimer & { fire: () => void; pendingCount: () => number } {
  let nextId = 1;
  const pending = new Map<number, () => void>();
  return {
    set(cb, _ms) {
      const id = nextId++;
      pending.set(id, cb);
      return id;
    },
    clear(handle) {
      pending.delete(handle as number);
    },
    fire() {
      const entries = [...pending.entries()];
      pending.clear();
      for (const [, cb] of entries) cb();
    },
    pendingCount: () => pending.size,
  };
}

// In-memory UrlHost recording every replace().
function fakeHost(initial: UrlParts): UrlHost & { writes: string[]; parts: () => UrlParts } {
  let cur = initial;
  const writes: string[] = [];
  return {
    writes,
    parts: () => cur,
    read: () => cur,
    replace(url) {
      writes.push(url);
      const u = new URL(url, "http://x");
      cur = { pathname: u.pathname, search: u.search, hash: u.hash };
    },
  };
}

interface FgState {
  fgView: { workerZoom: readonly string[]; offworkerZoom: readonly string[] };
}

function project(s: { fgView: FgState["fgView"] }): ViewState {
  const out: ViewState = {};
  if (s.fgView.workerZoom.length > 0) out.fgWorkerZoom = s.fgView.workerZoom;
  if (s.fgView.offworkerZoom.length > 0) out.fgOffworkerZoom = s.fgView.offworkerZoom;
  return out;
}

function setup(url: UrlParts) {
  const raf = fakeRaf();
  const timer = fakeTimer();
  const host = fakeHost(url);
  const store = createStore<FgState>(
    { fgView: { workerZoom: [], offworkerZoom: [] } },
    { scheduler: raf.scheduler },
  );
  const binding = bindViewStateToUrl(store, {
    slices: ["fgView"],
    project,
    mirrorToQuery: applyLegacyZoomToQuery,
    host,
    timer,
  });
  return { raf, timer, host, store, binding };
}

describe("bindViewStateToUrl", () => {
  it("writes legacy params AND the versioned hash in one replaceState", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/new/flamegraph.html",
      search: "?trace=demo-trace.bin",
      hash: "",
    });
    store.update("fgView", { workerZoom: ["main", "poll"] });
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual([
      "/new/flamegraph.html?trace=demo-trace.bin&worker-zoom=main%09poll#v=1&fg.w=main%09poll",
    ]);
  });

  it("coalesces a burst of updates into one debounced write", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/p",
      search: "",
      hash: "",
    });
    store.update("fgView", { workerZoom: ["a"] });
    raf.frame();
    store.update("fgView", { workerZoom: ["a", "b"] });
    raf.frame();
    store.update("fgView", { workerZoom: ["a", "b", "c"] });
    raf.frame();
    expect(timer.pendingCount()).toBe(1);
    timer.fire();
    expect(host.writes).toEqual(["/p?worker-zoom=a%09b%09c#v=1&fg.w=a%09b%09c"]);
  });

  it("clearing the zoom removes params and the whole hash (F152)", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/p",
      search: "?trace=t.bin&worker-zoom=a%09b",
      hash: "#v=1&fg.w=a%09b",
    });
    store.update("fgView", { workerZoom: [] });
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual(["/p?trace=t.bin"]);
  });

  it("skips no-op writes (URL already carries this state)", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/p",
      search: "?trace=t.bin&worker-zoom=a",
      hash: "#v=1&fg.w=a",
    });
    store.update("fgView", { workerZoom: ["a"] });
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual([]);
  });

  it("preserves unknown v=1 hash keys across rewrites", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/p",
      search: "",
      hash: "#v=1&vp=1-2&sel.task=42",
    });
    store.update("fgView", { workerZoom: ["x"] });
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual(["/p?worker-zoom=x#v=1&fg.w=x&vp=1-2&sel.task=42"]);
  });

  it("never overwrites a foreign hash (query mirror still applies)", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/p",
      search: "?trace=t.bin",
      hash: "#some-anchor",
    });
    store.update("fgView", { workerZoom: ["a"] });
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual(["/p?trace=t.bin&worker-zoom=a#some-anchor"]);
  });

  it("never overwrites a future-version hash", () => {
    const { raf, timer, host, store } = setup({
      pathname: "/p",
      search: "",
      hash: "#v=2&fg.w=reinterpreted",
    });
    store.update("fgView", { workerZoom: ["a"] });
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual(["/p?worker-zoom=a#v=2&fg.w=reinterpreted"]);
  });

  it("flush() writes pending state immediately; idle flush is a no-op", () => {
    const { raf, timer, host, store, binding } = setup({
      pathname: "/p",
      search: "",
      hash: "",
    });
    binding.flush(); // nothing pending
    expect(host.writes).toEqual([]);
    store.update("fgView", { workerZoom: ["a"] });
    raf.frame();
    binding.flush();
    expect(host.writes).toEqual(["/p?worker-zoom=a#v=1&fg.w=a"]);
    expect(timer.pendingCount()).toBe(0);
  });

  it("dispose() cancels the pending write and unsubscribes", () => {
    const { raf, timer, host, store, binding } = setup({
      pathname: "/p",
      search: "",
      hash: "",
    });
    store.update("fgView", { workerZoom: ["a"] });
    raf.frame();
    binding.dispose();
    timer.fire();
    store.update("fgView", { workerZoom: ["b"] });
    raf.frame();
    expect(timer.pendingCount()).toBe(0);
    expect(host.writes).toEqual([]);
  });
});
