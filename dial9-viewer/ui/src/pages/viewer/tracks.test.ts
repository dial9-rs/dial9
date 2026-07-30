import { describe, expect, it } from "vitest";
import { visibleTracks, type TracksViewModel } from "./tracks.js";
import type { TrackId } from "../../lib/canvas/track-layout.js";
import type { AxisInputs } from "./axis.js";
import type { CpuInputs } from "./cpu.js";

/** A view model carrying only the fields visibleTracks reads; axis/cpu are
 *  inert here (the column's order + filter logic never looks at them). */
function vm(over: Partial<TracksViewModel> = {}): TracksViewModel {
  return {
    hasTrace: true,
    taskSelected: false,
    viewStart: 0,
    viewEnd: 1000,
    axis: {} as AxisInputs,
    cpu: {} as CpuInputs,
    fieldCharts: [],
    trackOrder: [],
    collapsed: {},
    emptyTracks: new Set<TrackId>(),
    lanesViewportHeight: 130,
    ...over,
  };
}

const ids = (m: TracksViewModel): TrackId[] => visibleTracks(m).map((t) => t.id);

describe("visibleTracks", () => {
  it("places task detail directly under the worker lanes when a task is selected", () => {
    expect(ids(vm({ taskSelected: true }))).toEqual([
      "timeline",
      "lanes",
      "task-detail",
      "cpu",
      "queue",
      "spans",
      "events",
    ]);
  });

  it("omits the selection-only task-detail track with nothing selected", () => {
    expect(ids(vm())).not.toContain("task-detail");
  });

  it("drops a track the trace has no data for", () => {
    const shown = ids(vm({ emptyTracks: new Set<TrackId>(["cpu"]) }));
    expect(shown).not.toContain("cpu");
    expect(shown).toContain("queue");
  });

  it("keeps every track before a trace loads, so the empty column is intact", () => {
    // Pre-load, every track is trivially empty; hiding them would blank the
    // drop zone's backdrop rather than communicate anything.
    const shown = ids(vm({ hasTrace: false, emptyTracks: new Set<TrackId>(["cpu"]) }));
    expect(shown).toContain("cpu");
  });

  it("hides an empty track rather than merely collapsing it", () => {
    // Collapse keeps the row (it can be re-expanded); emptiness removes it.
    const collapsedOnly = ids(vm({ collapsed: { cpu: true } }));
    expect(collapsedOnly).toContain("cpu");
    expect(ids(vm({ emptyTracks: new Set<TrackId>(["cpu"]) }))).not.toContain("cpu");
  });

  it("applies emptiness after the user's reorder, not before", () => {
    const shown = ids(
      vm({
        trackOrder: ["events", "spans", "queue", "cpu"],
        emptyTracks: new Set<TrackId>(["queue"]),
      }),
    );
    expect(shown).toEqual(["timeline", "lanes", "events", "spans", "cpu"]);
  });

  it("includes URL-defined field charts in the manageable order", () => {
    expect(
      ids(
        vm({
          fieldCharts: [
            {
              id: "field-chart-1",
              eventName: "runtime.metrics",
              fieldName: "jobs",
              kind: "counter",
            },
          ],
          trackOrder: ["field-chart-1", "events"],
        }),
      ),
    ).toEqual([
      "timeline",
      "lanes",
      "field-chart-1",
      "events",
      "cpu",
      "queue",
      "spans",
    ]);
  });
});
