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
      resetPrefixForBucketChange: vi.fn(),
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
      resetPrefixForBucketChange: vi.fn(),
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

  it("opens the panel and picks nothing when several buckets are available", async () => {
    const creds = {
      has: vi.fn(() => false),
      get: vi.fn(() => ({ kind: "ambient" })),
      set: vi.fn(async () => ({ ok: true, error: null })),
      setRegion: vi.fn(),
      check: vi.fn(),
      listBuckets: vi.fn(async () => [
        { name: "dial9-traces-one", region: "us-east-1" },
        { name: "dial9-traces-two", region: "us-east-1" },
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
      resetPrefixForBucketChange: vi.fn(),
      discoverPrefixes: vi.fn(async () => {}),
      discoverServices,
    } as unknown as BrowserActions;
    const store = createBrowserStore();
    const els = fakeEls();
    const panel = mountCredsPanel({ store, els, actions });
    panel.init();

    els.credsAkid.value = "AK";
    els.credsSecret.value = "SK";
    els.credsApply.click();
    await drainMicrotasks();

    // Neither bucket is auto-selected — the user must choose — and the panel
    // is opened so the picker is in view.
    expect(els.bucketInput.value).toBe("");
    expect(store.getState().creds.panelOpen).toBe(true);
    expect(discoverServices).not.toHaveBeenCalled();
  });

  it.each([
    // Credentials scoped to a supplied bucket may be denied ListBuckets.
    { name: "skips the page-load bucket listing for a supplied bucket", bucket: "team-traces", lists: 0 },
    { name: "lists buckets on page load when no bucket is supplied", bucket: "", lists: 1 },
  ])("$name", async ({ bucket, lists }) => {
    const listBuckets = vi.fn(async () => [
      { name: "team-traces", region: "us-east-1" },
      { name: "other-traces", region: "us-east-1" },
    ]);
    const creds = {
      has: vi.fn(() => true),
      get: vi.fn(() => ({ kind: "role", roleArn: "arn:aws:iam::123456789012:role/R" })),
      listBuckets,
    } as unknown as Dial9CredsApi;
    vi.stubGlobal("window", { Dial9Creds: creds, addEventListener: vi.fn() });
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: () => new FakeElement(),
    });

    const actions = {
      syncUrl: vi.fn(),
      discoverPrefixes: vi.fn(async () => {}),
      discoverServices: vi.fn(async () => {}),
    } as unknown as BrowserActions;
    const store = createBrowserStore();
    const els = fakeEls();
    els.bucketInput.value = bucket;
    mountCredsPanel({ store, els, actions }).init();
    await drainMicrotasks();

    expect(listBuckets).toHaveBeenCalledTimes(lists);
    expect(store.getState().creds.panelOpen).toBe(false);
  });

  describe("Apply", () => {
    function setup(bucket: string, checkOk = true, fromServerDefault = false) {
      let active = false;
      let changed: Listener | undefined;
      // Like creds.js, every write dispatches the change event synchronously.
      const write = () => {
        active = true;
        changed?.();
      };
      const check = vi.fn(async () =>
        checkOk
          ? { ok: true, region: "eu-west-1", error: null }
          : { ok: false, region: null, error: "HTTP 403" },
      );
      const listBuckets = vi.fn(async () => [{ name: "dial9-traces", region: null }]);
      const creds = {
        has: vi.fn(() => active),
        get: vi.fn(() => ({ kind: "ambient" })),
        set: vi.fn(async () => {
          write();
          return { ok: true, error: null };
        }),
        setRegion: vi.fn(write),
        check,
        listBuckets,
      } as unknown as Dial9CredsApi;
      vi.stubGlobal("window", {
        Dial9Creds: creds,
        addEventListener: vi.fn((type: string, listener: Listener) => {
          if (type === "dial9:credentials-changed") changed = listener;
        }),
      });
      vi.stubGlobal("document", {
        createElement: () => new FakeElement(),
        createTextNode: () => new FakeElement(),
      });
      const actions = {
        syncUrl: vi.fn(),
        canRerunCurrentSearch: vi.fn(() => false),
        resetPrefixForBucketChange: vi.fn(),
        resetBrowsePane: vi.fn(),
        discoverPrefixes: vi.fn(async () => {}),
        discoverServices: vi.fn(async () => {}),
        requestPrefix: vi.fn(() => "traces/svc"),
      } as unknown as BrowserActions;
      const store = createBrowserStore();
      const els = fakeEls();
      els.bucketInput.value = bucket;
      store.update("form", { bucketFromServerDefault: fromServerDefault });
      mountCredsPanel({ store, els, actions }).init();
      els.credsAkid.value = "AK";
      els.credsSecret.value = "SK";
      return { store, els, actions, creds, check, listBuckets };
    }

    it.each([
      // Credentials scoped to a supplied source may be denied ListBuckets.
      {
        name: "checks a supplied bucket at the request prefix",
        bucket: "dial9-traces",
        fromServerDefault: false,
        lists: 0,
      },
      { name: "lists buckets when no bucket is supplied", bucket: "", fromServerDefault: false, lists: 1 },
      // The server default belongs to the ambient identity, even when the new
      // credentials can also see it.
      {
        name: "lists buckets instead of checking a server-default bucket",
        bucket: "dial9-traces",
        fromServerDefault: true,
        lists: 1,
      },
    ])("$name", async ({ bucket, fromServerDefault, lists }) => {
      const { store, els, actions, creds, check, listBuckets } = setup(
        bucket,
        true,
        fromServerDefault,
      );

      els.credsApply.click();
      await drainMicrotasks();

      expect(listBuckets).toHaveBeenCalledTimes(lists);
      expect(check).toHaveBeenCalledOnce();
      expect(check).toHaveBeenCalledWith("dial9-traces", { prefix: "traces/svc" });
      expect(els.bucketInput.value).toBe("dial9-traces");
      expect(store.getState().form.bucketFromServerDefault).toBe(false);
      // A prefix entered before the first explicit bucket is kept for it;
      // the server default's service and results are not.
      expect(actions.resetPrefixForBucketChange).toHaveBeenCalledWith(
        fromServerDefault ? "" : bucket,
        "dial9-traces",
      );
      expect(actions.resetBrowsePane).toHaveBeenCalledTimes(fromServerDefault ? 1 : 0);
      expect(creds.setRegion).toHaveBeenCalledWith("eu-west-1");
      // The panel's own writes do not start a second discovery.
      expect(actions.discoverServices).toHaveBeenCalledOnce();
    });

    it("keeps a rejected supplied bucket without listing buckets", async () => {
      const { store, els, actions, check, listBuckets } = setup("dial9-traces", false);

      els.credsApply.click();
      await drainMicrotasks();

      expect(check).toHaveBeenCalledOnce();
      expect(check).toHaveBeenCalledWith("dial9-traces", { prefix: "traces/svc" });
      expect(listBuckets).not.toHaveBeenCalled();
      expect(els.bucketInput.value).toBe("dial9-traces");
      expect(store.getState().creds.bucketsRowVisible).toBe(false);
      expect(store.getState().creds.status).toEqual({
        text: "Cannot use dial9-traces: HTTP 403",
        kind: "error",
      });
      expect(actions.discoverServices).not.toHaveBeenCalled();
    });
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
