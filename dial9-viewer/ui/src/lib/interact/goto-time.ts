// lib/interact/goto-time.ts - the goto-time (`g`) input grammar (T20;
// canonical spec docs/ui-inventory/mocks/keyboard.html: "g - go to time
// (absolute or +relative)", prompt placeholder "go to time (s or +s)").
//
// MECHANISM ONLY in chunk 1: no migrated page has a time axis at landing
// time, so nothing wires `g` yet - T23 binds it to the viewer's viewport
// slice in chunk 2. The parser lives here so the grammar is fixed (and
// unit-tested) before any surface consumes it.
//
// Grammar (seconds; the mock's units):
//   "1.72"  / "1.72s"  -> absolute trace time 1.72s
//   "+0.5"  / "+0.5s"  -> 0.5s forward from the current position
//   "-2"    / "-2s"    -> 2s back from the current position
// Whitespace is trimmed; anything else (empty, bare sign, non-numeric,
// non-finite) is rejected with null - callers re-prompt, never guess.

export interface GotoTime {
  /** True for `+`/`-` prefixed input: offset from the current position. */
  relative: boolean;
  /** Parsed magnitude in seconds (signed when relative). */
  seconds: number;
}

const GOTO_RE = /^([+-]?)(\d+(?:\.\d+)?)s?$/;

/** Parse a goto-time input. Null when the input is not valid. */
export function parseGotoTime(input: string): GotoTime | null {
  const m = GOTO_RE.exec(input.trim());
  if (m === null) return null;
  const sign = m[1];
  const magnitude = Number(m[2]);
  if (!Number.isFinite(magnitude)) return null;
  return {
    relative: sign === "+" || sign === "-",
    seconds: sign === "-" ? -magnitude : magnitude,
  };
}
