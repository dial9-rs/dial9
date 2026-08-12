import { html } from "lit-html";
import { describe, expect, it, vi } from "vitest";
import type { StoreState } from "../../types/state.js";
import { trackTooltipAtTarget } from "./index.js";

describe("trackTooltipAtTarget", () => {
  it("routes a track canvas to the page tooltip resolver", () => {
    const content = html`CPU`;
    const resolve = vi.fn(() => content);
    const target = { dataset: { trackCanvas: "cpu" } } as unknown as EventTarget;
    const state = {} as StoreState;

    expect(trackTooltipAtTarget(target, state, 42, resolve)).toBe(content);
    expect(resolve).toHaveBeenCalledWith("cpu", state, 42);
  });

  it("ignores events outside a track canvas", () => {
    const resolve = vi.fn(() => html`unexpected`);

    expect(trackTooltipAtTarget(null, {} as StoreState, 42, resolve)).toBeNull();
    expect(resolve).not.toHaveBeenCalled();
  });
});
