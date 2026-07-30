import { afterEach, describe, expect, it, vi } from "vitest";
import type { BrowserActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import { mountServiceTabs } from "./service-tabs.js";
import { createBrowserStore } from "./state.js";

type Listener = () => void;

class FakeElement {
  role = "";
  type = "";
  className = "";
  dataset: Record<string, string> = {};
  style = { display: "" };
  children: FakeElement[] = [];
  private text = "";
  private readonly classes = new Set<string>();
  private readonly listeners = new Map<string, Listener[]>();

  readonly classList = {
    toggle: (name: string, force?: boolean) => {
      const enabled = force ?? !this.classes.has(name);
      if (enabled) this.classes.add(name);
      else this.classes.delete(name);
      return enabled;
    },
    contains: (name: string) => this.classes.has(name),
  };

  get textContent(): string {
    return this.text + this.children.map((child) => child.textContent).join("");
  }

  set textContent(value: string | null) {
    this.text = value ?? "";
    this.children = [];
  }

  append(...children: FakeElement[]): void {
    this.children.push(...children);
  }

  appendChild(child: FakeElement): FakeElement {
    this.children.push(child);
    return child;
  }

  addEventListener(type: string, listener: Listener): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  setAttribute(): void {}

  querySelectorAll(): FakeElement[] {
    return this.children;
  }

  click(): void {
    for (const listener of this.listeners.get("click") ?? []) listener();
  }
}

async function flushStore(): Promise<void> {
  await Promise.resolve();
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("service tabs", () => {
  it("shows host counts, including for one auto-focused service", async () => {
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: (text: string) => {
        const node = new FakeElement();
        node.textContent = text;
        return node;
      },
    });

    const store = createBrowserStore();
    const serviceTabs = new FakeElement();
    const selectService = vi.fn();
    mountServiceTabs({
      store,
      els: { serviceTabs } as unknown as BrowserEls,
      actions: { selectService } as unknown as BrowserActions,
    });

    store.update("browse", {
      services: ["api"],
      serviceMetadata: [{ service: "api", host_count: 1 }],
      activeService: "api",
    });
    await flushStore();

    expect(serviceTabs.style.display).toBe("");
    expect(serviceTabs.children).toHaveLength(1);
    expect(serviceTabs.children[0]!.textContent).toBe("api1 host");
    expect(serviceTabs.children[0]!.classList.contains("active")).toBe(true);

    serviceTabs.children[0]!.click();
    expect(selectService).toHaveBeenCalledWith("api");
  });

  it("uses the plural host label", async () => {
    vi.stubGlobal("document", {
      createElement: () => new FakeElement(),
      createTextNode: (text: string) => {
        const node = new FakeElement();
        node.textContent = text;
        return node;
      },
    });

    const store = createBrowserStore();
    const serviceTabs = new FakeElement();
    mountServiceTabs({
      store,
      els: { serviceTabs } as unknown as BrowserEls,
      actions: { selectService: vi.fn() } as unknown as BrowserActions,
    });

    store.update("browse", {
      services: ["api"],
      serviceMetadata: [{ service: "api", host_count: 2 }],
    });
    await flushStore();

    expect(serviceTabs.children[0]!.textContent).toBe("api2 hosts");
  });
});
