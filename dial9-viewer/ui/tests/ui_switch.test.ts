// Unit tests for the dual-UI switch decision logic (T38, ADR-0004 section 8).
//
// ui-switch.js is a plain browser script at the ui/ root (NOT frozen core;
// copied into dist/ via the static-copy list). Its pure decision section is
// loaded here through Node's native CJS loader (createRequire), the same
// pattern tests/core/*.test.ts use for the guarded-module.exports core files.
//
// Covered at the logic level: precedence order (?ui= param > stored
// preference > default), query-string preservation including repeated trace=
// params (N10 deep links), hash dropping (raw switch - no view-state
// porting), the no-registered-target behavior (no redirect, no control),
// the storage-throw fallback, click-time target resolution against the
// live location (T38-audit finding 1: the pages rewrite their query string
// after boot), legacy-pin storage alignment (T38-audit finding 2: the
// pages strip the ui=legacy pin), and the registry cycle guard plus the
// shipped-registry acyclicity walk (T38-audit finding 3). The DoD items
// that need a real migrated page
// (round-trip in a browser, zoom-state isolation) are pending T13/T14 - see
// HANDOFF.md.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

interface Control {
  label: string;
  href: string;
  target: "new" | "legacy";
}

interface Decision {
  redirect: string | null;
  control: Control | null;
}

interface DecideInput {
  side: "legacy" | "new";
  page: string | null;
  search: string;
  hash?: string;
  storedPref: string | null;
  registry: Record<string, string>;
  defaultUi: "new" | "legacy";
}

const {
  resolveUi,
  buildQuery,
  decide,
  canonicalPageFromPath,
  pageForNewEntry,
  prefFromStorage,
  liveControlHref,
  pinWouldBounce,
  NEW_UI_ENTRIES,
} = require("../ui-switch.js") as {
  resolveUi: (
    search: string,
    storedPref: string | null,
    defaultUi: string,
  ) => string;
  buildQuery: (search: string, targetUi: "new" | "legacy") => string;
  decide: (input: DecideInput) => Decision;
  canonicalPageFromPath: (pathname: string) => string | null;
  pageForNewEntry: (
    pathname: string,
    registry: Record<string, string>,
  ) => string | null;
  prefFromStorage: (storageLike: {
    getItem: (key: string) => string | null;
  }) => string | null;
  liveControlHref: (
    side: "legacy" | "new",
    bootPage: string | null,
    pathname: string,
    search: string,
    registry: Record<string, string>,
  ) => string | null;
  pinWouldBounce: (
    search: string,
    storedPref: string | null,
    defaultUi: "new" | "legacy",
  ) => boolean;
  NEW_UI_ENTRIES: Record<string, string>;
};

// A registry as T13/T14 would write it (the shipped one is empty today).
const REG = { "flamegraph.html": "new/flamegraph.html" };

// Baseline decide() input; tests override what they exercise.
function input(over: Partial<DecideInput>): DecideInput {
  return {
    side: "legacy",
    page: "flamegraph.html",
    search: "",
    storedPref: null,
    registry: REG,
    defaultUi: "legacy",
    ...over,
  };
}

describe("resolveUi precedence", () => {
  it("explicit ?ui=new beats a stored legacy preference", () => {
    expect(resolveUi("?ui=new", "legacy", "legacy")).toBe("new");
  });
  it("explicit ?ui=legacy beats a stored new preference", () => {
    expect(resolveUi("?ui=legacy", "new", "new")).toBe("legacy");
  });
  it("stored preference applies when no param", () => {
    expect(resolveUi("?trace=x", "new", "legacy")).toBe("new");
  });
  it("default applies when neither param nor preference", () => {
    expect(resolveUi("", null, "legacy")).toBe("legacy");
    expect(resolveUi("", null, "new")).toBe("new");
  });
  it("unknown param value falls through to the stored preference", () => {
    expect(resolveUi("?ui=shiny", "new", "legacy")).toBe("new");
  });
  it("unknown stored value falls through to the default", () => {
    expect(resolveUi("", "shiny", "legacy")).toBe("legacy");
  });
});

describe("query-string preservation", () => {
  it("keeps repeated trace= params (N10 deep links)", () => {
    expect(buildQuery("?trace=a.bin.gz&trace=b.bin.gz", "new")).toBe(
      "?trace=a.bin.gz&trace=b.bin.gz",
    );
  });
  it("removes the ui param when targeting the new UI", () => {
    expect(buildQuery("?ui=new&trace=a", "new")).toBe("?trace=a");
    expect(buildQuery("?ui=legacy&trace=a", "new")).toBe("?trace=a");
  });
  it("pins ui=legacy when targeting legacy (storage-independent return)", () => {
    expect(buildQuery("?trace=a", "legacy")).toBe("?trace=a&ui=legacy");
    // A stale ui param is replaced, not duplicated.
    expect(buildQuery("?ui=new&trace=a", "legacy")).toBe("?trace=a&ui=legacy");
  });
  it("empty query stays empty toward new, gains only ui=legacy toward legacy", () => {
    expect(buildQuery("", "new")).toBe("");
    expect(buildQuery("", "legacy")).toBe("?ui=legacy");
  });
  it("preserves param values verbatim (encoding-normalized)", () => {
    // URLSearchParams round-trip: %2F stays encoded; the VALUE is identical.
    expect(buildQuery("?trace=traces%2F2026%2Fseg-0.bin.gz", "new")).toBe(
      "?trace=traces%2F2026%2Fseg-0.bin.gz",
    );
  });
});

describe("legacy-side dispatch (canonical URL)", () => {
  it("?ui=new redirects to the registered entry, query preserved, ui removed", () => {
    const d = decide(input({ search: "?trace=a&ui=new&trace=b" }));
    expect(d.redirect).toBe("/new/flamegraph.html?trace=a&trace=b");
    expect(d.control).toBeNull();
  });
  it("stored new preference redirects without any param", () => {
    const d = decide(input({ search: "?trace=a", storedPref: "new" }));
    expect(d.redirect).toBe("/new/flamegraph.html?trace=a");
  });
  it("?ui=legacy overrides a stored new preference: stay, show control", () => {
    const d = decide(
      input({ search: "?trace=a&ui=legacy", storedPref: "new" }),
    );
    expect(d.redirect).toBeNull();
    expect(d.control).not.toBeNull();
    expect(d.control!.label).toBe("Switch to new UI");
  });
  it("default legacy: stay and offer the switch to the new UI", () => {
    const d = decide(input({ search: "?trace=a" }));
    expect(d.redirect).toBeNull();
    expect(d.control).toEqual({
      label: "Switch to new UI",
      href: "/new/flamegraph.html?trace=a",
      target: "new",
    });
  });
  it("flipped default (defaultUi=new) redirects; ?ui=legacy still pins legacy", () => {
    expect(decide(input({ defaultUi: "new" })).redirect).toBe(
      "/new/flamegraph.html",
    );
    const pinned = decide(input({ search: "?ui=legacy", defaultUi: "new" }));
    expect(pinned.redirect).toBeNull();
    expect(pinned.control!.label).toBe("Switch to new UI");
  });
});

describe("hash dropping (raw switch, no view-state porting)", () => {
  it("legacy -> new redirect carries no hash", () => {
    const d = decide(
      input({ search: "?trace=a&ui=new", hash: "#t0=1&t1=2&sel=42" }),
    );
    expect(d.redirect).toBe("/new/flamegraph.html?trace=a");
    expect(d.redirect).not.toContain("#");
  });
  it("switch-control hrefs carry no hash on either side", () => {
    const legacySide = decide(input({ hash: "#zoom=9" }));
    expect(legacySide.control!.href).not.toContain("#");
    const newSide = decide(
      input({ side: "new", search: "?trace=a", hash: "#viewport=3..9" }),
    );
    expect(newSide.control!.href).toBe("/flamegraph.html?trace=a&ui=legacy");
  });
});

describe("no registered target (unmigrated pages)", () => {
  it("the shipped registry holds exactly the migrated pages", () => {
    // Not a freeze: page tickets EXTEND this expectation when they
    // register their entry (one line there, one here). T13: flamegraph;
    // T14: browser page.
    expect(NEW_UI_ENTRIES["flamegraph.html"]).toBe("new/flamegraph.html");
    expect(NEW_UI_ENTRIES["index.html"]).toBe("new/index.html");
    expect(Object.keys(NEW_UI_ENTRIES).sort()).toEqual([
      "flamegraph.html",
      "index.html",
    ]);
  });
  it("no redirect and no control, even with ?ui=new", () => {
    const d = decide(
      input({ page: "viewer.html", search: "?ui=new", registry: {} }),
    );
    expect(d.redirect).toBeNull();
    expect(d.control).toBeNull();
  });
  it("no control with a stored new preference either", () => {
    const d = decide(input({ storedPref: "new", registry: {} }));
    expect(d.redirect).toBeNull();
    expect(d.control).toBeNull();
  });
  it("a page missing from a non-empty registry gets nothing", () => {
    const d = decide(input({ page: "viewer.html", search: "?ui=new" }));
    expect(d.redirect).toBeNull();
    expect(d.control).toBeNull();
  });
  it("a self-registration is treated as unregistered (no reload loop)", () => {
    const d = decide(
      input({
        search: "?ui=new",
        registry: { "flamegraph.html": "flamegraph.html" },
      }),
    );
    expect(d.redirect).toBeNull();
    expect(d.control).toBeNull();
  });
});

describe("registry cycle guard (T38-audit finding 3)", () => {
  // A cross-registration cycle between two canonical pages would
  // location.replace()-loop forever with no escape: buildQuery strips the
  // ui param on every new-bound hop, so not even ?ui=legacy survives into
  // the loop. New-UI entries live off-root, so a legitimate entry is never
  // itself a registry key - any entry that IS one (self- or
  // cross-registration) is a misconfiguration and must not dispatch.
  const CYCLIC = { "a.html": "b.html", "b.html": "a.html" };
  it("a two-page cross-registration cycle never dispatches from either node", () => {
    for (const page of ["a.html", "b.html"]) {
      const viaParam = decide(
        input({ page, search: "?ui=new", registry: CYCLIC }),
      );
      expect(viaParam.redirect).toBeNull();
      expect(viaParam.control).toBeNull();
      // The pref-driven variant is the one that loops pre-fix (the param
      // is stripped on the first hop, the stored pref keeps firing):
      const viaPref = decide(
        input({ page, search: "", storedPref: "new", registry: CYCLIC }),
      );
      expect(viaPref.redirect).toBeNull();
      expect(viaPref.control).toBeNull();
    }
  });
  it("an entry that is itself a registry key is rejected; innocent keys still dispatch", () => {
    const chain = { "a.html": "b.html", "b.html": "new/b.html" };
    const bad = decide(input({ page: "a.html", search: "?ui=new", registry: chain }));
    expect(bad.redirect).toBeNull();
    expect(bad.control).toBeNull();
    const good = decide(input({ page: "b.html", search: "?ui=new", registry: chain }));
    expect(good.redirect).toBe("/new/b.html");
  });
  it("the live href resolver applies the same guard", () => {
    expect(
      liveControlHref("legacy", "a.html", "/a.html", "?trace=x", CYCLIC),
    ).toBeNull();
  });
  it("the shipped registry is acyclic: no entry is a registry key", () => {
    // The registry is data; T13/T14/T41 add lines independently during
    // the wave. Walk it: an entry that is itself a key (self- or
    // cross-registration) could only produce a dispatch loop.
    const keys = Object.keys(NEW_UI_ENTRIES);
    for (const [page, entry] of Object.entries(NEW_UI_ENTRIES)) {
      expect(keys, `entry for ${page} must not be a registry key`).not.toContain(entry);
    }
  });
});

describe("new side", () => {
  it("offers the way back with ui=legacy pinned and query preserved", () => {
    const d = decide(
      input({ side: "new", search: "?trace=a&trace=b", storedPref: "new" }),
    );
    expect(d.redirect).toBeNull();
    expect(d.control).toEqual({
      label: "Switch to legacy UI",
      href: "/flamegraph.html?trace=a&trace=b&ui=legacy",
      target: "legacy",
    });
  });
  it("never dispatches, even with ?ui=legacy in the query", () => {
    const d = decide(input({ side: "new", search: "?ui=legacy" }));
    expect(d.redirect).toBeNull();
    expect(d.control!.href).toBe("/flamegraph.html?ui=legacy");
  });
  it("renders nothing when the canonical page is unknown", () => {
    const d = decide(input({ side: "new", page: null }));
    expect(d.redirect).toBeNull();
    expect(d.control).toBeNull();
  });
});

describe("round trip keeps the trace loaded (logic level)", () => {
  it("legacy -> new -> legacy preserves the trace params throughout", () => {
    const start = "?trace=a.bin.gz&trace=b.bin.gz";
    const toNew = decide(input({ search: start + "&ui=new" }));
    expect(toNew.redirect).toBe(
      "/new/flamegraph.html?trace=a.bin.gz&trace=b.bin.gz",
    );
    const newSearch = toNew.redirect!.split("?")[1]!;
    const back = decide(input({ side: "new", search: "?" + newSearch }));
    expect(back.control!.href).toBe(
      "/flamegraph.html?trace=a.bin.gz&trace=b.bin.gz&ui=legacy",
    );
    // Landing on the canonical URL with that query stays legacy even if the
    // stored preference still says new (param precedence, storage-free).
    const landed = decide(
      input({
        search: "?trace=a.bin.gz&trace=b.bin.gz&ui=legacy",
        storedPref: "new",
      }),
    );
    expect(landed.redirect).toBeNull();
    expect(landed.control!.label).toBe("Switch to new UI");
  });
});

describe("page resolution helpers", () => {
  it("canonicalPageFromPath maps root-level pages, '/' is index.html", () => {
    expect(canonicalPageFromPath("/")).toBe("index.html");
    expect(canonicalPageFromPath("")).toBe("index.html");
    expect(canonicalPageFromPath("/viewer.html")).toBe("viewer.html");
    expect(canonicalPageFromPath("/tokio_stats.html")).toBe("tokio_stats.html");
  });
  it("off-root and non-.html paths are not canonical (new entries no-op)", () => {
    expect(canonicalPageFromPath("/new/flamegraph.html")).toBeNull();
    expect(canonicalPageFromPath("/api/traces")).toBeNull();
  });
  it("pageForNewEntry reverse-maps a new entry path to its canonical page", () => {
    expect(pageForNewEntry("/new/flamegraph.html", REG)).toBe(
      "flamegraph.html",
    );
    expect(pageForNewEntry("/new/other.html", REG)).toBeNull();
  });
});

describe("click-time target resolution (T38-audit finding 1)", () => {
  // Every legacy page rewrites its query string after boot
  // (history.replaceState/pushState of a rebuilt query: flamegraph browser
  // scope and worker zoom, tokio_stats hosts/periods, index browse state,
  // viewer start/end), so a control href computed once at boot goes stale
  // and would navigate with the boot-time query - losing the current
  // trace/scope. The browser layer re-resolves the href from the LIVE
  // location on mousedown/click via liveControlHref.
  it("legacy side: resolves from the live query after a simulated replaceState, not the boot query", () => {
    const bootSearch = "?bucket=b&host=all";
    const bootHref = decide(input({ search: bootSearch })).control!.href;
    expect(bootHref).toBe("/new/flamegraph.html?bucket=b&host=all");
    // The page narrows scope and rewrites its URL (the replaceState):
    const liveSearch = "?bucket=b&host=h1&period=custom";
    const href = liveControlHref(
      "legacy",
      "flamegraph.html",
      "/flamegraph.html",
      liveSearch,
      REG,
    );
    expect(href).toBe("/new/flamegraph.html?bucket=b&host=h1&period=custom");
    expect(href).not.toBe(bootHref);
  });
  it("new side: the live query is carried back with ui=legacy pinned", () => {
    const href = liveControlHref(
      "new",
      "flamegraph.html",
      "/new/flamegraph.html",
      "?trace=z.bin.gz",
      REG,
    );
    expect(href).toBe("/flamegraph.html?trace=z.bin.gz&ui=legacy");
  });
  it("re-resolves the page from the live pathname (an SPA pushState may move it)", () => {
    const reg2 = {
      "flamegraph.html": "new/flamegraph.html",
      "tokio_stats.html": "new/tokio_stats.html",
    };
    const href = liveControlHref(
      "legacy",
      "flamegraph.html",
      "/tokio_stats.html",
      "?host=h1",
      reg2,
    );
    expect(href).toBe("/new/tokio_stats.html?host=h1");
  });
  it("falls back to the boot page when the live pathname resolves nothing", () => {
    const href = liveControlHref(
      "new",
      "flamegraph.html",
      "/new/spa/deep/route",
      "?trace=a",
      REG,
    );
    expect(href).toBe("/flamegraph.html?trace=a&ui=legacy");
  });
  it("returns null when no target resolves (caller keeps the previous href)", () => {
    expect(
      liveControlHref("legacy", "viewer.html", "/viewer.html", "?trace=a", REG),
    ).toBeNull();
    expect(
      liveControlHref("new", null, "/new/unknown.html", "", REG),
    ).toBeNull();
  });
  it("owns the live ui param: stripped toward new, replaced toward legacy", () => {
    expect(
      liveControlHref(
        "legacy",
        "flamegraph.html",
        "/flamegraph.html",
        "?ui=legacy&trace=a",
        REG,
      ),
    ).toBe("/new/flamegraph.html?trace=a");
    expect(
      liveControlHref(
        "new",
        "flamegraph.html",
        "/new/flamegraph.html",
        "?ui=new&trace=a",
        REG,
      ),
    ).toBe("/flamegraph.html?trace=a&ui=legacy");
  });
});

describe("legacy-pin storage alignment (T38-audit finding 2)", () => {
  // Three of the four legacy pages rebuild their query string from scratch
  // and drop the unknown `ui` param, so the ui=legacy pin cannot be relied
  // on to survive the first in-page URL sync. When the pin is the ONLY
  // thing keeping the visitor on legacy (pinWouldBounce), the browser layer
  // writes "legacy" to the stored preference at boot, so a stripped pin
  // plus a reload still resolves legacy.
  it("detects the audit's bounce precondition: pin present, stored pref says new", () => {
    expect(pinWouldBounce("?trace=a&ui=legacy", "new", "legacy")).toBe(true);
  });
  it("middle-click scenario: once storage is aligned, a stripped pin no longer bounces", () => {
    // From the new UI, the switch is opened in a new tab (middle-click
    // skips the click handler, so storage still says "new"). The legacy
    // page boots pinned and stays legacy by param precedence:
    const boot = "?trace=a&ui=legacy";
    expect(resolveUi(boot, "new", "legacy")).toBe("legacy");
    // Pre-fix: the page's first URL sync strips the pin; a reload then
    // dispatches on storedPref=new straight back to the new UI:
    expect(resolveUi("?trace=a", "new", "legacy")).toBe("new"); // the bounce
    // The fix: the pin was load-bearing at boot, so the browser layer
    // aligned storage to "legacy". The same pin-less reload now stays:
    expect(pinWouldBounce(boot, "new", "legacy")).toBe(true);
    expect(resolveUi("?trace=a", "legacy", "legacy")).toBe("legacy");
  });
  it("no alignment when the visitor already resolves legacy without the pin", () => {
    // A shared ?ui=legacy link must not sticky-switch a recipient whose
    // preference/default is already legacy (mirrors write-on-click-only).
    expect(pinWouldBounce("?ui=legacy", null, "legacy")).toBe(false);
    expect(pinWouldBounce("?ui=legacy&trace=a", "legacy", "legacy")).toBe(
      false,
    );
    expect(pinWouldBounce("?ui=legacy", "legacy", "new")).toBe(false);
  });
  it("no alignment without an explicit ui=legacy pin", () => {
    expect(pinWouldBounce("?trace=a", "new", "legacy")).toBe(false);
    expect(pinWouldBounce("", "new", "legacy")).toBe(false);
    expect(pinWouldBounce("?ui=new", "new", "legacy")).toBe(false);
    expect(pinWouldBounce("?ui=shiny", "new", "legacy")).toBe(false);
  });
  it("post-flip: the pin is load-bearing for a no-preference visitor too", () => {
    // After DEFAULT_UI flips to "new", a pinned visitor with no stored
    // preference would bounce once the pin is stripped - align then too.
    expect(pinWouldBounce("?ui=legacy", null, "new")).toBe(true);
  });
});

describe("storage fallback", () => {
  it("a throwing storage reads as no preference", () => {
    expect(
      prefFromStorage({
        getItem: () => {
          throw new Error("denied (private mode)");
        },
      }),
    ).toBeNull();
  });
  it("valid values pass through, garbage reads as no preference", () => {
    expect(prefFromStorage({ getItem: () => "new" })).toBe("new");
    expect(prefFromStorage({ getItem: () => "legacy" })).toBe("legacy");
    expect(prefFromStorage({ getItem: () => "shiny" })).toBeNull();
    expect(prefFromStorage({ getItem: () => null })).toBeNull();
  });
  it("no preference + no param = the default UI decides", () => {
    // The end-to-end consequence of a storage failure: DEFAULT_UI wins.
    expect(resolveUi("?trace=a", null, "legacy")).toBe("legacy");
  });
});
