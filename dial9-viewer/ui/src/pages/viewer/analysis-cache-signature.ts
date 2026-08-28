import type { RegionAnalysisMode } from "../../types/state.js";
import type { TimeRange } from "../../types/trace.js";

const traceIds = new WeakMap<object, number>();
let nextTraceId = 1;

function traceIdentity(trace: object | null): number {
  if (trace === null) return 0;
  const existing = traceIds.get(trace);
  if (existing !== undefined) return existing;
  const id = nextTraceId++;
  traceIds.set(trace, id);
  return id;
}

export function pollFlamegraphCacheSignature(args: {
  trace: object | null;
  poll: { start: number; end: number; taskId: number };
  section: "cpu" | "sched";
  sampleCount: number;
}): string {
  return `${traceIdentity(args.trace)}:${args.poll.start}:${args.poll.taskId}-${args.poll.end}:${args.section}:${args.sampleCount}`;
}

export function regionComputedCacheSignature(args: {
  trace: object;
  mode: RegionAnalysisMode | null;
  range: TimeRange;
  heapMode: "bytes" | "count";
  groupBy: "leaf" | "full";
}): string {
  const range = `${args.range.startNs}-${args.range.endNs}`;
  if (args.mode === "heap") {
    return `${traceIdentity(args.trace)}:heap:${range}`;
  }
  if (args.mode === "blocking") {
    return `${traceIdentity(args.trace)}:blocking:${range}:${args.groupBy}`;
  }
  return `${traceIdentity(args.trace)}:${args.mode ?? "empty"}:${range}`;
}

export function regionWidgetCacheSignature(args: {
  trace: object;
  computed: string;
  blockingFlame: boolean;
  /** Rendering variant over the same derived data, such as heap Bytes/Count. */
  variant?: string;
}): string {
  const variant = args.variant === undefined ? "" : `:${args.variant}`;
  return `${traceIdentity(args.trace)}:${args.computed}${variant}${args.blockingFlame ? ":flame" : ""}`;
}
