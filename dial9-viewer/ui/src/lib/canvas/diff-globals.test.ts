// The frozen flamegraph_diff_view.js reads a bare `FlamegraphApi` global in the
// browser; flamegraph_api.js publishes it only as a classic <script> tag.
import { expect, it } from "vitest";

// Seam first: it pulls in ./diff-globals for its side effect.
import "./flamegraph_diff_view.js";
// `getApi` is internal to the frozen module, so it is not on the typed seam.
import * as frozen from "../../../flamegraph_diff_view.js";

const getApi = (frozen as unknown as {
  getApi: () => Record<string, unknown>;
}).getApi;

const seeded = (globalThis as Record<string, unknown>).FlamegraphApi as
  | Record<string, unknown>
  | undefined;

it("seeds a FlamegraphApi global covering the resolved module surface", () => {
  expect(seeded).toBeDefined();
  // Under Node getApi() takes the require branch, so it is the canonical surface.
  const missing = Object.entries(getApi())
    .filter(([k, v]) => typeof v === "function" && typeof seeded?.[k] !== "function")
    .map(([k]) => k);
  expect(missing).toEqual([]);
});

it("seeds the real helpers, not empty stubs", () => {
  const shouldAdopt = seeded?.shouldAdoptRefinementSnapshot as (
    preserveExisting: boolean,
    baseline: number,
    incoming: number,
  ) => boolean;
  // A same-scope refine ignores a snapshot shallower than what is on screen.
  expect(shouldAdopt(true, 5, 3)).toBe(false);
  expect(shouldAdopt(true, 5, 7)).toBe(true);
});
