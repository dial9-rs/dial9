// Integration tests for the flamegraph page's URL view-state wiring:
// restore-on-load at the vitest level (the widget is a recording fake; the
// in-browser twin against the real page is a separate parity journey). Covers:
// restore of the FULL view (zoom + inspect + search + filters) from legacy
// params / hash / both (hash precedence), the timeRangeMatched gate, zero URL
// writes on restore, the view->URL write carrying legacy params + hash
// together, Esc-reset cleanup, and context-param preservation.

import { describe, it, expect } from "vitest";
import type { FrameScheduler } from "../../store/store.js";
import type { DebounceTimer, UrlHost, UrlParts } from "../../lib/url/index.js";
import type { FlamegraphViewState } from "../../lib/canvas/index.js";
import { LEGACY_FIXTURE_URLS } from "../../lib/url/legacy-params.fixture.js";
import {
  createApiInspectSync,
  createFgUrlSync,
  restoreFgStateFromUrl,
} from "./view-state.js";

// Recording stand-in for the widget's view-state surface. `applied` records the
// silent restore(s); `getViewState` reports the widget's live state to the sync.
function fakeFg(): {
  applied: FlamegraphViewState[];
  setState: (s: FlamegraphViewState) => void;
  applyViewState: (s: FlamegraphViewState, opts?: { silent?: boolean }) => void;
  getViewState: () => FlamegraphViewState;
} {
  const applied: FlamegraphViewState[] = [];
  let live: FlamegraphViewState = {};
  return {
    applied,
    setState(s: FlamegraphViewState) {
      live = s;
    },
    applyViewState(s: FlamegraphViewState, _opts?: { silent?: boolean }) {
      applied.push(s);
      live = s;
    },
    getViewState: () => live,
  };
}

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
      const entries = [...pending.values()];
      pending.clear();
      for (const cb of entries) cb();
    },
    pendingCount: () => pending.size,
  };
}

function fakeHost(initial: UrlParts): UrlHost & { writes: string[] } {
  let cur = initial;
  const writes: string[] = [];
  return {
    writes,
    read: () => cur,
    replace(url) {
      writes.push(url);
      const u = new URL(url, "http://x");
      cur = { pathname: u.pathname, search: u.search, hash: u.hash };
    },
  };
}

describe("restoreFgStateFromUrl", () => {
  it("restores zoom from legacy query params (old links keep working)", () => {
    const fg = fakeFg();
    restoreFgStateFromUrl(
      { search: "?trace=t.bin&worker-zoom=main%09poll&offworker-zoom=blk", hash: "" },
      fg,
      true,
    );
    expect(fg.applied).toEqual([{ workerZoom: ["main", "poll"], offworkerZoom: ["blk"] }]);
  });

  it("restores the full view (inspect/search/filters) from legacy params", () => {
    const fg = fakeFg();
    restoreFgStateFromUrl(
      {
        search:
          "?trace=t.bin&worker-zoom=main&inspect=poll&inspect_full=core::poll" +
          "&search=tokio&spawn=src/main.rs:10&runtime=app",
        hash: "",
      },
      fg,
      true,
    );
    expect(fg.applied).toEqual([
      {
        workerZoom: ["main"],
        inspect: { name: "poll", fullName: "core::poll" },
        search: "tokio",
        spawn: "src/main.rs:10",
        runtime: "app",
      },
    ]);
  });

  it("restores the full view from the versioned hash", () => {
    const fg = fakeFg();
    restoreFgStateFromUrl(
      {
        search: "?trace=t.bin",
        hash: "#v=1&fg.w=a%09b&fg.i=poll&fg.if=core%3A%3Apoll&fg.s=tok",
      },
      fg,
      true,
    );
    expect(fg.applied).toEqual([
      {
        workerZoom: ["a", "b"],
        inspect: { name: "poll", fullName: "core::poll" },
        search: "tok",
      },
    ]);
  });

  it("hash wins per field when both are present", () => {
    const fg = fakeFg();
    restoreFgStateFromUrl(
      {
        search: "?worker-zoom=old&offworker-zoom=keep%09me&search=stale",
        hash: "#v=1&fg.w=new%09deep&fg.s=fresh",
      },
      fg,
      true,
    );
    expect(fg.applied).toEqual([
      {
        workerZoom: ["new", "deep"],
        offworkerZoom: ["keep", "me"],
        search: "fresh",
      },
    ]);
  });

  it("skips restore when the time range did not match", () => {
    const fg = fakeFg();
    const state = restoreFgStateFromUrl(
      { search: "?worker-zoom=a", hash: "#v=1&fg.o=b" },
      fg,
      false,
    );
    expect(fg.applied).toEqual([]);
    expect(state).toEqual({});
  });

  it("ignores foreign hashes (legacy params still restore)", () => {
    const fg = fakeFg();
    restoreFgStateFromUrl({ search: "?worker-zoom=a", hash: "#anchor" }, fg, true);
    expect(fg.applied).toEqual([{ workerZoom: ["a"] }]);
  });

  it("restores every recorded legacy fixture URL like the legacy reader", () => {
    for (const url of LEGACY_FIXTURE_URLS) {
      const fg = fakeFg();
      restoreFgStateFromUrl({ search: url, hash: "" }, fg, true);
      const p = new URLSearchParams(url);
      const expected: FlamegraphViewState = {};
      const wz = p.get("worker-zoom");
      if (wz !== null) expected.workerZoom = wz.split("\t");
      const oz = p.get("offworker-zoom");
      if (oz !== null) expected.offworkerZoom = oz.split("\t");
      const inspect = p.get("inspect");
      if (inspect !== null) {
        expected.inspect = {
          name: inspect,
          fullName: p.get("inspect_full") || inspect,
        };
      }
      expect(fg.applied, url).toEqual([expected]);
    }
  });
});

describe("createFgUrlSync (restore -> view -> share loop)", () => {
  function setup(url: UrlParts) {
    const fg = fakeFg();
    const raf = fakeRaf();
    const timer = fakeTimer();
    const host = fakeHost(url);
    const sync = createFgUrlSync(fg.getViewState, {
      host,
      timer,
      scheduler: raf.scheduler,
    });
    return { fg, raf, timer, host, sync };
  }

  it("restore-on-load produces ZERO url writes (like legacy)", () => {
    const url: UrlParts = {
      pathname: "/flamegraph.html",
      search: "?trace=t.bin&worker-zoom=main%09poll",
      hash: "",
    };
    const { fg, raf, timer, host } = setup(url);
    restoreFgStateFromUrl(host.read(), fg, true);
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual([]);
  });

  it("a user zoom after restore writes legacy params + hash, once", () => {
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/flamegraph.html",
      search: "?trace=t.bin&worker-zoom=main",
      hash: "",
    });
    restoreFgStateFromUrl(host.read(), fg, true);
    // User zooms deeper: the widget reports the live state.
    fg.setState({ workerZoom: ["main", "poll", "do_work"] });
    sync.onViewChange();
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual([
      "/flamegraph.html?trace=t.bin&worker-zoom=main%09poll%09do_work" +
        "#v=1&fg.w=main%09poll%09do_work",
    ]);
  });

  it("captures inspect + search + filters into legacy params and the hash", () => {
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/p",
      search: "?trace=t.bin",
      hash: "",
    });
    fg.setState({
      inspect: { name: "poll", fullName: "core::poll::poll" },
      search: "tokio",
      spawn: "src/main.rs:10",
      runtime: "app",
    });
    sync.onViewChange();
    raf.frame();
    timer.fire();
    expect(host.writes).toHaveLength(1);
    const u = new URL(host.writes[0]!, "http://x");
    // Legacy query mirror.
    expect(u.searchParams.get("inspect")).toBe("poll");
    expect(u.searchParams.get("inspect_full")).toBe("core::poll::poll");
    expect(u.searchParams.get("search")).toBe("tokio");
    expect(u.searchParams.get("spawn")).toBe("src/main.rs:10");
    expect(u.searchParams.get("runtime")).toBe("app");
    // Versioned hash carrier.
    const hash = new URLSearchParams(u.hash.slice(1));
    expect(hash.get("fg.i")).toBe("poll");
    expect(hash.get("fg.if")).toBe("core::poll::poll");
    expect(hash.get("fg.s")).toBe("tokio");
    expect(hash.get("fg.sp")).toBe("src/main.rs:10");
    expect(hash.get("fg.rt")).toBe("app");
  });

  it("Esc reset clears every view param and the hash", () => {
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/p",
      search: "?trace=t.bin&worker-zoom=a&offworker-zoom=b&search=q&inspect=poll",
      hash: "#v=1&fg.w=a&fg.o=b&fg.s=q&fg.i=poll",
    });
    fg.setState({});
    sync.onViewChange();
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual(["/p?trace=t.bin"]);
  });

  it("preserves every non-view context param on write", () => {
    const fixture = LEGACY_FIXTURE_URLS.find((url) => url.includes("trace=t/a.bin"))!;
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/flamegraph.html",
      search: fixture,
      hash: "",
    });
    restoreFgStateFromUrl(host.read(), fg, true);
    fg.setState({ workerZoom: ["main", "poll"] });
    sync.onViewChange();
    raf.frame();
    timer.fire();
    expect(host.writes).toHaveLength(1);
    const written = new URL(host.writes[0]!, "http://x");
    const before = new URLSearchParams(fixture);
    for (const [k, v] of before.entries()) {
      if (k === "worker-zoom" || k === "offworker-zoom") continue;
      expect(written.searchParams.getAll(k), k).toEqual(before.getAll(k));
    }
    expect(written.searchParams.get("worker-zoom")).toBe("main\tpoll");
    expect(written.hash).toBe("#v=1&fg.w=main%09poll");
  });

  it("flush() makes the URL current for copy-link mid-debounce", () => {
    const { fg, raf, host, sync, timer } = setup({
      pathname: "/p",
      search: "?trace=t.bin",
      hash: "",
    });
    fg.setState({ workerZoom: ["a"] });
    sync.onViewChange();
    raf.frame();
    expect(host.writes).toEqual([]); // still debouncing
    sync.flush();
    expect(host.writes).toEqual(["/p?trace=t.bin&worker-zoom=a#v=1&fg.w=a"]);
    expect(timer.pendingCount()).toBe(0);
  });
});

describe("createApiInspectSync", () => {
  function fakeApiFg() {
    let live: FlamegraphViewState = {};
    const available = new Set<string>();
    const applied: FlamegraphViewState[] = [];
    return {
      applied,
      makeAvailable(key: string) {
        available.add(key);
      },
      setState(state: FlamegraphViewState) {
        live = state;
      },
      getViewState: () => live,
      getInspectFocus: () => {
        const focus = live.inspect;
        return focus ? focus.fullName || focus.name : null;
      },
      applyViewState(state: FlamegraphViewState) {
        applied.push(state);
        live = { ...state };
        const focus = live.inspect;
        if (focus && !available.has(focus.fullName || focus.name)) delete live.inspect;
      },
    };
  }

  it("writes only inspection to the current api URL and removes it on exit", () => {
    const fg = fakeApiFg();
    const host = fakeHost({
      pathname: "/flamegraph.html",
      search: "?api=1&bucket=b",
      hash: "#foreign",
    });
    const sync = createApiInspectSync(new URLSearchParams(host.read().search), () => fg, {
      host,
    });

    fg.setState({ workerZoom: ["main"] });
    sync.onViewChange();
    expect(host.writes).toEqual([]);

    fg.setState({
      workerZoom: ["main"],
      inspect: { name: "poll", fullName: "core::poll" },
    });
    sync.onViewChange();
    expect(host.writes).toEqual([
      "/flamegraph.html?api=1&bucket=b&inspect=poll&inspect_full=core%3A%3Apoll#foreign",
    ]);

    fg.setState({});
    sync.onViewChange();
    expect(host.writes.at(-1)).toBe("/flamegraph.html?api=1&bucket=b#foreign");
  });

  it("retries URL restoration without clearing zoom or search", () => {
    const fg = fakeApiFg();
    fg.setState({ workerZoom: ["main"], search: "tokio" });
    const host = fakeHost({
      pathname: "/flamegraph.html",
      search: "?api=1&inspect=poll&inspect_full=core%3A%3Apoll",
      hash: "",
    });
    const sync = createApiInspectSync(new URLSearchParams(host.read().search), () => fg, {
      host,
    });

    expect(sync.restoreAfterTreeChange()).toBe(false);
    expect(fg.getViewState()).toEqual({ workerZoom: ["main"], search: "tokio" });

    fg.makeAvailable("core::poll");
    expect(sync.restoreAfterTreeChange()).toBe(true);
    expect(fg.getViewState()).toEqual({
      workerZoom: ["main"],
      search: "tokio",
      inspect: { name: "poll", fullName: "core::poll" },
    });
    expect(host.writes).toEqual([]);
  });

  it("carries live inspection through a scope change", () => {
    const fg = fakeApiFg();
    const host = fakeHost({
      pathname: "/flamegraph.html",
      search: "?api=1",
      hash: "",
    });
    const sync = createApiInspectSync(new URLSearchParams(host.read().search), () => fg, {
      host,
    });
    fg.setState({ inspect: { name: "poll", fullName: "core::poll" } });
    sync.preserveForTreeChange();
    fg.setState({ workerZoom: ["main"] });
    const rebuilt = new URLSearchParams("api=1&bucket=other");

    sync.carryTo(rebuilt);

    expect(rebuilt.toString()).toBe(
      "api=1&bucket=other&inspect=poll&inspect_full=core%3A%3Apoll",
    );
  });
});
