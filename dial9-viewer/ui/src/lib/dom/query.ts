// Shell element lookup, shared by the page `dom.ts` modules.
//
// Each page resolves its static skeleton once at boot so a drifted shell fails
// LOUDLY at startup, naming the missing id, instead of null-dereferencing deep
// inside a render. The three pages each had their own copy of this differing
// only in the error string.

/** Resolve `#id`, or throw naming the shell and the id. */
export function byId<T extends HTMLElement>(shell: string, id: string): T {
  const el = document.getElementById(id);
  if (el === null) throw new Error(`${shell} markup is missing #${id}`);
  return el as T;
}

/**
 * Resolve `#id` and CHECK it is really a `ctor`. Prefer this wherever the
 * caller goes on to use element-specific API: `byId` asserts the type, so a
 * markup change from <canvas> to <div> surfaces as "getContext is not a
 * function" at first paint rather than at boot.
 */
export function byIdOf<T extends HTMLElement>(
  shell: string,
  id: string,
  ctor: new () => T,
): T {
  const el = document.getElementById(id);
  if (el === null) throw new Error(`${shell} markup is missing #${id}`);
  if (!(el instanceof ctor)) {
    throw new Error(
      `${shell} markup: #${id} is <${el.tagName.toLowerCase()}>, expected ${ctor.name}`,
    );
  }
  return el;
}

/** Resolve `selector`, or throw naming the shell and the selector. */
export function bySelector<T extends HTMLElement>(shell: string, selector: string): T {
  const el = document.querySelector<T>(selector);
  if (el === null) throw new Error(`${shell} markup is missing ${selector}`);
  return el;
}
