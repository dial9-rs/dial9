// The page's pure stats computation: computeStats + the diff-view math.
// Everything here is a pure function of one cached /api/tokio-stats response,
// re-run on every render.

import type {
  PollExemplar,
  SpawnLocStats,
  TokioStatsResponse,
} from "../../lib/trace/index.js";

/** Per-spawn-location render model (one row of the single-period table). */
export interface LocStats {
  /** Polls above the current threshold (the "Long" column). */
  long: number;
  /** Off-CPU (class 0), on-CPU (class 1), mixed (class 2) among the long. */
  offCpu: number;
  onCpu: number;
  /** Computed then unused by the UI - carried for parity. */
  mixed: number;
  /** Long polls per minute, over the RESPONSE's observed span. */
  rate: number;
  /** Percentiles over the DESC-sorted above-threshold durations. */
  p50: number;
  p99: number;
  max: number;
  /** [off_cpu, on_cpu, mixed, unknown] worst-per-class exemplars. */
  exemplars: SpawnLocStats["exemplars"];
}

/** One period's full render model. */
export interface PeriodStats {
  totalLong: number;
  totalOffCpu: number;
  totalOnCpu: number;
  timeMinutes: number;
  byLoc: Record<string, LocStats>;
  rate: number;
  total_polls: number;
}

/**
 * Turn one cached response into the render model. Null in -> null out (an
 * unloaded period). Correctness of p50/p99/max depends on the wire contract
 * sorting `durations_ns` DESCENDING; re-sorting ascending would silently swap
 * p50/p99.
 */
export function computeStats(
  data: TokioStatsResponse | null,
  threshNs: number,
): PeriodStats | null {
  if (!data) return null;
  const timeMinutes = data.time_span_ns / 60e9;
  let totalLong = 0;
  let totalOffCpu = 0;
  let totalOnCpu = 0;
  const byLoc: Record<string, LocStats> = {};
  for (const loc of data.by_spawn_loc) {
    let long = 0;
    let offCpu = 0;
    let onCpu = 0;
    let mixed = 0;
    for (let i = 0; i < loc.durations_ns.length; i++) {
      if (loc.durations_ns[i]! >= threshNs) {
        long++;
        const c = loc.classes[i];
        if (c === 0) offCpu++;
        else if (c === 1) onCpu++;
        else if (c === 2) mixed++;
        // c === 3 is "unknown" (too short to classify) - counted in long
        // but not bucketed.
      }
    }
    totalLong += long;
    totalOffCpu += offCpu;
    totalOnCpu += onCpu;
    const above = loc.durations_ns.filter((d) => d >= threshNs);
    const n = above.length;
    byLoc[loc.spawn_loc] = {
      long,
      offCpu,
      onCpu,
      mixed,
      rate: timeMinutes > 0 ? long / timeMinutes : 0,
      p50: n > 0 ? above[Math.floor(n * 0.5)]! : 0,
      p99: n > 0 ? above[Math.floor(n * 0.01)]! : 0,
      max: n > 0 ? above[0]! : 0,
      exemplars: loc.exemplars,
    };
  }
  return {
    totalLong,
    totalOffCpu,
    totalOnCpu,
    timeMinutes,
    byLoc,
    rate: timeMinutes > 0 ? totalLong / timeMinutes : 0,
    total_polls: data.total_polls,
  };
}

/** One row of a single-period table (a byLoc entry with its location). */
export interface LocRow {
  loc: string;
  v: LocStats;
}

/**
 * Rows for the single-period table: locations with `long > 0`, sorted by rate
 * descending. Empty when nothing exceeds the threshold.
 */
export function singlePeriodRows(s: PeriodStats): LocRow[] {
  return Object.entries(s.byLoc)
    .filter(([, v]) => v.long > 0)
    .sort(([, a], [, b]) => b.rate - a.rate)
    .map(([loc, v]) => ({ loc, v }));
}

/** A diff-table row. `pct` is rendered verbatim - see the quirk below. */
export interface DiffRow {
  loc: string;
  fRate: number;
  lRate: number;
  delta: number;
  /**
   * The Delta% cell. QUIRK preserved: the value is always a string
   * (`toFixed(0)` or "new") and is rendered raw - the code path that would have
   * appended "%" / "+" is dead, so the cell shows e.g. "50" or "new", never
   * "50%".
   */
  pct: string;
  first: LocStats | undefined;
  last: LocStats | undefined;
}

/** The four diff summary cards. */
export interface DiffCards {
  firstRate: number;
  lastRate: number;
  arrow: string;
  cls: string;
  /** The "(...%)" label body, e.g. "+50%", "+∞%", "0%". */
  ratePctLabel: string;
  offFirst: number;
  offLast: number;
  offCls: string;
  firstTotalPolls: number;
  lastTotalPolls: number;
}

/** The full diff render model (cards + the three ranked tables). */
export interface DiffModel {
  cards: DiffCards;
  regressions: DiffRow[];
  improvements: DiffRow[];
  newOffenders: DiffRow[];
}

/**
 * Build the first-vs-last diff model. Compares stats[0] (P1) against stats[last]
 * ONLY; middle periods contribute locations to the union but not their rates.
 * KNOWN DEFECT (preserved): if P1 failed to load, `first` is null and this
 * throws a TypeError - no guard is added.
 */
export function buildDiffModel(
  stats: (PeriodStats | null)[],
  periodsLength: number,
): DiffModel {
  const first = stats[0]!;
  const last = stats[stats.length - 1]!;
  const rateDelta = last.rate - first.rate;
  const ratePct =
    first.rate > 0
      ? ((rateDelta / first.rate) * 100).toFixed(0)
      : last.rate > 0
        ? "+∞"
        : "0";
  const offFirst = first.timeMinutes > 0 ? first.totalOffCpu / first.timeMinutes : 0;
  const offLast = last.timeMinutes > 0 ? last.totalOffCpu / last.timeMinutes : 0;
  const offDelta = offLast - offFirst;

  const arrow = rateDelta > 0 ? "↑" : rateDelta < 0 ? "↓" : "→";
  const cls = rateDelta > 0.5 ? "bad" : rateDelta < -0.5 ? "good" : "";
  const offCls = offDelta > 0.1 ? "bad" : offDelta < -0.1 ? "good" : "";
  // Loose comparison `ratePct > 0` (string vs number): the "+" prefix shows
  // only for a positive numeric percentage; "+∞" and "0" and any negative
  // string coerce to NaN/<=0 and get no prefix (their sign, if any, is already
  // in the string).
  const ratePctLabel = (Number(ratePct) > 0 ? "+" : "") + ratePct + "%";

  const allLocs = new Set<string>();
  for (const s of stats) {
    if (s) for (const k of Object.keys(s.byLoc)) allLocs.add(k);
  }

  const diffs = [...allLocs]
    .map((loc) => {
      const f = first.byLoc[loc];
      const l = last.byLoc[loc];
      const fRate = f ? f.rate : 0;
      const lRate = l ? l.rate : 0;
      return { loc, fRate, lRate, delta: lRate - fRate, first: f, last: l };
    })
    .filter((d) => d.fRate > 0 || d.lRate > 0);

  const toRow = (d: (typeof diffs)[number]): DiffRow => ({
    loc: d.loc,
    fRate: d.fRate,
    lRate: d.lRate,
    delta: d.delta,
    pct: d.fRate > 0 ? ((d.delta / d.fRate) * 100).toFixed(0) : "new",
    first: d.first,
    last: d.last,
  });

  const regressions = diffs
    .filter((d) => d.delta > 0.1)
    .sort((a, b) => b.delta - a.delta)
    .map(toRow);
  const improvements = diffs
    .filter((d) => d.delta < -0.1)
    .sort((a, b) => a.delta - b.delta)
    .map(toRow);
  const newOffenders = diffs
    .filter((d) => d.fRate === 0 && d.lRate > 0)
    .sort((a, b) => b.lRate - a.lRate)
    .map(toRow);

  void periodsLength; // periodsLength is a render-time label (P{n}); see render.ts
  return {
    cards: {
      firstRate: first.rate,
      lastRate: last.rate,
      arrow,
      cls,
      ratePctLabel,
      offFirst,
      offLast,
      offCls,
      firstTotalPolls: first.total_polls,
      lastTotalPolls: last.total_polls,
    },
    regressions,
    improvements,
    newOffenders,
  };
}

// Re-export for tests that build synthetic exemplars.
export type { PollExemplar };
