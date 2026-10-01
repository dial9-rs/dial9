import { nothing } from "lit-html";
import { describe, expect, it, vi } from "vitest";
import { hiddenColumns } from "./columns.js";
import { moreColumnsTemplate } from "./exemplars.js";

interface Row {
  host: string;
  cacheHit: string;
}

interface TemplateParts {
  strings: string[];
  leaves: unknown[];
}

function isTemplateResult(
  value: unknown,
): value is { strings: readonly string[]; values: unknown[] } {
  return (
    typeof value === "object" &&
    value !== null &&
    "strings" in value &&
    "values" in value &&
    Array.isArray((value as { values: unknown }).values)
  );
}

function walkTemplate(value: unknown, parts: TemplateParts): void {
  if (value == null) return;
  if (Array.isArray(value)) {
    for (const item of value) walkTemplate(item, parts);
    return;
  }
  if (isTemplateResult(value)) {
    parts.strings.push(...value.strings);
    for (const item of value.values) walkTemplate(item, parts);
    return;
  }
  parts.leaves.push(value);
}

function splitTemplate(value: unknown): TemplateParts {
  const parts: TemplateParts = { strings: [], leaves: [] };
  walkTemplate(value, parts);
  return parts;
}

describe("Span Explorer hidden-column affordance", () => {
  const rows: Row[] = [
    { host: "host-a", cacheHit: "false" },
    { host: "host-a", cacheHit: "false" },
  ];
  const columns = [
    { id: "jump", label: "Jump", hideable: false },
    { id: "duration", label: "Duration" },
    { id: "host", label: "Host", degenValue: (row: Row) => row.host },
    {
      id: "attr:CacheHit",
      label: "CacheHit",
      degenValue: (row: Row) => row.cacheHit,
    },
  ];

  it("collects every currently hidden column", () => {
    expect(hiddenColumns(columns, rows, {}).map((column) => column.id)).toEqual([
      "host",
      "attr:CacheHit",
    ]);
    expect(hiddenColumns(columns, rows, { duration: false }).map((column) => column.id))
      .toEqual(["duration", "host", "attr:CacheHit"]);
  });

  it("renders hidden column names as actions that enable each column", () => {
    const onEnable = vi.fn();
    const template = moreColumnsTemplate(
      [
        { id: "host", label: "Host" },
        { id: "attr:CacheHit", label: "CacheHit" },
      ],
      onEnable,
    );
    const parts = splitTemplate(template);

    expect(parts.strings.join("")).toContain("More columns:");
    expect(parts.leaves).toContain("Host");
    expect(parts.leaves).toContain("CacheHit");

    const actions = parts.leaves.filter(
      (value): value is () => void => typeof value === "function",
    );
    expect(actions).toHaveLength(2);
    actions[1]!();
    expect(onEnable).toHaveBeenCalledWith("attr:CacheHit");
  });

  it("renders nothing when every column is visible", () => {
    expect(moreColumnsTemplate([], vi.fn())).toBe(nothing);
  });
});
