import { afterEach, describe, expect, it, vi } from "vitest";
import { createActions } from "./actions.js";
import type { BrowserEls } from "./dom.js";
import { createBrowserStore, type HeatmapSegment } from "./state.js";
import { toRows } from "./segments.js";

function input(value = ""): HTMLInputElement {
  return { value } as HTMLInputElement;
}

function browserEls(service: string): BrowserEls {
  return {
    bucketInput: input("traces-bucket"),
    prefixInput: input("dial9-traces"),
    serviceInput: input(service),
    rangeFrom: input("2026-04-09T19:08"),
    rangeTo: input("2026-04-09T20:08"),
    rawSearchInput: input(),
  } as BrowserEls;
}

function browseResponse(): Response {
  return new Response(JSON.stringify({ objects: [{ key: "opaque", size: 1 }] }), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("prefix discovery", () => {
  it.each([
    { prefixes: ["version=1/"] },
    { prefixes: ["version=1/", "diagnostics/"] },
  ])(
    "keeps the prefix empty for a versioned layout at the bucket root",
    async ({ prefixes }) => {
      vi.stubGlobal("history", { replaceState: vi.fn(), pushState: vi.fn() });
      vi.stubGlobal("window", {
        location: { pathname: "/browser.html" },
        Dial9Creds: undefined,
        Dial9UrlState: { serialize: () => "" },
      });
      vi.stubGlobal(
        "fetch",
        vi.fn(async () =>
          new Response(JSON.stringify(prefixes), {
            status: 200,
            headers: { "content-type": "application/json" },
          }),
        ),
      );

      const store = createBrowserStore();
      const els = browserEls("");
      els.prefixInput.value = "";
      const actions = createActions(store, els);

      await actions.discoverPrefixes();

      expect(els.prefixInput.value).toBe("");
      expect(store.getState().search.suggestions).toEqual([]);
      expect(store.getState().search.prefixPlaceholder).toBe(
        "(no prefix — layout at root)",
      );
    },
  );
});

describe("URL service loading", () => {
  it("echoes the discovered layout hint and updates host count after browsing", async () => {
    vi.stubGlobal("history", { replaceState: vi.fn(), pushState: vi.fn() });
    vi.stubGlobal("window", {
      location: { pathname: "/browser.html" },
      Dial9Creds: undefined,
      Dial9UrlState: { serialize: () => "" },
    });
    vi.stubGlobal("alert", vi.fn());

    const key =
      "traces/version=1/date=2026-04-09/service=checkout-api/time=1930/" +
      "instance=host%2Fone/boot=boot/1775763000-0.bin.gz";
    let objects = [{ key, size: 1 }];
    const fetchMock = vi.fn(async (url: string) =>
      url.includes("/api/services")
        ? new Response(
            JSON.stringify({
              services: ["checkout-api"],
              service_metadata: [
                { service: "checkout-api", layout_hint: "opaque/hint" },
              ],
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          )
        : new Response(JSON.stringify({ objects }), {
            status: 200,
            headers: { "content-type": "application/json" },
          }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const store = createBrowserStore();
    const actions = createActions(store, browserEls(""));
    await actions.discoverServices();

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(fetchMock.mock.calls[1]![0]).toContain("service=checkout-api");
    expect(fetchMock.mock.calls[1]![0]).toContain("layout_hint=opaque%2Fhint");
    expect(store.getState().browse.serviceMetadata).toEqual([
      {
        service: "checkout-api",
        layout_hint: "opaque/hint",
        host_count: 1,
      },
    ]);

    objects = [];
    await actions.doTimeRangeSearch();
    expect(store.getState().browse.serviceMetadata).toEqual([
      {
        service: "checkout-api",
        layout_hint: "opaque/hint",
        host_count: 0,
      },
    ]);
  });

  it("skips discovery, preserves the service, and reloads an unlisted history service", async () => {
    const replaceState = vi.fn();
    vi.stubGlobal("history", { replaceState, pushState: vi.fn() });
    vi.stubGlobal("window", {
      location: { pathname: "/browser.html" },
      Dial9Creds: undefined,
      Dial9UrlState: {
        serialize: (state: { service?: string }) =>
          state.service ? `service=${encodeURIComponent(state.service)}` : "",
      },
    });
    // Typed arg so mock.calls[n][0] (the request URL) is indexable (TS2493).
    const fetchMock = vi.fn(async (_url: string) => browseResponse());
    vi.stubGlobal("fetch", fetchMock);
    vi.stubGlobal("alert", vi.fn());

    const store = createBrowserStore();
    const els = browserEls("checkout-api");
    const actions = createActions(store, els);

    await actions.discoverServices();

    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock.mock.calls[0]![0]).toContain("/api/browse?");
    expect(fetchMock.mock.calls[0]![0]).toContain("service=checkout-api");
    expect(fetchMock.mock.calls[0]![0]).not.toContain("/api/services");
    expect(store.getState().browse.activeService).toBe("checkout-api");
    expect(replaceState).toHaveBeenLastCalledWith(
      null,
      "",
      "/browser.html?service=checkout-api",
    );

    actions.selectService("worker", "replace");
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));

    expect(fetchMock.mock.calls[1]![0]).toContain("service=worker");
    expect(store.getState().browse.services).toEqual(["worker"]);
    expect(store.getState().browse.activeService).toBe("worker");
    expect(replaceState).toHaveBeenLastCalledWith(
      null,
      "",
      "/browser.html?service=worker",
    );
  });

  it("ignores a stale discovery response after history selects a service", async () => {
    const replaceState = vi.fn();
    vi.stubGlobal("history", { replaceState, pushState: vi.fn() });
    vi.stubGlobal("window", {
      location: { pathname: "/browser.html" },
      Dial9Creds: undefined,
      Dial9UrlState: {
        serialize: (state: { service?: string }) =>
          state.service ? `service=${encodeURIComponent(state.service)}` : "",
      },
    });
    vi.stubGlobal("alert", vi.fn());

    let resolveDiscovery!: (response: Response) => void;
    const delayedDiscovery = new Promise<Response>((resolve) => {
      resolveDiscovery = resolve;
    });
    const fetchMock = vi.fn((url: string) =>
      url.includes("/api/services")
        ? delayedDiscovery
        : Promise.resolve(browseResponse()),
    );
    vi.stubGlobal("fetch", fetchMock);

    const store = createBrowserStore();
    const els = browserEls("");
    const actions = createActions(store, els);
    const discovery = actions.discoverServices();
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));

    actions.selectService("worker", "replace");
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    resolveDiscovery(
      new Response(JSON.stringify({ services: ["api"] }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );
    await discovery;

    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(store.getState().browse.services).toEqual(["worker"]);
    expect(store.getState().browse.activeService).toBe("worker");
    expect(els.serviceInput.value).toBe("worker");
    expect(replaceState).toHaveBeenLastCalledWith(
      null,
      "",
      "/browser.html?service=worker",
    );
  });
});

describe("submitBrowseSearch validation", () => {
  function stubEnv() {
    vi.stubGlobal("history", { replaceState: vi.fn(), pushState: vi.fn() });
    vi.stubGlobal("window", {
      location: { pathname: "/browser.html" },
      Dial9Creds: undefined,
      Dial9UrlState: { serialize: () => "" },
    });
    const fetchMock = vi.fn(async () => browseResponse());
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("alerts and does not fetch when the bucket is empty", () => {
    const fetchMock = stubEnv();
    const alertMock = vi.fn();
    vi.stubGlobal("alert", alertMock);

    const els = browserEls("");
    els.bucketInput.value = "";
    const actions = createActions(createBrowserStore(), els);

    actions.submitBrowseSearch();

    expect(alertMock).toHaveBeenCalledWith("Bucket is required");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("alerts and does not fetch when the time range is unparseable", () => {
    const fetchMock = stubEnv();
    const alertMock = vi.fn();
    vi.stubGlobal("alert", alertMock);

    const els = browserEls("");
    els.rangeFrom.value = "";
    const actions = createActions(createBrowserStore(), els);

    actions.submitBrowseSearch();

    expect(alertMock).toHaveBeenCalledWith("Select a time range");
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("runs discovery when the bucket and range are valid", async () => {
    const fetchMock = stubEnv();
    const alertMock = vi.fn();
    vi.stubGlobal("alert", alertMock);

    const els = browserEls("checkout-api");
    const actions = createActions(createBrowserStore(), els);

    actions.submitBrowseSearch();
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));

    expect(alertMock).not.toHaveBeenCalled();
  });
});

describe("clearBrowseNoService", () => {
  it("invalidates an in-flight browse so a late response cannot repopulate the cleared pane", async () => {
    vi.stubGlobal("history", { replaceState: vi.fn(), pushState: vi.fn() });
    vi.stubGlobal("window", {
      location: { pathname: "/browser.html" },
      Dial9Creds: undefined,
      Dial9UrlState: { serialize: () => "" },
    });
    vi.stubGlobal("alert", vi.fn());

    let resolveBrowse!: (response: Response) => void;
    const delayedBrowse = new Promise<Response>((resolve) => {
      resolveBrowse = resolve;
    });
    const fetchMock = vi.fn(() => delayedBrowse);
    vi.stubGlobal("fetch", fetchMock);

    const store = createBrowserStore();
    const els = browserEls("checkout-api");
    const actions = createActions(store, els);

    // A browse search is in flight for the previously-selected service.
    const search = actions.doTimeRangeSearch();
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));

    // Back-navigation to a no-service entry clears the pane.
    actions.clearBrowseNoService();
    expect(store.getState().browse.activeService).toBeNull();
    expect(els.serviceInput.value).toBe("");

    // The stale response resolves; the generation guard must drop it so the
    // cleared no-service pane survives.
    resolveBrowse(browseResponse());
    await search;

    expect(store.getState().browse.activeService).toBeNull();
    expect(store.getState().browse.rows).toEqual([]);
    expect(store.getState().browse.status.text).toBe(
      "Choose a service to browse its traces.",
    );
  });
});

describe("heatmap segment selection", () => {
  it("ends at the next segment start instead of its delayed upload time", () => {
    const store = createBrowserStore();
    const selected: HeatmapSegment = {
      key: "traces/2026-04-09/1910/shale/h1/100-0.bin.gz",
      size: 1,
      start: 100,
      // The segment rotated at 110, but S3 completed its upload a second later.
      end: 111,
      layout: "known",
      service: "shale",
      host: "h1",
      bootId: "",
    };
    const next: HeatmapSegment = {
      ...selected,
      key: "traces/2026-04-09/1910/shale/h1/110-1.bin.gz",
      start: 110,
      end: 120,
    };
    const segments = [selected, next];
    store.update("browse", {
      segments,
      rows: toRows(segments),
      domain: { tMin: 100, tMax: 120 },
    });
    const els = browserEls("");
    Object.assign(els, { heatmapCanvas: { clientWidth: 100, style: {} } });

    createActions(store, els).selectSegmentAt(0, 0);

    expect(store.getState().browse.selection).toMatchObject({
      keys: [selected.key],
      t0: 100,
      t1: 110,
    });
  });
});
