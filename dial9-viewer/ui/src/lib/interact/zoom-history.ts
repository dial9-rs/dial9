// The bounded zoom-history stack behind the unified `z` (undo last zoom)
// key. A generic mechanism: `record` tracks the CURRENT state and pushes
// the previous one; `undo` pops back one state. The stack is bounded
// (oldest entries drop) so a long session cannot grow it unboundedly.

export interface ZoomHistoryOptions<T> {
  /** Max undo depth; oldest entries drop beyond it. Default 64. */
  limit?: number;
  /** State equality; equal records are ignored (no no-op undo steps). */
  equals?: (a: T, b: T) => boolean;
}

export interface ZoomHistory<T> {
  /**
   * Track a state change: the previously recorded state (if any, and not
   * equal) is pushed onto the undo stack. The FIRST record establishes
   * the baseline and pushes nothing.
   */
  record(state: T): void;
  /**
   * Step back: pops the most recent past state, makes it current, and
   * returns it. Null when there is nothing to undo.
   */
  undo(): T | null;
  /** Number of undo steps available. */
  depth(): number;
  /** Drop all history (e.g. when the underlying data is replaced). */
  clear(): void;
}

const DEFAULT_LIMIT = 64;

export function createZoomHistory<T>(options: ZoomHistoryOptions<T> = {}): ZoomHistory<T> {
  const limit = options.limit ?? DEFAULT_LIMIT;
  const equals = options.equals ?? ((a: T, b: T) => a === b);
  const stack: T[] = [];
  // Distinguishes "no baseline yet" from a recorded undefined-able T.
  let hasCurrent = false;
  let current: T | undefined;

  return {
    record(state: T): void {
      if (hasCurrent && equals(current as T, state)) return;
      if (hasCurrent) {
        stack.push(current as T);
        if (stack.length > limit) stack.shift();
      }
      current = state;
      hasCurrent = true;
    },
    undo(): T | null {
      if (stack.length === 0) return null;
      const prev = stack.pop() as T;
      current = prev;
      return prev;
    },
    depth(): number {
      return stack.length;
    },
    clear(): void {
      stack.length = 0;
      hasCurrent = false;
      current = undefined;
    },
  };
}
