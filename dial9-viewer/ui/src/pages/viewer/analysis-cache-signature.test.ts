import { describe, expect, it } from "vitest";
import {
  pollFlamegraphCacheSignature,
  regionComputedCacheSignature,
  regionWidgetCacheSignature,
} from "./analysis-cache-signature.js";

describe("analysis cache signatures", () => {
  it("distinguishes polls with identical timing but different task IDs", () => {
    const trace = {};
    const first = pollFlamegraphCacheSignature({
      trace,
      poll: { start: 100, end: 200, taskId: 1 },
      section: "cpu",
      sampleCount: 3,
    });
    const second = pollFlamegraphCacheSignature({
      trace,
      poll: { start: 100, end: 200, taskId: 2 },
      section: "cpu",
      sampleCount: 3,
    });

    expect(second).not.toBe(first);
  });

  it("invalidates poll and region derivations when parsed trace identity changes", () => {
    const firstTrace = {};
    const replacementTrace = {};
    const poll = { start: 100, end: 200, taskId: 1 };

    const firstPoll = pollFlamegraphCacheSignature({
      trace: firstTrace,
      poll,
      section: "cpu",
      sampleCount: 3,
    });
    expect(
      pollFlamegraphCacheSignature({
        trace: firstTrace,
        poll,
        section: "cpu",
        sampleCount: 3,
      }),
    ).toBe(firstPoll);
    expect(
      pollFlamegraphCacheSignature({
        trace: replacementTrace,
        poll,
        section: "cpu",
        sampleCount: 3,
      }),
    ).not.toBe(firstPoll);

    const firstComputed = regionComputedCacheSignature({
      trace: firstTrace,
      mode: "cpu",
      range: { startNs: 100, endNs: 200 },
      heapMode: "bytes",
      groupBy: "leaf",
    });
    const replacementComputed = regionComputedCacheSignature({
      trace: replacementTrace,
      mode: "cpu",
      range: { startNs: 100, endNs: 200 },
      heapMode: "bytes",
      groupBy: "leaf",
    });
    expect(replacementComputed).not.toBe(firstComputed);
    expect(
      regionWidgetCacheSignature({
        trace: replacementTrace,
        computed: "same-derived-view",
        blockingFlame: false,
      }),
    ).not.toBe(
      regionWidgetCacheSignature({
        trace: firstTrace,
        computed: "same-derived-view",
        blockingFlame: false,
      }),
    );
  });

  it("reuses heap derivation while invalidating the widget weight variant", () => {
    const trace = {};
    const shared = {
      trace,
      mode: "heap" as const,
      range: { startNs: 100, endNs: 200 },
      groupBy: "leaf" as const,
    };
    const bytesComputed = regionComputedCacheSignature({
      ...shared,
      heapMode: "bytes",
    });
    const countComputed = regionComputedCacheSignature({
      ...shared,
      heapMode: "count",
    });
    expect(countComputed).toBe(bytesComputed);
    expect(
      regionWidgetCacheSignature({
        trace,
        computed: bytesComputed,
        blockingFlame: false,
        variant: "bytes",
      }),
    ).not.toBe(
      regionWidgetCacheSignature({
        trace,
        computed: countComputed,
        blockingFlame: false,
        variant: "count",
      }),
    );
  });
});
