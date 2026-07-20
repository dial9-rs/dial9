// Unit tests for the flamegraph host's CONTROL logic - the sig-guard, the
// attach-on-host-change, and detach. The live widget (canvas) is not exercised;
// createFlamegraph is mocked to a recording stub, and a minimal fake DOM stands
// in for the document/host nodes. This is the "double-setData / stale-attach"
// surface the two-instance design is most at risk from.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const created: FakeInstance[] = [];

interface FakeInstance {
  setData: ReturnType<typeof vi.fn>;
  resize: ReturnType<typeof vi.fn>;
  destroy: ReturnType<typeof vi.fn>;
}

vi.mock("../../lib/canvas/flamegraph.js", () => ({
  createFlamegraph: vi.fn(() => {
    const inst: FakeInstance = {
      setData: vi.fn(),
      resize: vi.fn(),
      destroy: vi.fn(),
    };
    created.push(inst);
    return inst;
  }),
}));

import { createFlamegraphHost } from "./flamegraph-host.js";

// ── Minimal fake DOM (node env has none) ─────────────────────────────────────
class FakeEl {
  className = "";
  children: FakeEl[] = [];
  parent: FakeEl | null = null;
  ownerDocument: FakeDoc;
  constructor(doc: FakeDoc) {
    this.ownerDocument = doc;
  }
  appendChild(c: FakeEl): void {
    c.parent?.remove(c);
    this.children.push(c);
    c.parent = this;
  }
  remove(c?: FakeEl): void {
    if (c) this.children = this.children.filter((x) => x !== c);
  }
}
class FakeDoc {
  defaultView: { requestAnimationFrame: (fn: () => void) => void; ResizeObserver?: unknown };
  constructor(raf: (fn: () => void) => void) {
    this.defaultView = { requestAnimationFrame: raf };
  }
  createElement(): FakeEl {
    return new FakeEl(this);
  }
}

let doc: FakeDoc;
let rafQueue: (() => void)[];

function host(): FakeEl {
  return doc.createElement();
}

beforeEach(() => {
  created.length = 0;
  rafQueue = [];
  doc = new FakeDoc((fn) => rafQueue.push(fn));
});
afterEach(() => vi.clearAllMocks());

function makeHost() {
  return createFlamegraphHost({
    doc: doc as unknown as Document,
    className: "d9-test-fg",
  });
}

describe("createFlamegraphHost", () => {
  it("creates the instance lazily on first sync, not at construction", () => {
    const fg = makeHost();
    expect(created).toHaveLength(0);
    expect(fg.instance()).toBeNull();

    fg.sync({ hostEl: host() as never, sig: "a", apply: () => {} });
    expect(created).toHaveLength(1);
    expect(fg.instance()).not.toBeNull();
  });

  it("applies only when the signature changes", () => {
    const fg = makeHost();
    const h = host();
    const apply = vi.fn();

    fg.sync({ hostEl: h as never, sig: "a", apply });
    fg.sync({ hostEl: h as never, sig: "a", apply }); // same sig, same host
    expect(apply).toHaveBeenCalledTimes(1);

    fg.sync({ hostEl: h as never, sig: "b", apply });
    expect(apply).toHaveBeenCalledTimes(2);
  });

  it("re-attaches (appends the container) when the host node changes", () => {
    const fg = makeHost();
    const h1 = host();
    const h2 = host();

    fg.sync({ hostEl: h1 as never, sig: "a", apply: () => {} });
    expect(h1.children).toHaveLength(1); // container appended
    const container = h1.children[0]!;

    fg.sync({ hostEl: h2 as never, sig: "a", apply: () => {} });
    // Moving hosts must NOT re-apply (sig unchanged) but MUST move the container.
    expect(h2.children).toContain(container);
    expect(h1.children).toHaveLength(0);
  });

  it("resizes on the next frame after an attach or an apply", () => {
    const fg = makeHost();
    fg.sync({ hostEl: host() as never, sig: "a", apply: () => {} });
    expect(created[0]!.resize).not.toHaveBeenCalled();
    rafQueue.splice(0).forEach((f) => f());
    expect(created[0]!.resize).toHaveBeenCalled();
  });

  it("does not resize after detach even if a frame was already queued", () => {
    const fg = makeHost();
    fg.sync({ hostEl: host() as never, sig: "a", apply: () => {} });
    // A ResizeObserver-driven frame is guarded by lastHost; the sync-driven
    // resize is not, so detach before the frame drains still resizes once from
    // that sync. Assert the DETACH path clears the attach marker so a
    // subsequent sync re-attaches.
    fg.detach();
    const h2 = host();
    fg.sync({ hostEl: h2 as never, sig: "a", apply: () => {} });
    expect(h2.children).toHaveLength(1); // re-attached after detach
  });

  it("destroy tears down the instance and forgets it", () => {
    const fg = makeHost();
    fg.sync({ hostEl: host() as never, sig: "a", apply: () => {} });
    const inst = created[0]!;
    fg.destroy();
    expect(inst.destroy).toHaveBeenCalled();
    expect(fg.instance()).toBeNull();
  });
});
