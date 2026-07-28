// Tests for the Escape cascade mechanism. These prove the ordering contract:
// highest priority open surface wins, and handle() reports whether anything
// consumed the key.

import { describe, it, expect } from "vitest";
import { createEscCascade, ESC_PRIORITY } from "./esc-cascade.js";

function surface(name: string, priority: number, open: { v: boolean }) {
  const closed: string[] = [];
  return {
    spec: {
      name,
      priority,
      isOpen: () => open.v,
      close: () => {
        open.v = false;
        closed.push(name);
      },
    },
    closed,
  };
}

describe("createEscCascade", () => {
  it("closes the highest-priority OPEN surface and consumes the key", () => {
    const esc = createEscCascade();
    const help = { v: true };
    const sidebar = { v: true };
    esc.register(surface("help", ESC_PRIORITY.help, help).spec);
    esc.register(surface("sidebar", ESC_PRIORITY.sidebar, sidebar).spec);

    expect(esc.handle()).toBe(true);
    expect(help.v).toBe(false); // help (higher priority) closed first
    expect(sidebar.v).toBe(true); // sidebar untouched

    expect(esc.handle()).toBe(true);
    expect(sidebar.v).toBe(false); // now sidebar closes
  });

  it("skips closed surfaces and returns false when nothing is open", () => {
    const esc = createEscCascade();
    const popup = { v: false };
    esc.register(surface("popup", ESC_PRIORITY.popup, popup).spec);
    expect(esc.handle()).toBe(false);
  });

  it("orders describe() by descending priority", () => {
    const esc = createEscCascade();
    esc.register(surface("selection", ESC_PRIORITY.selection, { v: false }).spec);
    esc.register(surface("help", ESC_PRIORITY.help, { v: false }).spec);
    esc.register(surface("sidebar", ESC_PRIORITY.sidebar, { v: false }).spec);
    expect(esc.describe()).toEqual(["help", "sidebar", "selection"]);
  });

  it("unregister removes a surface from the cascade", () => {
    const esc = createEscCascade();
    const open = { v: true };
    const unregister = esc.register(surface("help", ESC_PRIORITY.help, open).spec);
    unregister();
    expect(esc.handle()).toBe(false); // no surfaces left to consume
    expect(open.v).toBe(true);
  });
});
