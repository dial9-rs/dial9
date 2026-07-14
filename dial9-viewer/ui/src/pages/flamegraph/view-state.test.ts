// Integration tests for the flamegraph page's URL view-state wiring:
// restore-on-load at the vitest level (the widget is a recording fake; the
// in-browser twin against the real page is a separate parity journey). Covers:
// restore from legacy params / hash / both (hash precedence), the
// timeRangeMatched gate, zero URL writes on restore, the zoom->URL write
// carrying legacy params + hash together, Esc-reset cleanup, and context-param
// preservation over the recorded legacy fixture URLs.

import { describe, it, expect } from "vitest";
import type { FrameScheduler } from "../../store/store.js";
import type { DebounceTimer, UrlHost, UrlParts } from "../../lib/url/index.js";
import { LEGACY_FIXTURE_URLS } from "../../lib/url/legacy-params.fixture.js";
import {
  createFgUrlSync,
  restoreZoomFromUrl,
  type FgZoomPaths,
} from "./view-state.js";

// Recording stand-in for the widget's zoom surface.
function fakeFg(): {
  calls: Array<[string, readonly string[]]>;
  paths: FgZoomPaths;
  zoomToPath: (key: "worker" | "offworker", names: readonly string[]) => void;
  getZoomPath: () => FgZoomPaths;
} {
  const calls: Array<[string, readonly string[]]> = [];
  const self = {
    calls,
    paths: { worker: [], offworker: [] } as FgZoomPaths,
    zoomToPath(key: "worker" | "offworker", names: readonly string[]) {
      calls.push([key, names]);
      const arr = [...names];
      if (key === "worker") self.paths.worker = arr;
      else self.paths.offworker = arr;
    },
    getZoomPath: () => self.paths,
  };
  return self;
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

describe("restoreZoomFromUrl", () => {
  it("restores from legacy query params (old links keep working)", () => {
    const fg = fakeFg();
    restoreZoomFromUrl(
      { search: "?trace=t.bin&worker-zoom=main%09poll&offworker-zoom=blk", hash: "" },
      fg,
      true,
    );
    expect(fg.calls).toEqual([
      ["worker", ["main", "poll"]],
      ["offworker", ["blk"]],
    ]);
  });

  it("restores from the versioned hash", () => {
    const fg = fakeFg();
    restoreZoomFromUrl({ search: "?trace=t.bin", hash: "#v=1&fg.w=a%09b" }, fg, true);
    expect(fg.calls).toEqual([["worker", ["a", "b"]]]);
  });

  it("hash wins per field when both are present", () => {
    const fg = fakeFg();
    restoreZoomFromUrl(
      {
        search: "?worker-zoom=old&offworker-zoom=keep%09me",
        hash: "#v=1&fg.w=new%09deep",
      },
      fg,
      true,
    );
    expect(fg.calls).toEqual([
      ["worker", ["new", "deep"]],
      ["offworker", ["keep", "me"]],
    ]);
  });

  it("skips restore when the time range did not match", () => {
    const fg = fakeFg();
    const state = restoreZoomFromUrl(
      { search: "?worker-zoom=a", hash: "#v=1&fg.o=b" },
      fg,
      false,
    );
    expect(fg.calls).toEqual([]);
    expect(state).toEqual({});
  });

  it("ignores foreign hashes (legacy params still restore)", () => {
    const fg = fakeFg();
    restoreZoomFromUrl({ search: "?worker-zoom=a", hash: "#anchor" }, fg, true);
    expect(fg.calls).toEqual([["worker", ["a"]]]);
  });

  it("restores every recorded legacy fixture URL like the legacy reader", () => {
    for (const url of LEGACY_FIXTURE_URLS) {
      const fg = fakeFg();
      restoreZoomFromUrl({ search: url, hash: "" }, fg, true);
      const p = new URLSearchParams(url);
      const expected: Array<[string, readonly string[]]> = [];
      const wz = p.get("worker-zoom");
      if (wz !== null) expected.push(["worker", wz.split("\t")]);
      const oz = p.get("offworker-zoom");
      if (oz !== null) expected.push(["offworker", oz.split("\t")]);
      expect(fg.calls, url).toEqual(expected);
    }
  });
});

describe("createFgUrlSync (restore -> zoom -> share loop)", () => {
  function setup(url: UrlParts) {
    const fg = fakeFg();
    const raf = fakeRaf();
    const timer = fakeTimer();
    const host = fakeHost(url);
    const sync = createFgUrlSync(fg.getZoomPath, {
      host,
      timer,
      scheduler: raf.scheduler,
    });
    return { fg, raf, timer, host, sync };
  }

  it("restore-on-load produces ZERO url writes (like legacy)", () => {
    const url: UrlParts = {
      pathname: "/new/flamegraph.html",
      search: "?trace=t.bin&worker-zoom=main%09poll",
      hash: "",
    };
    const { fg, raf, timer, host } = setup(url);
    restoreZoomFromUrl(host.read(), fg, true);
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual([]);
  });

  it("a user zoom after restore writes legacy params + hash, once", () => {
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/new/flamegraph.html",
      search: "?trace=t.bin&worker-zoom=main",
      hash: "",
    });
    restoreZoomFromUrl(host.read(), fg, true);
    // User zooms deeper: the widget fires onZoomChange with live stacks.
    fg.paths.worker = ["main", "poll", "do_work"];
    sync.onZoomChange();
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual([
      "/new/flamegraph.html?trace=t.bin&worker-zoom=main%09poll%09do_work" +
        "#v=1&fg.w=main%09poll%09do_work",
    ]);
  });

  it("Esc reset clears both zoom params and the hash", () => {
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/p",
      search: "?trace=t.bin&worker-zoom=a&offworker-zoom=b",
      hash: "#v=1&fg.w=a&fg.o=b",
    });
    fg.paths.worker = [];
    fg.paths.offworker = [];
    sync.onZoomChange();
    raf.frame();
    timer.fire();
    expect(host.writes).toEqual(["/p?trace=t.bin"]);
  });

  it("preserves every non-zoom context param on write", () => {
    const fixture = LEGACY_FIXTURE_URLS[3]!; // the full context-param fixture URL
    const { fg, raf, timer, host, sync } = setup({
      pathname: "/new/flamegraph.html",
      search: fixture,
      hash: "",
    });
    restoreZoomFromUrl(host.read(), fg, true);
    fg.paths.worker = ["main", "poll"];
    sync.onZoomChange();
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
    fg.paths.worker = ["a"];
    sync.onZoomChange();
    raf.frame();
    expect(host.writes).toEqual([]); // still debouncing
    sync.flush();
    expect(host.writes).toEqual(["/p?trace=t.bin&worker-zoom=a#v=1&fg.w=a"]);
    expect(timer.pendingCount()).toBe(0);
  });
});
