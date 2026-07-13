// Tests for the load-chrome state machine. DOM-free: the controller is driven
// directly, the load handle is a scripted fake (the same shape loadTraceInWorker
// returns), and the elapsed timer uses fake timers.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  createLoadController,
  progressLabel,
  initialUrlLabel,
  loadErrorMessage,
  NEW_FILE_CONFIRM_MESSAGE,
  type LoadController,
  type LoadControllerDeps,
  type LoadHandle,
  type StartLoadOptions,
} from "./load-controller.js";
import { ESC_PRIORITY } from "./esc-cascade.js";
import type { TraceWorkerProgress } from "../../lib/trace/index.js";

// ── A scripted load handle: the test plays the worker side ─────────────────

interface FakeLoad extends LoadHandle {
  resolve(): void;
  reject(err: unknown): void;
  emitProgress(p: TraceWorkerProgress): void;
  aborted: boolean;
  opts: StartLoadOptions;
  urls: readonly string[];
}

function makeFakeLoad(urls: readonly string[], opts: StartLoadOptions): FakeLoad {
  let resolveDone!: () => void;
  let rejectDone!: (err: unknown) => void;
  const done = new Promise<void>((res, rej) => {
    resolveDone = res;
    rejectDone = rej;
  });
  done.catch(() => {}); // fire-and-forget friendliness, like the real handle
  return {
    done,
    urls,
    opts,
    aborted: false,
    abort(): void {
      this.aborted = true;
      rejectDone(new DOMException("aborted", "AbortError"));
    },
    resolve: () => resolveDone(),
    reject: (err) => rejectDone(err),
    emitProgress: (p) => opts.onProgress(p),
  };
}

function progress(over: Partial<TraceWorkerProgress> = {}): TraceWorkerProgress {
  return {
    phase: "parsing",
    mode: "stream",
    urlCount: 1,
    bytesRead: 0,
    totalBytes: null,
    eventCount: 0,
    startMs: 0,
    elapsedMs: 0,
    ...over,
  };
}

interface Harness {
  ctrl: LoadController;
  loads: FakeLoad[];
  changes: number;
  errors: string[];
  confirmCalls: string[];
  setHasTrace(v: boolean): void;
  setConfirmResult(v: boolean): void;
}

function makeHarness(over: Partial<LoadControllerDeps> = {}): Harness {
  const loads: FakeLoad[] = [];
  const state = { hasTrace: false, confirmResult: true, changes: 0 };
  const errors: string[] = [];
  const confirmCalls: string[] = [];
  const deps: LoadControllerDeps = {
    hasTrace: () => state.hasTrace,
    startLoad: (urls, opts) => {
      const load = makeFakeLoad(urls, opts);
      loads.push(load);
      return load;
    },
    confirm: (msg) => {
      confirmCalls.push(msg);
      return state.confirmResult;
    },
    onError: (msg) => errors.push(msg),
    onChange: () => {
      state.changes += 1;
    },
    now: () => 0,
    ...over,
  };
  const ctrl = createLoadController(deps);
  return {
    ctrl,
    loads,
    get changes() {
      return state.changes;
    },
    errors,
    confirmCalls,
    setHasTrace: (v) => {
      state.hasTrace = v;
    },
    setConfirmResult: (v) => {
      state.confirmResult = v;
    },
  };
}

// ── Pure label helpers ──────────────────────────────────────────────────────

describe("progressLabel (B8 keep-exactly)", () => {
  it("fetching: singular and multi-trace labels", () => {
    expect(progressLabel(progress({ phase: "fetching", urlCount: 1 }))).toBe("Fetching...");
    expect(progressLabel(progress({ phase: "fetching", urlCount: 3 }))).toBe(
      "Fetching 3 traces...",
    );
  });

  it("parsing with a known total shows a percentage", () => {
    expect(
      progressLabel(progress({ bytesRead: 500, totalBytes: 1000, eventCount: 12_345 })),
    ).toBe("Parsing: 50% - 12k events");
  });

  it("parsing while streaming (total unknown) shows MB decoded", () => {
    expect(
      progressLabel(
        progress({ bytesRead: 2 * 1024 * 1024, totalBytes: null, eventCount: 3200 }),
      ),
    ).toBe("Parsing: 2.0 MB - 3k events");
  });

  it("initialUrlLabel covers single vs multi", () => {
    expect(initialUrlLabel(1)).toBe("Loading trace...");
    expect(initialUrlLabel(4)).toBe("Loading 4 traces...");
  });
});

describe("loadErrorMessage (B13)", () => {
  it("gives the credentials hint on HTTP 401 with a creds module present", () => {
    expect(loadErrorMessage("HTTP 401 fetching /api/object", true)).toMatch(
      /requires AWS credentials/,
    );
  });
  it("passes through other errors", () => {
    expect(loadErrorMessage("bad magic", false)).toBe("Could not load trace: bad magic");
    // 401 without a creds module is a plain error, not the hint.
    expect(loadErrorMessage("HTTP 401", false)).toBe("Could not load trace: HTTP 401");
  });
});

// ── Section lifecycle ──────────────────────────────────────────────────────

describe("createLoadController: resting state", () => {
  it("starts in the chooser with no trace and nothing dismissible", () => {
    const h = makeHarness();
    expect(h.ctrl.getState()).toMatchObject({
      section: "chooser",
      hasTrace: false,
      canDismiss: false,
      dragActive: false,
    });
  });

  it("registers at the load esc priority, below help and above popups", () => {
    const h = makeHarness();
    expect(h.ctrl.escSurface.priority).toBe(ESC_PRIORITY.load);
    expect(ESC_PRIORITY.load).toBeLessThan(ESC_PRIORITY.help);
    expect(ESC_PRIORITY.load).toBeGreaterThan(ESC_PRIORITY.popup);
  });
});

describe("URL / demo loads (B6/B12) feed the worker and close on success", () => {
  it("loadUrls starts a load, tracks progress, and closes when it resolves", async () => {
    const h = makeHarness();
    h.ctrl.loadUrls(["/a", "/b"], initialUrlLabel(2));
    expect(h.ctrl.getState().section).toBe("loading");
    expect(h.ctrl.getState().progressLabel).toBe("Loading 2 traces...");
    expect(h.loads).toHaveLength(1);
    expect(h.loads[0]?.urls).toEqual(["/a", "/b"]);

    h.loads[0]?.emitProgress(progress({ phase: "fetching", urlCount: 2 }));
    expect(h.ctrl.getState().progressLabel).toBe("Fetching 2 traces...");

    h.setHasTrace(true);
    h.loads[0]?.resolve();
    await h.loads[0]?.done;
    await Promise.resolve();
    expect(h.ctrl.getState().section).toBe("closed");
  });

  it("loadDemo requests the bundled demo url", () => {
    const h = makeHarness();
    h.ctrl.loadDemo();
    expect(h.loads[0]?.urls).toEqual(["/demo-trace.bin"]);
  });

  it("attaches credential headers to URL loads but not file loads (B17)", () => {
    const h = makeHarness({ headers: () => ({ "x-dial9-aws-key": "k" }) });
    h.ctrl.loadUrls(["/a"], initialUrlLabel(1));
    expect(h.loads[0]?.opts.headers).toEqual({ "x-dial9-aws-key": "k" });

    h.ctrl.loadFile(fakeFile("t.bin"));
    expect(h.loads[1]?.opts.headers).toBeUndefined();
  });

  it("surfaces a load failure and returns to the chooser (B13)", async () => {
    const h = makeHarness({ credsMissing: () => true });
    h.ctrl.loadUrls(["/a"], initialUrlLabel(1));
    h.loads[0]?.reject(new Error("HTTP 401 fetching /a"));
    await flush();
    expect(h.errors[0]).toMatch(/requires AWS credentials/);
    expect(h.ctrl.getState().section).toBe("chooser");
  });
});

// ── File drop / pick ────────────────────────────────────────────────────────

describe("file loads via object URL (B2/B3)", () => {
  it("creates an object URL, loads it, and revokes on settle", async () => {
    const created: string[] = [];
    const revoked: string[] = [];
    const h = makeHarness({
      createObjectUrl: () => {
        const url = `blob:${created.length}`;
        created.push(url);
        return url;
      },
      revokeObjectUrl: (u) => revoked.push(u),
    });
    h.ctrl.loadFile(fakeFile("my-trace.bin.gz"));
    expect(h.ctrl.getState().progressLabel).toBe("Loading my-trace.bin.gz...");
    expect(h.loads[0]?.urls).toEqual(["blob:0"]);
    expect(revoked).toEqual([]);

    h.setHasTrace(true);
    h.loads[0]?.resolve();
    await flush();
    expect(revoked).toEqual(["blob:0"]); // revoked only after the load settled
  });
});

// ── New File confirms before opening over a loaded trace ────────────────────

describe("requestNewFile (S3 confirm amendment)", () => {
  it("confirms before opening the chooser when a trace is loaded", async () => {
    const h = makeHarness();
    await loadATrace(h);
    expect(h.ctrl.getState().section).toBe("closed");

    h.setConfirmResult(false);
    h.ctrl.requestNewFile();
    expect(h.confirmCalls).toEqual([NEW_FILE_CONFIRM_MESSAGE]);
    expect(h.ctrl.getState().section).toBe("closed"); // declined -> no change

    h.setConfirmResult(true);
    h.ctrl.requestNewFile();
    expect(h.ctrl.getState()).toMatchObject({ section: "chooser", canDismiss: true });
  });

  it("is a no-op while the section is already open", async () => {
    const h = makeHarness();
    await loadATrace(h);
    h.ctrl.requestNewFile(); // opens chooser
    h.confirmCalls.length = 0;
    h.ctrl.requestNewFile(); // already open -> no second confirm
    expect(h.confirmCalls).toEqual([]);
  });
});

// ── The chooser is dismissible (Esc + close control), reopen works ──────────

describe("dismissal (#281 amendment)", () => {
  it("Esc/close dismisses the New-File chooser back to the trace, and reopening works", async () => {
    const h = makeHarness();
    await loadATrace(h);

    // Open via New File.
    h.ctrl.requestNewFile();
    expect(h.ctrl.getState().section).toBe("chooser");
    expect(h.ctrl.escSurface.isOpen()).toBe(true); // dismissible (trace behind)

    // Close via the visible control (dismiss) OR the esc cascade (escSurface).
    h.ctrl.escSurface.close();
    expect(h.ctrl.getState().section).toBe("closed"); // back to the trace

    // Reopen.
    h.ctrl.requestNewFile();
    expect(h.ctrl.getState().section).toBe("chooser");
    h.ctrl.dismiss();
    expect(h.ctrl.getState().section).toBe("closed");
  });

  it("the boot chooser (no trace behind) is NOT dismissible", () => {
    const h = makeHarness(); // hasTrace stays false
    expect(h.ctrl.getState()).toMatchObject({ section: "chooser", canDismiss: false });
    expect(h.ctrl.escSurface.isOpen()).toBe(false); // Esc falls through
    h.ctrl.dismiss();
    expect(h.ctrl.getState().section).toBe("chooser"); // cannot dismiss to nothing
  });

  it("a failed/cancelled replace keeps the old trace resident (S3 recovery)", async () => {
    const h = makeHarness();
    await loadATrace(h); // hasTrace true, section closed
    h.ctrl.requestNewFile(); // chooser over the trace
    h.ctrl.loadFile(fakeFile("new.bin")); // replace attempt
    expect(h.ctrl.getState().section).toBe("loading");

    // Cancel: the worker never wrote the trace slice, so hasTrace is still
    // true and the chooser stays dismissible back to the old trace.
    h.ctrl.cancel();
    expect(h.ctrl.getState()).toMatchObject({ section: "chooser", canDismiss: true });
    h.ctrl.dismiss();
    expect(h.ctrl.getState().section).toBe("closed");
  });
});

// ── Esc during loading cancels ──────────────────────────────────────────────

describe("cancel (B10/B11)", () => {
  it("Esc during a load aborts it and returns to the chooser", () => {
    const h = makeHarness();
    h.ctrl.loadUrls(["/a"], initialUrlLabel(1));
    expect(h.ctrl.escSurface.isOpen()).toBe(true); // loading is always escapable
    h.ctrl.escSurface.close(); // Esc
    expect(h.loads[0]?.aborted).toBe(true);
    expect(h.ctrl.getState().section).toBe("chooser");
  });

  it("a superseding load aborts the previous in-flight one", () => {
    const h = makeHarness();
    h.ctrl.loadUrls(["/a"], initialUrlLabel(1));
    h.ctrl.loadUrls(["/b"], initialUrlLabel(1));
    expect(h.loads[0]?.aborted).toBe(true);
    expect(h.loads).toHaveLength(2);
  });
});

// ── Drag feedback counter ───────────────────────────────────────────────────

describe("drag feedback counter", () => {
  it("tracks nested dragenter/dragleave and resets on drop", () => {
    const h = makeHarness();
    h.ctrl.dragEnter();
    expect(h.ctrl.getState().dragActive).toBe(true);
    h.ctrl.dragEnter(); // nested (child element)
    h.ctrl.dragLeave();
    expect(h.ctrl.getState().dragActive).toBe(true); // still over the window
    h.ctrl.dragLeave();
    expect(h.ctrl.getState().dragActive).toBe(false);

    h.ctrl.dragEnter();
    h.ctrl.endDrag(); // a drop clears it regardless of depth
    expect(h.ctrl.getState().dragActive).toBe(false);
  });
});

// ── Live elapsed timer ──────────────────────────────────────────────────────

describe("elapsed timer (B9)", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("ticks the elapsed suffix ~4x/second while loading and stops on settle", async () => {
    let clock = 0;
    const h = makeHarness({ now: () => clock });
    h.ctrl.loadUrls(["/a"], initialUrlLabel(1));
    expect(h.ctrl.getState().elapsedLabel).toBe(" - 0.0s");

    clock = 1300;
    vi.advanceTimersByTime(250); // one tick -> re-render
    expect(h.ctrl.getState().elapsedLabel).toBe(" - 1.3s");

    h.setHasTrace(true);
    h.loads[0]?.resolve();
    await vi.runAllTimersAsync();
    // Closed: no more elapsed suffix, and the interval is cleared.
    expect(h.ctrl.getState().section).toBe("closed");
    expect(h.ctrl.getState().elapsedLabel).toBe("");
  });
});

// ── helpers ────────────────────────────────────────────────────────────────

function fakeFile(name: string): Blob & { name: string } {
  return Object.assign(new Blob([new Uint8Array([1, 2, 3])]), { name });
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

async function loadATrace(h: Harness): Promise<void> {
  h.ctrl.loadUrls(["/boot"], initialUrlLabel(1));
  h.setHasTrace(true);
  h.loads[h.loads.length - 1]?.resolve();
  await flush();
}
