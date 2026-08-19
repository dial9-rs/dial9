import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";
import { beforeAll, describe, expect, it } from "vitest";
import { deriveLaneData } from "../../components/canvas/lanes/index.js";
import {
  parseTraceBuffer,
  type ParsedTrace,
  type PollSpan,
} from "../../lib/trace/index.js";
import { encodeViewState } from "../../lib/url/index.js";
import { createViewerStore } from "./store.js";
import {
  mirrorViewerToQuery,
  projectViewerState,
  VIEWER_STATE_OWNERSHIP,
} from "./url-state.js";
import { resolveUrlSelection } from "./url-selection.js";
import { createViewerReconstruction } from "./viewer-reconstruction.js";
import { taskIndexFor } from "./tasks-model.js";

let settledTrace: ParsedTrace;
let reconstructedTrace: ParsedTrace;

beforeAll(async () => {
  const fileBytes = readFileSync(
    fileURLToPath(new URL("../../../public/demo-trace.bin", import.meta.url)),
  );
  const raw =
    fileBytes[0] === 0x1f && fileBytes[1] === 0x8b
      ? new Uint8Array(gunzipSync(fileBytes))
      : new Uint8Array(fileBytes);
  [settledTrace, reconstructedTrace] = await Promise.all([
    parseTraceBuffer(raw),
    parseTraceBuffer(raw),
  ]);

  const start = settledTrace.minTs!;
  const end = settledTrace.maxTs!;
  for (const trace of [settledTrace, reconstructedTrace]) {
    trace.timeFiltered = true;
    trace.filterStartTime = start;
    trace.filterEndTime = end;
  }
});

function atFraction(trace: ParsedTrace, fraction: number): number {
  return Math.round(trace.minTs! + (trace.maxTs! - trace.minTs!) * fraction);
}

function lanePolls(lane: ReturnType<typeof deriveLaneData>): PollSpan[] {
  const polls: PollSpan[] = [];
  for (const workerId of lane.workerIds) {
    const workerPolls = lane.workerSpans[workerId]?.polls;
    if (workerPolls !== undefined) polls.push(...workerPolls);
  }
  return polls;
}

function firstOwnedSpan(
  lane: ReturnType<typeof deriveLaneData>,
): { spanId: string; start: number; end: number; taskId: number } {
  const span = lane.allSpans.find((candidate) => candidate.taskId != null);
  if (span?.taskId != null) {
    return {
      spanId: span.spanId,
      start: span.start,
      end: span.end,
      taskId: span.taskId,
    };
  }

  const columnar = lane.columnarSpans;
  if (columnar !== undefined) {
    for (let row = 0; row < columnar.length; row++) {
      const taskId = columnar.taskIdAt(row);
      if (taskId != null) {
        return {
          spanId: columnar.spanIdAt(row),
          start: columnar.startAt(row),
          end: columnar.endAt(row),
          taskId,
        };
      }
    }
  }
  throw new Error("demo trace has no task-owned span");
}

function normalizedProjection(
  state: Parameters<typeof projectViewerState>[0],
): { query: [string, string][]; hash: string } {
  const projected = projectViewerState(state);
  const query = new URLSearchParams();
  mirrorViewerToQuery(query, projected);
  return {
    query: [...query.entries()].sort(([a], [b]) => a.localeCompare(b)),
    hash: encodeViewState(projected),
  };
}

describe("viewer deep-link reconstruction", () => {
  it("selects focus_task even when no span matches the focus window", () => {
    const task = taskIndexFor(settledTrace).rows.find((row) => row.pollCount > 0)!;
    const offset = settledTrace.clockOffsetNs ?? 0;
    const focusStart = task.firstPollStart + offset;
    const store = createViewerStore({ scheduler: () => {} });
    const reconstruction = createViewerReconstruction(store, {
      search:
        `?focus_start=${focusStart}&focus_end=${focusStart + 1_000_000}` +
        `&focus_span_name=definitely-absent&focus_task=${task.taskId}`,
      hash: "",
    });

    reconstruction.applyLoadedTrace(settledTrace, "source");

    expect(store.getState().selection.selectedTaskId).toBe(task.taskId);
  });

  it("does not infer a span from an unnamed task exemplar focus", () => {
    const lane = deriveLaneData(settledTrace);
    const overlappingSpan = firstOwnedSpan(lane);
    const task = taskIndexFor(settledTrace).rows.find(
      (row) => row.pollCount > 0 && row.taskId !== overlappingSpan.taskId,
    )!;
    const offset = settledTrace.clockOffsetNs ?? 0;
    const store = createViewerStore({ scheduler: () => {} });
    const reconstruction = createViewerReconstruction(store, {
      search:
        `?focus_start=${overlappingSpan.start + offset}` +
        `&focus_end=${overlappingSpan.end + offset}` +
        `&focus_task=${task.taskId}`,
      hash: "",
    });

    reconstruction.applyLoadedTrace(settledTrace, "source");

    expect(store.getState().selection).toMatchObject({
      selectedTaskId: task.taskId,
      spanFocus: null,
      focusedSpanId: null,
    });
  });

  it("reconstructs every URL-owned analytical value after the trace loads", () => {
    const source = createViewerStore({ scheduler: () => {} });
    createViewerReconstruction(source, { search: "", hash: "" }).applyLoadedTrace(
      settledTrace,
      "source",
    );

    const lane = deriveLaneData(settledTrace);
    const poll = lanePolls(lane)
      .find((candidate) => candidate.taskId > 0)!;
    const spanId =
      lane.allSpans[0]?.spanId ??
      lane.columnarSpans?.spanIdAt(0);
    const event = settledTrace.customEvents.find(
      (candidate) => !candidate.name.startsWith("Span"),
    )!;
    const [dumpTaskId, dumps] = [...settledTrace.taskDumps.entries()].find(
      ([, candidates]) => candidates.length > 0,
    )!;
    expect(spanId).toBeDefined();

    const range = {
      startNs: atFraction(settledTrace, 0.25),
      endNs: atFraction(settledTrace, 0.75),
    };
    const selection = resolveUrlSelection(settledTrace, {
      selectedSpanId: spanId!,
      focusedSpanId: spanId!,
      poll: { startNs: poll.start, taskId: poll.taskId },
      taskDump: { taskId: dumpTaskId, timestamps: [dumps[0]!.timestamp] },
      pinnedEventTs: event.timestamp,
      sidebarRange: range,
      spawnedRange: range,
    });
    expect(selection).toMatchObject({
      focusedSpanId: spanId,
      pollDetail: { start: poll.start, taskId: poll.taskId },
      pinnedEvent: { timestamp: event.timestamp },
      sidebarRange: range,
      spawnedTasksRange: range,
    });

    source.update("viewport", {
      viewStart: atFraction(settledTrace, 0.2),
      viewEnd: atFraction(settledTrace, 0.8),
    });
    source.update("selection", {
      ...selection,
      selectedTaskId: poll.taskId,
    });
    source.update("poi", {
      filter: "long-poll",
      sortKey: "time",
      sortDir: "asc",
      index: 4,
      railTab: "tasks",
      taskSort: "lifetime",
      taskSortDir: "asc",
      taskIndex: 7,
    });
    source.update("uiPrefs", {
      trackOrder: ["events", "spans", "cpu"],
      collapsed: { queue: true },
      collapsedRuntimes: { runtime_a: true },
      collapsedRuntimeMetrics: { runtime_b: true },
      sidebarWidth: 444,
      railWidth: 460,
      taskColWidths: { loc: 260, polls: 48 },
      issueColWidths: { dot: 14, kind: 120 },
      lanesViewportHeight: 280,
      lanesScrollTop: 96,
      selectedSpanNames: new Set(["http, request", "poll"]),
      selectedEventNames: new Set(["flush"]),
      spanFilter: "request_id",
      spanPctFilter: 99,
      timeMode: "abs",
      tz: "local",
      stacksAsFlamegraph: true,
    });
    source.update("view", {
      fieldCharts: [
        {
          id: "fc-1",
          eventName: event.name,
          fieldName: "value",
          kind: "counter",
        },
      ],
      inspectorTab: "related",
      expandedPollGroups: new Set(["cpu-0", "sched-1"]),
      pollFlamegraphSection: "sched",
      pollWorkerZoom: ["root", "poll"],
      pollOffworkerZoom: ["off", "wait"],
      relatedCollapsed: { "Same task": true },
      relatedExpand: { "Same span": { before: 25, after: 50 } },
      relatedCorrelate: { key: "request,id", val: "abc/123" },
      regionMode: "heap",
      regionHeapMode: "count",
      regionGroupBy: "full",
      regionWorkerZoom: ["root", "alloc"],
      regionOffworkerZoom: ["off", "alloc"],
      regionInspectFocus: "tokio::runtime::task::harness::poll_future",
      spanNavIndex: 5,
    });

    const projected = projectViewerState(source.getState());
    const query = new URLSearchParams("?trace=fixture.bin");
    mirrorViewerToQuery(query, projected);
    const hash = encodeViewState(projected);

    const ownedParams = Object.values(VIEWER_STATE_OWNERSHIP)
      .flatMap((slice) => Object.values(slice))
      .flatMap((field) => field.kind === "url" ? [...field.params] : []);
    const emittedParams = [
      ...[...query.keys()].filter((key) => key !== "trace"),
      ...[...new URLSearchParams(hash).keys()]
        .filter((key) => key !== "v")
        .map((key) => `#${key}`),
    ];
    expect([...new Set(emittedParams)].sort()).toEqual(
      [...new Set(ownedParams)].sort(),
    );

    const fresh = createViewerStore({ scheduler: () => {} });
    const reconstruction = createViewerReconstruction(fresh, {
      search: `?${query.toString()}`,
      hash: hash === "" ? "" : `#${hash}`,
    });
    reconstruction.applyLoadedTrace(reconstructedTrace, "source");

    expect(normalizedProjection(fresh.getState())).toEqual(
      normalizedProjection(source.getState()),
    );
  });

  it("re-resolves poll and custom-event references against a reparsed trace", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const reconstruction = createViewerReconstruction(store, {
      search: "",
      hash: "",
    });
    reconstruction.applyLoadedTrace(settledTrace, "source");

    const oldLane = deriveLaneData(settledTrace);
    const oldPoll = lanePolls(oldLane)
      .find((candidate) => candidate.taskId > 0)!;
    const oldEvent = settledTrace.customEvents.find(
      (candidate) => !candidate.name.startsWith("Span"),
    )!;
    const oldSelection = resolveUrlSelection(settledTrace, {
      poll: { startNs: oldPoll.start, taskId: oldPoll.taskId },
      pinnedEventTs: oldEvent.timestamp,
    });
    store.update("selection", oldSelection);
    store.update("poi", { index: 4, taskIndex: 7 });
    store.update("view", {
      expandedPollGroups: new Set(["cpu-0"]),
      pollFlamegraphSection: "sched",
      pollWorkerZoom: ["poll", "worker"],
      relatedCorrelate: { key: "request_id", val: "same-source" },
      regionWorkerZoom: ["region", "worker"],
      regionInspectFocus: "same::frame",
      spanNavIndex: 5,
    });
    expect(store.getState().selection.pollDetail).toBe(oldPoll);
    expect(store.getState().selection.pinnedEvent?.events).toContain(oldEvent);

    reconstruction.applyLoadedTrace(reconstructedTrace, "reparse");

    const selection = store.getState().selection;
    const replacementPolls = lanePolls(deriveLaneData(reconstructedTrace));
    expect(selection.pollDetail).not.toBe(oldPoll);
    expect(replacementPolls).toContain(selection.pollDetail);
    expect(selection.pinnedEvent?.events).not.toContain(oldEvent);
    for (const event of selection.pinnedEvent?.events ?? []) {
      expect(reconstructedTrace.customEvents).toContain(event);
    }
    expect(store.getState().poi).toMatchObject({ index: 4, taskIndex: 7 });
    expect(store.getState().view).toMatchObject({
      expandedPollGroups: new Set(["cpu-0"]),
      pollFlamegraphSection: "sched",
      pollWorkerZoom: ["poll", "worker"],
      relatedCorrelate: { key: "request_id", val: "same-source" },
      regionWorkerZoom: ["region", "worker"],
      regionInspectFocus: "same::frame",
      spanNavIndex: 5,
    });
  });

  it("resets source-scoped URL state while retaining reusable controls on source replacement", () => {
    const store = createViewerStore({ scheduler: () => {} });
    const reconstruction = createViewerReconstruction(store, {
      search: "",
      hash: "",
    });
    reconstruction.applyLoadedTrace(settledTrace, "source");

    const lane = deriveLaneData(settledTrace);
    const poll = lanePolls(lane)
      .find((candidate) => candidate.taskId > 0)!;
    const event = settledTrace.customEvents.find(
      (candidate) => !candidate.name.startsWith("Span"),
    )!;
    store.update("selection", {
      ...resolveUrlSelection(settledTrace, {
        poll: { startNs: poll.start, taskId: poll.taskId },
        pinnedEventTs: event.timestamp,
        sidebarRange: {
          startNs: atFraction(settledTrace, 0.25),
          endNs: atFraction(settledTrace, 0.75),
        },
      }),
      selectedTaskId: poll.taskId,
      hoveredWakerTaskId: poll.taskId,
    });
    store.update("poi", {
      filter: "long-poll",
      sortKey: "time",
      sortDir: "asc",
      index: 4,
      railTab: "tasks",
      taskSort: "lifetime",
      taskSortDir: "asc",
      taskIndex: 7,
    });
    store.update("uiPrefs", {
      spanFilter: "request_id",
      timeMode: "abs",
      stacksAsFlamegraph: true,
    });
    store.update("view", {
      inspectorTab: "related",
      expandedPollGroups: new Set(["cpu-0"]),
      pollFlamegraphSection: "sched",
      pollWorkerZoom: ["old", "poll"],
      pollOffworkerZoom: ["old", "wait"],
      relatedCollapsed: { "Same task": true },
      relatedExpand: { "Same span": { before: 25, after: 50 } },
      relatedCorrelate: { key: "request_id", val: "old" },
      regionMode: "heap",
      regionHeapMode: "count",
      regionGroupBy: "full",
      regionWorkerZoom: ["old", "alloc"],
      regionOffworkerZoom: ["old", "free"],
      regionInspectFocus: "old::frame",
      spanNavIndex: 5,
    });

    reconstruction.applyLoadedTrace(reconstructedTrace, "source");

    const state = store.getState();
    expect(state.selection).toEqual({
      selectedTaskId: null,
      spanFocus: null,
      focusedSpanId: null,
      pinnedEvent: null,
      pollDetail: null,
      taskDump: null,
      sidebarRange: null,
      hoveredWakerTaskId: null,
      spawnedTasksRange: null,
    });
    expect(state.poi).toEqual({
      filter: "long-poll",
      sortKey: "time",
      sortDir: "asc",
      index: -1,
      railTab: "tasks",
      taskSort: "lifetime",
      taskSortDir: "asc",
      taskIndex: -1,
    });
    expect(state.uiPrefs).toMatchObject({
      spanFilter: "request_id",
      timeMode: "abs",
      stacksAsFlamegraph: true,
    });
    expect(state.view).toEqual({
      fieldCharts: [],
      inspectorTab: "task",
      expandedPollGroups: new Set(),
      pollFlamegraphSection: "cpu",
      pollWorkerZoom: [],
      pollOffworkerZoom: [],
      relatedCollapsed: {},
      relatedExpand: {},
      relatedCorrelate: null,
      regionMode: "heap",
      regionHeapMode: "count",
      regionGroupBy: "full",
      regionWorkerZoom: [],
      regionOffworkerZoom: [],
      regionInspectFocus: null,
      spanNavIndex: -1,
    });

    const query = new URLSearchParams();
    mirrorViewerToQuery(query, projectViewerState(state));
    for (const staleParam of [
      "task",
      "span",
      "span-focus",
      "poll",
      "task-dump",
      "event",
      "region",
      "spawned",
      "issue-index",
      "task-index",
      "inspector",
      "poll-section",
      "poll-expanded",
      "poll-worker-zoom",
      "poll-offworker-zoom",
      "related-collapsed",
      "related-expand",
      "related-key",
      "related-value",
      "analysis-worker-zoom",
      "analysis-offworker-zoom",
      "analysis-inspect",
      "span-index",
    ]) {
      expect(query.has(staleParam), staleParam).toBe(false);
    }
    expect(query.get("issue")).toBe("long-poll");
    expect(query.get("issue-sort")).toBe("time,asc");
    expect(query.get("rail")).toBe("tasks");
    expect(query.get("task-sort")).toBe("lifetime,asc");
    expect(query.get("span-filter")).toBe("request_id");
    expect(query.get("stack-view")).toBe("flame");
    expect(query.get("analysis")).toBe("heap");
    expect(query.get("heap-weight")).toBe("count");
    expect(query.get("blocking-group")).toBe("full");
  });
});
