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
// porting), the no-registered-target behavior (no redirect, no control), and
// the storage-throw fallback. The DoD items that need a real migrated page
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

describe("no registered target (the shipped state today)", () => {
  it("the shipped registry is empty until T13/T14/T41 register pages", () => {
    // Not a freeze: page tickets REPLACE this expectation when they
    // register their entry (one line there, one here).
    expect(Object.keys(NEW_UI_ENTRIES)).toEqual([]);
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
