// Parser for the feature-inventory markdown files plus the shared verdict
// mapping.
//
// An inventory file contains:
//   - feature tables (sections A..): first cell "A1. <name>" — these are the
//     rows the walker enumerates;
//   - a base live-validation verdict table (header "| Feature | Verdict |..."),
//     using the 7-value legend;
//   - optionally a refresh verdict table (header "| Row | Verdict |...").
//     Where a refresh entry amends a row that also appears in the base
//     table, the refresh entry SUPERSEDES it.
//
// Verdict mapping:
//   - recorded VERIFIED and DEAD-CONFIRMED rows are GATED: the walker must
//     re-derive VERIFIED for them (a DEAD row's access path producing no
//     effect IS its expected behavior);
//   - recorded PARTIAL / NOT-TESTED / CODE-ONLY / NOT-TRIGGERABLE — and the
//     refresh table's CODE-READ / VERIFIED(API) / NOT-TRIGGERABLE entries —
//     map to NOT-TRIGGERABLE: listed, not gated. NOT-OBSERVED (the base
//     table's 7th value) is likewise listed, not gated.
//   - "green" = zero FAILED.

import fs from "node:fs";

const ROW_ID = /^([A-Z]\d+)\b/;

/** Split a markdown table line into trimmed cells. */
function cells(line) {
  return line
    .replace(/^\s*\|/, "")
    .replace(/\|\s*$/, "")
    .split("|")
    .map((c) => c.trim());
}

function isTableLine(line) {
  return /^\s*\|/.test(line);
}

function isSeparatorRow(line) {
  return /^\s*\|[\s\-:|]+\|\s*$/.test(line);
}

/**
 * Normalize a recorded verdict cell to a canonical token.
 * Order matters: "VERIFIED (API)" must not classify as plain VERIFIED, and
 * "DEAD (re-confirmed by code)" counts as DEAD-CONFIRMED.
 */
export function normalizeRecorded(cell) {
  const v = cell.trim();
  if (/^VERIFIED\s*\(API\)/i.test(v)) return "VERIFIED-API";
  if (/^VERIFIED\b/i.test(v)) return "VERIFIED";
  if (/^DEAD\b/i.test(v)) return "DEAD-CONFIRMED";
  if (/^PARTIAL\b/i.test(v)) return "PARTIAL";
  if (/^NOT-TRIGGERABLE\b/i.test(v)) return "NOT-TRIGGERABLE";
  if (/^NOT-TESTED\b/i.test(v)) return "NOT-TESTED";
  if (/^NOT-OBSERVED\b/i.test(v)) return "NOT-OBSERVED";
  if (/^CODE-ONLY\b/i.test(v)) return "CODE-ONLY";
  if (/^CODE-READ\b/i.test(v)) return "CODE-READ";
  throw new Error(`unrecognized recorded verdict: "${cell}"`);
}

/** Recorded verdicts whose rows the walker must re-derive as VERIFIED. */
export function isGated(normalized) {
  return normalized === "VERIFIED" || normalized === "DEAD-CONFIRMED";
}

/**
 * Parse an inventory markdown file.
 *
 * Returns:
 *   rows      — [{ id, feature }] in file order, from the feature tables;
 *   recorded  — Map<id, { verdict, normalized, source, note }> with refresh
 *               entries superseding base entries for the same id.
 */
export function parseInventory(path) {
  const text = fs.readFileSync(path, "utf8");
  const lines = text.split("\n");

  const rows = [];
  const seen = new Set();
  const base = new Map();
  const refresh = new Map();

  // Which verdict table (if any) the current markdown table is, keyed off its
  // header row. Feature tables have header "Feature | What it does | ...".
  let table = null; // null | "feature" | "base" | "refresh"

  for (const line of lines) {
    if (!isTableLine(line)) {
      table = null;
      continue;
    }
    if (isSeparatorRow(line)) continue;
    const c = cells(line);

    // Header rows select the table kind.
    if (c[0] === "Feature" && c[1] === "Verdict") {
      table = "base";
      continue;
    }
    if (c[0] === "Row" && c[1] === "Verdict") {
      table = "refresh";
      continue;
    }
    if ((c[0] === "Feature" || c[0] === "Behavior") && c[1] !== "Verdict") {
      table = "feature";
      continue;
    }
    if (table === null && !ROW_ID.test(c[0])) continue; // e.g. the "How to read" table

    if (table === "feature") {
      const m = c[0].match(/^([A-Z]\d+)\.\s*(.*)/);
      if (!m) continue;
      const id = m[1];
      if (!seen.has(id)) {
        seen.add(id);
        rows.push({ id, feature: m[2].replace(/`/g, "") });
      }
    } else if (table === "base" || table === "refresh") {
      // First cell may name several rows ("F3/G7 (amended) ..."); collect
      // every leading row-id token before the descriptive text starts.
      const ids = [];
      let rest = c[0];
      for (;;) {
        const m = rest.match(/^([A-Z]\d+)\s*(?:\/\s*)?/);
        if (!m) break;
        ids.push(m[1]);
        rest = rest.slice(m[0].length);
        if (!rest.startsWith("/") && !/^[A-Z]\d+/.test(rest)) break;
      }
      if (!ids.length) continue;
      const entry = {
        verdict: c[1],
        normalized: normalizeRecorded(c[1]),
        source: table,
        note: c[2] ?? "",
      };
      const target = table === "base" ? base : refresh;
      for (const id of ids) target.set(id, entry);
    }
  }

  // Refresh supersedes base where both mention the same row.
  const recorded = new Map(base);
  for (const [id, entry] of refresh) recorded.set(id, entry);

  return { rows, recorded };
}
