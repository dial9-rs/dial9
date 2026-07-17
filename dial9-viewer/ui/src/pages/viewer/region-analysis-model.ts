// Pure derivations for the in-viewer region-analysis panel: CPU flamegraph
// samples, off-CPU blocking-call groups, heap (Horvitz-Thompson) estimates, and
// partial-window coverage. No store, no DOM - every function is referentially
// transparent so region-analysis.ts (which owns the widget lifecycle) renders
// from these.

import {
  OFF_WORKER_WORKER_ID,
  findSpanAt,
  formatFrame,
  formatHumanDuration,
  symbolizeChain,
} from "../../lib/trace/index.js";
import type {
  AllocEvent,
  CallframeSymbols,
  CpuSample,
  ParsedTrace,
  PollSpan,
  WorkerLane,
} from "../../lib/trace/index.js";
import type { FlamegraphDataSample } from "../../lib/canvas/index.js";
import type { TimeRange } from "../../types/trace.js";
import type { SegmentsSlice } from "../../types/state.js";

/** Worker-lane bundle the region analyses read. */
export type WorkerSpans = Record<number, WorkerLane>;

// ── Region modes present + default ────────────────────────────────────────────

/** The three range-scoped analysis modes (Flamegraph / Blocking / Heap). */
export type RegionMode = "cpu" | "blocking" | "heap";

/** Which analyses have data in the region. */
export interface RegionModesPresent {
  cpu: boolean;
  blocking: boolean;
  heap: boolean;
}

/**
 * The `filterCpuSamples` predicate: an on-CPU sample (`source !== 1`) carrying a
 * stack (`callchain.length > 0`). Off-CPU/scheduling samples (`source === 1`)
 * and stackless samples are excluded - they are the units the count
 * reconciliation separates (see cpuRegionView).
 */
export function isFoldableCpuSample(
  s: Pick<CpuSample, "callchain" | "source">,
): boolean {
  return s.callchain.length > 0 && s.source !== 1;
}

/** Foldable on-CPU samples in [start,end] (nulls = open bound). */
export function filterRegionCpuSamples(
  cpuSamples: readonly CpuSample[],
  startNs: number | null,
  endNs: number | null,
): CpuSample[] {
  const out: CpuSample[] = [];
  for (const s of cpuSamples) {
    if (!isFoldableCpuSample(s)) continue;
    if (startNs != null && s.timestamp < startNs) continue;
    if (endNs != null && s.timestamp > endNs) continue;
    out.push(s);
  }
  return out;
}

/** One in-range scheduling (off-CPU) sample attached to the poll it blocked in. */
export interface SchedEntry {
  sample: CpuSample;
  poll: PollSpan;
  worker: number;
}

/**
 * Collect the in-poll scheduling (off-CPU) samples within `range` (null = whole
 * trace): kernel deschedules recorded inside a worker poll. Only these back the
 * blocking-calls panel; a sched sample outside any poll is not counted.
 */
export function collectSchedSamplesInRange(
  workerSpans: WorkerSpans,
  workerIds: readonly number[],
  range: TimeRange | null,
): SchedEntry[] {
  const out: SchedEntry[] = [];
  for (const w of workerIds) {
    const lane = workerSpans[w];
    if (!lane) continue;
    for (const poll of lane.polls) {
      if (!poll.schedSamples) continue;
      if (range && (poll.end < range.startNs || poll.start > range.endNs)) continue;
      for (const s of poll.schedSamples) {
        if (range && (s.timestamp < range.startNs || s.timestamp > range.endNs)) continue;
        out.push({ sample: s, poll, worker: w });
      }
    }
  }
  return out;
}

/** Whether alloc events with stacks exist in `range`. */
export function hasHeapInRange(
  allocEvents: readonly AllocEvent[],
  range: TimeRange | null,
): boolean {
  for (const a of allocEvents) {
    if (a.callchain.length === 0) continue;
    if (range && (a.timestamp < range.startNs || a.timestamp > range.endNs)) continue;
    return true;
  }
  return false;
}

/**
 * Which analyses have data in `range` (null = whole trace). Drives the sub-tab
 * family (Flamegraph / Blocking / Heap) and the default-mode pick.
 */
export function regionModesPresent(
  trace: ParsedTrace,
  workerSpans: WorkerSpans,
  workerIds: readonly number[],
  range: TimeRange | null,
): RegionModesPresent {
  const cpu =
    filterRegionCpuSamples(trace.cpuSamples, range?.startNs ?? null, range?.endNs ?? null)
      .length > 0;
  const blocking = collectSchedSamplesInRange(workerSpans, workerIds, range).length > 0;
  const heap = hasHeapInRange(trace.allocEvents, range);
  return { cpu, blocking, heap };
}

/**
 * The mode a fresh region opens in: sched-only -> blocking; heap-but-no-cpu ->
 * heap; otherwise CPU flamegraph. Returns null when the region has NO analyzable
 * data (the caller shows the no-data hint).
 */
export function defaultRegionMode(present: RegionModesPresent): RegionMode | null {
  if (present.blocking && !present.cpu && !present.heap) return "blocking";
  if (!present.cpu && present.heap) return "heap";
  if (present.cpu) return "cpu";
  if (present.blocking) return "blocking";
  if (present.heap) return "heap";
  return null;
}

// ── CPU region view + count reconciliation ────────────────────────────────────

/** The CPU flamegraph view for a region. */
export interface CpuRegionView {
  /** Foldable on-CPU samples, ready for the widget's setData. */
  samples: CpuSample[];
  /** How many foldable samples build the tree (the flamegraph "N samples"). */
  foldableCount: number;
  /** Total CPU sample RECORDS in range - the unit the toolbar label counts. */
  totalRecords: number;
  /**
   * The toolbar "Flamegraph (8993)" counts ALL cpuSamples records (on-CPU +
   * off-CPU/scheduling + stackless); the flamegraph shows only the foldable
   * on-CPU-with-stacks subset ("147 samples"). Not a bug - different units.
   * This label states both so the two never read as a contradiction on screen.
   */
  countLabel: string;
}

/** Count CPU sample records (all sources/stacks) in `range` (null = whole). */
export function countCpuRecords(
  cpuSamples: readonly CpuSample[],
  range: TimeRange | null,
): number {
  if (range === null) return cpuSamples.length;
  let n = 0;
  for (const s of cpuSamples) {
    if (s.timestamp < range.startNs || s.timestamp > range.endNs) continue;
    n += 1;
  }
  return n;
}

/** Build the CPU flamegraph view for a region. */
export function cpuRegionView(
  trace: ParsedTrace,
  range: TimeRange | null,
): CpuRegionView {
  const samples = filterRegionCpuSamples(
    trace.cpuSamples,
    range?.startNs ?? null,
    range?.endNs ?? null,
  );
  const foldableCount = samples.length;
  const totalRecords = countCpuRecords(trace.cpuSamples, range);
  return {
    samples,
    foldableCount,
    totalRecords,
    countLabel: cpuCountLabel(foldableCount, totalRecords),
  };
}

/** The reconciliation label: "147 samples (on-CPU, with stacks) of 8,993 CPU
 *  records". When the two coincide (all records foldable), the of-clause drops. */
export function cpuCountLabel(foldable: number, total: number): string {
  const base = `${foldable.toLocaleString()} sample${foldable === 1 ? "" : "s"} (on-CPU, with stacks)`;
  if (total <= foldable) return base;
  return `${base} of ${total.toLocaleString()} CPU records`;
}

// ── Heap region view ──────────────────────────────────────────────────────────

/** Sampling reservoir constant R for the Horvitz-Thompson estimator. */
const HEAP_R = 524288;

/** Allocator hook frames stripped from the top of each heap callchain. */
const HOOK_PATTERNS: readonly string[] = [
  "memory_profiling::hook::on_alloc",
  "memory_profiling::allocator::Dial9Allocator",
  "__rustc::__rust_alloc",
  "__rustc::__rust_realloc",
];

/** A heap sample carrying BOTH weight axes; mapped to widget samples per mode. */
export interface HeapBaseSample {
  callchain: string[];
  workerId: number;
  spawnLoc: string | null;
  /** size * 1/P(sample) - unbiased bytes (Horvitz-Thompson). */
  byteWeight: number;
  /** 1/P(sample) - unbiased alloc count. */
  countWeight: number;
}

/** The heap flamegraph view for a region. */
export interface HeapRegionView {
  baseSamples: HeapBaseSample[];
  /** Raw sampled alloc count (post hook-strip / stack filter). */
  sampleCount: number;
  /** Estimated total bytes / allocs (Horvitz-Thompson sums). */
  estimatedBytes: number;
  estimatedAllocs: number;
}

/** Strip leading allocator-hook frames. */
function stripHookFrames(
  chain: readonly string[],
  callframeSymbols: CallframeSymbols,
): string[] {
  let start = 0;
  while (start < chain.length) {
    const key = chain[start];
    if (key === undefined) break;
    const entry = callframeSymbols.get(key);
    const frames = Array.isArray(entry) ? entry : [entry];
    const name = frames.map((f) => (f ? f.symbol : "")).join(" ");
    if (HOOK_PATTERNS.some((p) => name.includes(p))) {
      start += 1;
      continue;
    }
    break;
  }
  return start > 0 ? chain.slice(start) : chain.slice();
}

/** Spawn location of the poll `timestamp` fell in for tid->worker `wId` (binary
 *  search over the worker's polls). */
function spawnLocAt(
  workerSpans: WorkerSpans,
  wId: number | undefined,
  timestamp: number,
): string | null {
  if (wId == null) return null;
  const lane = workerSpans[wId];
  if (!lane || lane.polls.length === 0) return null;
  // findSpanAt: rightmost start <= timestamp, then confirm timestamp <= end -
  // identical to the previous inline search, and accessor-based so a columnar
  // lane view backs it.
  const cand = findSpanAt(lane.polls, timestamp);
  return cand ? cand.spawnLoc : null;
}

/**
 * Build the heap region view: filter allocs to `range`, strip hook frames, and
 * compute the per-sample unbiased byte/count weights via the Horvitz-Thompson
 * estimator invP = 1 / (1 - exp(-size/R)).
 */
export function heapRegionView(
  trace: ParsedTrace,
  range: TimeRange | null,
  workerSpans: WorkerSpans,
): HeapRegionView {
  const baseSamples: HeapBaseSample[] = [];
  let estimatedBytes = 0;
  let estimatedAllocs = 0;
  for (const a of trace.allocEvents) {
    if (range && (a.timestamp < range.startNs || a.timestamp > range.endNs)) continue;
    const chain = stripHookFrames(a.callchain, trace.callframeSymbols);
    if (chain.length === 0) continue;
    const wId = trace.tidToWorker.get(a.tid);
    const size = a.size;
    const ratio = size / HEAP_R;
    const invP = size <= 0 || ratio > 50 ? 1 : 1 / (1 - Math.exp(-ratio));
    const byteWeight = size * invP;
    const countWeight = invP;
    baseSamples.push({
      callchain: chain,
      workerId: wId != null ? 0 : OFF_WORKER_WORKER_ID,
      spawnLoc: spawnLocAt(workerSpans, wId, a.timestamp),
      byteWeight,
      countWeight,
    });
    estimatedBytes += byteWeight;
    estimatedAllocs += countWeight;
  }
  return {
    baseSamples,
    sampleCount: baseSamples.length,
    estimatedBytes,
    estimatedAllocs,
  };
}

/** Map heap base samples to widget samples for the Bytes/Count toggle:
 *  bytes -> weight=bytes/allocWeight=count; count -> swapped. */
export function heapSamplesForMode(
  base: readonly HeapBaseSample[],
  mode: "bytes" | "count",
): FlamegraphDataSample[] {
  return base.map((s) => ({
    callchain: s.callchain,
    workerId: s.workerId,
    spawnLoc: s.spawnLoc,
    weight: mode === "bytes" ? s.byteWeight : s.countWeight,
    allocWeight: mode === "bytes" ? s.countWeight : s.byteWeight,
  }));
}

/** Human bytes for heap labels. */
export function formatHeapBytes(bytes: number): string {
  return bytes >= 1e9
    ? (bytes / 1e9).toFixed(1) + " GB"
    : bytes >= 1e6
      ? (bytes / 1e6).toFixed(1) + " MB"
      : bytes >= 1e3
        ? (bytes / 1e3).toFixed(1) + " KB"
        : Math.round(bytes) + " B";
}

// ── Blocking-calls panel ──────────────────────────────────────────────────────

/** How the blocking-calls panel groups: by leaf frame or by full stack. */
export type BlockingGroupBy = "leaf" | "full";

/** One symbolized frame in a blocking sub-stack. */
export interface BlockingFrame {
  text: string;
  docsUrl: string | null;
  /** Frame's raw symbol contains "tokio::runtime" - dimmed as framework noise. */
  isTokio: boolean;
}

/** One unique full stack within a blocking group. */
export interface BlockingSubStack {
  count: number;
  /** Percent of the group. */
  pct: number;
  frames: BlockingFrame[];
}

/** One example poll link within a blocking group. */
export interface BlockingExample {
  worker: number;
  start: number;
  end: number;
  durLabel: string;
}

/** One blocking-call group (summary bar + sub-stacks + examples). */
export interface BlockingGroup {
  /** Leaf display text. */
  leaf: string;
  /** Leaf raw symbol (color classification + full-stack keys). */
  leafRaw: string;
  count: number;
  /** Percent of all sched events in scope. */
  pct: number;
  /** Summary bar width as a percent. */
  barPct: number;
  /** Color class from the leaf symbol. */
  color: BlockingColor;
  subStacks: BlockingSubStack[];
  examples: BlockingExample[];
  /** Count of polls beyond the 5 shown examples. */
  moreExamples: number;
}

/** The blocking-calls panel view. */
export interface BlockingView {
  total: number;
  groups: BlockingGroup[];
}

/** Color classification for a blocking leaf symbol. */
export type BlockingColor = "lock" | "io" | "poll" | "syscall" | "other";

/** Classify a leaf symbol into the summary palette. */
export function blockingColor(leafRaw: string): BlockingColor {
  if (leafRaw.includes("lock") || leafRaw.includes("mutex")) return "lock";
  if (leafRaw.includes("epoll") || leafRaw.includes("poll")) return "poll";
  if (
    leafRaw.includes("write") ||
    leafRaw.includes("read") ||
    leafRaw.includes("recv") ||
    leafRaw.includes("send")
  ) {
    return "io";
  }
  if (leafRaw.includes("syscall")) return "syscall";
  return "other";
}

function frameOf(frame: { symbol: string; location: string | null }): BlockingFrame {
  const f = formatFrame(frame);
  return { text: f.text, docsUrl: f.docsUrl, isTokio: frame.symbol.includes("tokio::runtime") };
}

/**
 * Build the blocking-calls panel view: group in-poll sched samples by leaf frame
 * or full stack, each with a summary bar, the unique full-stacks it spans, and
 * up to five example polls. Percentages are scope-total for the summary and
 * group-local for the sub-stacks.
 */
export function buildBlockingView(
  entries: readonly SchedEntry[],
  callframeSymbols: CallframeSymbols,
  groupBy: BlockingGroupBy,
): BlockingView {
  const total = entries.length;
  interface Acc {
    leaf: string;
    leafRaw: string;
    count: number;
    stacks: Map<string, { count: number; frames: { symbol: string; location: string | null }[] }>;
    polls: { poll: PollSpan; worker: number }[];
  }
  const groups = new Map<string, Acc>();
  for (const { sample, poll, worker } of entries) {
    const frames = symbolizeChain(sample.callchain, callframeSymbols);
    const first = frames[0];
    const leafRaw = first ? first.symbol : "(unknown)";
    const leafDisplay = first ? formatFrame(first).text : "(unknown)";
    const fullKey = frames.map((f) => f.symbol).join("\0");
    const key = groupBy === "leaf" ? leafRaw : fullKey;
    let g = groups.get(key);
    if (!g) {
      g = { leaf: leafDisplay, leafRaw, count: 0, stacks: new Map(), polls: [] };
      groups.set(key, g);
    }
    g.count += 1;
    g.polls.push({ poll, worker });
    let sub = g.stacks.get(fullKey);
    if (!sub) {
      sub = { count: 0, frames };
      g.stacks.set(fullKey, sub);
    }
    sub.count += 1;
  }

  const sorted = [...groups.values()].sort((a, b) => b.count - a.count);
  const out: BlockingGroup[] = sorted.map((g) => {
    const subStacks: BlockingSubStack[] = [...g.stacks.values()]
      .sort((a, b) => b.count - a.count)
      .map((sub) => ({
        count: sub.count,
        pct: g.count > 0 ? Math.round((sub.count / g.count) * 100) : 0,
        frames: sub.frames.map(frameOf),
      }));
    const examples: BlockingExample[] = g.polls.slice(0, 5).map((p) => ({
      worker: p.worker,
      start: p.poll.start,
      end: p.poll.end,
      durLabel: formatHumanDuration(p.poll.end - p.poll.start),
    }));
    return {
      leaf: g.leaf,
      leafRaw: g.leafRaw,
      count: g.count,
      pct: total > 0 ? Math.round((g.count / total) * 1000) / 10 : 0,
      barPct: Math.max(4, total > 0 ? (g.count / total) * 100 : 0),
      color: blockingColor(g.leafRaw),
      subStacks,
      examples,
      moreExamples: Math.max(0, g.polls.length - 5),
    };
  });
  return { total, groups: out };
}

// ── Frame -> timeline extent ──────────────────────────────────────────────────

/** A flamegraph zoom path per panel (getZoomPath()'s return shape). */
export interface ZoomPath {
  /** Display-name path (root child -> zoomed frame) in the worker panel. */
  worker: readonly string[];
  /** Display-name path in the off-worker panel. */
  offworker: readonly string[];
}

/**
 * The display-name path a sample contributes to the flamegraph tree, root child
 * first (outermost caller) to leaf. Reproduces the tree's per-node naming
 * exactly (reverse + inline-frame expansion + formatFrame) so a prefix match
 * against getZoomPath()'s names is exact.
 */
export function displayNamePath(
  callchain: readonly string[],
  callframeSymbols: CallframeSymbols,
): string[] {
  const names: string[] = [];
  const chain = callchain.slice().reverse();
  for (const addr of chain) {
    const entry = callframeSymbols.get(addr);
    const frames = Array.isArray(entry) ? entry : [entry];
    for (let fi = 0; fi < frames.length; fi++) {
      const resolved = frames[fi];
      if (fi > 0 && !resolved) continue;
      const formatted = resolved ? formatFrame(resolved) : formatFrame(addr, callframeSymbols);
      names.push(formatted.text);
    }
  }
  return names;
}

function pathStartsWith(path: readonly string[], prefix: readonly string[]): boolean {
  if (prefix.length === 0 || prefix.length > path.length) return false;
  for (let i = 0; i < prefix.length; i++) {
    if (path[i] !== prefix[i]) return false;
  }
  return true;
}

/** A sample the extent walk reads: a stack + panel routing + a timestamp. */
export interface ExtentSample {
  callchain: string[];
  workerId: number;
  timestamp: number;
}

/**
 * The [min,max] timestamp extent of the samples flowing through the currently
 * zoomed frame. `samples` are the SAME samples handed to the widget for this
 * analysis (region-scoped), so the extent is bounded by the analyzed region.
 * Returns null when nothing is zoomed or no sample matches (e.g. an
 * off-worker-only zoom over worker samples). The worker panel wins when both
 * panels are zoomed (getZoomPath can report both).
 */
export function frameSampleTimeExtent(
  samples: readonly ExtentSample[],
  zoomPath: ZoomPath,
  callframeSymbols: CallframeSymbols,
): TimeRange | null {
  const onWorker = zoomPath.worker.length > 0;
  const onOff = zoomPath.offworker.length > 0;
  if (!onWorker && !onOff) return null;
  const names = onWorker ? zoomPath.worker : zoomPath.offworker;
  const wantOff = !onWorker && onOff;
  let min = Infinity;
  let max = -Infinity;
  for (const s of samples) {
    const isOff = s.workerId === OFF_WORKER_WORKER_ID;
    if (isOff !== wantOff) continue;
    const path = displayNamePath(s.callchain, callframeSymbols);
    if (!pathStartsWith(path, names)) continue;
    if (s.timestamp < min) min = s.timestamp;
    if (s.timestamp > max) max = s.timestamp;
  }
  return min <= max ? { startNs: min, endNs: max } : null;
}

// ── Partial-window coverage ───────────────────────────────────────────────────

/** Region data completeness against the resident segment window. */
export type RegionCoverage = "complete" | "partial" | "oversized";

/**
 * Whether the analyzed region is fully backed by resident (parsed) segment data.
 * With no segments (the non-windowed whole-trace load) coverage is always
 * "complete". Otherwise, a segment overlapping the region that is not "parsed"
 * makes the region "partial"; an "oversized" overlap (never resident at the
 * budget) reports "oversized". Consumers MUST badge a non-"complete" region
 * rather than presenting a truncated window as the whole picture.
 */
export function regionCoverage(
  segments: SegmentsSlice,
  range: TimeRange,
): RegionCoverage {
  let sawOverlap = false;
  let oversized = false;
  let partial = false;
  for (const entry of segments.segments.values()) {
    if (entry.extent.endNs < range.startNs || entry.extent.startNs > range.endNs) continue;
    sawOverlap = true;
    if (entry.state === "oversized") oversized = true;
    else if (entry.state !== "parsed") partial = true;
  }
  if (!sawOverlap) return "complete";
  if (oversized) return "oversized";
  if (partial) return "partial";
  return "complete";
}

// ── Titles ────────────────────────────────────────────────────────────────────

/** The sidebar title for a region analysis: "Nms selected" or "Whole trace". */
export function regionTitle(range: TimeRange | null): string {
  if (range === null) return "Whole trace";
  return `${((range.endNs - range.startNs) / 1e6).toFixed(2)}ms selected`;
}
