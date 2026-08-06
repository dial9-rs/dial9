import { afterEach, describe, expect, it, vi } from "vitest";
import type { Dial9CredsApi } from "../../lib/trace/creds.js";
import type { BrowserActions } from "./actions.js";
import { mountCredsPanel } from "./creds-panel.js";
import type { BrowserEls } from "./dom.js";
import { createBrowserStore } from "./state.js";

type Listener = () => void;

class FakeElement {
  value = "";
  style = { display: "", cssText: "" };
  className = "";
  children: FakeElement[] = [];
  private text = "";
  private readonly classes = new Set<string>();
  private readonly listeners = new Map<string, Listener[]>();

  readonly classList = {
    add: (name: string) => this.classes.add(name),
    remove: (name: string) => this.classes.delete(name),
    toggle: (name: string, force?: boolean) => {
      const enabled = force ?? !this.classes.has(name);
      if (enabled) this.classes.add(name);
      else this.classes.delete(name);
      return enabled;
    },
    contains: (name: string) => this.classes.has(name),
  };

  get textContent(): string {
    return this.text;
  }

  set textContent(value: string | null) {
    this.text = value ?? "";
    this.children = [];
  }

  addEventListener(type: string, listener: Listener): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  appendChild(child: FakeElement): FakeElement {
    this.children.push(child);
    return child;
  }

  append(...children: FakeElement[]): void {
    this.children.push(...children);
  }

  focus(): void {}

  click(): void {
    for (const listener of this.listeners.get("click") ?? []) listener();
  }
}

function fakeEls(): BrowserEls {
  const el = () => new FakeElement();
  return {
    credsBtn: el(),
    credsBtnLabel: el(),
    credsPanel: el(),
    credsClose: el(),
    credsModeSummary: el(),
    credsUseLiteral: el(),
    credsLiteralFields: el(),
    credsPaste: el(),
    credsPasteFill: el(),
    credsAkid: el(),
    credsSecret: el(),
    credsToken: el(),
    credsRegion: el(),
    credsApply: el(),
    credsClear: el(),
    credsStatus: el(),
    credsBucketsRow: el(),
    credsBuckets: el(),
    bucketInput: el(),
    serviceInput: el(),
  } as unknown as BrowserEls;
}

async function drainMicrotasks(): Promise<void> {
  for (let i = 0; i < 8; i += 1) await Promise.resolve();
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("mountCredsPanel", () => {
  it("renders role identity without key fields and can switch to literal", async () => {
    const roleArn = "arn:aws:iam::123456789012:role/Dial9TraceReader";
    const setLiteralMode = vi.fn(() => ({
      kind: "literal",
      accessKeyId: "",
      secretAccessKey: "",
    }));
    const creds = {
      has: vi.fn(() => true),
      get: vi.fn(() => ({ kind: "role", roleArn, region: "us-west-2" })),
      setLiteralMode,
      listBuckets: vi.fn(async () => []),
    } as unknown as Dial9CredsApi;
    vi.stubGlobal("window", {
      Dial9Creds: creds,
      addEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: () => new FakeElement(),
    });

    const store = createBrowserStore();
    const els = fakeEls();
    const panel = mountCredsPanel({
      store,
      els,
      actions: { syncUrl: vi.fn() } as unknown as BrowserActions,
    });
    panel.init();
    await drainMicrotasks();

    expect(els.credsLiteralFields.style.display).toBe("none");
    expect(els.credsUseLiteral.style.display).toBe("");
    expect(els.credsModeSummary.textContent).toContain(roleArn);
    // Userscript-controlled fields remain mounted even though their wrapper is hidden.
    expect(els.credsAkid).toBeDefined();
    expect(els.credsApply).toBeDefined();

    els.credsUseLiteral.click();
    await drainMicrotasks();
    expect(setLiteralMode).toHaveBeenCalledOnce();
    expect(els.credsLiteralFields.style.display).toBe("");
    expect(store.getState().source.credentials.kind).toBe("literal");
  });

  it("uses a listed af-south-1 region without probing the bucket", async () => {
    const setRegion = vi.fn();
    const check = vi.fn();
    const creds = {
      has: vi.fn(() => false),
      get: vi.fn(() => ({ kind: "ambient" })),
      set: vi.fn(async () => ({ ok: true, error: null })),
      setRegion,
      check,
      listBuckets: vi.fn(async () => [
        { name: "dial9-cape-town", region: "af-south-1" },
      ]),
      clear: vi.fn(),
      parse: vi.fn(),
    } as unknown as Dial9CredsApi;
    vi.stubGlobal("window", {
      Dial9Creds: creds,
      addEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: (text: string) => {
        const node = new FakeElement();
        node.textContent = text;
        return node;
      },
    });

    const discoverPrefixes = vi.fn(async () => {});
    const discoverServices = vi.fn(async () => {});
    const actions = {
      syncUrl: vi.fn(),
      discoverPrefixes,
      discoverServices,
    } as unknown as BrowserActions;
    const els = fakeEls();
    const panel = mountCredsPanel({
      store: createBrowserStore(),
      els,
      actions,
    });
    panel.init();

    els.credsAkid.value = "AK";
    els.credsSecret.value = "SK";
    els.credsApply.click();
    await drainMicrotasks();

    expect(els.bucketInput.value).toBe("dial9-cape-town");
    expect(els.credsRegion.value).toBe("af-south-1");
    expect(setRegion).toHaveBeenCalledWith("af-south-1");
    expect(check).not.toHaveBeenCalled();
    expect(discoverPrefixes).toHaveBeenCalledOnce();
    expect(discoverServices).toHaveBeenCalledOnce();
  });

  it("discovers services after credentials supply the only bucket", async () => {
    const creds = {
      has: vi.fn(() => false),
      get: vi.fn(() => ({ kind: "ambient" })),
      set: vi.fn(async () => ({ ok: true, error: null })),
      setRegion: vi.fn(),
      check: vi.fn(),
      listBuckets: vi.fn(async () => [
        { name: "dial9-traces", region: "us-east-1" },
      ]),
      clear: vi.fn(),
      parse: vi.fn(),
    } as unknown as Dial9CredsApi;
    vi.stubGlobal("window", {
      Dial9Creds: creds,
      addEventListener: vi.fn(),
    });
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: (text: string) => {
        const node = new FakeElement();
        node.textContent = text;
        return node;
      },
    });

    const discoverServices = vi.fn(async () => {});
    const actions = {
      syncUrl: vi.fn(),
      discoverPrefixes: vi.fn(async () => {}),
      discoverServices,
    } as unknown as BrowserActions;
    const els = fakeEls();
    const panel = mountCredsPanel({
      store: createBrowserStore(),
      els,
      actions,
    });
    panel.init();

    els.credsAkid.value = "AK";
    els.credsSecret.value = "SK";
    els.credsApply.click();
    await drainMicrotasks();

    expect(els.bucketInput.value).toBe("dial9-traces");
    expect(discoverServices).toHaveBeenCalledOnce();
  });

  it("discovers services when userscript credentials arrive for a picked bucket", async () => {
    let active = false;
    let credentialsChanged: Listener | undefined;
    const creds = {
      has: vi.fn(() => active),
      get: vi.fn(() => ({ kind: "ambient" })),
    } as unknown as Dial9CredsApi;
    vi.stubGlobal("window", {
      Dial9Creds: creds,
      addEventListener: vi.fn((type: string, listener: Listener) => {
        if (type === "dial9:credentials-changed") credentialsChanged = listener;
      }),
    });
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: (text: string) => {
        const node = new FakeElement();
        node.textContent = text;
        return node;
      },
    });

    const discoverPrefixes = vi.fn(async () => {});
    const discoverServices = vi.fn(async () => {});
    const actions = {
      canRerunCurrentSearch: vi.fn(() => false),
      reRunCurrentSearch: vi.fn(),
      discoverPrefixes,
      discoverServices,
    } as unknown as BrowserActions;
    const els = fakeEls();
    els.bucketInput.value = "dial9-traces";
    const panel = mountCredsPanel({
      store: createBrowserStore(),
      els,
      actions,
    });
    panel.init();

    active = true;
    expect(credentialsChanged).toBeDefined();
    credentialsChanged!();
    await drainMicrotasks();

    expect(actions.canRerunCurrentSearch).toHaveBeenCalledOnce();
    expect(actions.reRunCurrentSearch).not.toHaveBeenCalled();
    expect(discoverPrefixes).toHaveBeenCalledOnce();
    expect(discoverServices).toHaveBeenCalledOnce();
  });
});
