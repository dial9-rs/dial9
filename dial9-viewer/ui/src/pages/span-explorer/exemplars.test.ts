import { nothing } from "lit-html";
import { describe, expect, it, vi } from "vitest";
import { hiddenColumns } from "./columns.js";
import {
  columnHeaderTemplate,
  moreColumnsTemplate,
} from "./exemplars.js";

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
    {
      id: "composition",
      label: "Time composition",
      hideable: false,
      degenValue: () => "same",
    },
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
    expect(
      hiddenColumns(columns, rows, { composition: false }).map((column) => column.id),
    ).not.toContain("composition");
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

  it("adds a hide action to hideable headers only", () => {
    const onHide = vi.fn();
    const hideable = splitTemplate(
      columnHeaderTemplate(
        { id: "attr:CacheHit", th: "CacheHit", label: "CacheHit" },
        onHide,
      ),
    );

    expect(hideable.leaves).toContain("CacheHit");
    expect(hideable.strings.join("")).toContain("✕");
    const actions = hideable.leaves.filter(
      (value): value is () => void => typeof value === "function",
    );
    expect(actions).toHaveLength(1);
    actions[0]!();
    expect(onHide).toHaveBeenCalledWith("attr:CacheHit");

    const permanent = splitTemplate(
      columnHeaderTemplate(
        {
          id: "composition",
          th: "Time composition",
          label: "Time composition",
          hideable: false,
        },
        onHide,
      ),
    );
    expect(permanent.leaves).toContain("Time composition");
    expect(permanent.strings.join("")).not.toContain("✕");
    expect(permanent.leaves.filter((value) => typeof value === "function")).toHaveLength(0);
  });
});
