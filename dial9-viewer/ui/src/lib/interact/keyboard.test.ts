// The focus-tolerant key router. The routing policy is pure over structural
// events (KeyEventLike), so every rule is driven here under plain Node.

import { describe, it, expect, vi } from "vitest";
import {
  createKeyRouter,
  isTextEntryTarget,
  mountKeyRouter,
} from "./keyboard.js";
import type { KeyBinding, KeyEventLike } from "./keyboard.js";

interface EvOpts {
  key: string;
  target?: unknown;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  defaultPrevented?: boolean;
}

function ev(opts: EvOpts): KeyEventLike & { prevented: boolean } {
  const e = {
    key: opts.key,
    ctrlKey: opts.ctrlKey ?? false,
    metaKey: opts.metaKey ?? false,
    altKey: opts.altKey ?? false,
    shiftKey: opts.shiftKey ?? false,
    target: opts.target ?? { tagName: "BODY" },
    defaultPrevented: opts.defaultPrevented ?? false,
    prevented: false,
    preventDefault(): void {
      e.prevented = true;
    },
  };
  return e;
}

const textInput = { tagName: "INPUT", type: "text" };
const searchInput = { tagName: "INPUT", type: "search" };
const checkbox = { tagName: "INPUT", type: "checkbox" };
const selectEl = { tagName: "SELECT" };
const buttonEl = { tagName: "BUTTON" };
const textarea = { tagName: "TEXTAREA" };
const editable = { isContentEditable: true, tagName: "DIV" };

describe("isTextEntryTarget (suppression rule)", () => {
  it("text-like inputs, textareas and contenteditable are text entry", () => {
    expect(isTextEntryTarget(textInput)).toBe(true);
    expect(isTextEntryTarget(searchInput)).toBe(true);
    expect(isTextEntryTarget(textarea)).toBe(true);
    expect(isTextEntryTarget(editable)).toBe(true);
    // Missing `type` on an <input> defaults to text.
    expect(isTextEntryTarget({ tagName: "INPUT" })).toBe(true);
  });

  it("non-text form controls stay accelerator-live", () => {
    // Without this, nav keys die the moment a <select> grabs focus.
    expect(isTextEntryTarget(selectEl)).toBe(false);
    expect(isTextEntryTarget(checkbox)).toBe(false);
    expect(isTextEntryTarget(buttonEl)).toBe(false);
    expect(isTextEntryTarget({ tagName: "INPUT", type: "range" })).toBe(false);
    expect(isTextEntryTarget({ tagName: "INPUT", type: "radio" })).toBe(false);
    expect(isTextEntryTarget({ tagName: "A" })).toBe(false);
    expect(isTextEntryTarget({ tagName: "CANVAS" })).toBe(false);
    expect(isTextEntryTarget({ tagName: "BODY" })).toBe(false);
  });

  it("non-element targets are not text entry", () => {
    expect(isTextEntryTarget(null)).toBe(false);
    expect(isTextEntryTarget(undefined)).toBe(false);
    expect(isTextEntryTarget("window")).toBe(false);
  });
});

describe("createKeyRouter", () => {
  it("dispatches to the matching binding and preventDefaults", () => {
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "f", onKey }]);
    const e = ev({ key: "f" });
    expect(router.handle(e)).toBe(true);
    expect(onKey).toHaveBeenCalledTimes(1);
    expect(e.prevented).toBe(true);
  });

  it("single-char keys match case-insensitively (CapsLock-proof WASD)", () => {
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "w", onKey }]);
    expect(router.handle(ev({ key: "W", shiftKey: true }))).toBe(true);
    expect(onKey).toHaveBeenCalledTimes(1);
  });

  it("named keys match exactly", () => {
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "Enter", onKey }]);
    expect(router.handle(ev({ key: "enter" }))).toBe(false);
    expect(router.handle(ev({ key: "Enter" }))).toBe(true);
  });

  it("composition: an already-consumed event is never re-handled", () => {
    // The frozen flamegraph widget preventDefaults its own '/' handler; the
    // router must compose, not double-handle.
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "/", onKey }]);
    expect(router.handle(ev({ key: "/", defaultPrevented: true }))).toBe(false);
    expect(onKey).not.toHaveBeenCalled();
  });

  it("Ctrl/Cmd/Alt chords are never matched", () => {
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "f", onKey }]);
    expect(router.handle(ev({ key: "f", ctrlKey: true }))).toBe(false);
    expect(router.handle(ev({ key: "f", metaKey: true }))).toBe(false);
    expect(router.handle(ev({ key: "f", altKey: true }))).toBe(false);
    expect(onKey).not.toHaveBeenCalled();
  });

  it("Shift is allowed when it produced the key itself (?)", () => {
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "?", onKey }]);
    expect(router.handle(ev({ key: "?", shiftKey: true }))).toBe(true);
    expect(onKey).toHaveBeenCalledTimes(1);
  });

  it("suppressed in text-entry targets unless the binding opts in", () => {
    const pageKey = vi.fn();
    const escKey = vi.fn();
    const router = createKeyRouter([
      { key: "f", onKey: pageKey },
      { key: "Escape", inTextEntry: true, onKey: escKey },
    ]);
    // 'f' typed into a text input must insert text, not fit.
    expect(router.handle(ev({ key: "f", target: textInput }))).toBe(false);
    expect(pageKey).not.toHaveBeenCalled();
    // Escape opted in (the flamegraph search field cascade).
    expect(router.handle(ev({ key: "Escape", target: textInput }))).toBe(true);
    expect(escKey).toHaveBeenCalledTimes(1);
  });

  it("accelerators fire while a select/button/checkbox has focus", () => {
    const onKey = vi.fn();
    const router = createKeyRouter([{ key: "z", onKey }]);
    expect(router.handle(ev({ key: "z", target: selectEl }))).toBe(true);
    expect(router.handle(ev({ key: "z", target: buttonEl }))).toBe(true);
    expect(router.handle(ev({ key: "z", target: checkbox }))).toBe(true);
    expect(onKey).toHaveBeenCalledTimes(3);
  });

  it("decline fallthrough: returning false passes to later bindings", () => {
    const declined = vi.fn(() => false as const);
    const taken = vi.fn();
    const router = createKeyRouter([
      { key: "Enter", onKey: declined },
      { key: "Enter", onKey: taken },
    ]);
    const e = ev({ key: "Enter" });
    expect(router.handle(e)).toBe(true);
    expect(declined).toHaveBeenCalledTimes(1);
    expect(taken).toHaveBeenCalledTimes(1);
    expect(e.prevented).toBe(true);
  });

  it("all bindings declining leaves the page default intact", () => {
    const router = createKeyRouter([{ key: "Enter", onKey: () => false }]);
    const e = ev({ key: "Enter" });
    expect(router.handle(e)).toBe(false);
    expect(e.prevented).toBe(false);
  });

  it("first registered match wins (order = priority)", () => {
    const first = vi.fn();
    const second = vi.fn();
    const router = createKeyRouter([
      { key: "z", onKey: first },
      { key: "z", onKey: second },
    ]);
    router.handle(ev({ key: "z" }));
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).not.toHaveBeenCalled();
  });

  it("preserveDefault consumes without preventDefault", () => {
    // The legacy flamegraph Escape wiring never called preventDefault.
    const router = createKeyRouter([
      { key: "Escape", preserveDefault: true, onKey: vi.fn() },
    ]);
    const e = ev({ key: "Escape" });
    expect(router.handle(e)).toBe(true);
    expect(e.prevented).toBe(false);
  });
});

describe("mountKeyRouter", () => {
  it("attaches one keydown listener and detaches it on dispose", () => {
    const listeners = new Set<(e: KeyboardEvent) => void>();
    const source = {
      addEventListener(_t: "keydown", l: (e: KeyboardEvent) => void): void {
        listeners.add(l);
      },
      removeEventListener(_t: "keydown", l: (e: KeyboardEvent) => void): void {
        listeners.delete(l);
      },
    };
    const onKey = vi.fn();
    const bindings: KeyBinding[] = [{ key: "f", onKey }];
    const dispose = mountKeyRouter(source, bindings);
    expect(listeners.size).toBe(1);
    for (const l of listeners) l(ev({ key: "f" }) as unknown as KeyboardEvent);
    expect(onKey).toHaveBeenCalledTimes(1);
    dispose();
    expect(listeners.size).toBe(0);
  });
});
