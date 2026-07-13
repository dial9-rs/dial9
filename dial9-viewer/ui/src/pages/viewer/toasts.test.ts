// Tests for the toast manager (T21; features/02 U). The env is node (no
// DOM), so a minimal structural fake element stands in - the manager only
// uses createElement / append / remove / textContent / className /
// classList, which the fake models exactly. Fake timers drive the
// auto-hide (U1) and wiggle (U2) windows.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { createToasts } from "./toasts.js";

interface FakeEl {
  tagName: string;
  className: string;
  textContent: string;
  children: FakeEl[];
  ownerDocument: { createElement(tag: string): FakeEl };
  classList: {
    add(c: string): void;
    remove(c: string): void;
    contains(c: string): boolean;
  };
  appendChild(child: FakeEl): void;
  remove(): void;
  parent: FakeEl | null;
}

function makeEl(tag: string): FakeEl {
  const classes = new Set<string>();
  const el: FakeEl = {
    tagName: tag,
    className: "",
    textContent: "",
    children: [],
    parent: null,
    ownerDocument: { createElement: makeEl },
    classList: {
      add: (c) => {
        classes.add(c);
        el.className = [...classes].join(" ");
      },
      remove: (c) => {
        classes.delete(c);
        el.className = [...classes].join(" ");
      },
      contains: (c) => classes.has(c),
    },
    appendChild: (child) => {
      child.parent = el;
      el.children.push(child);
    },
    remove: () => {
      if (el.parent) {
        el.parent.children = el.parent.children.filter((c) => c !== el);
        el.parent = null;
      }
    },
  };
  // className setter must also seed the class set (the manager assigns
  // className directly for the type classes).
  return new Proxy(el, {
    set(target, prop, value) {
      if (prop === "className" && typeof value === "string") {
        classes.clear();
        for (const c of value.split(/\s+/).filter(Boolean)) classes.add(c);
        target.className = value;
        return true;
      }
      // deno-lint keep: structural set-through
      (target as unknown as Record<string, unknown>)[prop as string] = value;
      return true;
    },
  });
}

let container: FakeEl;

beforeEach(() => {
  vi.useFakeTimers();
  container = makeEl("div");
});
afterEach(() => {
  vi.useRealTimers();
});

describe("createToasts", () => {
  it("shows a toast with its type class", () => {
    const toasts = createToasts(container as unknown as HTMLElement);
    toasts.show({ id: "a", message: "hi", type: "error" });
    expect(container.children).toHaveLength(1);
    expect(container.children[0]?.textContent).toBe("hi");
    expect(container.children[0]?.className).toContain("d9-toast-error");
  });

  it("U1: a duplicate id refreshes + wiggles instead of stacking", () => {
    const toasts = createToasts(container as unknown as HTMLElement);
    toasts.show({ id: "a", message: "one" });
    toasts.show({ id: "a", message: "two" });
    expect(container.children).toHaveLength(1);
    expect(container.children[0]?.textContent).toBe("two");
    expect(container.children[0]?.className).toContain("d9-toast-wiggle");
    vi.advanceTimersByTime(400);
    expect(container.children[0]?.className).not.toContain("d9-toast-wiggle");
  });

  it("U1: auto-hide removes the toast after the timeout", () => {
    const toasts = createToasts(container as unknown as HTMLElement);
    toasts.show({ id: "a", message: "bye", autoHideMs: 1000 });
    expect(container.children).toHaveLength(1);
    vi.advanceTimersByTime(1000);
    expect(container.children).toHaveLength(0);
  });

  it("U3/U5: clear() removes non-persistent toasts but keeps persistent ones", () => {
    const toasts = createToasts(container as unknown as HTMLElement);
    toasts.show({ id: "transient", message: "t" });
    toasts.show({ id: "hint", message: "h", persistent: true });
    toasts.clear();
    expect(container.children.map((c) => c.textContent)).toEqual(["h"]);
  });

  it("hide() removes a specific toast by id", () => {
    const toasts = createToasts(container as unknown as HTMLElement);
    toasts.show({ id: "a", message: "a" });
    toasts.show({ id: "b", message: "b" });
    toasts.hide("a");
    expect(container.children.map((c) => c.textContent)).toEqual(["b"]);
  });
});
