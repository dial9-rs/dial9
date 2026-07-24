// The URL-contract pin. The contract doc is dial9-viewer/ui/README.md, section
// "URL contract (stable deep-link API)"; this suite keeps that section, the
// recorded legacy-param fixture, url_state.js, and the view-state codec in
// lockstep:
//
// - the schema VERSION is pinned (a bump is a contract event, not a detail);
// - every param the README tables document is exactly what the recorded
//   fixtures/serializers know, per table (renaming a table heading or a
//   param name in the doc fails here BY DESIGN - the doc is the API);
// - hash keys documented "live" round-trip through the codec; keys
//   documented "reserved" are preserved-verbatim-but-not-honored (the
//   tolerant-reader rule that makes "reserved" an honest status);
// - the recipe URL shapes resolve at codec level (the live-page twin is
//   parity/url-contract.mjs, which drives real pages on a dev-server).
//
// The promise this enforces: old params stay valid forever, evolution is
// additive-only. Removing or renaming ANY name below is a breaking change and
// must not happen; adding one means updating the README section, the ledger,
// and this suite in the same PR.

import { describe, it, expect } from "vitest";
import { createRequire } from "node:module";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import {
  VIEW_STATE_VERSION,
  decodeViewState,
  encodeViewState,
  resolveViewState,
} from "./view-state.js";
import { FLAMEGRAPH_LEGACY_PARAMS } from "./legacy-params.fixture.js";

const require = createRequire(import.meta.url);
const uiDir = fileURLToPath(new URL("../../../", import.meta.url));

const UrlState = require(path.join(uiDir, "url_state.js")) as {
  parse(search: string): Record<string, unknown>;
  serialize(state: Record<string, unknown>): string;
};

// ── README section parsing ───────────────────────────────────────────────

const readme = fs.readFileSync(path.join(uiDir, "README.md"), "utf8");

/** The whole "## URL contract" section (up to the next h2). */
function contractSection(): string {
  const m = readme.match(/^## URL contract[^\n]*\n([\s\S]*?)(?=^## )/m);
  expect(m, 'README.md must contain a "## URL contract" section').not.toBeNull();
  return m![1]!;
}

/** One "### <heading>" subsection of the contract section. */
function subsection(headingPrefix: string): string {
  const section = contractSection();
  const lines = section.split("\n");
  const start = lines.findIndex((l) => l.startsWith(`### ${headingPrefix}`));
  expect(
    start,
    `contract section must contain a "### ${headingPrefix}" subsection`,
  ).toBeGreaterThanOrEqual(0);
  let end = lines.length;
  for (let i = start + 1; i < lines.length; i++) {
    if (lines[i]!.startsWith("### ")) {
      end = i;
      break;
    }
  }
  return lines.slice(start + 1, end).join("\n");
}

/** Param/key names from a markdown table: first cell of each `| `x` |` row. */
function tableNames(md: string): string[] {
  const names: string[] = [];
  for (const line of md.split("\n")) {
    const m = line.match(/^\| `([^`]+)`/);
    if (m) names.push(m[1]!);
  }
  return names;
}

const sorted = (xs: Iterable<string>) => [...xs].sort();

// ── Version pin ──────────────────────────────────────────────────────────

describe("URL contract: schema version", () => {
  it("view-state schema version is pinned at 1", () => {
    // Bumping this is an incompatible reinterpretation of an existing key
    // (expected: never, per the schema doc's version rules). If you are
    // here because VIEW_STATE_VERSION changed: that is a contract event -
    // update docs/ui-inventory/05-url-view-state.md, the README contract
    // section, and the reader-behavior expectations below first.
    expect(VIEW_STATE_VERSION).toBe(1);
  });

  it("the README documents the pinned version", () => {
    expect(subsection("Hash")).toContain("currently `1`");
  });
});

// ── Doc <-> fixture lockstep, per table ──────────────────────────────────

describe("URL contract: README tables match the recorded fixtures", () => {
  it("exact-mode table = flamegraph fixture exact params + viewer-only prof", () => {
    const documented = tableNames(subsection("Query params - viewer.html"));
    const fixture = FLAMEGRAPH_LEGACY_PARAMS.filter((p) => p.mode === "exact").map(
      (p) => p.param,
    );
    expect(sorted(documented)).toEqual(sorted([...fixture, "prof"]));
  });

  it("api-mode table = flamegraph fixture api params", () => {
    const documented = tableNames(subsection("Query params - flamegraph.html"));
    const fixture = FLAMEGRAPH_LEGACY_PARAMS.filter((p) => p.mode === "api").map(
      (p) => p.param,
    );
    expect(sorted(documented)).toEqual(sorted(fixture));
  });

  it("browser-page table = url_state.js serialization surface", () => {
    const documented = tableNames(subsection("Query params - index.html"));
    // url_state.js's two mutually-exclusive range forms together cover the
    // full wire vocabulary; the union is the serialization surface.
    const full = {
      bucket: "b",
      region: "eu-west-1",
      prefix: "p",
      service: "svc",
      tab: "raw",
      tz: "local",
      q: "query",
    };
    const withLast = new URLSearchParams(UrlState.serialize({ ...full, last: 24 }));
    const withRange = new URLSearchParams(
      UrlState.serialize({ ...full, from: 1700000000, to: 1700003600 }),
    );
    const wire = new Set([...withLast.keys(), ...withRange.keys()]);
    expect(sorted(documented)).toEqual(sorted(wire));
  });

  it("every-page table is exactly the ui switch param", () => {
    expect(tableNames(subsection("Query params - every page"))).toEqual(["ui"]);
  });

  it("hash table = v + live/defined codec keys + reserved keys", () => {
    const documented = tableNames(subsection("Hash"));
    // Live + defined keys: what the codec encodes today. Derived from the
    // codec's behavior (not a copied list) so the doc pins the code.
    const encoded = new URLSearchParams(
      encodeViewState({
        fgWorkerZoom: ["a"],
        fgOffworkerZoom: ["b"],
        fgInspect: "poll",
        fgInspectFull: "core::poll::poll",
        fgSearch: "tokio",
        fgSpawn: "src/main.rs:10",
        fgRuntime: "app",
        timeMode: "abs",
        timeZone: "utc",
      }),
    );
    const codecKeys = [...encoded.keys()]; // includes "v"
    const reserved = ["vp", "sel.*", "poi"];
    expect(sorted(documented)).toEqual(sorted([...codecKeys, ...reserved]));
  });
});

// ── Reserved keys: preserved verbatim, never honored ─────────────────────

describe("URL contract: reserved hash keys", () => {
  const reservedPayload = "#v=1&vp=100-200&sel.task=3&poi=p2";

  it("decode does not honor reserved keys as state", () => {
    const decoded = decodeViewState(reservedPayload);
    expect(decoded).not.toBeNull();
    expect(decoded!.version).toBe(VIEW_STATE_VERSION);
    // No state field materializes from a reserved key today. When one is
    // implemented, it moves the key to "live" in the README table and
    // REMOVES it from this list in the same PR.
    expect(decoded!.state).toEqual({});
  });

  it("rewrite preserves reserved keys verbatim (tolerant reader)", () => {
    const decoded = decodeViewState(reservedPayload)!;
    expect(decoded.unknown).toEqual([
      ["vp", "100-200"],
      ["sel.task", "3"],
      ["poi", "p2"],
    ]);
    const reEncoded = encodeViewState(decoded.state, decoded.unknown);
    const p = new URLSearchParams(reEncoded);
    expect(p.get("vp")).toBe("100-200");
    expect(p.get("sel.task")).toBe("3");
    expect(p.get("poi")).toBe("p2");
  });
});

// ── The recipe URL shapes, at codec level ────────────────────────────────

describe("URL contract: recipe URLs resolve", () => {
  it("recipe 2, query form: worker-zoom resolves as the zoom path", () => {
    const url = {
      search: "?trace=traces/full.bin&start=1000&end=9000&worker-zoom=main%09poll",
      hash: "",
    };
    expect(resolveViewState(url).fgWorkerZoom).toEqual(["main", "poll"]);
  });

  it("recipe 2, hash form: #v=1&fg.w resolves and wins over the query form", () => {
    const url = {
      search: "?trace=traces/full.bin&worker-zoom=stale%09path",
      hash: "#v=1&fg.w=main%09poll%09do_work",
    };
    expect(resolveViewState(url).fgWorkerZoom).toEqual(["main", "poll", "do_work"]);
  });

  it("inspect/search/filter hash keys resolve and win over their legacy params", () => {
    const url = {
      search:
        "?trace=t.bin&inspect=old&inspect_full=old::sym&search=stale&spawn=s0&runtime=r0",
      hash: "#v=1&fg.i=poll&fg.if=core%3A%3Apoll&fg.s=tokio&fg.sp=src%2Fmain.rs%3A10&fg.rt=app",
    };
    const s = resolveViewState(url);
    expect(s.fgInspect).toBe("poll");
    expect(s.fgInspectFull).toBe("core::poll");
    expect(s.fgSearch).toBe("tokio");
    expect(s.fgSpawn).toBe("src/main.rs:10");
    expect(s.fgRuntime).toBe("app");
  });

  it("legacy inspect/search/filter params fill fields the hash omits", () => {
    const url = {
      search: "?trace=t.bin&inspect=poll&search=tokio&spawn=s&runtime=r",
      hash: "#v=1&fg.w=main",
    };
    const s = resolveViewState(url);
    expect(s.fgWorkerZoom).toEqual(["main"]);
    expect(s.fgInspect).toBe("poll");
    expect(s.fgInspectFull).toBe("poll"); // symbol defaults to display name
    expect(s.fgSearch).toBe("tokio");
    expect(s.fgSpawn).toBe("s");
    expect(s.fgRuntime).toBe("r");
  });

  it("recipe 1 params are load scope: the codec never touches them", () => {
    const scope = FLAMEGRAPH_LEGACY_PARAMS.filter(
      (p) => p.mode === "exact" && !p.viewState,
    ).map((p) => p.param);
    expect(sorted(scope)).toEqual(
      sorted(["trace", "start", "end", "svc", "host", "segs", "from", "to"]),
    );
  });
});
