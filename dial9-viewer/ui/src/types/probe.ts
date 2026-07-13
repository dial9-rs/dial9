// Type-checking probe. Imports every API declared in src/types/*.d.ts and
// uses each in a type-checked position, mirroring the real call shapes from
// the pages. This file exists ONLY for `tsc --noEmit`; it is not referenced
// from any Vite input and never ships to dist. Nothing here runs -- the
// exported functions are never called.

import { TraceDecoder, FieldType } from "../../decode.js";
import type { DecodedFrame, EventSchema } from "../../decode.js";
import {
  EVENT_TYPES,
  OFF_WORKER_WORKER_ID,
  parseTrace,
  parseTraceStream,
  parseOne,
  fetchTraces,
  fetchTraceStream,
  fetchTracesStream,
  canStreamDecode,
  formatFrame,
  symbolizeChain,
  deduplicateSamples,
  deriveBlockInPlaceGaps,
} from "../../trace_parser.js";
import type {
  ParsedTrace,
  TraceEvent,
  CpuSample,
  BlockInPlaceGap,
  SymbolFrame,
} from "../../trace_parser.js";
import {
  buildWorkerSpans,
  computeRuntimeGroups,
  buildRuntimeFilterData,
  attachCpuSamples,
  buildActiveTaskTimeline,
  computeSchedulingDelays,
  computePollWakes,
  filterPointsOfInterest,
  flamegraphColor,
  buildFlamegraphTree,
  flattenFlamegraph,
  buildFgData,
  getTraceTimeRange,
  hasCpuProfileSamples,
  buildProcessCpuUsageSeries,
  buildSpanData,
  collectDescendants,
  selectSpanRenderSet,
  enclosingSpans,
  computeSpanLayout,
  analyzeAllocations,
  pollHeatmapColor,
  pollHeatmapColorQuantized,
  makeBarCoalescer,
  pixelDownsampleSpans,
  pixelCoverage,
} from "../../trace_analysis.js";
import type {
  WorkerLane,
  TracingSpan,
  FlamegraphNode,
} from "../../trace_analysis.js";
import {
  formatHumanDuration,
  formatHumanBytes,
  formatFieldValue,
} from "../../format.js";
import {
  MAX_OPEN_BYTES,
  MIN_SEGMENT_SECONDS,
  segmentSpan,
  groupByHost,
  bootTransitions,
  tileSegments,
  segmentGaps,
  accumulateDensity,
  segmentsOverlapping,
  totalBytes,
  densityColor,
} from "../../heatmap.js";
import { lastSegment, isDateLayer } from "../../prefix_detect.js";
import { Dial9Creds } from "../../creds.js";
import { makeTimePanelLayout } from "../../panel_layout.js";
import { createFlamegraph, filterCpuSamples } from "../../flamegraph.js";
import type { FlamegraphInstance } from "../../flamegraph.js";
import {
  treeToFolded,
  treeToInteractiveSvg,
  buildExportRoot,
  layoutTree,
  filenameStem,
  flamegraphColor as exportFlamegraphColor,
  escapeXml,
} from "../../flamegraph_export.js";

// ── decode.js ─────────────────────────────────────────────────────────────

export function probeDecode(buffer: Uint8Array): EventSchema | undefined {
  const dec = new TraceDecoder(buffer);
  dec.enableStreaming();
  const snap = dec.snapshot();
  dec.restore(snap);
  dec.setBuffer(buffer);
  dec.rewindToStart();
  if (!dec.decodeHeader()) throw new Error("Invalid trace header");
  const pos: number = dec.position;
  const len: number = dec.byteLength;
  if (pos > len && dec.needMoreBytes) throw new Error("unreachable");
  let frame: DecodedFrame | null;
  while ((frame = dec.nextFrame()) !== null) {
    // Discriminated-union narrowing over frame.type.
    if (frame.type === "event") {
      const name: string = frame.name;
      const value = frame.values[name];
      if (typeof value === "bigint") void value.toString();
      // Units lookup off the decoder's schema map.
      const units: Record<string, string> | undefined = dec.schemas.get(
        frame.typeId
      )?.units;
      void units;
      const ts: string | undefined = frame.timestamp_ns;
      void ts;
    } else if (frame.type === "string_pool") {
      for (const e of frame.entries) void (e.poolId + e.data.length);
    } else if (frame.type === "stack_pool") {
      for (const e of frame.entries) void e.addrs.join(";");
    } else if (frame.type === "schema_annotations") {
      for (const a of frame.annotations) void (a.fieldIndex + a.key + a.value);
    } else {
      // "schema"
      const optional = frame.fields.some(
        (f) => (f.fieldType & 0x80) !== 0 || f.fieldType === FieldType.I64
      );
      void optional;
    }
  }
  const all: DecodedFrame[] = dec.decodeAll();
  void all;
  const ver: number = dec.version;
  return dec.schemas.get(ver);
}

// ── trace_parser.js ───────────────────────────────────────────────────────

// Mirrors the trace load paths.
export async function probeParser(list: string[]): Promise<ParsedTrace> {
  const credHeaders: Record<string, string> = Dial9Creds.headers();
  let trace: ParsedTrace;
  if (canStreamDecode()) {
    const stream =
      list.length === 1 && list[0] !== undefined
        ? await fetchTraceStream(list[0], { headers: credHeaders })
        : fetchTracesStream(list, { headers: credHeaders });
    trace = await parseTraceStream(stream, {
      onParseProgress: ({ bytesRead, totalBytes: total, eventCount }) => {
        void (bytesRead + (total ?? 0) + eventCount);
      },
    });
  } else {
    const buffer: ArrayBuffer = await fetchTraces(list, {
      headers: credHeaders,
    });
    trace = await parseTrace(buffer, { startTime: 0, endTime: 1e18 });
  }

  // Node-only directory form: async-iterable overload.
  const iter: AsyncIterable<ParsedTrace> = parseTrace("/tmp/traces", {
    sample: 50,
  });
  void iter;
  const one: Promise<ParsedTrace> = parseOne(new Uint8Array());
  void one;

  // Block-in-place gap nullability, encoded and consumed explicitly.
  const gaps: BlockInPlaceGap[] = trace.blockInPlaceGaps;
  for (const g of gaps) void (g.workerId + g.fromTid + g.toTid + g.startNs + g.endNs);
  const rederived: BlockInPlaceGap[] = deriveBlockInPlaceGaps(
    trace.events,
    trace.cpuSamples
  );
  void rederived;
  for (const e of trace.events) {
    if (e.eventType === EVENT_TYPES.WorkerPark && e.tid !== undefined) {
      const tid: number = e.tid; // optional -> narrowed
      void tid;
    }
  }
  for (const s of trace.cpuSamples) {
    if (s.workerId === OFF_WORKER_WORKER_ID) continue; // unattributable
    const cpu: number | null = s.cpu;
    void cpu;
  }
  // minTs/maxTs are null on empty traces; callers must handle it.
  const durNs: number =
    trace.minTs !== null && trace.maxTs !== null ? trace.maxTs - trace.minTs : 0;
  void formatHumanDuration(durNs);
  const offset: number | null = trace.clockOffsetNs;
  void offset;

  // Symbol utilities.
  const frames: SymbolFrame[] = symbolizeChain(
    trace.cpuSamples[0]?.callchain ?? [],
    trace.callframeSymbols
  );
  const first = frames[0];
  if (first !== undefined) {
    const f: { text: string; docsUrl: string | null } = formatFrame(first);
    void f.text;
  }
  const byAddr = formatFrame("0x55cc6d053893", trace.callframeSymbols);
  void byAddr.docsUrl;
  const groups = deduplicateSamples(trace.cpuSamples, trace.callframeSymbols);
  for (const g of groups) void (g.count + g.leaf.length + g.leafRaw.length + g.frames.length);
  return trace;
}

// ── trace_analysis.js ─────────────────────────────────────────────────────

export function probeAnalysis(trace: ParsedTrace): void {
  const evts: TraceEvent[] = trace.events;
  const workerIds: number[] = [0, 1, 2];
  const maxTs: number = trace.maxTs ?? 0;

  const timeRange = getTraceTimeRange(evts, trace.cpuSamples, trace.customEvents);
  if (timeRange !== null) void (timeRange.minTs + timeRange.maxTs + timeRange.durationNs);

  const runtimeGroups = computeRuntimeGroups(workerIds, trace.runtimeWorkers);
  for (const g of runtimeGroups) void (g.name + g.workerIds.length + (g.inferred ? 1 : 0));

  // One caller passes blockInPlaceGaps; another omits it.
  const spanResult = buildWorkerSpans(evts, workerIds, maxTs, trace.blockInPlaceGaps);
  const spanResult3Arg = buildWorkerSpans(evts, workerIds, maxTs);
  void spanResult3Arg;
  const workerSpans: Record<number, WorkerLane> = spanResult.workerSpans;
  const counts = attachCpuSamples(trace.cpuSamples, workerSpans);
  void (counts.pollsWithCpuSamples + counts.pollsWithSchedSamples);

  const lane = workerSpans[0];
  if (lane !== undefined) {
    for (const p of lane.polls) {
      // openEnded polls have unknown duration.
      if (p.openEnded === true) void p.taskId;
      if (p.schedSamples !== undefined) {
        void deduplicateSamples(p.schedSamples, trace.callframeSymbols);
      }
    }
    for (const park of lane.parks) {
      // schedWait is absent on the synthetic trace-end park.
      if (park.schedWait !== undefined && park.schedWait > 100) void park.end;
    }
    for (const a of lane.actives) {
      const ratio: number = a.ratio; // gap-crossing actives are discarded
      void ratio;
    }
  }

  const atResult = buildActiveTaskTimeline(trace.taskSpawnTimes, trace.taskTerminateTimes);
  void atResult.taskFirstPoll.get(1);
  const firstSample = atResult.activeTaskSamples[0];
  if (firstSample !== undefined) void (firstSample.t + firstSample.count);

  const schedDelays = computeSchedulingDelays(workerSpans, workerIds, spanResult.wakesByTask);
  const pointsOfInterest = filterPointsOfInterest("long-poll", workerSpans, workerIds, schedDelays, {
    hasSchedWait: trace.hasSchedWait,
    sortByWorst: true,
    taskInstrumented: trace.taskInstrumented,
  });
  for (const poi of pointsOfInterest) {
    void (poi.time + poi.worker + poi.value);
    if (poi.schedDelay !== undefined) void poi.schedDelay.delay;
  }

  const taskWakes = spanResult.wakesByTask[7];
  const pollWakes = computePollWakes(lane !== undefined ? lane.polls : [], taskWakes);
  for (const pw of pollWakes) {
    if (pw !== null) void (pw.effectiveWake + pw.wake.timestamp);
  }

  void hasCpuProfileSamples(trace.cpuSamples);
  const usage = buildProcessCpuUsageSeries(trace.customEvents, 8);
  void (usage.maxCores + usage.avgCores + (usage.availableParallelism ?? 0));
  const interval = usage.intervals[0];
  if (interval !== undefined) void (interval.cores + (interval.totalPercent ?? 0));

  const rtFilter = buildRuntimeFilterData(trace.cpuSamples, trace.runtimeWorkers);
  for (const o of rtFilter.options) void (o.name + o.sampleCount);

  // Span data.
  const spanData = buildSpanData(trace.customEvents);
  const allSpans: TracingSpan[] = spanData.allSpans;
  const renderSet = selectSpanRenderSet({
    allSpans,
    focusedSpanId: null,
    childrenByParent: spanData.childrenByParent,
  });
  const descendants: Set<string> = collectDescendants(
    renderSet.map((s) => s.spanId),
    spanData.childrenByParent
  );
  void descendants;
  const enclosing = enclosingSpans(allSpans, {
    timestamp: maxTs,
    fields: { worker_id: 3 },
  });
  for (const s of enclosing) void (s.depth + s.activeNs + s.segments.length);
  const layout = computeSpanLayout({
    spans: renderSet,
    viewStart: 0,
    viewEnd: maxTs,
    drawW: 1500,
    panelH: 120,
    clusterXPx: 4,
    barH: 10,
  });
  const bucket = layout.buckets[0];
  if (bucket !== undefined) {
    const rep: TracingSpan = bucket.representative; // generic preserved
    void (rep.spanName + bucket.x1 + bucket.y);
  }

  // Allocation analysis.
  const alloc = analyzeAllocations(trace.allocEvents, trace.freeEvents, {
    events: trace.events,
    tidToWorker: trace.tidToWorker,
    memoryOverflows: trace.memoryOverflows,
  });
  void (alloc.summary.leakedBytes + alloc.summary.totalDroppedFrees);
  void alloc.perTask.get(1)?.estimatedBytes;

  // Rendering helpers.
  const color: string = pollHeatmapColorQuantized(12345);
  const color2: string = pollHeatmapColor(12345);
  void (color + color2 + flamegraphColor("frame"));
  const merge = makeBarCoalescer((x, w, fill) => {
    void (x + w + fill.length);
  }, 1);
  merge.push(0, 2, "#ff4444");
  merge.flush();
  if (lane !== undefined) {
    const reps = pixelDownsampleSpans(lane.polls, 0, 0, maxTs, 1500, (s) => s.end - s.start);
    for (const r of reps) void r.spawnLoc; // element type preserved
    const cov: Float64Array = pixelCoverage(lane.polls, 0, 0, maxTs, 1500);
    void cov[0];
  }

  // Flamegraph tree building.
  const fg = buildFgData(trace.cpuSamples, trace.callframeSymbols);
  if (fg !== null) void (fg.totalSamples + fg.maxDepth + fg.nodes.length);
  const tree = buildFlamegraphTree(trace.cpuSamples, trace.callframeSymbols);
  const flat = flattenFlamegraph(tree, trace.cpuSamples.length);
  const flatNode = flat.nodes[0];
  if (flatNode !== undefined) void (flatNode.x + flatNode.w + flatNode.depth);
}

// ── format.js ─────────────────────────────────────────────────────────────

export function probeFormat(): string {
  return (
    formatHumanDuration(1_500_000) +
    formatHumanBytes(1536) +
    formatFieldValue("42", "ns") +
    formatFieldValue(7n) // schema-driven values include bigint
  );
}

// ── heatmap.js ────────────────────────────────────────────────────────────

interface BrowseSegment {
  key: string;
  size: number;
  start: number;
  end: number;
  service: string | undefined;
  host: string | undefined;
  bootId: string | undefined;
}

export function probeHeatmap(heatmapSegments: BrowseSegment[]): string {
  const rows = groupByHost(heatmapSegments);
  const row = rows[0];
  if (row === undefined) return densityColor(0);
  // Generic preserved: row.segments keeps the caller's extra `key` field.
  const key: string | undefined = row.segments[0]?.key;
  void key;
  const tiled = tileSegments(row.segments);
  for (const t of tiled) void (t.realEnd ?? t.end);
  const gaps = segmentGaps(row.segments);
  const boots = bootTransitions(row.segments);
  for (const b of boots) void (b.time + b.fromBoot + b.toBoot);
  const cols: Float64Array = accumulateDensity(tiled, 0, 3600, 1200);
  void (cols[0] ?? 0);
  const hit = segmentsOverlapping(row.segments, 100, 200);
  if (totalBytes(hit) > MAX_OPEN_BYTES) void hit[0]?.key;
  const span = segmentSpan({ start: 5 }); // end omitted: floored span
  void (span.end - span.start + MIN_SEGMENT_SECONDS + gaps.length);
  return densityColor(0.5);
}

// ── prefix_detect.js ──────────────────────────────────────────────────────

export function probePrefixDetect(prefixes: string[]): boolean {
  const seg: string = lastSegment("traces/2026-06-12/");
  return isDateLayer(prefixes) && seg.length > 0;
}

// ── creds.js ──────────────────────────────────────────────────────────────

export async function probeCreds(blob: string): Promise<void> {
  const parsed = Dial9Creds.parse(blob);
  const result = await Dial9Creds.set({ ...parsed, autoDetectRegion: true });
  if (!result.ok) void (result.error ?? "unknown");
  void (result.region ?? "");
  const stored = Dial9Creds.get();
  if (stored !== null) void (stored.accessKeyId + (stored.sessionToken ?? ""));
  const active: boolean = Dial9Creds.has();
  if (!active) Dial9Creds.clear();
  const check = await Dial9Creds.check("some-bucket");
  void check.ok;
  const buckets: string[] = await Dial9Creds.listBuckets();
  void buckets;
  const headers: Record<string, string> = Dial9Creds.headers();
  await fetch("/api/browse", { headers });
  Dial9Creds._setStorage({
    getItem: () => null,
    setItem: () => {},
    removeItem: () => {},
  });
}

// ── panel_layout.js ───────────────────────────────────────────────────────

export function probePanelLayout(
  panel: HTMLElement,
  scrollbarW: number | undefined,
  viewStart: number,
  viewEnd: number
): number {
  const base = makeTimePanelLayout(panel.clientWidth, 100, scrollbarW, viewStart, viewEnd);
  const x: number = base.nsToPanelX(viewStart + 1);
  const xc: number = base.nsToPanelXClamped(viewEnd + 1);
  const ns: number = base.panelXToNs(x);
  return base.pw + base.labelW + base.drawW + xc + ns;
}

// ── flamegraph.js ─────────────────────────────────────────────────────────

export function probeFlamegraph(container: HTMLElement, trace: ParsedTrace): void {
  const fg: FlamegraphInstance = createFlamegraph(container, () => {});

  // Filter shape; generic keeps CpuSample fields.
  const samples: CpuSample[] = filterCpuSamples(trace.cpuSamples, null, null);
  const ranged = filterCpuSamples(trace.cpuSamples, 0, 1e18);
  void ranged[0]?.tid;

  // CPU shape.
  fg.setData(samples, trace.callframeSymbols, {
    exportTitle: "CPU flamegraph",
    runtimeWorkers: trace.runtimeWorkers,
  });

  // Heap pseudo-samples with weights + formatCount.
  const heapSamples = samples.map((s) => ({ ...s, weight: 8, allocWeight: 1 }));
  fg.setData(heapSamples, trace.callframeSymbols, {
    workerLabel: "Allocations",
    offworkerLabel: "Off-worker allocations",
    exportTitle: "Heap flamegraph (by bytes)",
    exportFormatValue: (count) => `~${formatHumanBytes(count)}`,
    formatCount: (count, total, self, treeNode) => {
      const pct = ((count / total) * 100).toFixed(1);
      const allocs =
        treeNode !== null && treeNode !== undefined && treeNode.allocCount != null
          ? Math.round(treeNode.allocCount)
          : null;
      return `~${count} (${pct}%) ${self} self${allocs !== null ? ` ${allocs} allocs` : ""}`;
    },
  });

  // API mode: minimal toFgTree node into setTreeDirect.
  function toFgTree(node: { name: string; count: number; self: number; children?: { name: string; count: number; self: number }[] }): FlamegraphNode {
    const m = new Map<string, FlamegraphNode>();
    for (const child of node.children ?? []) m.set(child.name, toFgTree(child));
    return { name: node.name, count: node.count, self: node.self, children: m };
  }
  fg.setTreeDirect(toFgTree({ name: "(all)", count: 10, self: 0 }), 10);

  fg.resize();
  if (fg.isZoomed()) {
    const path: { worker: string[]; offworker: string[] } = fg.getZoomPath();
    fg.zoomToPath("worker", path.worker);
  }
  const consumed: boolean = fg.handleEscape();
  void consumed;
  fg.destroy();
}

// ── flamegraph_export.js (consumed via flamegraph.js export menu) ──────────

export function probeFlamegraphExport(tree: FlamegraphNode): string {
  const panels = [
    { label: "Worker threads", tree },
    { label: "Off-worker (sampler thread)", tree: null },
  ];
  const root: FlamegraphNode | null = buildExportRoot(panels);
  if (root !== null) {
    const { frames, maxDepth } = layoutTree(root, 0.1);
    const f = frames[0];
    if (f !== undefined) void (f.sTime + f.eTime + f.depth + f.node.count);
    void maxDepth;
  }
  const svg: string = treeToInteractiveSvg(panels, {
    title: "dial9 flamegraph",
    formatValue: (count) => `${Math.round(count)} samples`,
  });
  const folded: string = treeToFolded(tree);
  const stem: string = filenameStem("Flamegraph — worker 0");
  return svg + folded + stem + exportFlamegraphColor("root") + escapeXml("<&>");
}
