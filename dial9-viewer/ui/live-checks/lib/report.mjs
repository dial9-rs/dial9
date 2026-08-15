// Report rendering shared by the live UI checks: the verdict table renders one
// markdown row per feature — | Feature | Verdict | Evidence / note |.

import fs from "node:fs";
import path from "node:path";

export function verdictTable(results) {
  const lines = ["| Feature | Verdict | Evidence / note |", "|---|---|---|"];
  for (const r of results) {
    const note = (r.note ?? "").replace(/\|/g, "\\|").replace(/\n/g, " ");
    lines.push(`| ${r.id} ${r.feature} | ${r.verdict} | ${note} |`);
  }
  return lines.join("\n");
}

export function summarize(results) {
  const counts = {};
  for (const r of results) counts[r.verdict] = (counts[r.verdict] ?? 0) + 1;
  return counts;
}

export function writeReport(outPath, content) {
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, content);
}

export function writeJson(outPath, data) {
  writeReport(outPath, JSON.stringify(data, null, 2) + "\n");
}
