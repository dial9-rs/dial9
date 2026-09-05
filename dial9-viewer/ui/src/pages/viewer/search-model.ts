// Unified viewer search: a trace -> a searchable index -> ranked results across
// the three navigable domains (tasks, spans, points of interest). Pure - no DOM,
// no store. The overlay renders the results; the shell dispatches the jump.

import { deriveLaneData } from "../../components/canvas/lanes/index.js";
import { POI_FILTERS, poiSourceFor, poisForFilter, filterLabel } from "./poi.js";
import { formatFieldValue } from "../../lib/trace/index.js";
import type { ParsedTrace, PointOfInterest } from "../../types/trace.js";

export type SearchKind = "task" | "span" | "poi";

/** Where picking a result takes the viewer (dispatched by the shell). */
export type SearchNav =
  | { kind: "task"; taskId: number; startNs: number; endNs: number }
  | { kind: "span"; spanId: string; startNs: number; endNs: number }
  | { kind: "poi"; poi: PointOfInterest };

export interface SearchResult {
  kind: SearchKind;
  /** Primary line (task id / span name / POI label) + muted secondary. */
  label: string;
  sublabel: string;
  nav: SearchNav;
}

// Index entries carry a pre-lowercased `hay` (matched against) and `primary`
// (the field a prefix hit ranks highest on), so a keystroke never re-lowercases.
interface Entry {
  hay: string;
  primary: string;
  label: string;
  sublabel: string;
}
interface TaskEntry extends Entry { taskId: number; startNs: number; endNs: number; }
interface SpanEntry extends Entry { spanId: string; startNs: number; endNs: number; }
interface PoiEntry extends Entry { poi: PointOfInterest; }

/** The frozen per-trace index (built once via store.derived(["trace"])). */
export interface SearchIndex {
  tasks: readonly TaskEntry[];
  spans: readonly SpanEntry[];
  pois: readonly PoiEntry[];
}

export interface SearchResults {
  tasks: readonly SearchResult[];
  spans: readonly SearchResult[];
  pois: readonly SearchResult[];
  /** Total rows across all groups, post-cap. */
  total: number;
}

type Lane = ReturnType<typeof deriveLaneData>;

function hexId(id: number): string {
  return "0x" + id.toString(16);
}

/** Distinct tasks with their time extent, from every worker's polls. */
function buildTasks(lane: Lane): TaskEntry[] {
  const byId = new Map<number, { start: number; end: number; spawnLoc: string | null }>();
  for (const w of lane.workerIds) {
    for (const p of lane.workerSpans[w]?.polls ?? []) {
      const cur = byId.get(p.taskId);
      if (cur === undefined) {
        byId.set(p.taskId, { start: p.start, end: p.end, spawnLoc: p.spawnLoc });
      } else {
        if (p.start < cur.start) cur.start = p.start;
        if (p.end > cur.end) cur.end = p.end;
        if (cur.spawnLoc === null && p.spawnLoc !== null) cur.spawnLoc = p.spawnLoc;
      }
    }
  }
  const out: TaskEntry[] = [];
  for (const [taskId, v] of byId) {
    const id = hexId(taskId);
    const loc = v.spawnLoc ?? "";
    out.push({
      taskId,
      startNs: v.start,
      endNs: v.end,
      label: id,
      sublabel: loc === "" ? "(no spawn location)" : loc,
      primary: id.toLowerCase(),
      // Include the decimal id so a paste of either form matches.
      hay: (id + " " + taskId + " " + loc).toLowerCase(),
    });
  }
  return out;
}

/** Every uniquely-identified span, matched by name + user fields. */
function buildSpans(lane: Lane): SpanEntry[] {
  const out: SpanEntry[] = [];
  for (const span of lane.spanByIdSingle.values()) {
    const fieldsText = Object.entries(span.fields)
      .map(([k, val]) => `${k}=${formatFieldValue(val)}`)
      .join(" ");
    out.push({
      spanId: span.spanId,
      startNs: span.start,
      endNs: span.end,
      label: span.spanName,
      sublabel: fieldsText,
      primary: span.spanName.toLowerCase(),
      hay: (span.spanName + " " + fieldsText).toLowerCase(),
    });
  }
  return out;
}

/**
 * Points of interest across every detector, deduped by (type, time, worker).
 *
 * Built ONCE per trace, so "spawn-delay" is indexed at its default threshold:
 * re-indexing every task and span on each threshold edit would cost far more
 * than the few jump targets it would add or drop.
 */
function buildPois(trace: ParsedTrace): PoiEntry[] {
  const source = poiSourceFor(trace);
  const out: PoiEntry[] = [];
  const seen = new Set<string>();
  for (const type of POI_FILTERS) {
    const label = filterLabel(type);
    for (const poi of poisForFilter(source, type)) {
      const key = `${type}:${poi.time}:${poi.worker}`;
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({
        poi,
        label,
        sublabel: `W${poi.worker}`,
        primary: type.toLowerCase(),
        hay: (type + " " + label + " w" + poi.worker).toLowerCase(),
      });
    }
  }
  return out;
}

export function buildSearchIndex(trace: ParsedTrace): SearchIndex {
  const lane = deriveLaneData(trace);
  return { tasks: buildTasks(lane), spans: buildSpans(lane), pois: buildPois(trace) };
}

/** Substring match; a prefix hit on the primary field ranks above a body hit. */
function rank<T extends Entry>(entries: readonly T[], q: string, cap: number): T[] {
  const hits: { e: T; score: number; i: number }[] = [];
  for (let i = 0; i < entries.length; i++) {
    const e = entries[i]!;
    if (!e.hay.includes(q)) continue;
    hits.push({ e, score: e.primary.startsWith(q) ? 2 : 1, i });
  }
  hits.sort((a, b) => b.score - a.score || a.i - b.i);
  return hits.slice(0, cap).map((h) => h.e);
}

function taskResult(e: TaskEntry): SearchResult {
  return {
    kind: "task",
    label: e.label,
    sublabel: e.sublabel,
    nav: { kind: "task", taskId: e.taskId, startNs: e.startNs, endNs: e.endNs },
  };
}
function spanResult(e: SpanEntry): SearchResult {
  return {
    kind: "span",
    label: e.label,
    sublabel: e.sublabel,
    nav: { kind: "span", spanId: e.spanId, startNs: e.startNs, endNs: e.endNs },
  };
}
function poiResult(e: PoiEntry): SearchResult {
  return { kind: "poi", label: e.label, sublabel: e.sublabel, nav: { kind: "poi", poi: e.poi } };
}

/**
 * Rank `query` across the index, capped per kind. An empty/whitespace query
 * returns nothing (the palette shows its prompt instead of every row).
 */
export function searchQuery(index: SearchIndex, query: string, perKind = 8): SearchResults {
  const q = query.trim().toLowerCase();
  if (q === "") return { tasks: [], spans: [], pois: [], total: 0 };
  const tasks = rank(index.tasks, q, perKind).map(taskResult);
  const spans = rank(index.spans, q, perKind).map(spanResult);
  const pois = rank(index.pois, q, perKind).map(poiResult);
  return { tasks, spans, pois, total: tasks.length + spans.length + pois.length };
}

/**
 * A viewport window framing `[startNs, endNs]` with 30% padding (min 0.5ms),
 * clamped to the navigable extent. Falls back to a fixed window around the
 * start when the range is degenerate.
 */
export function searchWindow(
  startNs: number,
  endNs: number,
  minTs: number,
  maxTs: number,
): { start: number; end: number } {
  const pad = Math.max((endNs - startNs) * 0.3, 5e5);
  let start = Math.max(minTs, startNs - pad);
  let end = Math.min(maxTs, endNs + pad);
  if (end <= start) {
    start = Math.max(minTs, startNs - 5e5);
    end = Math.min(maxTs, startNs + 5e5);
  }
  return { start, end };
}
