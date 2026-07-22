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
  it("uses a listed af-south-1 region without probing the bucket", async () => {
    const setRegion = vi.fn();
    const check = vi.fn();
    const creds = {
      has: vi.fn(() => false),
      get: vi.fn(() => null),
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
    const reRunCurrentSearch = vi.fn();
    const actions = {
      syncUrl: vi.fn(),
      discoverPrefixes,
      reRunCurrentSearch,
      isAutoSearched: vi.fn(() => false),
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
    expect(reRunCurrentSearch).toHaveBeenCalledOnce();
  });
});
