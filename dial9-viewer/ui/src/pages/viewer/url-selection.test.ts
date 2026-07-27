import { beforeEach, describe, expect, it, vi } from "vitest";
import type { LaneData } from "../../components/canvas/lanes/index.js";
import type { ParsedTrace, TracingSpan } from "../../lib/trace/index.js";
import type { ViewerUrlState } from "./url-state.js";

const { deriveLaneDataMock } = vi.hoisted(() => ({
  deriveLaneDataMock: vi.fn(),
}));

vi.mock("../../components/canvas/lanes/index.js", () => ({
  deriveLaneData: deriveLaneDataMock,
}));

import { resolveUrlSelection } from "./url-selection.js";

function span(spanId: string, parentSpanId: string | null): TracingSpan {
  return {
    start: 0,
    end: 10,
    spanId,
    spanName: spanId,
    fields: {},
    parentSpanId,
    segments: [],
    activeNs: 10,
    depth: 0,
    taskId: null,
  };
}

function urlFor(spanId: string): ViewerUrlState {
  return { selectedSpanId: spanId } as ViewerUrlState;
}

const trace = { customEvents: [] } as unknown as ParsedTrace;

describe("URL span selection restoration", () => {
  beforeEach(() => deriveLaneDataMock.mockReset());

  it("restores the complete ancestor chain from object spans", () => {
    const spans = [span("root", null), span("parent", "root"), span("child", "parent")];
    deriveLaneDataMock.mockReturnValue({
      columnarSpans: undefined,
      spanByIdSingle: new Map(spans.map((s) => [s.spanId, s])),
    } as unknown as LaneData);

    const patch = resolveUrlSelection(trace, urlFor("child"));

    expect(patch.focusedSpanId).toBe("child");
    expect([...patch.spanFocus!.chain]).toEqual(["child", "parent", "root"]);
  });

  it("restores the complete ancestor chain without materializing columnar spans", () => {
    const parents = [null, "root", "parent"] as const;
    deriveLaneDataMock.mockReturnValue({
      columnarSpans: {
        spanIdToRow: new Map([["root", 0], ["parent", 1], ["child", 2]]),
        parentSpanIdAt: (row: number) => parents[row] ?? null,
      },
      spanByIdSingle: new Map(),
    } as unknown as LaneData);

    const patch = resolveUrlSelection(trace, urlFor("child"));

    expect(patch.focusedSpanId).toBe("child");
    expect([...patch.spanFocus!.chain]).toEqual(["child", "parent", "root"]);
  });
});


describe("URL range selection restoration", () => {
  beforeEach(() => deriveLaneDataMock.mockReset());

  it("drops nonintersecting ranges and clamps a partial overlap", () => {
    deriveLaneDataMock.mockReturnValue({
      columnarSpans: undefined,
      spanByIdSingle: new Map(),
      workerIds: [],
      workerSpans: {},
    } as unknown as LaneData);
    const boundedTrace = {
      customEvents: [],
      minTs: 100,
      maxTs: 200,
    } as unknown as ParsedTrace;

    const patch = resolveUrlSelection(boundedTrace, {
      sidebarRange: { startNs: 0, endNs: 50 },
      spawnedRange: { startNs: 150, endNs: 250 },
    } as ViewerUrlState);

    expect(patch.sidebarRange).toBeUndefined();
    expect(patch.spawnedTasksRange).toEqual({ startNs: 150, endNs: 200 });
  });
});

describe("URL task-dump restoration", () => {
  beforeEach(() => deriveLaneDataMock.mockReset());

  it("keeps only capture timestamps present for the anchored task", () => {
    deriveLaneDataMock.mockReturnValue({
      columnarSpans: undefined,
      spanByIdSingle: new Map(),
      workerIds: [],
      workerSpans: {},
    } as unknown as LaneData);
    const withDumps = {
      customEvents: [],
      taskDumps: new Map([
        [
          7,
          [
            { timestamp: 101, callchain: ["a"] },
            { timestamp: 205, callchain: ["b"] },
          ],
        ],
      ]),
    } as unknown as ParsedTrace;

    const patch = resolveUrlSelection(withDumps, {
      taskDump: { taskId: 7, timestamps: [101, 999] },
    });

    expect(patch.taskDump).toEqual({ taskId: 7, timestamps: [101] });
  });
});
