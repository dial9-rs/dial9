// Exemplar-table column visibility.
//
// A column is hidden when the user says so, or - for columns carrying a
// `degenValue` extractor - automatically once every visible exemplar shares one
// value, since it then adds no information. Explicit choices persist so they
// survive streaming re-renders and reloads.

import { columnIsDegenerate } from "../../lib/trace/index.js";

const STORAGE_KEY = "dial9.spanExplorer.exemplarColOverrides";

/** true = force shown, false = force hidden, absent = follow the auto rule. */
export type ColumnOverrides = Record<string, boolean>;

/** Whether a column renders, and whether the automatic rule is what hid it. */
export interface ColumnVisibility {
  hidden: boolean;
  /** True only when the auto-degenerate rule hid it (never a user choice). */
  auto: boolean;
}

/** The parts of a column descriptor visibility depends on. */
export interface VisibilityInput<T> {
  id: string;
  /** false for core navigation columns, which never hide. */
  hideable?: boolean;
  /** Absent for columns that must never auto-hide (Duration, %ile). */
  degenValue?: (row: T) => string | number | null | undefined;
}

/**
 * Effective visibility: an explicit override always wins, then the auto rule.
 * Columns without `degenValue` never auto-hide - they stay informative even
 * when the bounded exemplars happen to be close in value.
 */
export function colVisibility<T>(
  col: VisibilityInput<T>,
  rows: readonly T[],
  overrides: ColumnOverrides,
): ColumnVisibility {
  if (col.hideable === false) return { hidden: false, auto: false };
  const override = overrides[col.id];
  if (override === true) return { hidden: false, auto: false };
  if (override === false) return { hidden: true, auto: false };
  if (col.degenValue && columnIsDegenerate(rows, col.degenValue)) {
    return { hidden: true, auto: true };
  }
  return { hidden: false, auto: false };
}

/**
 * Load persisted overrides. Best-effort: any storage or parse failure yields no
 * overrides, i.e. pure automatic behavior.
 */
export function loadOverrides(storage: Storage | undefined = globalThis.localStorage): ColumnOverrides {
  try {
    const raw = storage?.getItem(STORAGE_KEY);
    if (!raw) return {};
    const obj: unknown = JSON.parse(raw);
    return obj != null && typeof obj === "object" && !Array.isArray(obj)
      ? (obj as ColumnOverrides)
      : {};
  } catch {
    return {};
  }
}

/** Persist overrides; storage failures leave the in-memory choice intact. */
export function saveOverrides(
  overrides: ColumnOverrides,
  storage: Storage | undefined = globalThis.localStorage,
): void {
  try {
    storage?.setItem(STORAGE_KEY, JSON.stringify(overrides));
  } catch {
    // Storage unavailable (private mode, disabled): keep in-memory only.
  }
}

/** Set an explicit choice, or clear it (`undefined`) to follow the auto rule. */
export function setOverride(
  overrides: ColumnOverrides,
  id: string,
  value: boolean | undefined,
): ColumnOverrides {
  const next = { ...overrides };
  if (value === undefined) delete next[id];
  else next[id] = value;
  return next;
}
