import { afterEach, describe, expect, it, vi } from "vitest";
import { createActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import { mountSearchControls } from "./search-controls.js";
import { createBrowserStore } from "./state.js";

class Element extends EventTarget {
  value = "";
  placeholder = "";
  disabled = false;
  dataset: Record<string, string> = {};
  style = { cssText: "" };
  children: Element[] = [];
  private text = "";
  classList = { toggle: vi.fn() };

  get textContent(): string {
    return this.text;
  }

  set textContent(value: string) {
    this.text = value;
    this.children = [];
  }

  appendChild(child: Element): void {
    this.children.push(child);
  }

  querySelectorAll(): Element[] {
    return this.children;
  }
}

function input(value = ""): Element {
  return Object.assign(new Element(), { value });
}

function setup() {
  const replaceState = vi.fn();
  vi.stubGlobal("history", { replaceState, pushState: vi.fn() });
  vi.stubGlobal("window", {
    location: { pathname: "/index.html" },
    Dial9Creds: undefined,
    Dial9UrlState: {
      serialize: (state: { prefix: string; service: string }) =>
        new URLSearchParams({ prefix: state.prefix, service: state.service }).toString(),
    },
  });
  vi.stubGlobal("document", { createElement: () => new Element() });

  const elements = {
    bucketInput: input("traces-bucket"),
    prefixInput: input("checkout-traces"),
    serviceInput: input("checkout-api"),
    rangeFrom: input("2026-04-09T19:08"),
    rangeTo: input("2026-04-09T20:08"),
    rawSearchInput: input(),
    searchBtn: new Element(),
    prefixSuggestions: new Element(),
    quickBtns: [],
  };
  const store = createBrowserStore();
  const els = elements as unknown as BrowserEls;
  const actions = createActions(store, els);
  actions.mirrorPrefix();
  mountSearchControls({ store, els, actions });
  store.update("search", {
    suggestions: ["checkout-traces", "worker-traces"],
    activeSuggestion: "checkout-traces",
  });
  return { elements, store, actions, replaceState };
}

function response(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    headers: { "content-type": "application/json" },
  });
}

const objects = [{
  key: "checkout-traces/version=1/date=2026-04-09/service=checkout-api/" +
    "time=1930/instance=host-1/boot=demo/1775763000-0.bin.gz",
  size: 1,
  last_modified: "2026-04-09T19:31:00Z",
}];

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("prefix changes", () => {
  it.each(["input", "suggestion"] as const)(
    "clears the old service and discovers the new prefix after a %s edit",
    async (edit) => {
      const fetchMock = vi.fn(async (url: string) =>
        response(url.includes("/api/services")
          ? { services: ["worker-api", "worker-jobs"] }
          : { objects }),
      );
      vi.stubGlobal("fetch", fetchMock);
      const { elements, store, actions, replaceState } = setup();
      await actions.discoverServices();
      expect(store.getState().browse.activeService).toBe("checkout-api");
      expect(store.getState().browse.rows).toHaveLength(1);
      fetchMock.mockClear();
      // An auto-selected prefix under an implicit server-default bucket stays
      // out of the URL; replacing it by typing or a chip click is explicit.
      store.update("form", { bucketFromServerDefault: true, prefixAutoSelected: true });

      if (edit === "input") {
        elements.prefixInput.value = "worker-traces";
        elements.prefixInput.dispatchEvent(new Event("input"));
      } else {
        const chip = elements.prefixSuggestions.children.find(
          (child) => child.textContent === "worker-traces",
        )!;
        chip.dispatchEvent(new Event("click"));
      }

      expect(elements.serviceInput.value).toBe("");
      expect(store.getState().browse.activeService).toBeNull();
      expect(store.getState().browse.services).toEqual([]);
      expect(store.getState().browse.rows).toEqual([]);
      expect(store.getState().form.prefixAutoSelected).toBe(false);
      const url = new URL(replaceState.mock.lastCall![2], "http://localhost");
      expect(url.searchParams.get("prefix")).toBe("worker-traces");
      expect(url.searchParams.get("service")).toBe("");
      expect(fetchMock).not.toHaveBeenCalled();

      elements.searchBtn.dispatchEvent(new Event("click"));
      await vi.waitFor(() =>
        expect(store.getState().browse.services).toEqual(["worker-api", "worker-jobs"]),
      );
      expect(fetchMock).toHaveBeenCalledTimes(1);
      const request = new URL(fetchMock.mock.calls[0]![0], "http://localhost");
      expect(request.pathname).toBe("/api/services");
      expect(request.searchParams.get("prefix")).toBe("worker-traces");
      expect(request.searchParams.has("service")).toBe(false);
    },
  );

  it("keeps the selected service when the prefix only changes whitespace", async () => {
    vi.stubGlobal("fetch", vi.fn(async () =>
      response({ objects }),
    ));
    const { elements, store, actions } = setup();
    await actions.discoverServices();
    const rows = store.getState().browse.rows;

    elements.prefixInput.value = " checkout-traces ";
    elements.prefixInput.dispatchEvent(new Event("input"));

    expect(elements.serviceInput.value).toBe("checkout-api");
    expect(store.getState().browse.activeService).toBe("checkout-api");
    expect(store.getState().browse.rows).toBe(rows);
  });

  it.each(["success", "http error", "network error"] as const)(
    "ignores an old prefix's browse %s after the prefix changes",
    async (result) => {
      let resolveBrowse!: (value: Response) => void;
      let rejectBrowse!: (error: Error) => void;
      const fetchMock = vi.fn(() => new Promise<Response>((resolve, reject) => {
        resolveBrowse = resolve;
        rejectBrowse = reject;
      }));
      vi.stubGlobal("fetch", fetchMock);
      const { elements, store, actions } = setup();
      const browsing = actions.discoverServices();
      await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());

      elements.prefixInput.value = "worker-traces";
      elements.prefixInput.dispatchEvent(new Event("input"));
      const status = store.getState().browse.status;
      if (result === "network error") rejectBrowse(new Error("old request failed"));
      else resolveBrowse(result === "http error"
        ? new Response("old request failed", { status: 503 })
        : response({ objects }));
      await browsing;

      expect(elements.serviceInput.value).toBe("");
      expect(store.getState().browse.activeService).toBeNull();
      expect(store.getState().browse.rows).toEqual([]);
      expect(store.getState().browse.heatmapVisible).toBe(false);
      expect(store.getState().browse.status).toBe(status);
    },
  );
});
