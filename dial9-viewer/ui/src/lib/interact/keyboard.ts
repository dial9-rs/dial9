// lib/interact/keyboard.ts - the focus-tolerant page key router (T20;
// docs/ui-inventory/04-ux-findings.md K5, 02-architecture.md 2.5, canonical
// interactive spec docs/ui-inventory/mocks/keyboard.html).
//
// One vocabulary on every page: `/` search, n/p POI step, g goto-time,
// f fit, z zoom-undo, W/A/S/D nav, `?` help. This module is the MECHANISM
// only - pages register their landing-time bindings; the viewer surfaces
// wire theirs in chunk 2 (T23).
//
// Focus-tolerant (K5): single-key accelerators fire even while a
// non-text-entry form control (button, select, checkbox, link, canvas)
// has focus - the finding is that nav keys die the moment a <select>
// grabs focus. They are SUPPRESSED while the user is typing in a
// text-entry context (text-like <input>, <textarea>, contenteditable),
// where letters must insert text; a binding opts back in with
// `inTextEntry: true` (e.g. Escape, which the flamegraph page must
// deliver to the widget's cascade even from its search field - F44/F157).
//
// Composition rules (the router NEVER steals someone else's key):
//   - events something already consumed (e.defaultPrevented) are skipped -
//     the frozen flamegraph widget's own keydown handler (Ctrl/Cmd+F, `/`;
//     features/03 F41) registers first and preventDefaults, so its keys
//     pass through untouched;
//   - Ctrl/Cmd/Alt chords are never matched (browser + widget territory);
//     Shift is allowed only insofar as it produced the event key itself
//     (`?` IS Shift+/ on most layouts);
//   - a binding can DECLINE by returning false: the event falls through to
//     later bindings and, if none consume, to the page default (no
//     preventDefault) - this is how a binding scoped to specific elements
//     (e.g. the browser page's Enter-submits) stays out of everyone
//     else's way.
//
// Per architecture 2.5, bindings translate keys into store actions /
// component calls only - they never render.
//
// The router core is DOM-free (structural event/target types) so the
// policy is unit-testable under plain Node; mountKeyRouter is the thin
// browser attachment.

/** Structural subset of KeyboardEvent the router reads (Node-testable). */
export interface KeyEventLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  /** Where the event was targeted; used for the text-entry policy. */
  target: unknown;
  defaultPrevented: boolean;
  preventDefault(): void;
}

/** One key binding. First registered match that does not decline wins. */
export interface KeyBinding {
  /**
   * KeyboardEvent.key to match. Single characters match
   * case-insensitively (so CapsLock does not kill WASD); named keys
   * ("Enter", "Escape", "ArrowLeft") match exactly.
   */
  key: string;
  /**
   * Also fire while a text-entry element has focus (default false).
   * Bindings that opt in are responsible for their own target checks.
   */
  inTextEntry?: boolean;
  /**
   * Handle the key. Return false to DECLINE (event falls through);
   * any other return consumes the event (preventDefault is called
   * unless `preserveDefault` is set).
   */
  onKey(e: KeyEventLike): boolean | void;
  /**
   * Consume without preventDefault (default false). Used where the
   * legacy page deliberately left the browser default intact (e.g. the
   * flamegraph Escape wiring never called preventDefault).
   */
  preserveDefault?: boolean;
}

/** Text-like <input> types: typing must insert characters, not run keys. */
const TEXT_INPUT_TYPES = new Set([
  "text",
  "search",
  "url",
  "tel",
  "email",
  "password",
  "number",
  "date",
  "datetime-local",
  "month",
  "time",
  "week",
]);

interface ElementLike {
  tagName?: unknown;
  isContentEditable?: unknown;
  type?: unknown;
}

/**
 * Is `target` a text-entry context (K5 suppression rule)? Buttons,
 * selects, checkboxes, radios, ranges, links, canvases and the body are
 * NOT: accelerators stay live there - that is the "focus-tolerant" in the
 * ticket. Structural (no DOM types) for Node tests.
 */
export function isTextEntryTarget(target: unknown): boolean {
  if (target === null || typeof target !== "object") return false;
  const el = target as ElementLike;
  if (el.isContentEditable === true) return true;
  const tag = typeof el.tagName === "string" ? el.tagName.toUpperCase() : "";
  if (tag === "TEXTAREA") return true;
  if (tag === "INPUT") {
    const type = typeof el.type === "string" ? el.type.toLowerCase() : "text";
    return TEXT_INPUT_TYPES.has(type);
  }
  return false;
}

function keyMatches(bindingKey: string, eventKey: string): boolean {
  if (bindingKey.length === 1 && eventKey.length === 1) {
    return bindingKey.toLowerCase() === eventKey.toLowerCase();
  }
  return bindingKey === eventKey;
}

export interface KeyRouter {
  /**
   * Route one event through the bindings. Returns true when a binding
   * consumed it (preventDefault already applied per the binding).
   */
  handle(e: KeyEventLike): boolean;
}

/** Build a router over `bindings` (order = priority). */
export function createKeyRouter(bindings: readonly KeyBinding[]): KeyRouter {
  return {
    handle(e: KeyEventLike): boolean {
      // Someone earlier on the target owned this key (e.g. the frozen
      // flamegraph widget's `/`): compose, don't double-handle.
      if (e.defaultPrevented) return false;
      // Chords belong to the browser / other handlers.
      if (e.ctrlKey || e.metaKey || e.altKey) return false;
      const inText = isTextEntryTarget(e.target);
      for (const b of bindings) {
        if (!keyMatches(b.key, e.key)) continue;
        if (inText && b.inTextEntry !== true) continue;
        if (b.onKey(e) === false) continue; // declined - keep falling through
        if (b.preserveDefault !== true) e.preventDefault();
        return true;
      }
      return false;
    },
  };
}

/** Minimal event-target surface of Window/HTMLElement (Node-testable). */
export interface KeyEventSource {
  addEventListener(type: "keydown", listener: (e: KeyboardEvent) => void): void;
  removeEventListener(type: "keydown", listener: (e: KeyboardEvent) => void): void;
}

/**
 * Attach a router to a live event source (window, usually). Returns the
 * dispose function.
 */
export function mountKeyRouter(
  target: KeyEventSource,
  bindings: readonly KeyBinding[],
): () => void {
  const router = createKeyRouter(bindings);
  const listener = (e: KeyboardEvent): void => {
    router.handle(e);
  };
  target.addEventListener("keydown", listener);
  return () => {
    target.removeEventListener("keydown", listener);
  };
}
