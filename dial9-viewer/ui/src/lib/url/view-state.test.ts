// Tests for the URL view-state codec: the encode/decode round-trip PROPERTY
// test (seeded PRNG, no new deps - no property-testing library allowed), the
// recorded legacy-param fixture semantics (old links keep resolving
// identically), hash-vs-query precedence, tolerant-reader rules, and
// foreign-hash refusal.

import { describe, it, expect } from "vitest";
import {
  VIEW_STATE_VERSION,
  encodeViewState,
  decodeViewState,
  legacyZoomFromQuery,
  applyLegacyZoomToQuery,
  resolveViewState,
  type UnknownEntry,
  type ViewState,
} from "./view-state.js";
import {
  FLAMEGRAPH_LEGACY_PARAMS,
  LEGACY_FIXTURE_URLS,
} from "./legacy-params.fixture.js";

// ── Seeded PRNG (mulberry32) for the property test ──────────────────────

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// Frame names are arbitrary text EXCEPT tab (the path separator - the
// legacy wire format's own limitation, see the codec header). The pool
// deliberately covers the URL metacharacters the form encoding must escape.
const NAME_CHARS = [..."abzXYZ09_:<>&=#%+?/\\ .'\"-é中"];

function randName(rnd: () => number): string {
  const len = 1 + Math.floor(rnd() * 12);
  let s = "";
  for (let i = 0; i < len; i++) {
    s += NAME_CHARS[Math.floor(rnd() * NAME_CHARS.length)];
  }
  return s;
}

function randPath(rnd: () => number): string[] {
  const depth = 1 + Math.floor(rnd() * 6);
  return Array.from({ length: depth }, () => randName(rnd));
}

function randState(rnd: () => number): ViewState {
  const s: ViewState = {};
  if (rnd() < 0.7) s.fgWorkerZoom = randPath(rnd);
  if (rnd() < 0.5) s.fgOffworkerZoom = randPath(rnd);
  if (rnd() < 0.5) {
    // Inspect always carries both name + symbol (the widget defaults fullName
    // to name); the symbol sometimes equals the name (the redundant case the
    // codec collapses) and sometimes differs.
    const name = randName(rnd);
    s.fgInspect = name;
    s.fgInspectFull = rnd() < 0.5 ? name : randName(rnd);
  }
  if (rnd() < 0.4) s.fgSearch = randName(rnd);
  if (rnd() < 0.3) s.fgSpawn = randName(rnd);
  if (rnd() < 0.3) s.fgRuntime = randName(rnd);
  if (rnd() < 0.4) s.timeMode = rnd() < 0.5 ? "rel" : "abs";
  if (rnd() < 0.4) s.timeZone = rnd() < 0.5 ? "utc" : "local";
  return s;
}

/** True when a state carries nothing the codec would serialize. */
function isEmptyState(s: ViewState): boolean {
  return (
    s.fgWorkerZoom === undefined &&
    s.fgOffworkerZoom === undefined &&
    s.fgInspect === undefined &&
    s.fgSearch === undefined &&
    s.fgSpawn === undefined &&
    s.fgRuntime === undefined &&
    s.timeMode === undefined &&
    s.timeZone === undefined
  );
}

function randUnknown(rnd: () => number): UnknownEntry[] {
  const n = Math.floor(rnd() * 3);
  // Keys avoid the known registry by construction ("x-" prefix).
  return Array.from({ length: n }, (_, i) => [`x-${i}${randName(rnd)}`, randName(rnd)] as const);
}

describe("view-state codec round-trip (property)", () => {
  it("decode(encode(state)) === state for 500 seeded random states", () => {
    const rnd = mulberry32(0xd1a19);
    for (let i = 0; i < 500; i++) {
      const state = randState(rnd);
      const unknown = randUnknown(rnd);
      const payload = encodeViewState(state, unknown);
      if (isEmptyState(state) && unknown.length === 0) {
        expect(payload).toBe("");
        continue;
      }
      const decoded = decodeViewState(payload);
      expect(decoded, `case ${i}: ${payload}`).not.toBeNull();
      expect(decoded!.version).toBe(VIEW_STATE_VERSION);
      expect(decoded!.state, `case ${i}: ${payload}`).toEqual(state);
      expect(decoded!.unknown, `case ${i}: ${payload}`).toEqual(unknown);
    }
  });

  it("encode is stable: encode(decode(encode(s))) === encode(s)", () => {
    const rnd = mulberry32(0xfeed);
    for (let i = 0; i < 200; i++) {
      const first = encodeViewState(randState(rnd), randUnknown(rnd));
      if (first === "") continue;
      const decoded = decodeViewState(first);
      expect(decoded).not.toBeNull();
      expect(encodeViewState(decoded!.state, decoded!.unknown)).toBe(first);
    }
  });

  it("legacy query mirror round-trips zoom + inspect/search/filters", () => {
    const rnd = mulberry32(0xbeef);
    for (let i = 0; i < 200; i++) {
      const state = randState(rnd);
      const params = new URLSearchParams("trace=demo-trace.bin");
      applyLegacyZoomToQuery(params, state);
      const back = legacyZoomFromQuery(params);
      const expected: ViewState = {};
      if (state.fgWorkerZoom !== undefined && state.fgWorkerZoom.length > 0) {
        expected.fgWorkerZoom = state.fgWorkerZoom;
      }
      if (state.fgOffworkerZoom !== undefined && state.fgOffworkerZoom.length > 0) {
        expected.fgOffworkerZoom = state.fgOffworkerZoom;
      }
      if (state.fgInspect) {
        expected.fgInspect = state.fgInspect;
        expected.fgInspectFull = state.fgInspectFull || state.fgInspect;
      }
      if (state.fgSearch) expected.fgSearch = state.fgSearch;
      if (state.fgSpawn) expected.fgSpawn = state.fgSpawn;
      if (state.fgRuntime) expected.fgRuntime = state.fgRuntime;
      expect(back).toEqual(expected);
      // The mirror never touches load-scope params.
      expect(params.get("trace")).toBe("demo-trace.bin");
    }
  });

  it("mirror emits inspect_full only when it differs from the display name", () => {
    const same = new URLSearchParams();
    applyLegacyZoomToQuery(same, { fgInspect: "leaf", fgInspectFull: "leaf" });
    expect(same.get("inspect")).toBe("leaf");
    expect(same.get("inspect_full")).toBeNull();

    const diff = new URLSearchParams();
    applyLegacyZoomToQuery(diff, { fgInspect: "poll", fgInspectFull: "core::poll" });
    expect(diff.get("inspect")).toBe("poll");
    expect(diff.get("inspect_full")).toBe("core::poll");
  });

  it("mirror deletes the view params on absence (URL stays clean)", () => {
    const p = new URLSearchParams(
      "trace=t.bin&worker-zoom=a&offworker-zoom=b&inspect=poll&inspect_full=core::poll" +
        "&search=q&spawn=s&runtime=r",
    );
    applyLegacyZoomToQuery(p, {});
    for (const k of ["worker-zoom", "offworker-zoom", "inspect", "inspect_full", "search", "spawn", "runtime"]) {
      expect(p.get(k), k).toBeNull();
    }
    expect(p.get("trace")).toBe("t.bin");
  });
});

describe("versioning and tolerant-reader rules", () => {
  it("emits v first and accepts it anywhere", () => {
    const payload = encodeViewState({ timeMode: "abs" });
    expect(payload.startsWith("v=1&")).toBe(true);
    expect(decodeViewState("tm=abs&v=1")?.state).toEqual({ timeMode: "abs" });
  });

  it("returns null for empty and foreign hashes", () => {
    expect(decodeViewState("")).toBeNull();
    expect(decodeViewState("#")).toBeNull();
    expect(decodeViewState("#section-anchor")).toBeNull();
    expect(decodeViewState("v=abc&tm=abs")).toBeNull();
  });

  it("a future version decodes to empty state (no restore, no rewrite)", () => {
    const d = decodeViewState("#v=2&fg.w=reinterpreted&new=key");
    expect(d).toEqual({ version: 2, state: {}, unknown: [] });
  });

  it("preserves unrecognized v=1 keys verbatim, in order", () => {
    const d = decodeViewState("v=1&vp=1-2&tm=abs&sel.task=42");
    expect(d?.state).toEqual({ timeMode: "abs" });
    expect(d?.unknown).toEqual([
      ["vp", "1-2"],
      ["sel.task", "42"],
    ]);
    // Re-encode keeps them (an older build must not strip newer state).
    expect(encodeViewState(d!.state, d!.unknown)).toBe("v=1&tm=abs&vp=1-2&sel.task=42");
  });

  it("drops invalid values of known keys instead of propagating them", () => {
    expect(decodeViewState("v=1&tm=sideways&tz=mars")?.state).toEqual({});
    // Empty path segments cannot come from the legacy writer: no restore.
    expect(decodeViewState("v=1&fg.w=%09broken")?.state).toEqual({});
  });
});

describe("recorded legacy-param fixture", () => {
  it("records exact-mode zoom and cross-mode inspection as view state", () => {
    const vs = FLAMEGRAPH_LEGACY_PARAMS.filter((p) => p.viewState);
    expect(vs.map((p) => p.param).sort()).toEqual([
      "inspect",
      "inspect_full",
      "offworker-zoom",
      "worker-zoom",
    ]);
    for (const p of vs) {
      expect(p.mechanism).toBe("replaceState");
    }
  });

  it("resolves each fixture URL identically to the legacy reader", () => {
    for (const url of LEGACY_FIXTURE_URLS) {
      const [search = "", hash = ""] = url.split("#");
      const resolved = resolveViewState({ search, hash: hash === "" ? "" : `#${hash}` });
      // The legacy reader: worker-zoom/offworker-zoom split on TAB.
      const p = new URLSearchParams(search);
      const wz = p.get("worker-zoom");
      const oz = p.get("offworker-zoom");
      const inspect = p.get("inspect");
      expect(resolved.fgWorkerZoom).toEqual(wz === null ? undefined : wz.split("\t"));
      expect(resolved.fgOffworkerZoom).toEqual(oz === null ? undefined : oz.split("\t"));
      expect(resolved.fgInspect).toBe(inspect || undefined);
      expect(resolved.fgInspectFull).toBe(
        inspect ? p.get("inspect_full") || inspect : undefined,
      );
      // No fixture URL smuggles in hash state.
      expect(resolved.timeMode).toBeUndefined();
      expect(resolved.timeZone).toBeUndefined();
    }
  });

  it("keeps every non-zoom fixture param byte-identical through the mirror", () => {
    for (const url of LEGACY_FIXTURE_URLS) {
      const [search = ""] = url.split("#");
      const params = new URLSearchParams(search);
      const before = [...params.entries()].filter(
        ([k]) => k !== "worker-zoom" && k !== "offworker-zoom",
      );
      applyLegacyZoomToQuery(params, resolveViewState({ search, hash: "" }));
      const after = [...params.entries()].filter(
        ([k]) => k !== "worker-zoom" && k !== "offworker-zoom",
      );
      expect(after).toEqual(before);
    }
  });
});

describe("precedence: hash wins per field, legacy fills gaps", () => {
  it("hash zoom overrides legacy zoom for the same tree", () => {
    const s = resolveViewState({
      search: "?trace=t.bin&worker-zoom=old%09path",
      hash: "#v=1&fg.w=new%09path",
    });
    expect(s.fgWorkerZoom).toEqual(["new", "path"]);
  });

  it("legacy fills fields the hash does not carry", () => {
    const s = resolveViewState({
      search: "?worker-zoom=a&offworker-zoom=b%09c",
      hash: "#v=1&fg.w=z&tm=rel",
    });
    expect(s).toEqual({
      fgWorkerZoom: ["z"],
      fgOffworkerZoom: ["b", "c"],
      timeMode: "rel",
    });
  });

  it("hash inspect/search/filters override the legacy params per field", () => {
    const s = resolveViewState({
      search: "?inspect=old&inspect_full=old::s&search=stale&spawn=s0&runtime=r0",
      hash: "#v=1&fg.i=poll&fg.if=core%3A%3Apoll&fg.s=tokio&fg.sp=s1",
    });
    expect(s.fgInspect).toBe("poll");
    expect(s.fgInspectFull).toBe("core::poll");
    expect(s.fgSearch).toBe("tokio");
    expect(s.fgSpawn).toBe("s1");
    // runtime not in the hash -> legacy fills it.
    expect(s.fgRuntime).toBe("r0");
  });

  it("round-trips inspect/search/filters through the full codec", () => {
    const state: ViewState = {
      fgInspect: "poll",
      fgInspectFull: "core::poll::poll",
      fgSearch: "tokio::spawn",
      fgSpawn: "src/main.rs:10",
      fgRuntime: "app",
    };
    const decoded = decodeViewState(encodeViewState(state));
    expect(decoded?.state).toEqual(state);
  });

  it("a foreign hash leaves the legacy interpretation untouched", () => {
    const s = resolveViewState({
      search: "?worker-zoom=a%09b",
      hash: "#some-anchor",
    });
    expect(s).toEqual({ fgWorkerZoom: ["a", "b"] });
  });

  it("a future-version hash is ignored for state (legacy still resolves)", () => {
    const s = resolveViewState({
      search: "?worker-zoom=a",
      hash: "#v=2&fg.w=reinterpreted",
    });
    expect(s).toEqual({ fgWorkerZoom: ["a"] });
  });
});
